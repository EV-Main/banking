//! Integration tests for the user-wallet withdrawal saga — real Postgres **and**
//! TigerBeetle (no mocks, per the project rules). They run when `DATABASE_URL` is set
//! and a TigerBeetle replica is reachable (`nix run .#db` + `.#tb`), and skip
//! otherwise. Each test uses a fresh provisioned user, so runs are isolated on shared
//! infrastructure. The relay is driven explicitly via `Relay::drain` to apply
//! committed events deterministically; the custody broadcast is the [`StubCustody`]
//! no-op, so the saga's two-phase ledger behaviour (reserve → settle/void) is what's
//! under test.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};

use async_trait::async_trait;
use domain::{
	auth::AuthSubject,
	balance::{FeePayee, LedgerAccountKey},
	error::DomainError,
	money::{Network, TxRef, Usdt, WalletAddress},
	users::{Email, UserId},
	withdrawals::{WithdrawalId, WithdrawalState},
};
use piggybank_core::{
	application::{balance as balance_app, withdrawal_limits::WithdrawalLimits, withdrawals as withdrawal_app},
	config::KycGate,
	infrastructure::{custody::StubCustody, deposits::PgDeposits, dispatcher::Dispatcher, operations, outflow::PgOutflowPolicy, relay::Relay, users::PgUsers, withdrawals::PgWithdrawals},
	ports::{
		BroadcastRequest, Custody, CustodyError, DepositAddresses, OutflowPolicy, PayoutStanding, UserRepository, WithdrawalRepository,
		audit::RequestAudit,
		ledger::Ledger,
		withdrawals::{LimitProfile, Screening},
	},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

// Address alphabets for the test-only deposit-address stub (defined at the bottom).
const BASE58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const BASE64URL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Serialises the tests that drive a `Dispatcher` sweep or toggle the read-only
/// kill-switch.
///
/// Both are **global** by construction: `Dispatcher::sweep` walks every `queued`
/// withdrawal in the control plane rather than only this test's, and `operations_mode` is
/// a singleton row. Two such tests in parallel therefore act on each other — a sweep whose
/// chain view is liquid on a rail will dispatch a sibling's queued withdrawal out from
/// under it, and a kill-switch flipped by one test pauses another's sweep. Partitioning by
/// rail is not a fix: there are four rails and more tests than that, and the kill-switch
/// has no rail at all.
static GLOBAL_SWEEP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
	pool: PgPool,
	deposits: PgDeposits,
	ledger: Arc<dyn Ledger>,
	withdrawals: Arc<dyn WithdrawalRepository>,
	users: Arc<dyn UserRepository>,
	outflow: PgOutflowPolicy,
	deposit_addresses: Arc<dyn DepositAddresses>,
	relay: Relay,
	notify: Arc<Notify>,
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "withdrawal test").await?;

	let withdrawals: Arc<dyn WithdrawalRepository> = Arc::new(PgWithdrawals::new(pool.clone()));
	let users: Arc<dyn UserRepository> = Arc::new(PgUsers::new(pool.clone()));
	let deposit_addresses: Arc<dyn DepositAddresses> = Arc::new(StubDepositAddresses::new(pool.clone()));
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone());
	Some(Harness {
		deposits: PgDeposits::new(pool.clone()),
		outflow: PgOutflowPolicy::new(pool.clone()),
		pool,
		ledger,
		withdrawals,
		users,
		deposit_addresses,
		relay,
		notify,
	})
}

/// The withdrawal ports, borrowed out of the harness for one call. `custody` stays a
/// parameter because it is the one port these tests vary — the whole rail-liquidity
/// behaviour turns on which chain view answers.
fn withdrawal_ports<'a>(h: &'a Harness, custody: &'a dyn Custody) -> withdrawal_app::WithdrawalPorts<'a> {
	withdrawal_app::WithdrawalPorts {
		withdrawals: h.withdrawals.as_ref(),
		ledger: h.ledger.as_ref(),
		custody,
		relay: &h.notify,
	}
}

/// The user-facing admission gates over every rail, at the given gate position.
/// `KycGate::ENFORCED` is the deployment default; a suite passes `LIFTED` only to prove
/// what the switch does.
fn admission(h: &Harness, kyc: KycGate) -> withdrawal_app::AdmissionGates<'_> {
	withdrawal_app::AdmissionGates {
		policy: &h.outflow,
		configured: &Network::ALL,
		kyc,
	}
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn unique_tx_ref() -> TxRef {
	TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap()
}

/// A destination address valid for `network` (distinct from the user's own).
fn destination(network: Network) -> WalletAddress {
	let raw = match network {
		Network::Bep20 | Network::Polygon => "0x52908400098527886E0F7030069857D2E4169EE7",
		Network::Trc20 => "TJRabPrwbZy45sbavfcjinPJC18kjpRTv8",
		Network::Ton => "EQCD39VS5jcptHL8vMjEXrzGaRcCVYto7HUn4bpAOg8xqB2N",
	};
	WalletAddress::parse(network, raw).unwrap()
}

/// The outflow policy over the harness's control plane — the kill-switch and the owner's
/// freeze/KYC standing that `dispatch_withdrawal` gates on. A borrow-holder, so a test can
/// mutate the underlying rows between calls and the next call sees the new state.
fn policy(h: &Harness) -> PgOutflowPolicy {
	PgOutflowPolicy::new(h.pool.clone())
}

/// A fresh user who is active AND verified — what every money-moving path here needs.
/// `provision` leaves the row at KYC tier 0 (the schema default), which the deposit and
/// withdrawal gates refuse, so the mirrored tier is set the way the lifecycle bridge sets
/// it. Tier 0 itself is exercised in `kyc_gating.rs`.
async fn active_user(h: &Harness) -> UserId {
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("u{}@example.com", Uuid::new_v4().simple())).unwrap();
	let user = h.users.provision(subject, email, true).await.unwrap().id();
	common::set_kyc_level(&h.pool, user, 1).await;
	user
}

/// A `Usdt`-typed view of a ledger balance for assertions (the ledger port speaks raw
/// base units; the cash ledger's unit is 18-dp USDT).
struct Bal {
	posted: Usdt,
	#[allow(dead_code)]
	pending: Usdt,
	locked: Usdt,
}

impl Bal {
	fn available(&self) -> Usdt {
		self.posted.checked_sub(self.locked).unwrap_or(Usdt::ZERO)
	}
}

async fn bal(h: &Harness, key: &LedgerAccountKey) -> Bal {
	let b = h.ledger.balance(key).await.unwrap();
	Bal {
		posted: Usdt::from_base_units(b.posted),
		pending: Usdt::from_base_units(b.pending),
		locked: Usdt::from_base_units(b.locked),
	}
}

async fn deposit(h: &Harness, user: UserId, network: Network, amount: &str) {
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, network, usdt(amount)).await.unwrap();
	h.relay.drain().await;
}

