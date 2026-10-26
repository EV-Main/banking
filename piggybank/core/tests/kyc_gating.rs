//! KYC enforcement — the money plane refusing an unverified investor. Real Postgres
//! **and** TigerBeetle (no mocks, per the project rules); runs when `DATABASE_URL` is set
//! and a replica is reachable (`nix run .#db` + `.#tb`), skips otherwise. Each test uses a
//! fresh provisioned user, so runs are isolated on shared infrastructure.
//!
//! `kyc_level` is the concierge tier mirrored onto the local `users` row by the lifecycle
//! bridge; banking never authors it, so these tests set the column the way the bridge
//! does. Tier 0 is what `provision` leaves behind — a registration and a confirmed email,
//! nothing verified.
//!
//! What's proven:
//!
//! - tier 0 gets **no deposit address**, and the address gateway is never reached — the
//!   gate sits above the port precisely because the first call there mints a signer
//!   keypair, and a key issued to an unverified user is an address the fund must watch,
//!   sweep and account for forever;
//! - tier 0 **cannot withdraw**, with a funded claim, so the refusal is the gate and not
//!   insolvency;
//! - tier 1 does both;
//! - the refusals are told apart at the wire: an unconfigured rail is `Ok(None)` ("this
//!   rail cannot fund you"), an unverified caller is `Forbidden` ("finish verification");
//! - a **payment out of the fee allocation is not gated** — its source is the platform's
//!   own money, held by people through units, with no user behind it to verify; what
//!   gates it is the owners' quorum.

use std::sync::{
	Arc,
	atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use domain::{
	auth::AuthSubject,
	balance::{LedgerAccountKey, Party, ServiceId, TransferCode},
	error::DomainError,
	money::{Network, TxRef, Usdt, WalletAddress},
	payments::{PaymentDestination, PaymentReason, PaymentTerms},
	users::{Email, UserId},
	withdrawals::WithdrawalId,
};
use piggybank_core::{
	application::{balance as balance_app, payments as payments_app, wallet as wallet_app, withdrawals as withdrawal_app},
	config::KycGate,
	infrastructure::{
		allocations::PgAllocations, custody::StubCustody, deposits::PgDeposits, nav::PgNav, outflow::PgOutflowPolicy, payments::PgPayments, positions::PgFundPositions, relay::Relay,
		users::PgUsers, withdrawals::PgWithdrawals,
	},
	ports::{
		DepositAddresses, UserRepository, WithdrawalRepository,
		ledger::{Ledger, LedgerTransfer},
		payments::SeatView,
	},
};
use sqlx::PgPool;
use tokio::sync::{MutexGuard, Notify};
use uuid::Uuid;

mod common;

/// A structurally-valid derived-grade address per network — the shape only has to survive
/// `WalletAddress::parse`; nothing here signs or broadcasts.
fn sample_address(network: Network) -> &'static str {
	match network {
		Network::Bep20 | Network::Polygon => "0x52908400098527886E0F7030069857D2E4169EE7",
		Network::Trc20 => "TJRabPrwbZy45sbavfcjinPJC18kjpRTv8",
		Network::Ton => "EQCD39VS5jcptHL8vMjEXrzGaRcCVYto7HUn4bpAOg8xqB2N",
	}
}

/// An in-memory [`DepositAddresses`] that counts the calls it receives. The count is the
/// assertion that matters: a gate placed *after* the port would still return no address
/// while having already minted the key, and only the counter can tell the two apart.
struct CountingAddresses {
	calls: Arc<AtomicUsize>,
}

impl domain::architecture::Gateway for CountingAddresses {}

#[async_trait]
impl DepositAddresses for CountingAddresses {
	async fn address(&self, _user: UserId, network: Network) -> Result<Option<WalletAddress>, DomainError> {
		self.calls.fetch_add(1, Ordering::SeqCst);
		Ok(Some(WalletAddress::parse(network, sample_address(network))?))
	}
}

