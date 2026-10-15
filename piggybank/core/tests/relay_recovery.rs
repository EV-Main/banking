//! Integration tests for FB-05 — parked outbox events + reconciliation + the saga reaper.
//! Real Postgres **and** TigerBeetle (no mocks, per the project rules). They run when
//! `DATABASE_URL` is set and a TigerBeetle replica is reachable (`nix run .#db` + `.#tb`),
//! and skip otherwise. The relay is driven explicitly via `Relay::drain`; the
//! reconciliation/reaper jobs via their public `scan`/`sweep` (no waiting on the interval).
//!
//! What's proven:
//!   - a non-retryable event is moved to the distinct **parked** terminal state — NOT
//!     marked dispatched — so it stays queryable and is surfaced by reconciliation
//!     (BANK-FAULT-01 / BANK-ARCH-05);
//!   - an abandoned `processing` withdrawal is surfaced by the reaper (alert-only, never
//!     auto-voided), and an abandoned `queued` withdrawal is auto-cancelled (BANK-FAULT-04);
//!   - `outbox::unpark` re-drives an open park (`parked_at` cleared, `attempts` reset)
//!     and refuses compensated/dispatched rows — and an unpark composes with the
//!     broadcast-state guard rather than bypassing it.

use std::{
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	},
	time::Duration,
};

use async_trait::async_trait;
use domain::{
	architecture::{DomainEvent, Gateway},
	auth::AuthSubject,
	balance::{LedgerAccountKey, LedgerEvent, Party, TransferCode},
	money::{Network, TxRef, Usdt, WalletAddress},
	users::{Email, UserId},
	withdrawals::{WithdrawalEvent, WithdrawalId, WithdrawalSource, WithdrawalState},
};
use piggybank_core::{
	application::{balance as balance_app, withdrawals as withdrawal_app},
	config::KycGate,
	infrastructure::{
		allocations::PgAllocations, custody::StubCustody, deposits::PgDeposits, outbox, outflow::PgOutflowPolicy, reaper::Reaper, reconciliation::Reconciliation, redemptions::PgRedemptions,
		relay::Relay, users::PgUsers, withdrawals::PgWithdrawals,
	},
	ports::{
		BroadcastRequest, Custody, CustodyError, RedemptionRepository, UserRepository, WithdrawalRepository,
		ledger::{CashInvariant, HoldingScope, Ledger, LedgerBalance, LedgerError, LedgerTransfer, LinkedLeg, LockedReads, PendingCompletion},
	},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

struct Harness {
	pool: PgPool,
	deposits: PgDeposits,
	ledger: Arc<dyn Ledger>,
	withdrawals: Arc<dyn WithdrawalRepository>,
	redemptions: Arc<dyn RedemptionRepository>,
	users: Arc<dyn UserRepository>,
	outflow: PgOutflowPolicy,
	relay: Relay,
	notify: Arc<Notify>,
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "relay recovery test").await?;

	let withdrawals: Arc<dyn WithdrawalRepository> = Arc::new(PgWithdrawals::new(pool.clone()));
	let redemptions: Arc<dyn RedemptionRepository> = Arc::new(PgRedemptions::new(pool.clone()));
	let users: Arc<dyn UserRepository> = Arc::new(PgUsers::new(pool.clone()));
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone());
	Some(Harness {
		deposits: PgDeposits::new(pool.clone()),
		outflow: PgOutflowPolicy::new(pool.clone()),
		pool,
		ledger,
		withdrawals,
		redemptions,
		users,
		relay,
		notify,
	})
}