#[tokio::test]
async fn withdraw_reserves_then_settles_and_retains_fee() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	let claim = LedgerAccountKey::UserClaim(user);
	let fee_account = FeePayee::treasury().claim_key();

	deposit(&h, user, network, "100").await;
	let fee_before = bal(&h, &fee_account).await.posted;

	// Request a 50 USDT withdrawal (fee 1, net 49) — the gross is reserved as pending.
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.net_amount(), usdt("49"));
	h.relay.drain().await;

	let reserved = bal(&h, &claim).await;
	assert_eq!(reserved.posted, usdt("100"), "settled balance unchanged until the withdrawal settles");
	assert_eq!(reserved.locked, usdt("50"), "the gross is locked as a pending debit");
	assert_eq!(reserved.available(), usdt("50"), "available drops by the reserved gross");

	// Settle on confirmations — posts both legs.
	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), unique_tx_ref())
		.await
		.unwrap();
	h.relay.drain().await;

	let settled = bal(&h, &claim).await;
	assert_eq!(settled.posted, usdt("50"), "the gross left the user's claim");
	assert_eq!(settled.locked, Usdt::ZERO, "nothing remains reserved");
	// The fee is banking's own and lands in its treasury (D17) — one account shared across
	// (parallel) tests, so assert this withdrawal credited *at least* its fee — concurrent
	// tests only add more.
	assert!(bal(&h, &fee_account).await.posted.checked_sub(fee_before).unwrap() >= usdt("1"), "the fee was retained");
}

#[tokio::test]
async fn a_repeated_withdrawal_key_returns_the_first_and_reserves_once() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	let claim = LedgerAccountKey::UserClaim(user);
	deposit(&h, user, network, "100").await;
	let key = format!("page-load-{}", Uuid::new_v4());
	let ask = |amount: &str, network: Network, key: &str| {
		let amount = usdt(amount);
		let key = key.to_owned();
		let h = &h;
		async move {
			withdrawal_app::request_withdrawal_keyed(
				&withdrawal_ports(h, &StubCustody),
				&admission(h, KycGate::ENFORCED),
				user,
				network,
				destination(network),
				amount,
				&key,
			)
			.await
		}
	};

	let first = ask("30", network, &key).await.unwrap();
	h.relay.drain().await;
	let again = ask("30", network, &key).await.unwrap();
	assert_eq!(again.id(), first.id(), "a retry returns the first withdrawal");
	h.relay.drain().await;
	assert_eq!(bal(&h, &claim).await.locked, usdt("30"), "the gross is reserved once");
	assert_eq!(h.withdrawals.list_by_user(user).await.unwrap().len(), 1);

	// The same key for other terms is a conflict, not a second withdrawal.
	let other_amount = ask("31", network, &key).await;
	assert!(matches!(other_amount, Err(DomainError::Conflict(_))), "{other_amount:?}");
	let other_rail = ask("30", Network::Polygon, &key).await;
	assert!(matches!(other_rail, Err(DomainError::Conflict(_))), "{other_rail:?}");

	// Keys are per user: another user's identical key is their own request.
	let other = active_user(&h).await;
	deposit(&h, other, network, "100").await;
	let theirs = withdrawal_app::request_withdrawal_keyed(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		other,
		network,
		destination(network),
		usdt("30"),
		&key,
	)
	.await
	.unwrap();
	assert_ne!(theirs.id(), first.id());

	// A key outside 1..=128 bytes is refused before anything is read.
	let empty = ask("30", network, "").await;
	assert!(matches!(empty, Err(DomainError::Validation(_))), "{empty:?}");
	let long = ask("30", network, &"k".repeat(129)).await;
	assert!(matches!(long, Err(DomainError::Validation(_))), "{long:?}");
}

/// Two twins of one request racing: the claim lock serializes them, one records, the other
/// gets that one back.
#[tokio::test]
async fn racing_withdrawals_under_one_key_record_one() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "100").await;
	let key = format!("race-{}", Uuid::new_v4());
	let ports = withdrawal_ports(&h, &StubCustody);
	let gates = admission(&h, KycGate::ENFORCED);
	let one = withdrawal_app::request_withdrawal_keyed(&ports, &gates, user, network, destination(network), usdt("30"), &key);
	let two = withdrawal_app::request_withdrawal_keyed(&ports, &gates, user, network, destination(network), usdt("30"), &key);
	let (a, b) = tokio::join!(one, two);
	assert_eq!(a.unwrap().id(), b.unwrap().id());
	assert_eq!(h.withdrawals.list_by_user(user).await.unwrap().len(), 1);
}

/// One self-service request through the screen, as `WalletService.RequestWithdrawal` makes it.
async fn screened(h: &Harness, user: UserId, amount: &str, key: Option<&str>, profile: &LimitProfile, audit: &RequestAudit) -> Result<domain::withdrawals::Withdrawal, DomainError> {
	let network = Network::Bep20;
	withdrawal_app::request_withdrawal_screened(
		&withdrawal_ports(h, &StubCustody),
		&admission(h, KycGate::ENFORCED),
		user,
		network,
		destination(network),
		usdt(amount),
		key,
		&Screening { profile, audit, hold_secs: 0 },
	)
	.await
}

fn small_profile(per_op: &str, per_24h: &str, max_per_hour: Option<u32>) -> LimitProfile {
	LimitProfile {
		per_op: usdt(per_op),
		per_24h: usdt(per_24h),
		max_per_hour,
	}
}

#[tokio::test]
async fn the_operation_and_daily_limits_refuse_and_a_failure_frees_the_headroom() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	deposit(&h, user, Network::Bep20, "500").await;
	let profile = small_profile("100", "150", None);
	let audit = RequestAudit::default();

	let over_op = screened(&h, user, "101", None, &profile, &audit).await;
	assert!(matches!(over_op, Err(DomainError::Precondition(_))), "{over_op:?}");

	let first = screened(&h, user, "100", None, &profile, &audit).await.unwrap();
	let over_day = screened(&h, user, "60", None, &profile, &audit).await;
	assert!(matches!(over_day, Err(DomainError::Precondition(_))), "{over_day:?}");
	assert!(screened(&h, user, "50", None, &profile, &audit).await.is_ok(), "exactly the remaining headroom fits");

	// A failed withdrawal returned its money, so it no longer weighs on the day. (It shipped at
	// once on the liquid stub rail, so `fail` is the way back, not `cancel`.)
	withdrawal_app::fail_withdrawal(h.withdrawals.as_ref(), &h.notify, first.id()).await.unwrap();
	assert!(screened(&h, user, "100", None, &profile, &audit).await.is_ok());
}

#[tokio::test]
async fn the_hourly_cap_counts_requests_not_amounts() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	deposit(&h, user, Network::Bep20, "500").await;
	let profile = small_profile("100", "1000", Some(2));
	let audit = RequestAudit::default();
	screened(&h, user, "20", None, &profile, &audit).await.unwrap();
	screened(&h, user, "20", None, &profile, &audit).await.unwrap();
	let third = screened(&h, user, "20", None, &profile, &audit).await;
	assert!(matches!(third, Err(DomainError::Precondition(_))), "{third:?}");
}