struct Harness {
	pool: PgPool,
	deposits: PgDeposits,
	ledger: Arc<dyn Ledger>,
	withdrawals: Arc<dyn WithdrawalRepository>,
	users: Arc<dyn UserRepository>,
	outflow: PgOutflowPolicy,
	addresses: CountingAddresses,
	address_calls: Arc<AtomicUsize>,
	relay: Relay,
	notify: Arc<Notify>,
	/// This suite is exposed to the shared-outbox race the same way `allocation_registry`
	/// was (#294/#298): every test deposits and drains, and the since-retired `a_revenue_payout_is_not_gated_on_kyc`
	/// drains five times. It has failed for it in CI — run 34894158429, `a_revenue_payout_is_not_gated_on_kyc`
	/// panicking on `fund the fee claim: Validation("insufficient available balance to
	/// withdraw")`: the 200 USDT this test had just deposited and drained was not on the
	/// ledger when the next line spent it. The user is freshly provisioned per test, so no
	/// sibling could have moved that claim — the single `drain()` returned before applying
	/// the test's own row, which is the early return this guard and `drain_to_quiescence`
	/// close between them. (The local repro attempt did not land it: 60 runs at eight threads
	/// came back green.) Held for the test's whole life, declared last so it is released
	/// after the relay.
	_serial: MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "KYC gating test").await?;

	let withdrawals: Arc<dyn WithdrawalRepository> = Arc::new(PgWithdrawals::new(pool.clone()));
	let users: Arc<dyn UserRepository> = Arc::new(PgUsers::new(pool.clone()));
	let address_calls = Arc::new(AtomicUsize::new(0));
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone());
	Some(Harness {
		deposits: PgDeposits::new(pool.clone()),
		outflow: PgOutflowPolicy::new(pool.clone()),
		pool,
		ledger,
		withdrawals,
		users,
		addresses: CountingAddresses { calls: address_calls.clone() },
		address_calls,
		relay,
		notify,
		_serial: serial,
	})
}

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

fn address_ports(h: &Harness) -> wallet_app::DepositAddressPorts<'_> {
	wallet_app::DepositAddressPorts {
		deposit_addresses: &h.addresses,
		users: h.users.as_ref(),
	}
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn unique_tx_ref() -> TxRef {
	TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap()
}

fn destination(network: Network) -> WalletAddress {
	WalletAddress::parse(network, sample_address(network)).unwrap()
}

/// A fresh user at the given mirrored KYC tier. `provision` leaves tier 0 (the schema
/// default); anything above it is written the way the lifecycle bridge writes it, since
/// banking owns no transition for the column.
async fn user_at_tier(h: &Harness, tier: i32) -> UserId {
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("u{}@example.com", Uuid::new_v4().simple())).unwrap();
	let user = h.users.provision(subject, email, true).await.unwrap().id();
	if tier != 0 {
		common::set_kyc_level(&h.pool, user, tier).await;
	}
	user
}

async fn deposit(h: &Harness, user: UserId, network: Network, amount: &str) {
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, network, usdt(amount)).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn an_unverified_user_gets_no_deposit_address_and_never_reaches_the_signer() {
	let Some(h) = harness().await else { return };
	let user = user_at_tier(&h, 0).await;

	let err = wallet_app::get_deposit_address(&address_ports(&h), &Network::ALL, KycGate::ENFORCED, user, Network::Bep20)
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)), "an unverified caller is forbidden an address, got {err:?}");
	assert_eq!(h.address_calls.load(Ordering::SeqCst), 0, "no keypair may be provisioned for an unverified user");
}

#[tokio::test]
async fn a_verified_user_gets_a_deposit_address() {
	let Some(h) = harness().await else { return };
	let user = user_at_tier(&h, 1).await;

	let address = wallet_app::get_deposit_address(&address_ports(&h), &Network::ALL, KycGate::ENFORCED, user, Network::Bep20)
		.await
		.expect("a verified caller passes the gate")
		.expect("the rail serves its derived address");
	assert_eq!(address.as_str(), sample_address(Network::Bep20));
	assert_eq!(h.address_calls.load(Ordering::SeqCst), 1, "the gateway is reached exactly once");
}

/// The two refusals must stay distinguishable at the wire: the wallet host shows "try another
/// chain" for one and "finish verification" for the other, and it can only pick from the
/// shape of the answer. An unconfigured rail is an absent value; an unverified caller is
/// an error. Folding the second into the first would leave the screen unable to choose.
#[tokio::test]
async fn an_unconfigured_rail_and_an_unverified_caller_answer_differently() {
	let Some(h) = harness().await else { return };
	let verified = user_at_tier(&h, 1).await;
	let unverified = user_at_tier(&h, 0).await;
	let configured = [Network::Bep20];

	let rail_gate = wallet_app::get_deposit_address(&address_ports(&h), &configured, KycGate::ENFORCED, verified, Network::Ton)
		.await
		.expect("an unconfigured rail is not an error");
	assert!(rail_gate.is_none(), "an unconfigured rail answers with no address");

	let kyc_gate = wallet_app::get_deposit_address(&address_ports(&h), &configured, KycGate::ENFORCED, unverified, Network::Bep20)
		.await
		.unwrap_err();
	assert!(matches!(kyc_gate, DomainError::Forbidden(_)), "an unverified caller answers with an error, got {kyc_gate:?}");
	assert_eq!(h.address_calls.load(Ordering::SeqCst), 0, "neither refusal reached the gateway");
}