/// The withdrawal ports, borrowed out of the harness for one call.
fn withdrawal_ports(h: &Harness) -> withdrawal_app::WithdrawalPorts<'_> {
	withdrawal_app::WithdrawalPorts {
		withdrawals: h.withdrawals.as_ref(),
		ledger: h.ledger.as_ref(),
		custody: &StubCustody,
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

fn destination(network: Network) -> WalletAddress {
	let raw = match network {
		Network::Bep20 | Network::Polygon => "0x52908400098527886E0F7030069857D2E4169EE7",
		Network::Trc20 => "TJRabPrwbZy45sbavfcjinPJC18kjpRTv8",
		Network::Ton => "EQCD39VS5jcptHL8vMjEXrzGaRcCVYto7HUn4bpAOg8xqB2N",
	};
	WalletAddress::parse(network, raw).unwrap()
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

/// A non-retryable event must be moved to the distinct **parked** state, never marked
/// dispatched. We inject an unplannable outbox row (a `withdrawals` event whose payload is
/// not a `WithdrawalEvent`), which `plan()` rejects → the relay parks it. We then assert:
/// it stays undispatched-but-parked (so the drain skips it without dropping it), it stays
/// queryable, and reconciliation surfaces it in the parked-row scan.
#[tokio::test]
async fn a_parked_event_is_not_dispatched_and_reconciliation_surfaces_it() {
	let _serial = common::outbox_serial().await;
	let Some(h) = harness().await else { return };
	let event_id = Uuid::new_v4();
	let aggregate_id = Uuid::new_v4();
	sqlx::query("INSERT INTO outbox (event_id, aggregate, aggregate_id, kind, payload) VALUES ($1, 'withdrawals', $2, 'withdrawals', $3::jsonb)")
		.bind(event_id)
		.bind(aggregate_id)
		.bind("\"not-a-withdrawal-event\"")
		.execute(&h.pool)
		.await
		.expect("inject the unplannable outbox row");

	h.relay.drain().await;

	// Parked, NOT dispatched — the row stays queryable with its last_error, and is excluded
	// from the drain (parked_at set, dispatched_at null). We read the two timestamps as
	// booleans so the test needs no `time`/`chrono` decode of a column it never inspects.
	let (is_dispatched, is_parked, last_error): (bool, bool, Option<String>) =
		sqlx::query_as("SELECT dispatched_at IS NOT NULL, parked_at IS NOT NULL, last_error FROM outbox WHERE event_id = $1")
			.bind(event_id)
			.fetch_one(&h.pool)
			.await
			.expect("the parked row is still queryable");
	assert!(!is_dispatched, "a parked event must NOT be marked dispatched");
	assert!(is_parked, "a parked event must be in the distinct parked terminal state");
	assert!(last_error.is_some_and(|e| e.contains("unplannable")), "the park reason is recorded for forensics");

	// Reconciliation's parked-row scan surfaces it.
	let report = Reconciliation::new(h.pool.clone(), h.ledger.clone(), Arc::new(PgAllocations::new(h.pool.clone())))
		.scan()
		.await
		.expect("reconciliation scan");
	assert!(report.parked_rows >= 1, "reconciliation must surface the parked row");
	assert!(report.uncompensated_parked >= 1, "an un-compensated park is reported for intervention");
}

/// The reaper owns the timeout for abandoned sagas. A `processing` withdrawal past the max
/// age is surfaced (alert-only — never auto-voided, per the cardinal rule), while a
/// `queued` withdrawal past the max age is auto-cancelled (safe — never broadcast).
#[tokio::test]
async fn the_reaper_alerts_on_stuck_processing_and_reaps_queued_withdrawals() {
	let _serial = common::outbox_serial().await;
	let Some(h) = harness().await else { return };
	let network = Network::Bep20;

	// A `processing` withdrawal: deposit, request with a liquid rail (auto-dispatched →
	// processing), then backdate its last transition past the reaper's window.
	let processing_user = active_user(&h).await;
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), processing_user, network, usdt("100"))
		.await
		.unwrap();
	h.relay.drain().await;
	// Fund the rail (a stranger's deposit) so the request auto-dispatches to `processing`.
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), UserId::new(), network, usdt("100"))
		.await
		.unwrap();
	h.relay.drain().await;
	let processing = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		processing_user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(processing.state(), WithdrawalState::Processing, "a liquid rail auto-dispatches to processing");
	h.relay.drain().await;
	backdate_withdrawal(&h.pool, processing.id().raw()).await;

	// A `queued` withdrawal: the unified claim is funded by a large deposit on a DIFFERENT
	// rail (Bep20), and the withdrawal targets TRC20 for a gross so large no parallel test's
	// rail liquidity can cover it — so it is accepted-and-queued, not dispatched (rails are
	// global singletons shared across the test binaries, so this must not rely on a short
	// rail by default). Then backdate it past the reaper window.
	let queued_user = active_user(&h).await;
	let short_network = Network::Trc20;
	let big = usdt("1000000000");
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), queued_user, network, big).await.unwrap();
	h.relay.drain().await;
	let queued = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		queued_user,
		short_network,
		destination(short_network),
		big,
	)
	.await
	.unwrap();
	assert_eq!(queued.state(), WithdrawalState::Queued, "a gross beyond any rail's liquidity leaves the withdrawal queued");
	h.relay.drain().await;
	backdate_withdrawal(&h.pool, queued.id().raw()).await;

	// Sweep with a 30-minute window: our rows are backdated an hour (abandoned), while any
	// queued saga a parallel test binary left behind is seconds old and stays untouched — so
	// the shared rails/DB don't make this flaky.
	let reaper = Reaper::new(h.pool.clone(), h.withdrawals.clone(), h.redemptions.clone(), h.notify.clone()).with_max_age(Duration::from_secs(30 * 60));
	let report = reaper.sweep().await.expect("reaper sweep");

	assert!(report.stuck_processing_withdrawals >= 1, "the stuck processing withdrawal is surfaced (alert-only)");
	assert!(report.reaped_queued_withdrawals >= 1, "the abandoned queued withdrawal is auto-cancelled");

	// The processing one is untouched (alert-only); the queued one is now cancelled.
	let after_processing = h.withdrawals.find_by_id(processing.id()).await.unwrap().unwrap();
	assert_eq!(after_processing.state(), WithdrawalState::Processing, "the reaper never auto-voids a processing withdrawal");
	let after_queued = h.withdrawals.find_by_id(queued.id()).await.unwrap().unwrap();
	assert_eq!(after_queued.state(), WithdrawalState::Cancelled, "the abandoned queued withdrawal was refunded");
}