#[tokio::test]
async fn limits_are_kept_by_racing_requests_and_a_key_replay_outlives_them() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	deposit(&h, user, Network::Bep20, "500").await;
	let profile = small_profile("100", "100", None);
	let audit = RequestAudit::default();
	// Two requests that each fit alone and not together: the claim lock lets exactly one in.
	let (a, b) = tokio::join!(
		screened(&h, user, "70", Some("race-a"), &profile, &audit),
		screened(&h, user, "70", Some("race-b"), &profile, &audit)
	);
	assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1, "{a:?} / {b:?}");
	// The winner's key still answers with its withdrawal after the day is used up.
	let winner = if a.is_ok() { "race-a" } else { "race-b" };
	let replay = screened(&h, user, "70", Some(winner), &profile, &audit).await.unwrap();
	assert_eq!(h.withdrawals.list_by_user(user).await.unwrap().len(), 1);
	assert_eq!(replay.id(), h.withdrawals.list_by_user(user).await.unwrap()[0].id());
}

#[tokio::test]
async fn the_audit_context_is_recorded_and_an_unknown_origin_gets_the_host_profile() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	deposit(&h, user, Network::Bep20, "50").await;
	let audit = RequestAudit::new("https://evinvest.ltd", "203.0.113.9", "test-agent/1.0", "", "fresh_login");
	let limits = WithdrawalLimits::default();
	let recorded = screened(&h, user, "20", Some("audit-1"), limits.profile_for(&audit.origin), &audit).await.unwrap();
	let row: (Option<String>, Option<String>, Option<String>, Option<String>, Option<String>) =
		sqlx::query_as("SELECT origin, client_ip, user_agent, intent_id, stepup_method FROM withdrawals WHERE id = $1")
			.bind(recorded.id().raw())
			.fetch_one(&h.pool)
			.await
			.unwrap();
	assert_eq!(
		row,
		(
			Some("https://evinvest.ltd".into()),
			Some("203.0.113.9".into()),
			Some("test-agent/1.0".into()),
			None,
			Some("fresh_login".into())
		)
	);
	// A host origin is judged on the host profile: 1001 USDT is over its per-operation ceiling.
	assert_eq!(limits.profile_for("https://sa.evinvest.ltd").per_op, usdt("1000"));
	assert_eq!(limits.profile_for("https://evinvest.ltd.evil.example").max_per_hour, Some(6));
}

/// A screened request to `to` with a given hold, on the profile that never limits these amounts.
async fn screened_to(h: &Harness, user: UserId, amount: &str, to: &WalletAddress, hold_secs: u64) -> Result<domain::withdrawals::Withdrawal, DomainError> {
	let profile = small_profile("1000", "100000", None);
	let audit = RequestAudit::default();
	withdrawal_app::request_withdrawal_screened(
		&withdrawal_ports(h, &StubCustody),
		&admission(h, KycGate::ENFORCED),
		user,
		to.network(),
		to.clone(),
		usdt(amount),
		None,
		&Screening {
			profile: &profile,
			audit: &audit,
			hold_secs,
		},
	)
	.await
}

async fn dispatch_now(h: &Harness, id: WithdrawalId) -> Result<domain::withdrawals::Withdrawal, DomainError> {
	withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &StubCustody, &policy(h), KycGate::ENFORCED, &h.notify, id).await
}

/// Move a destination's release (and its withdrawals') into the past, as the clock would.
async fn release_now(h: &Harness, user: UserId) {
	sqlx::query("UPDATE withdrawal_addresses SET release_at = now() - interval '1 second' WHERE user_id = $1")
		.bind(user.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("UPDATE withdrawals SET release_at = now() - interval '1 second' WHERE user_id = $1 AND release_at IS NOT NULL")
		.bind(user.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

#[tokio::test]
async fn a_first_seen_address_is_held_queued_until_its_release() {
	let Some(h) = harness().await else { return };
	let _sweep = GLOBAL_SWEEP.lock().await;
	let user = active_user(&h).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "500").await;
	let to = destination(network);

	// The rail is liquid, yet the first withdrawal to a new address waits.
	let first = screened_to(&h, user, "50", &to, 3600).await.unwrap();
	assert_eq!(first.state(), WithdrawalState::Queued, "held, not dispatched");
	let release: bool = sqlx::query_scalar("SELECT release_at > now() + interval '59 minutes' FROM withdrawals WHERE id = $1")
		.bind(first.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert!(release, "release_at is recorded about an hour out");

	// A second one to the same still-held address waits as well.
	let second = screened_to(&h, user, "50", &to, 3600).await.unwrap();
	assert_eq!(second.state(), WithdrawalState::Queued);

	// Neither the sweep nor the operator RPC sends anything before the release.
	let dispatcher = Dispatcher::new(
		h.pool.clone(),
		h.withdrawals.clone(),
		h.ledger.clone(),
		Arc::new(StubCustody),
		h.notify.clone(),
		KycGate::ENFORCED,
	);
	dispatcher.sweep().await.unwrap();
	assert_eq!(
		h.withdrawals.find_by_id(first.id()).await.unwrap().unwrap().state(),
		WithdrawalState::Queued,
		"the sweep skips a held row"
	);
	let refused = dispatch_now(&h, first.id()).await;
	assert!(matches!(refused, Err(DomainError::Precondition(_))), "{refused:?}");

	// The user may still cancel a held withdrawal.
	let cancelled = withdrawal_app::cancel_withdrawal(h.withdrawals.as_ref(), &h.notify, second.id(), user).await.unwrap();
	assert_eq!(cancelled.state(), WithdrawalState::Cancelled);

	// After the release the same row dispatches.
	release_now(&h, user).await;
	dispatcher.sweep().await.unwrap();
	assert_eq!(h.withdrawals.find_by_id(first.id()).await.unwrap().unwrap().state(), WithdrawalState::Processing);

	// A released address is no longer new: the next withdrawal goes straight out.
	let third = screened_to(&h, user, "50", &to, 3600).await.unwrap();
	assert_eq!(third.state(), WithdrawalState::Processing, "a released destination dispatches at once");
}

#[tokio::test]
async fn no_hold_configured_dispatches_at_once_and_still_remembers_the_address() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "200").await;
	let to = destination(network);
	let first = screened_to(&h, user, "50", &to, 0).await.unwrap();
	assert_eq!(first.state(), WithdrawalState::Processing);
	let known: i64 = sqlx::query_scalar("SELECT count(*) FROM withdrawal_addresses WHERE user_id = $1")
		.bind(user.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(known, 1);
	// Turning the hold on later does not hold a destination that was already released.
	let again = screened_to(&h, user, "50", &to, 3600).await.unwrap();
	assert_eq!(again.state(), WithdrawalState::Processing);
}

#[tokio::test]
async fn every_screened_withdrawal_tells_the_owner_and_a_new_address_says_so() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "300").await;
	let to = destination(network);

	let held = screened_to(&h, user, "50", &to, 3600).await.unwrap();
	release_now(&h, user).await;
	let released = screened_to(&h, user, "50", &to, 3600).await.unwrap();

	let notices = |id: WithdrawalId| {
		let pool = h.pool.clone();
		async move {
			sqlx::query_as::<_, (String, String, String, String)>("SELECT topic, kind, link, body FROM scope_notification_outbox WHERE dedupe_key = $1")
				.bind(format!("withdrawal:{id}:requested"))
				.fetch_all(&pool)
				.await
				.unwrap()
		}
	};
	let first = notices(held.id()).await;
	assert_eq!(first.len(), 1, "one notice per withdrawal");
	let (topic, kind, link, body) = &first[0];
	assert_eq!(
		(topic.as_str(), kind.as_str(), link.as_str()),
		("bank:wallet-security", "withdrawal_new_address", "/cabinet/wallet")
	);
	assert!(body.contains(to.as_str()) && body.contains("50 USDT"), "{body}");
	let second = notices(released.id()).await;
	assert_eq!(second[0].1, "withdrawal_requested", "a known address is an ordinary withdrawal notice");
}