/// The wallet overview stays fully readable for an unverified user — their balance is
/// their own — but no rail carries an address, because presenting one would have minted
/// the key the gate exists to withhold.
#[tokio::test]
async fn an_unverified_wallet_shows_the_balance_but_no_address() {
	let Some(h) = harness().await else { return };
	let user = user_at_tier(&h, 0).await;
	let positions = PgFundPositions::new(h.pool.clone());
	let nav = PgNav::new(h.pool.clone());

	deposit(&h, user, Network::Bep20, "100").await;

	let wallet = wallet_app::get_wallet(
		&wallet_app::WalletPorts {
			ledger: h.ledger.as_ref(),
			positions: &positions,
			nav: &nav,
			deposit_addresses: &h.addresses,
			users: h.users.as_ref(),
			compartments: &piggybank_core::infrastructure::compartments::PgCompartments::new(h.pool.clone()),
		},
		&[Network::Bep20],
		KycGate::ENFORCED,
		user,
	)
	.await
	.expect("the overview is readable at any tier");

	assert_eq!(wallet.balance.available, usdt("100"), "the user's own balance is never hidden from them");
	assert_eq!(wallet.deposit_addresses.len(), 1, "the configured rail is still listed");
	assert!(wallet.deposit_addresses[0].address.is_none(), "an unverified user carries no address on any rail");
	assert_eq!(h.address_calls.load(Ordering::SeqCst), 0, "no keypair may be provisioned for an unverified user");
}

#[tokio::test]
async fn an_unverified_user_cannot_withdraw() {
	let Some(h) = harness().await else { return };
	let user = user_at_tier(&h, 0).await;
	let network = Network::Bep20;
	// Funded first, so the refusal below can only be the verification gate — an unfunded
	// claim would fail the solvency Read-First instead and prove nothing.
	deposit(&h, user, network, "100").await;

	let err = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)), "an unverified account is forbidden from withdrawing, got {err:?}");

	assert_eq!(h.withdrawals.list_by_user(user).await.unwrap().len(), 0, "a refused request must leave no withdrawal behind");
}

#[tokio::test]
async fn a_verified_user_can_withdraw() {
	let Some(h) = harness().await else { return };
	let user = user_at_tier(&h, 1).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "100").await;

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
	.expect("a verified account withdraws");
	assert_eq!(withdrawal.net_amount(), usdt("49"), "the flat 1 USDT fee is retained");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let claim = h.ledger.balance(&LedgerAccountKey::UserClaim(user)).await.unwrap();
	assert_eq!(Usdt::from_base_units(claim.locked), usdt("50"), "the gross is reserved into clearing");
}