/// The broadcast-state guard: a `Dispatched` event unparked AFTER the withdrawal was
/// failed (its clearing reservation voided, the user refunded) must be parked again —
/// custody is never called, so the unpark-after-fail double-pay hazard (a real on-chain
/// send with nothing locked behind it) is structurally impossible, not just a runbook
/// discipline. The parked `Dispatched` row is injected to mirror the incident shape (a
/// custody park), because a live one is only drainable while the row is `processing`.
#[tokio::test]
async fn an_unparked_dispatch_after_fail_is_reparked_and_never_broadcast() {
	let _serial = common::outbox_serial().await;
	let Some(h) = harness().await else { return };
	let network = Network::Trc20;
	let user = active_user(&h).await;
	// Fund the claim on BEP20 and withdraw a gross no rail can cover, so the request is
	// accepted-and-queued deterministically on the shared rails (same shape as the reaper
	// test); the reserve then applies and its saga step is recorded.
	let big = usdt("1000000000");
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, Network::Bep20, big).await.unwrap();
	h.relay.drain().await;
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		big,
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Queued);
	h.relay.drain().await;

	// Operator dispatch, then fail (a confirmed not-broadcast) — the void refunds in full.
	withdrawal_app::dispatch_withdrawal(
		h.withdrawals.as_ref(),
		&StubCustody,
		&PgOutflowPolicy::new(h.pool.clone()),
		KycGate::ENFORCED,
		&h.notify,
		withdrawal.id(),
	)
	.await
	.unwrap();
	h.relay.drain().await;
	withdrawal_app::fail_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id()).await.unwrap();
	h.relay.drain().await;

	// Inject the incident's parked `Dispatched` row for the now-failed withdrawal, then
	// unpark it through the operator API the console uses — proving the unpark path
	// composes with the broadcast-state guard rather than bypassing it.
	let event = WithdrawalEvent::Dispatched {
		withdrawal_id: withdrawal.id(),
		source: WithdrawalSource::User(user),
		network,
		address: destination(network),
		amount: withdrawal.amount(),
		fee: withdrawal.fee(),
	};
	let event_id = Uuid::new_v4();
	let seq: i64 = sqlx::query_scalar(
		"INSERT INTO outbox (event_id, aggregate, aggregate_id, kind, payload, parked_at, last_error) VALUES ($1, 'withdrawals', $2, 'withdrawals', $3::jsonb, now(), 'custody rejected: treasury underfunded on-chain (test)') RETURNING seq",
	)
	.bind(event_id)
	.bind(withdrawal.id().raw())
	.bind(serde_json::to_string(&event).unwrap())
	.fetch_one(&h.pool)
	.await
	.expect("inject the parked Dispatched row");
	assert!(outbox::unpark(&h.pool, seq).await.expect("unpark"), "an open (uncompensated) park must unpark");

	// Drain with a counting custody: the guard must park the event again without a send.
	let broadcasts = Arc::new(AtomicUsize::new(0));
	let relay = Relay::new(h.pool.clone(), h.ledger.clone(), Arc::new(CountingCustody { broadcasts: broadcasts.clone() }), h.notify.clone());
	relay.drain().await;

	let (is_dispatched, is_parked, last_error): (bool, bool, Option<String>) =
		sqlx::query_as("SELECT dispatched_at IS NOT NULL, parked_at IS NOT NULL, last_error FROM outbox WHERE event_id = $1")
			.bind(event_id)
			.fetch_one(&h.pool)
			.await
			.expect("the re-parked row is still queryable");
	assert!(!is_dispatched, "the unparked Dispatched event must never be marked dispatched");
	assert!(is_parked, "the guard must park the event again");
	assert!(last_error.is_some_and(|e| e.contains("not processing")), "the park reason names the state guard");
	assert_eq!(broadcasts.load(Ordering::SeqCst), 0, "custody must never see a broadcast for a non-processing withdrawal");
}