/// Polygon is the second EVM rail but carries USDT at 6-dp (unlike BEP20's 18-dp), so this
/// exercises the whole reserve → settle flow across the 6-dp `to_onchain(Polygon)` edge — the
/// withdrawal net must be representable at 6 decimals, and the ledger legs still move canonical
/// 18-dp `Usdt` (the ledger never sees the chain's precision).
#[tokio::test]
async fn withdraw_on_polygon_reserves_then_settles_and_retains_fee() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Polygon;
	let claim = LedgerAccountKey::UserClaim(user);
	let fee_account = FeePayee::treasury().claim_key();

	deposit(&h, user, network, "100").await;
	let fee_before = bal(&h, &fee_account).await.posted;

	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.net_amount(), usdt("49"));
	h.relay.drain().await;

	let reserved = bal(&h, &claim).await;
	assert_eq!(reserved.locked, usdt("50"), "the gross is locked as a pending debit on Polygon too");
	assert_eq!(reserved.available(), usdt("50"), "available drops by the reserved gross");

	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), unique_tx_ref())
		.await
		.unwrap();
	h.relay.drain().await;

	let settled = bal(&h, &claim).await;
	assert_eq!(settled.posted, usdt("50"), "the gross left the user's claim");
	assert_eq!(settled.locked, Usdt::ZERO, "nothing remains reserved");
	assert!(bal(&h, &fee_account).await.posted.checked_sub(fee_before).unwrap() >= usdt("1"), "the fee was retained");
}

#[tokio::test]
async fn withdraw_fail_voids_and_refunds_in_full() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Trc20;
	let claim = LedgerAccountKey::UserClaim(user);

	deposit(&h, user, network, "100").await;

	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("30"),
	)
	.await
	.unwrap();
	h.relay.drain().await;
	assert_eq!(bal(&h, &claim).await.locked, usdt("30"), "the gross is reserved");

	// The broadcast never landed — fail it. Both legs void; the user is made whole.
	withdrawal_app::fail_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id()).await.unwrap();
	h.relay.drain().await;

	let refunded = bal(&h, &claim).await;
	assert_eq!(refunded.posted, usdt("100"), "the reservation was voided");
	assert_eq!(refunded.locked, Usdt::ZERO, "nothing remains reserved");
	assert_eq!(refunded.available(), usdt("100"), "the full balance is spendable again");
}

#[tokio::test]
async fn withdraw_below_minimum_is_rejected() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Ton;
	let err = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("5"),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "below-minimum is a validation error, got {err:?}");
}

#[tokio::test]
async fn withdraw_beyond_available_is_rejected_read_first() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "10").await;

	// 50 clears the minimum but exceeds the available balance — Read-First rejects it.
	let err = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "insufficient available is a validation error, got {err:?}");
}

#[tokio::test]
async fn a_disabled_user_cannot_withdraw() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Trc20;
	deposit(&h, user, network, "100").await;

	h.users.disable(user).await.unwrap();

	let err = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(_)), "a disabled account is refused a withdrawal, got {err:?}");
}

/// The cross-plane freeze (a concierge SUSPENDED mirrored onto `frozen`, status still
/// `active`) is refused at ADMISSION, not only caught later by the dispatcher: the gate reads
/// the same `frozen OR disabled` fold the payout re-check does. Lifting the freeze lets the
/// identical request through, proving the freeze was the only hold.
#[tokio::test]
async fn a_frozen_user_cannot_request_a_withdrawal() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Trc20;
	deposit(&h, user, network, "100").await;

	sqlx::query("UPDATE users SET frozen = TRUE WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
	let err = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(_)), "a frozen account is refused at admission, got {err:?}");

	sqlx::query("UPDATE users SET frozen = FALSE WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.expect("the same request is admitted once the freeze is lifted");
	assert_eq!(withdrawal.state(), WithdrawalState::Processing);
	h.relay.drain().await;

	// Settle so the shared rail isn't left with a dangling in-flight reservation.
	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), unique_tx_ref())
		.await
		.unwrap();
	h.relay.drain().await;
}

#[tokio::test]
async fn withdraw_on_a_short_rail_is_queued_then_dispatched() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let claim = LedgerAccountKey::UserClaim(user);
	// 1e9 USDT dwarfs any liquidity the shared TON rail accumulates across tests, so the
	// short-rail (queue) path is deterministic regardless of test interleaving.
	let big = usdt("1000000000");

	// Fund the user's unified claim via BEP20; the TON rail cannot possibly cover 1e9.
	deposit(&h, user, Network::Bep20, "1000000000").await;

	// Withdraw on TON: the user is solvent but the TON rail is short, so the withdrawal
	// is accepted and QUEUED (accept-and-queue), not refused.
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		Network::Ton,
		destination(Network::Ton),
		big,
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Queued, "a short rail queues, it does not refuse");
	h.relay.drain().await;
	assert_eq!(bal(&h, &claim).await.locked, big, "the gross is reserved while queued");

	// Another investor's deposit puts liquidity on the TON rail past the net; the worker then dispatches it.
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), UserId::new(), Network::Ton, big)
		.await
		.unwrap();
	h.relay.drain().await;
	let dispatched = withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &StubCustody, &policy(&h), KycGate::ENFORCED, &h.notify, withdrawal.id())
		.await
		.unwrap();
	assert_eq!(dispatched.state(), WithdrawalState::Processing, "a funded rail dispatches");
	h.relay.drain().await;

	// Settle it — the gross leaves the claim (fresh user, so deterministic).
	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), unique_tx_ref())
		.await
		.unwrap();
	h.relay.drain().await;
	let settled = bal(&h, &claim).await;
	assert_eq!(settled.posted, Usdt::ZERO, "the gross left the claim after settle");
	assert_eq!(settled.locked, Usdt::ZERO, "nothing remains reserved");
}