/// A payment out of the fee allocation is the platform's own money moving — there is no
/// user behind `Party::Service(fee)` (its holders own it through units), so a KYC tier is
/// not merely unchecked at open, there is nothing to check: what stands in front of it is
/// the owners' approval. The gate stays out of the DESTINATION too: the money lands on a
/// tier-0 investor's claim, and the verification floor applies when they try to take it
/// out, not when it arrives. Exercised end to end rather than asserted by reading the code.
#[tokio::test]
async fn a_payment_from_the_fee_allocation_is_not_gated_on_kyc() {
	let Some(h) = harness().await else { return };
	let recipient = user_at_tier(&h, 0).await;
	let fee_claim = LedgerAccountKey::ServiceClaim(ServiceId::fee());
	// Fund the whole amount rather than leaning on whatever the shared claim happens to
	// hold: the open runs the real solvency pre-check against it.
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: fee_claim.clone(),
			amount: usdt("2").base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.expect("fund the fee allocation's claim");
	let initiator = user_at_tier(&h, 0).await;
	let (payments, allocations) = (PgPayments::new(h.pool.clone()), PgAllocations::new(h.pool.clone()));
	let ports = payments_app::PaymentPorts {
		payments: &payments,
		users: h.users.as_ref(),
		withdrawals: h.withdrawals.as_ref(),
		ledger: h.ledger.as_ref(),
		custody: &StubCustody,
		policy: &h.outflow,
		allocations: &allocations,
		relay: &h.notify,
		configured: &Network::ALL,
		kyc: KycGate::ENFORCED,
	};
	let terms = PaymentTerms::new(
		Party::Service(ServiceId::fee()),
		PaymentDestination::Internal(Party::User(recipient)),
		usdt("2"),
		PaymentReason::new("a documented reason").unwrap(),
	)
	.unwrap();
	// Neither end is a verified account, and the open passes: the tier-0 initiator is not the
	// source, and the tier-0 recipient is only being paid.
	let view = payments_app::open(&ports, initiator, terms, now_unix())
		.await
		.expect("a payment from the fee allocation is never gated on a user's KYC tier");
	assert!(
		matches!(view.seat, SeatView::Concierge(_)),
		"the fund's money is the owners' to release, and no investor is asked to consent to it"
	);
	// The open order would hold the fee claim's one fund-owned slot for every later test.
	sqlx::query("UPDATE payments SET state = 'cancelled', decided_at = now() WHERE id = $1")
		.bind(view.order.id().raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

fn now_unix() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// The switch in its other position — `KYC_GATE_ENABLED=false`. It is not a third
/// behaviour: an unverified caller is issued an address and the gateway IS reached, which
/// is exactly what the deposit surface did before the gate landed.
#[tokio::test]
async fn a_lifted_gate_issues_an_unverified_user_an_address() {
	let Some(h) = harness().await else { return };
	let user = user_at_tier(&h, 0).await;

	let address = wallet_app::get_deposit_address(&address_ports(&h), &Network::ALL, KycGate::LIFTED, user, Network::Bep20)
		.await
		.expect("a lifted gate refuses nobody")
		.expect("the rail serves its derived address");
	assert_eq!(address.as_str(), sample_address(Network::Bep20));
	assert_eq!(h.address_calls.load(Ordering::SeqCst), 1, "the gateway is reached for an unverified user once the gate is lifted");
}

/// The wallet overview follows the same switch: with the gate lifted the unverified
/// caller's rails carry their addresses instead of `None`.
#[tokio::test]
async fn a_lifted_gate_puts_an_address_on_an_unverified_wallet() {
	let Some(h) = harness().await else { return };
	let user = user_at_tier(&h, 0).await;
	let positions = PgFundPositions::new(h.pool.clone());
	let nav = PgNav::new(h.pool.clone());

	let wallet = wallet_app::get_wallet(
		&wallet_app::WalletPorts {
			ledger: h.ledger.as_ref(),
			positions: &positions,
			nav: &nav,
			deposit_addresses: &h.addresses,
			users: h.users.as_ref(),
			compartments: &piggybank_core::infrastructure::compartments::PgCompartments::new(h.pool.clone()),
		},
		&[Network::Bep20],
		KycGate::LIFTED,
		user,
	)
	.await
	.expect("the overview is readable at any tier");

	assert_eq!(
		wallet.deposit_addresses[0].address.as_ref().map(|a| a.as_str().to_owned()),
		Some(sample_address(Network::Bep20).to_owned()),
		"a lifted gate leaves the rail fundable for an unverified user"
	);
	assert_eq!(h.address_calls.load(Ordering::SeqCst), 1, "the gateway is reached once the gate is lifted");
}

/// The half that is easy to forget: lifting the gate at *admission* only would accept an
/// unverified user's withdrawal and then leave it queued forever, since the payout gate
/// re-checks the tier at dispatch. Both points are exercised here on one withdrawal —
/// refused by an enforced gate, paid by a lifted one — so the two can never be wired
/// apart again.
#[tokio::test]
async fn a_lifted_gate_both_admits_and_dispatches_an_unverified_withdrawal() {
	let Some(h) = harness().await else { return };
	let user = user_at_tier(&h, 0).await;
	let network = Network::Bep20;
	deposit(&h, user, network, "100").await;

	let withdrawal = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h),
		&admission(&h, KycGate::LIFTED),
		WithdrawalId::new(),
		user,
		network,
		destination(network),
		usdt("50"),
	)
	.await
	.expect("a lifted gate admits an unverified withdrawal");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let policy = PgOutflowPolicy::new(h.pool.clone());
	let refused = withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &StubCustody, &policy, KycGate::ENFORCED, &h.notify, withdrawal.id())
		.await
		.unwrap_err();
	assert!(
		matches!(refused, DomainError::Forbidden(_)),
		"the payout gate still refuses an unverified owner while the gate is enforced, got {refused:?}"
	);

	withdrawal_app::dispatch_withdrawal(h.withdrawals.as_ref(), &StubCustody, &policy, KycGate::LIFTED, &h.notify, withdrawal.id())
		.await
		.expect("a lifted gate pays the same withdrawal out");

	// Accepting the dispatch only queues it. Drain so the claim that it was *paid* is
	// actually proven, and so this test does not hand its undrained row to whichever test
	// takes the guard next — the outbox is one table per binary and the relay works it in
	// `seq` order, so a backlog left here would be reported against a stranger.
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE aggregate_id = $1 AND dispatched_at IS NULL")
		.bind(withdrawal.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(queued, 0, "both of the withdrawal's events reached the relay, the payout included");
}