/// The never-void rule, enforced at the void itself: a `Failed` event for a withdrawal
/// custody already acted on (its `withdrawal_broadcasts` row exists, so the transfer
/// may have landed on-chain) must PARK, not void — the clearing reservation stays
/// locked for the operator instead of refunding a user who may also be paid on-chain.
#[tokio::test]
async fn a_fail_void_parks_when_a_broadcast_row_exists() {
	let _serial = common::outbox_serial().await;
	let Some(h) = harness().await else { return };
	let network = Network::Bep20;
	let user = active_user(&h).await;
	let claim = domain::balance::LedgerAccountKey::UserClaim(user);
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, network, usdt("100")).await.unwrap();
	h.relay.drain().await;
	// Fund the rail (a stranger's deposit) so the request auto-dispatches to `processing` (fail is only legal
	// from there — the shape of a real broadcast-then-operator-fail incident).
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), UserId::new(), network, usdt("100"))
		.await
		.unwrap();
	h.relay.drain().await;
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Processing, "a liquid rail auto-dispatches to processing");
	h.relay.drain().await;
	let reserved = h.ledger.balance(&claim).await.unwrap();
	assert_eq!(Usdt::from_base_units(reserved.locked), usdt("50"), "the gross is reserved before the fail");

	// Custody acted: the signed transaction was persisted before the send (the adapters'
	// crash-safety record). The stub custody records nothing, so inject the row.
	sqlx::query("INSERT INTO withdrawal_broadcasts (withdrawal_id, network, nonce, raw_tx, tx_hash) VALUES ($1, 'bep20', 0, '0xdead', '0xbeef')")
		.bind(withdrawal.id().raw())
		.execute(&h.pool)
		.await
		.expect("inject the broadcast row");

	// An operator fails it anyway (mistaken "confirmed not-broadcast") — the relay must
	// refuse the void and park the Failed event.
	withdrawal_app::fail_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id()).await.unwrap();
	h.relay.drain().await;

	let (is_dispatched, is_parked, last_error): (bool, bool, Option<String>) =
		sqlx::query_as("SELECT dispatched_at IS NOT NULL, parked_at IS NOT NULL, last_error FROM outbox WHERE aggregate_id = $1 ORDER BY seq DESC LIMIT 1")
			.bind(withdrawal.id().raw())
			.fetch_one(&h.pool)
			.await
			.expect("the Failed row is queryable");
	assert!(!is_dispatched, "the guarded Failed event must never be marked dispatched");
	assert!(is_parked, "the Failed event parks when a broadcast row exists");
	assert!(last_error.is_some_and(|e| e.contains("refusing to void")), "the park reason names the never-void guard");

	// The clearing pending was NOT voided — the gross stays locked for the operator.
	let after = h.ledger.balance(&claim).await.unwrap();
	assert_eq!(Usdt::from_base_units(after.locked), usdt("50"), "the reservation survives the refused void");
}