#[tokio::test]
async fn a_queued_withdrawal_can_be_cancelled_and_refunds() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let claim = LedgerAccountKey::UserClaim(user);
	let big = usdt("1000000000");
	deposit(&h, user, Network::Bep20, "1000000000").await;

	// Withdraw on the TRC20 rail, which cannot cover 1e9 → queued (no test seeds TRC20).
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &StubCustody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		Network::Trc20,
		destination(Network::Trc20),
		big,
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Queued);
	h.relay.drain().await;
	assert_eq!(bal(&h, &claim).await.locked, big);

	// The user cancels the queued withdrawal — full refund, nothing was broadcast.
	let cancelled = withdrawal_app::cancel_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), user).await.unwrap();
	assert_eq!(cancelled.state(), WithdrawalState::Cancelled);
	h.relay.drain().await;
	let refunded = bal(&h, &claim).await;
	assert_eq!(refunded.locked, Usdt::ZERO, "nothing remains reserved");
	assert_eq!(refunded.available(), big, "the full balance is spendable again");
}

/// The dispatch gate is `min(TB rail, on-chain treasury)`: a treasury holding zero USDT
/// on-chain queues the withdrawal even though the TB rail accounting looks liquid (it
/// counts confirmed deposits still on users' derived addresses, which the treasury hot
/// wallet cannot spend) — and the queued withdrawal stays user-cancellable (full refund).
#[tokio::test]
async fn an_onchain_short_treasury_queues_despite_a_liquid_tb_rail() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	let claim = LedgerAccountKey::UserClaim(user);
	// The deposit credits the TB rail, so the accounting balance covers the net.
	deposit(&h, user, network, "100").await;

	let custody = TestCustody::with_view(network, TreasuryView::OnChain(Usdt::ZERO));
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &custody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Queued, "an on-chain-short treasury queues, never dispatches");
	h.relay.drain().await;
	assert_eq!(bal(&h, &claim).await.locked, usdt("50"), "the clearing reserve holds the gross while queued");

	// Queued = cancellable: the user gets the full gross back.
	let cancelled = withdrawal_app::cancel_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), user).await.unwrap();
	assert_eq!(cancelled.state(), WithdrawalState::Cancelled);
	h.relay.drain().await;
	let refunded = bal(&h, &claim).await;
	assert_eq!(refunded.locked, Usdt::ZERO, "nothing remains reserved");
	assert_eq!(refunded.available(), usdt("100"), "the full balance is spendable again");
}

/// A treasury provably liquid on-chain (and a liquid TB rail) dispatches immediately.
#[tokio::test]
async fn an_onchain_liquid_treasury_dispatches_immediately() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "100").await;

	let custody = TestCustody::with_view(network, TreasuryView::OnChain(usdt("1000000000")));
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &custody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Processing, "both liquidity sources cover the net — dispatched");
	h.relay.drain().await;

	// Settle so the shared rails aren't left with a dangling in-flight reservation.
	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), unique_tx_ref())
		.await
		.unwrap();
	h.relay.drain().await;
}

/// A treasury read failure DEGRADES to accept-and-queue — never a refusal: acceptance
/// and the clearing reserve must not depend on a flaky chain node.
#[tokio::test]
async fn a_treasury_read_failure_queues_and_never_rejects() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	let claim = LedgerAccountKey::UserClaim(user);
	deposit(&h, user, network, "100").await;

	let custody = TestCustody::with_view(network, TreasuryView::Unreachable);
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &custody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.expect("a chain-view outage must not refuse the user");
	assert_eq!(withdrawal.state(), WithdrawalState::Queued, "degrade to queued on a treasury read failure");
	h.relay.drain().await;
	assert_eq!(bal(&h, &claim).await.locked, usdt("50"), "the reserve is untouched by the degraded gate");

	withdrawal_app::cancel_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), user).await.unwrap();
	h.relay.drain().await;
}

/// The operator dispatch RPC refuses a provably on-chain-short rail — the withdrawal
/// stays queued (still cancellable) instead of marching into a custody park.
#[tokio::test]
async fn admin_dispatch_is_refused_when_the_treasury_is_short_onchain() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "100").await;

	let custody = TestCustody::with_view(network, TreasuryView::OnChain(Usdt::ZERO));
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &custody),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Queued);
	h.relay.drain().await;

	let err = withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &custody, &policy(&h), KycGate::ENFORCED, &h.notify, withdrawal.id())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "an underfunded rail refuses the dispatch, got {err:?}");
	let after = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(after.state(), WithdrawalState::Queued, "a refused dispatch leaves the withdrawal queued");

	withdrawal_app::cancel_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), user).await.unwrap();
	h.relay.drain().await;
}

/// The dispatcher worker drains the accept-and-queue backlog: a queued withdrawal is
/// dispatched exactly once when BOTH gates pass (TB rail + on-chain treasury), stays
/// queued while the treasury is short on-chain, and a second sweep no-ops (the state is
/// no longer `queued`; dispatch itself is idempotent).
#[tokio::test]
async fn the_dispatcher_sweeps_a_queued_withdrawal_once_both_gates_pass() {
	let _sweep = GLOBAL_SWEEP.lock().await;
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	// TON keeps this test's dispatchable row off the BEP20 rail other tests queue on; the
	// baseline all-rails-short custody view keeps THEIR queued rows out of this sweep.
	let network = Network::Ton;
	deposit(&h, user, Network::Bep20, "100").await;
	// A small top-up so the TB TON gate covers the net without dwarfing the shared rail.
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), UserId::new(), network, usdt("60"))
		.await
		.unwrap();
	h.relay.drain().await;

	let custody = Arc::new(TestCustody::short_everywhere());
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, custody.as_ref()),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Queued, "the on-chain-short treasury queues the request");
	h.relay.drain().await;

	let dispatcher = Dispatcher::new(h.pool.clone(), h.withdrawals.clone(), h.ledger.clone(), custody.clone(), h.notify.clone(), KycGate::ENFORCED);

	// On-chain still short — the sweep leaves everything queued.
	assert_eq!(dispatcher.sweep().await.unwrap(), 0, "an on-chain-short rail dispatches nothing");
	let still_queued = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(still_queued.state(), WithdrawalState::Queued, "still queued (and cancellable) while the treasury is short");

	// The treasury is topped up on-chain — the next sweep dispatches it.
	custody.set(network, TreasuryView::OnChain(usdt("1000000000")));
	assert!(dispatcher.sweep().await.unwrap() >= 1, "a topped-up rail dispatches the queued withdrawal");
	let processing = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(processing.state(), WithdrawalState::Processing, "the dispatcher moved it to processing");
	h.relay.drain().await;

	// A second sweep no-ops for this withdrawal — it is no longer queued.
	dispatcher.sweep().await.unwrap();
	let after = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(after.state(), WithdrawalState::Processing, "a second sweep does not re-dispatch");

	// Settle so the shared rails aren't left with a dangling in-flight reservation.
	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), unique_tx_ref())
		.await
		.unwrap();
	h.relay.drain().await;
}

/// A queued withdrawal whose owner is frozen (a concierge SUSPENDED lifecycle event, or a
/// disabled account) must NOT be dispatched by the async sweep, even with both liquidity
/// gates satisfied. The read-only kill-switch and the cross-plane freeze are enforced at the
/// sync RPC boundary (`unfrozen_caller`); the sweep re-checks the freeze so anything already
/// in the accept-and-queue backlog can't bypass an AML hold within ~30s of a rail top-up.
/// Lifting the freeze lets the very next sweep dispatch it — proving the freeze was the only
/// thing holding an otherwise-dispatchable withdrawal.
#[tokio::test]
async fn the_dispatcher_skips_a_frozen_owners_queued_withdrawal() {
	let _sweep = GLOBAL_SWEEP.lock().await;
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	// TON with a small top-up keeps this test's row off the rails other tests queue on and
	// the shared TON rail out of a huge dispatch (mirrors the sibling dispatcher test).
	let network = Network::Ton;
	deposit(&h, user, Network::Bep20, "100").await;
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), UserId::new(), network, usdt("60"))
		.await
		.unwrap();
	h.relay.drain().await;

	let custody = Arc::new(TestCustody::short_everywhere());
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, custody.as_ref()),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Queued, "the on-chain-short treasury queues the request");
	h.relay.drain().await;

	// Both liquidity gates now pass on-chain — only the freeze may hold it.
	custody.set(network, TreasuryView::OnChain(usdt("1000000000")));
	let dispatcher = Dispatcher::new(h.pool.clone(), h.withdrawals.clone(), h.ledger.clone(), custody.clone(), h.notify.clone(), KycGate::ENFORCED);

	// Freeze the owner (mirrors a concierge SUSPENDED: `frozen` set, status still active).
	sqlx::query("UPDATE users SET frozen = TRUE WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
	dispatcher.sweep().await.unwrap();
	let held = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(held.state(), WithdrawalState::Queued, "a frozen owner's queued withdrawal is not dispatched by the sweep");

	// Lift the freeze — the next sweep dispatches it, proving the freeze was the only hold.
	sqlx::query("UPDATE users SET frozen = FALSE WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
	assert!(dispatcher.sweep().await.unwrap() >= 1, "once unfrozen, the topped-up rail dispatches it");
	let processing = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(processing.state(), WithdrawalState::Processing, "the dispatcher dispatched it once the freeze lifted");
	h.relay.drain().await;

	// Settle so the shared rails aren't left with a dangling in-flight reservation.
	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), unique_tx_ref())
		.await
		.unwrap();
	h.relay.drain().await;
}

/// One sweep must account the liquidity it already dispatched: three queued
/// withdrawals summing past the rail's on-chain treasury each pass a *static* gate
/// (the balances only move when the relay settles), so an unaccounted sweep would
/// dispatch all three and mass-park downstream. The dispatcher walks the backlog
/// FIFO and deducts every dispatched net from the rail's remaining liquidity — only
/// the oldest fits this sweep; the rest stay queued (and cancellable) for the next
/// top-up. TRC20 keeps this test's rows off the rails the other dispatcher test tops
/// up, and the ~50 on-chain cap keeps any parallel test's huge queued grosses out.
#[tokio::test]
async fn a_sweep_dispatches_fifo_within_the_rails_remaining_liquidity() {
	let _sweep = GLOBAL_SWEEP.lock().await;
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Trc20;
	deposit(&h, user, Network::Bep20, "200").await;
	// The TB rail covers every net individually — the on-chain view is the binding
	// budget, so what's proven is the running deduction, not a static shortfall.
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), UserId::new(), network, usdt("200"))
		.await
		.unwrap();
	h.relay.drain().await;

	let custody = Arc::new(TestCustody::short_everywhere());
	let mut withdrawals = Vec::new();
	for _ in 0..3 {
		let withdrawal = withdrawal_app::request_withdrawal(
			&withdrawal_ports(&h, custody.as_ref()),
			&admission(&h, KycGate::ENFORCED),
			WithdrawalId::new(),
			user,
			network,
			destination(network),
			usdt("50"),
		)
		.await
		.unwrap();
		assert_eq!(withdrawal.state(), WithdrawalState::Queued, "the on-chain-short treasury queues each request");
		withdrawals.push(withdrawal);
	}
	h.relay.drain().await;

	// Top up the treasury to cover exactly one net (49) — a static gate would let all
	// three (net 147) through.
	custody.set(network, TreasuryView::OnChain(usdt("50")));
	let dispatcher = Dispatcher::new(h.pool.clone(), h.withdrawals.clone(), h.ledger.clone(), custody.clone(), h.notify.clone(), KycGate::ENFORCED);
	dispatcher.sweep().await.unwrap();

	let first = h.withdrawals.find_by_id(withdrawals[0].id()).await.unwrap().unwrap();
	assert_eq!(first.state(), WithdrawalState::Processing, "the FIFO-first withdrawal fits the rail's liquidity");
	for later in &withdrawals[1..] {
		let after = h.withdrawals.find_by_id(later.id()).await.unwrap().unwrap();
		assert_eq!(after.state(), WithdrawalState::Queued, "the rest exceed the remaining liquidity and stay queued");
	}

	// Settle the dispatched one and cancel the rest, so the shared rails aren't left
	// with dangling in-flight reservations.
	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawals[0].id(), unique_tx_ref())
		.await
		.unwrap();
	for later in &withdrawals[1..] {
		withdrawal_app::cancel_withdrawal(h.withdrawals.as_ref(), &h.notify, later.id(), user).await.unwrap();
	}
	h.relay.drain().await;
}

#[tokio::test]
async fn deposit_address_is_stable_per_user_and_network() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	for network in Network::ALL {
		let first = h.deposit_addresses.address(user, network).await.unwrap().expect("stub yields a fundable address");
		let second = h.deposit_addresses.address(user, network).await.unwrap().expect("stub yields a fundable address");
		assert_eq!(first, second, "the cached deposit address is stable across reads");
		assert_eq!(first.network(), network, "the address is for the requested network");
	}
}