/// The operator unpark path end to end: a parked row — here a retry-exhausted but valid
/// deposit event, injected atomically already-parked so no concurrent drain touches it
/// first — is cleared by `outbox::unpark` (`parked_at` NULL **and** `attempts` reset to
/// 0; a retry-exhausted row would otherwise re-park on its first redelivery, making the
/// feature a no-op) with `last_error` kept for forensics, and the relay then re-queries
/// the outbox (no in-memory floor) and dispatches it.
#[tokio::test]
async fn an_unparked_event_is_re_driven_and_dispatched() {
	let _serial = common::outbox_serial().await;
	let Some(h) = harness().await else { return };
	let user = active_user(&h).await;
	let event = LedgerEvent::Deposited {
		party: Party::User(user),
		network: Network::Bep20,
		amount: usdt("25"),
	};
	let seq: i64 = sqlx::query_scalar(
		"INSERT INTO outbox (event_id, aggregate, aggregate_id, kind, payload, parked_at, attempts, last_error) \
		 VALUES ($1, 'deposit', $2, $3, $4::jsonb, now(), 25, 'retryable exhausted after 25 attempts (test)') RETURNING seq",
	)
	.bind(Uuid::new_v4())
	.bind(Uuid::new_v4())
	.bind(LedgerEvent::KIND)
	.bind(serde_json::to_string(&event).unwrap())
	.fetch_one(&h.pool)
	.await
	.expect("inject the retry-exhausted parked deposit row");

	assert!(outbox::unpark(&h.pool, seq).await.expect("unpark"), "an open park must unpark");

	let (attempts, last_error): (i32, Option<String>) = sqlx::query_as("SELECT attempts, last_error FROM outbox WHERE seq = $1")
		.bind(seq)
		.fetch_one(&h.pool)
		.await
		.expect("the unparked row is queryable");
	assert_eq!(attempts, 0, "attempts must reset or a retry-exhausted row re-parks on first redelivery");
	assert!(last_error.is_some(), "the old park reason stays for forensics");

	// The deposit is valid, so the re-drive dispatches it. (A parallel test's drain may
	// race us to the row — the terminal assertion holds either way.)
	h.relay.drain().await;
	let (is_dispatched, is_parked): (bool, bool) = sqlx::query_as("SELECT dispatched_at IS NOT NULL, parked_at IS NOT NULL FROM outbox WHERE seq = $1")
		.bind(seq)
		.fetch_one(&h.pool)
		.await
		.expect("the re-driven row is queryable");
	assert!(is_dispatched, "the unparked event must be re-driven to dispatched");
	assert!(!is_parked, "the unparked valid event must not re-park");
}

/// The unpark guards. A **compensated** park must refuse — its recovery event already
/// applied, so re-driving would double-apply (the money bug the guard exists for) — and
/// stay parked; a dispatched row has nothing to re-drive; an unknown seq reports as
/// such. `unpark_refusal` distinguishes the three so the service can answer with
/// FAILED_PRECONDITION vs NOT_FOUND precisely.
#[tokio::test]
async fn unpark_refuses_compensated_and_dispatched_rows() {
	let _serial = common::outbox_serial().await;
	let Some(h) = harness().await else { return };
	let compensated_seq: i64 = sqlx::query_scalar(
		"INSERT INTO outbox (event_id, aggregate, aggregate_id, kind, payload, parked_at, last_error) \
		 VALUES ($1, 'withdrawals', $2, 'withdrawals', '\"not-a-withdrawal-event\"'::jsonb, now(), 'half-applied (test)') RETURNING seq",
	)
	.bind(Uuid::new_v4())
	.bind(Uuid::new_v4())
	.fetch_one(&h.pool)
	.await
	.expect("inject the parked row");
	outbox::mark_compensated(&h.pool, compensated_seq).await.expect("mark compensated");

	assert!(!outbox::unpark(&h.pool, compensated_seq).await.expect("unpark refuses"), "a compensated park must never unpark");
	assert_eq!(
		outbox::unpark_refusal(&h.pool, compensated_seq).await.expect("refusal read"),
		Some((false, true)),
		"the refusal names compensation"
	);
	let still_parked: bool = sqlx::query_scalar("SELECT parked_at IS NOT NULL FROM outbox WHERE seq = $1")
		.bind(compensated_seq)
		.fetch_one(&h.pool)
		.await
		.expect("the compensated row is queryable");
	assert!(still_parked, "a refused unpark leaves the row parked");

	let dispatched_seq: i64 = sqlx::query_scalar(
		"INSERT INTO outbox (event_id, aggregate, aggregate_id, kind, payload, dispatched_at) \
		 VALUES ($1, 'withdrawals', $2, 'withdrawals', '\"not-a-withdrawal-event\"'::jsonb, now()) RETURNING seq",
	)
	.bind(Uuid::new_v4())
	.bind(Uuid::new_v4())
	.fetch_one(&h.pool)
	.await
	.expect("inject the dispatched row");
	assert!(
		!outbox::unpark(&h.pool, dispatched_seq).await.expect("unpark refuses"),
		"a dispatched row has nothing to re-drive"
	);
	assert_eq!(outbox::unpark_refusal(&h.pool, dispatched_seq).await.expect("refusal read"), Some((true, false)));

	// An unknown seq: the service's NOT_FOUND arm (bigserial never issues -1).
	assert!(!outbox::unpark(&h.pool, -1).await.expect("unpark refuses"));
	assert_eq!(outbox::unpark_refusal(&h.pool, -1).await.expect("refusal read"), None);
}