/// A verification revoked AFTER the request was accepted must stop the payout.
///
/// The tier-1 floor used to be a one-shot admission check, so a withdrawal already sitting
/// in the accept-and-queue backlog was dispatched on the next rail top-up even though its
/// owner was no longer verified — and unlike a suspension, a `KYC_CHANGED{kyc_level: 0}`
/// sets no freeze for the sweep to catch. Restoring the tier lets the very next sweep ship
/// it, which is what proves the tier was the only thing holding an otherwise-dispatchable
/// withdrawal.
#[tokio::test]
async fn the_dispatcher_skips_a_queued_withdrawal_whose_owner_lost_their_tier() {
	let _sweep = GLOBAL_SWEEP.lock().await;
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	// TON with a small top-up keeps this test's row off the rails other tests queue on
	// (mirrors the sibling dispatcher tests).
	let network = Network::Ton;
	deposit(&h, user, Network::Bep20, "100").await;
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), UserId::new(), network, usdt("60"))
		.await
		.unwrap();
	h.relay.drain().await;

	let custody = Arc::new(TestCustody::short_everywhere());
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, custody.as_ref()),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Queued, "the on-chain-short treasury queues the request");
	h.relay.drain().await;

	// Both liquidity gates now pass on-chain — only the verification floor may hold it.
	custody.set(network, TreasuryView::OnChain(usdt("1000000000")));
	let dispatcher = Dispatcher::new(h.pool.clone(), h.withdrawals.clone(), h.ledger.clone(), custody.clone(), h.notify.clone(), KycGate::ENFORCED);

	// Compliance revokes the tier the way the lifecycle bridge does: the mirrored column,
	// with no freeze alongside it.
	common::set_kyc_level(&h.pool, user, 0).await;
	dispatcher.sweep().await.unwrap();
	let held = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(held.state(), WithdrawalState::Queued, "an unverified owner's queued withdrawal is not dispatched by the sweep");

	// Re-verified — the next sweep dispatches it, proving the tier was the only hold.
	common::set_kyc_level(&h.pool, user, 1).await;
	assert!(dispatcher.sweep().await.unwrap() >= 1, "once re-verified, the topped-up rail dispatches it");
	let processing = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(processing.state(), WithdrawalState::Processing, "the dispatcher dispatched it once the tier came back");
	h.relay.drain().await;

	// Settle so the shared rails aren't left with a dangling in-flight reservation.
	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), unique_tx_ref())
		.await
		.unwrap();
	h.relay.drain().await;
}

/// The operator dispatch command carries the SAME outflow policy as the user path and the
/// sweep: `bank:withdrawal:dispatch` says who may ask, not that the platform is
/// currently willing. Each of the three gates is proven to be the only hold, by clearing
/// it and re-running the identical call.
#[tokio::test]
async fn admin_dispatch_is_refused_under_read_only_a_freeze_and_a_revoked_tier() {
	// The read-only arm flips the singleton `operations_mode` row, which every parallel
	// sweep reads.
	let _sweep = GLOBAL_SWEEP.lock().await;
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "100").await;

	// Queue it on an on-chain-short treasury, then dispatch against a liquid view: the
	// liquidity gate is covered elsewhere and must not be what refuses here.
	let short = TestCustody::with_view(network, TreasuryView::OnChain(Usdt::ZERO));
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &short),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Queued);
	h.relay.drain().await;
	let liquid = TestCustody::with_view(network, TreasuryView::OnChain(usdt("1000000000")));

	// 1. The tier is revoked after acceptance.
	common::set_kyc_level(&h.pool, user, 0).await;
	let err = withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &liquid, &policy(&h), KycGate::ENFORCED, &h.notify, withdrawal.id())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)), "an unverified owner refuses the admin dispatch, got {err:?}");
	common::set_kyc_level(&h.pool, user, 1).await;

	// 2. The owner is frozen (a concierge SUSPENDED).
	sqlx::query("UPDATE users SET frozen = TRUE WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
	let err = withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &liquid, &policy(&h), KycGate::ENFORCED, &h.notify, withdrawal.id())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(_)), "a frozen owner refuses the admin dispatch, got {err:?}");
	sqlx::query("UPDATE users SET frozen = FALSE WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();

	// 3. The global read-only kill-switch. Cleared before asserting, so a failing
	// assertion cannot leave the switch on for every other test on this database.
	operations::set_read_only(&h.pool, true).await.unwrap();
	let refused = withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &liquid, &policy(&h), KycGate::ENFORCED, &h.notify, withdrawal.id()).await;
	operations::set_read_only(&h.pool, false).await.unwrap();
	assert!(
		matches!(refused, Err(DomainError::Precondition(_))),
		"the kill-switch refuses the admin dispatch — permission is not an override, got {refused:?}"
	);

	let still_queued = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(still_queued.state(), WithdrawalState::Queued, "every refusal leaves the withdrawal queued and cancellable");

	// With all three clear the identical call ships it — the policy was the only hold.
	let dispatched = withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &liquid, &policy(&h), KycGate::ENFORCED, &h.notify, withdrawal.id())
		.await
		.unwrap();
	assert_eq!(dispatched.state(), WithdrawalState::Processing, "a clear policy dispatches");
	h.relay.drain().await;

	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), unique_tx_ref())
		.await
		.unwrap();
	h.relay.drain().await;
}

/// The real policy with one rail frozen. Not the `frozen_rails` row: that is shared by every
/// test binary on this database, and a frozen rail would refuse their withdrawals on it.
struct FrozenRail(PgOutflowPolicy, Network);

#[async_trait]
impl OutflowPolicy for FrozenRail {
	async fn outflows_paused(&self) -> Result<bool, DomainError> {
		self.0.outflows_paused().await
	}

	async fn standing(&self, user: UserId) -> Result<Option<PayoutStanding>, DomainError> {
		self.0.standing(user).await
	}

	async fn frozen_rails(&self) -> Result<Vec<Network>, DomainError> {
		Ok(vec![self.1])
	}
}

#[tokio::test]
async fn a_frozen_rail_refuses_new_withdrawals_and_holds_queued_ones() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "100").await;
	let frozen = FrozenRail(policy(&h), network);

	let short = TestCustody::with_view(network, TreasuryView::OnChain(Usdt::ZERO));
	let queued = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &short),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(queued.state(), WithdrawalState::Queued);
	h.relay.drain().await;

	let refused = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &short),
		&withdrawal_app::AdmissionGates {
			policy: &frozen,
			configured: &Network::ALL,
			kyc: KycGate::ENFORCED,
		},
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("10"),
	)
	.await;
	assert!(matches!(refused, Err(DomainError::Precondition(_))), "a frozen rail admits no new withdrawal, got {refused:?}");

	let liquid = TestCustody::with_view(network, TreasuryView::OnChain(usdt("1000000000")));
	let held = withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &liquid, &frozen, KycGate::ENFORCED, &h.notify, queued.id()).await;
	assert!(
		matches!(held, Err(DomainError::Precondition(_))),
		"a frozen rail holds its queue rather than parking it, got {held:?}"
	);
	let still = h.withdrawals.find_by_id(queued.id()).await.unwrap().unwrap();
	assert_eq!(still.state(), WithdrawalState::Queued, "held, so still cancellable");

	let dispatched = withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &liquid, &policy(&h), KycGate::ENFORCED, &h.notify, queued.id())
		.await
		.unwrap();
	assert_eq!(dispatched.state(), WithdrawalState::Processing, "unfrozen, the same withdrawal ships");
	h.relay.drain().await;
	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, queued.id(), unique_tx_ref()).await.unwrap();
	h.relay.drain().await;
}