/// Issue #37 — the settle liquidity pre-check must be idempotent w.r.t. the event's own
/// already-applied legs. A transient failure on the fee leg leaves the withdrawal settle
/// half-applied (clearing posted, net disbursed) with the row undispatched; the redelivery
/// re-reads the rail balance, which now already reflects the disburse's own outflow, and —
/// whenever the rail held between `net` and `2·net` — used to compare that post-outflow
/// balance against the full net and spuriously park (with `applied_legs = 0`, hiding the
/// half-applied state), stranding the fee in clearing with every `UnparkEvent` re-parking.
/// The redelivery must instead recognize the applied legs and complete the fee.
#[tokio::test]
async fn a_redelivered_half_applied_settle_completes_instead_of_parking() {
	let _serial = common::outbox_serial().await;
	let Some(h) = harness().await else { return };
	let network = Network::Bep20;
	let user = active_user(&h).await;
	// The gross dwarfs anything parallel tests leave on the shared rail, so once the
	// disburse leg drains the net back out the rail holds ≈ base + fee < net — the
	// issue's `[net, 2·net)` trigger window, hit deterministically.
	let gross = usdt("1000000000000000");
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, network, gross).await.unwrap();
	h.relay.drain().await;
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		gross,
	)
	.await
	.unwrap();
	assert_eq!(
		withdrawal.state(),
		WithdrawalState::Processing,
		"the deposit makes the rail liquid, so the request auto-dispatches"
	);
	h.relay.drain().await;
	withdrawal_app::settle_withdrawal(h.withdrawals.as_ref(), &h.notify, withdrawal.id(), unique_tx_ref())
		.await
		.unwrap();

	// Half-apply the settle: this withdrawal's fee leg fails once, transiently, after the
	// clearing posted and the net disbursed — the drain stops to retry, the row stays
	// undispatched. This is the redelivery state from the issue.
	let flaky_ledger = Arc::new(FailFeeLegOnce {
		inner: h.ledger.clone(),
		target: withdrawal.id().raw().as_u128(),
		tripped: AtomicBool::new(false),
	});
	let flaky_relay = Relay::new(h.pool.clone(), flaky_ledger.clone(), Arc::new(StubCustody), h.notify.clone());
	flaky_relay.drain().await;
	assert!(flaky_ledger.tripped.load(Ordering::SeqCst), "the injected fee-leg outage fired, leaving the settle half-applied");

	// Self-check the trigger window, so amount drift can never make this test pass
	// vacuously: the drained rail must sit below the net, i.e. a naïve re-check of the
	// disburse guard WOULD spuriously park here.
	let rail = h.ledger.balance(&LedgerAccountKey::CryptoWallet(network)).await.unwrap();
	let net = withdrawal.amount().checked_sub(withdrawal.fee()).unwrap();
	assert!(rail.posted < net.base_units(), "precondition: the rail is short of the net after the disburse's own outflow");

	// The redelivery through the normal relay: the pre-check re-reads the drained rail but
	// must skip the guard for the already-applied disburse leg — completing the fee, not
	// parking "liquidity insufficient at settle".
	h.relay.drain().await;

	let (is_dispatched, is_parked, last_error): (bool, bool, Option<String>) =
		sqlx::query_as("SELECT dispatched_at IS NOT NULL, parked_at IS NOT NULL, last_error FROM outbox WHERE aggregate_id = $1 AND payload->>'type' = 'settled'")
			.bind(withdrawal.id().raw())
			.fetch_one(&h.pool)
			.await
			.expect("the settled outbox row is queryable");
	assert!(is_dispatched, "a redelivered half-applied settle must complete, not park (last_error = {last_error:?})");
	assert!(!is_parked, "no spurious liquidity park on redelivery");

	// All three legs recorded — the fee reached fee-revenue instead of stranding in clearing.
	let legs: i64 = sqlx::query_scalar("SELECT count(*) FROM saga_steps WHERE event_id = (SELECT event_id FROM outbox WHERE aggregate_id = $1 AND payload->>'type' = 'settled')")
		.bind(withdrawal.id().raw())
		.fetch_one(&h.pool)
		.await
		.expect("saga steps are queryable");
	assert_eq!(legs, 3, "clearing post, disburse and fee all applied");
}

/// A fault-injecting decorator over the real TigerBeetle ledger (no mock — every delegated
/// call hits TB): the target withdrawal's fee post fails once with a transient
/// `Unavailable`, freezing its settle in the half-applied redelivery state.
struct FailFeeLegOnce {
	inner: Arc<dyn Ledger>,
	target: u128,
	tripped: AtomicBool,
}

impl Gateway for FailFeeLegOnce {}

#[async_trait]
impl Ledger for FailFeeLegOnce {
	async fn ensure_account(&self, key: &LedgerAccountKey) -> Result<(), LedgerError> {
		self.inner.ensure_account(key).await
	}

	async fn balance(&self, key: &LedgerAccountKey) -> Result<LedgerBalance, LedgerError> {
		self.inner.balance(key).await
	}

	async fn transfer_exists(&self, id: u128) -> Result<bool, LedgerError> {
		self.inner.transfer_exists(id).await
	}

	async fn total_debits(&self, key: &LedgerAccountKey) -> Result<u128, LedgerError> {
		self.inner.total_debits(key).await
	}

	async fn spendable(&self, key: &LedgerAccountKey) -> Result<u128, LedgerError> {
		self.inner.spendable(key).await
	}

	async fn balance_locked(&self, key: &LedgerAccountKey, reads: &mut dyn LockedReads) -> Result<LedgerBalance, LedgerError> {
		self.inner.balance_locked(key, reads).await
	}

	async fn transfer_amount(&self, id: u128) -> Result<Option<u128>, LedgerError> {
		self.inner.transfer_amount(id).await
	}

	async fn share_holdings(&self, scope: &HoldingScope) -> Result<Vec<(LedgerAccountKey, u128)>, LedgerError> {
		self.inner.share_holdings(scope).await
	}

	async fn post(&self, transfer: &LedgerTransfer) -> Result<(), LedgerError> {
		if transfer.code == TransferCode::WithdrawFee && transfer.reference == self.target && !self.tripped.swap(true, Ordering::SeqCst) {
			return Err(LedgerError::Unavailable("injected fee-leg outage (test)".into()));
		}
		self.inner.post(transfer).await
	}

	async fn post_linked(&self, transfers: &[LedgerTransfer]) -> Result<(), LedgerError> {
		self.inner.post_linked(transfers).await
	}

	async fn reserve(&self, transfer: &LedgerTransfer) -> Result<(), LedgerError> {
		self.inner.reserve(transfer).await
	}

	async fn complete(&self, completion: &PendingCompletion) -> Result<(), LedgerError> {
		self.inner.complete(completion).await
	}

	async fn apply_linked(&self, legs: &[LinkedLeg]) -> Result<(), LedgerError> {
		self.inner.apply_linked(legs).await
	}

	async fn cash_invariant(&self) -> Result<CashInvariant, LedgerError> {
		self.inner.cash_invariant().await
	}
}

/// A counting custody port adapter (no chain): every broadcast is recorded and refused, so
/// a guard regression is observable as both a call count and a park-not-dispatch.
struct CountingCustody {
	broadcasts: Arc<AtomicUsize>,
}