/// A withdrawal whose owner has no control-plane row must NOT be dispatched.
///
/// `withdrawals.user_id` carries no foreign key, and the dispatch path reads the owner's
/// freeze and tier from `users` — so "no row" is a state the gate genuinely cannot
/// evaluate, on a withdrawal that has already reserved the gross and whose next step is a
/// broadcast. Absence must read as refusal, not as permission.
#[tokio::test]
async fn dispatch_is_refused_when_the_owner_has_no_control_plane_row() {
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "100").await;

	let short = TestCustody::with_view(network, TreasuryView::OnChain(Usdt::ZERO));
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h, &short),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Queued);
	h.relay.drain().await;

	// Re-point the row at an id the control plane has never seen — the shape a withdrawal
	// takes when its owner row is gone.
	let orphan = Uuid::new_v4();
	sqlx::query("UPDATE withdrawals SET user_id = $2 WHERE id = $1")
		.bind(withdrawal.id().raw())
		.bind(orphan)
		.execute(&h.pool)
		.await
		.unwrap();

	let liquid = TestCustody::with_view(network, TreasuryView::OnChain(usdt("1000000000")));
	let err = withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &liquid, &policy(&h), KycGate::ENFORCED, &h.notify, withdrawal.id())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(_)), "a missing owner row fails closed, got {err:?}");
	let after = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(after.state(), WithdrawalState::Queued, "the refused withdrawal stays queued");

	// Give the row its owner back so the reserve refunds to the claim it came from.
	sqlx::query("UPDATE withdrawals SET user_id = $2 WHERE id = $1")
		.bind(withdrawal.id().raw())
		.bind(user.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	withdrawal_app::cancel_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), user).await.unwrap();
	h.relay.drain().await;
}

// ── Test-only custody with a configurable on-chain treasury view ────────────────
// A `Custody` port adapter over no chain (Postgres/TigerBeetle stay real, per the
// no-DB-mocks rule) — like `StubCustody`, but its `treasury_liquidity` serves a
// per-rail view the test controls, so the dispatch gate's three arms (`Some`, `None`,
// `Err`) are each drivable. Broadcasts are stub no-ops.

/// One rail's on-chain treasury as the test custody reports it. A rail with no entry
/// has no chain view at all (`Ok(None)`), like an unwired rail.
#[derive(Clone, Copy)]
enum TreasuryView {
	/// The treasury holds this much spendable USDT on-chain.
	OnChain(Usdt),
	/// The chain read fails (node outage).
	Unreachable,
}

struct TestCustody {
	views: Mutex<HashMap<Network, TreasuryView>>,
}

impl TestCustody {
	fn with_view(network: Network, view: TreasuryView) -> Self {
		Self {
			views: Mutex::new(HashMap::from([(network, view)])),
		}
	}

	/// Every rail short on-chain (`OnChain(0)`) — the dispatcher test's baseline, so a
	/// concurrently-queued withdrawal from a parallel test can never be swept up by it.
	fn short_everywhere() -> Self {
		Self {
			views: Mutex::new(Network::ALL.iter().map(|network| (*network, TreasuryView::OnChain(Usdt::ZERO))).collect()),
		}
	}

	fn set(&self, network: Network, view: TreasuryView) {
		self.views.lock().unwrap().insert(network, view);
	}
}

impl domain::architecture::Gateway for TestCustody {}

#[async_trait]
impl Custody for TestCustody {
	async fn broadcast(&self, request: &BroadcastRequest) -> Result<(), CustodyError> {
		let _ = request;
		Ok(())
	}

	async fn treasury_liquidity(&self, network: Network) -> Result<Option<Usdt>, CustodyError> {
		match self.views.lock().unwrap().get(&network) {
			Some(TreasuryView::OnChain(balance)) => Ok(Some(*balance)),
			Some(TreasuryView::Unreachable) => Err(CustodyError::Unavailable("test: chain view down".into())),
			None => Ok(None),
		}
	}
}

// ── Test-only deposit-address stub ──────────────────────────────────────────────
// Stands in for HD derivation from the fund's xpub so the saga runs without the
// separate signer process. It deterministically derives a *structurally valid* (not
// spendable) per-(user, network) address and caches it. Production wires the
// signer-backed `SignerDepositAddresses`; this double lives only here.

struct StubDepositAddresses {
	pool: PgPool,
}

impl StubDepositAddresses {
	fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

impl domain::architecture::Gateway for StubDepositAddresses {}

#[async_trait]
impl DepositAddresses for StubDepositAddresses {
	async fn address(&self, user: UserId, network: Network) -> Result<Option<WalletAddress>, DomainError> {
		// The stub stands in for a working derivation, so it caches a `derived` (fundable)
		// address — the placeholder-gating path is exercised by the signer-adapter tests.
		if let Some(existing) = sqlx::query_scalar::<_, String>("SELECT address FROM user_deposit_addresses WHERE user_id = $1 AND network = $2")
			.bind(user.raw())
			.bind(network.as_str())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?
		{
			return Ok(Some(WalletAddress::parse(network, &existing)?));
		}
		let derived = derive_address(user, network);
		sqlx::query("INSERT INTO user_deposit_addresses (user_id, network, address, address_kind) VALUES ($1, $2, $3, 'derived') ON CONFLICT (user_id, network) DO NOTHING")
			.bind(user.raw())
			.bind(network.as_str())
			.bind(derived.as_str())
			.execute(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(Some(derived))
	}
}

/// Deterministic per-(user, network) material via chained UUID v5 (stable across
/// runs/platforms — SHA-1 with fixed namespaces, no RNG).
fn derive_bytes(seed: &str, n: usize) -> Vec<u8> {
	let mut out = Vec::with_capacity(n);
	let mut acc = Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes());
	while out.len() < n {
		out.extend_from_slice(acc.as_bytes());
		acc = Uuid::new_v5(&acc, seed.as_bytes());
	}
	out.truncate(n);
	out
}

/// A structurally valid address for `network` from the derived bytes — each byte mapped
/// onto the chain's alphabet so [`WalletAddress::parse`] always accepts it.
fn derive_address(user: UserId, network: Network) -> WalletAddress {
	let seed = format!("{user}:{network}");
	let address = match network {
		Network::Bep20 | Network::Polygon => {
			let bytes = derive_bytes(&seed, 20);
			let mut s = String::from("0x");
			for byte in bytes {
				s.push_str(&format!("{byte:02x}"));
			}
			s
		}
		Network::Trc20 => {
			let bytes = derive_bytes(&seed, 33);
			let mut s = String::from("T");
			for byte in bytes {
				s.push(BASE58[byte as usize % BASE58.len()] as char);
			}
			s
		}
		Network::Ton => {
			let bytes = derive_bytes(&seed, 48);
			let mut s = String::with_capacity(48);
			for byte in bytes {
				s.push(BASE64URL[byte as usize % BASE64URL.len()] as char);
			}
			s
		}
	};
	WalletAddress::parse(network, &address).expect("derived stub address is structurally valid by construction")
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

#[test]
fn derived_addresses_are_valid_and_stable() {
	let user = UserId::new();
	for network in Network::ALL {
		let a = derive_address(user, network);
		let b = derive_address(user, network);
		assert_eq!(a, b, "derivation must be deterministic");
		assert_eq!(a.network(), network);
		assert!(WalletAddress::parse(network, a.as_str()).is_ok());
	}
}