impl Gateway for CountingCustody {}

#[async_trait]
impl Custody for CountingCustody {
	async fn broadcast(&self, _request: &BroadcastRequest) -> Result<(), CustodyError> {
		self.broadcasts.fetch_add(1, Ordering::SeqCst);
		Err(CustodyError::Rejected("test custody refuses every broadcast".into()))
	}
}

/// Push a withdrawal's last transition past the reaper's abandonment window.
async fn backdate_withdrawal(pool: &PgPool, id: Uuid) {
	sqlx::query("UPDATE withdrawals SET updated_at = now() - interval '1 hour' WHERE id = $1")
		.bind(id)
		.execute(pool)
		.await
		.expect("backdate the withdrawal");
}

/// A custody adapter that refuses every broadcast with a caller-chosen reason — the shape
/// `custody.rs` produces for any non-retryable signer status (e.g. `FailedPrecondition`
/// with the custodian's activity id in the message).
struct RefusingCustody {
	reason: &'static str,
	broadcasts: Arc<AtomicUsize>,
}

impl Gateway for RefusingCustody {}

#[async_trait]
impl Custody for RefusingCustody {
	async fn broadcast(&self, _request: &BroadcastRequest) -> Result<(), CustodyError> {
		self.broadcasts.fetch_add(1, Ordering::SeqCst);
		Err(CustodyError::Rejected(self.reason.to_owned()))
	}
}

/// banking#196: a signer answer the hub must not retry (the custodian parked the activity
/// for a human) parks the Dispatched event exactly once, and the park reason — what
/// `ListParkedEvents` shows the operator — carries the custodian's activity id verbatim.
#[tokio::test]
async fn a_custody_refusal_parks_the_broadcast_once_and_names_the_activity_in_last_error() {
	let _serial = common::outbox_serial().await;
	let Some(h) = harness().await else { return };
	let network = Network::Bep20;
	const ACTIVITY_ID: &str = "0f6a2b3c-4d5e-4f70-8a9b-0c1d2e3f4a5b";

	let user = active_user(&h).await;
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, network, usdt("100")).await.unwrap();
	h.relay.drain().await;
	// Fund the rail (a stranger's deposit) so the request auto-dispatches to `processing` (a Dispatched row exists).
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), UserId::new(), network, usdt("100"))
		.await
		.unwrap();
	h.relay.drain().await;
	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap();
	assert_eq!(withdrawal.state(), WithdrawalState::Processing, "a liquid rail auto-dispatches to processing");

	let broadcasts = Arc::new(AtomicUsize::new(0));
	let refusing = Relay::new(
		h.pool.clone(),
		h.ledger.clone(),
		Arc::new(RefusingCustody {
			reason: "signer: key custodian requires approval for activity 0f6a2b3c-4d5e-4f70-8a9b-0c1d2e3f4a5b",
			broadcasts: broadcasts.clone(),
		}),
		h.notify.clone(),
	);
	refusing.drain().await;
	// A second drain must not re-deliver a parked row: a refusal is terminal, not a retry.
	refusing.drain().await;

	let (is_dispatched, is_parked, last_error): (bool, bool, Option<String>) =
		sqlx::query_as("SELECT dispatched_at IS NOT NULL, parked_at IS NOT NULL, last_error FROM outbox WHERE aggregate_id = $1 AND kind = 'withdrawals' ORDER BY seq DESC LIMIT 1")
			.bind(withdrawal.id().raw())
			.fetch_one(&h.pool)
			.await
			.expect("the withdrawal's Dispatched row is queryable");
	assert!(!is_dispatched, "a refused broadcast must never be marked dispatched");
	assert!(is_parked, "a non-retryable custody refusal parks the event");
	let last_error = last_error.expect("a parked row records why");
	assert!(last_error.contains("custody rejected"), "the park reason names custody: {last_error}");
	assert!(last_error.contains(ACTIVITY_ID), "the operator must see the custodian's activity id: {last_error}");
	assert_eq!(broadcasts.load(Ordering::SeqCst), 1, "a parked broadcast is not retried");

	let after = h.withdrawals.find_by_id(withdrawal.id()).await.unwrap().unwrap();
	assert_eq!(after.state(), WithdrawalState::Processing, "the park is the outbox's; the withdrawal waits for a human");
}
