//! Integration test for the withdrawal double-broadcast race — the over-withdrawal
//! exploit. Real Postgres **and** TigerBeetle (no mocks, per the project rules); runs
//! when `DATABASE_URL` is set and a TigerBeetle replica is reachable, skips otherwise.
//!
//! The race: `request_withdrawal`'s Read-First used to read TigerBeetle alone, which lags
//! committed-but-undrained outbox rows — so a double-submit of the full balance passed
//! the solvency check twice and committed two Requested(+Dispatched) pairs. The Read-First
//! now counts the undrained reserve and refuses the second (#507); the guard below stays
//! for a pair it could not stop — one written by an older build during a rolling deploy.
//! The relay parks the second reserve (`InsufficientFunds`), but without a guard the
//! trailing Dispatched row still reaches custody: the Broadcast op has no TB leg to
//! refuse it, and real money leaves the chain with nothing locked behind it.
//!
//! What's proven: the relay refuses to broadcast a withdrawal whose clearing
//! reservation never applied (the `saga_steps` Read-First in `Relay::dispatch`) —
//! custody sees exactly one broadcast, the second withdrawal's rows are parked (not
//! dispatched), and the user's claim is never over-spent.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use domain::{
	auth::AuthSubject,
	balance::LedgerAccountKey,
	money::{Network, TxRef, Usdt, WalletAddress},
	users::{Email, UserId},
	withdrawals::{Withdrawal, WithdrawalId, WithdrawalPolicy, WithdrawalSource, WithdrawalState},
};
use piggybank_core::{
	application::{balance as balance_app, withdrawals as withdrawal_app},
	config::KycGate,
	infrastructure::{custody::StubCustody, deposits::PgDeposits, outflow::PgOutflowPolicy, relay::Relay, users::PgUsers, withdrawals::PgWithdrawals},
	ports::{
		UserRepository, WithdrawalRepository,
		custody::{BroadcastRequest, Custody, CustodyError},
		ledger::Ledger,
	},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

/// Records every broadcast custody is asked to perform, so the test can assert the
/// guarded relay sent exactly one. A port stub (not a DB mock — Postgres and
/// TigerBeetle stay real), like the `StubCustody` the other withdrawal tests wire.
struct RecordingCustody(Mutex<Vec<Uuid>>);

impl domain::architecture::Gateway for RecordingCustody {}

#[async_trait]
impl Custody for RecordingCustody {
	async fn broadcast(&self, request: &BroadcastRequest) -> Result<(), CustodyError> {
		self.0.lock().unwrap().push(request.withdrawal_id);
		Ok(())
	}
}

struct Harness {
	pool: PgPool,
	deposits: PgDeposits,
	ledger: Arc<dyn Ledger>,
	withdrawals: Arc<dyn WithdrawalRepository>,
	users: Arc<dyn UserRepository>,
	outflow: PgOutflowPolicy,
	relay: Relay,
	notify: Arc<Notify>,
	custody: Arc<RecordingCustody>,
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "broadcast guard test").await?;

	let withdrawals: Arc<dyn WithdrawalRepository> = Arc::new(PgWithdrawals::new(pool.clone()));
	let users: Arc<dyn UserRepository> = Arc::new(PgUsers::new(pool.clone()));
	let notify = Arc::new(Notify::new());
	let custody = Arc::new(RecordingCustody(Mutex::new(Vec::new())));
	let relay = Relay::new(pool.clone(), ledger.clone(), custody.clone(), notify.clone());
	Some(Harness {
		deposits: PgDeposits::new(pool.clone()),
		outflow: PgOutflowPolicy::new(pool.clone()),
		pool,
		ledger,
		withdrawals,
		users,
		relay,
		notify,
		custody,
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

/// The double-submit exploit, end to end. Two full-balance withdrawals are recorded
/// back-to-back while the first's reserve is only in the outbox, not yet in TB — the
/// second past the cover check, as an older build would have let it through. After one
/// drain: the first broadcasts, the second's reserve parks, and — the fix under test — the
/// second's broadcast is REFUSED, so custody is asked to move money exactly once.
#[tokio::test]
async fn a_withdrawal_whose_reserve_parked_is_never_broadcast() {
	let Some(h) = harness().await else { return };
	let network = Network::Bep20;
	let destination = WalletAddress::parse(network, "0x52908400098527886E0F7030069857D2E4169EE7").unwrap();

	// Fund the user with 100 and apply it to TB, so both Read-Firsts see available=100.
	let user = active_user(&h).await;
	let tx_ref = TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	balance_app::record_deposit(&h.deposits, &h.notify, tx_ref, user, network, usdt("100")).await.unwrap();
	h.relay.drain().await;

	// Double-submit the full balance WITHOUT draining in between — the exploit window.
	// The shared rail's liquidity decides queued-vs-dispatched; dispatch explicitly so
	// both withdrawals carry a Dispatched (broadcast) row regardless of rail state.
	let first = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination.clone(),
		usdt("100"),
	)
	.await
	.unwrap();
	if first.state() == WithdrawalState::Queued {
		withdrawal_app::dispatch_withdrawal(
			h.withdrawals.as_ref(),
			&StubCustody,
			&PgOutflowPolicy::new(h.pool.clone()),
			KycGate::ENFORCED,
			&h.notify,
			first.id(),
		)
		.await
		.unwrap();
	}
	let refused = withdrawal_app::request_withdrawal(
		&withdrawal_ports(&h),
		&admission(&h, KycGate::ENFORCED),
		WithdrawalId::new(),
		user,
		network,
		destination.clone(),
		usdt("100"),
	)
	.await;
	assert!(refused.is_err(), "the Read-First counts the undrained reserve: {refused:?}");
	let source = WithdrawalSource::User(user);
	let mut second = Withdrawal::request(WithdrawalId::new(), source, network, destination, usdt("100"), WithdrawalPolicy::fee_for(source, network)).unwrap();
	h.withdrawals.open(&mut second, &common::NoSpendCheck).await.unwrap();
	if second.state() == WithdrawalState::Queued {
		withdrawal_app::dispatch_withdrawal(
			h.withdrawals.as_ref(),
			&StubCustody,
			&PgOutflowPolicy::new(h.pool.clone()),
			KycGate::ENFORCED,
			&h.notify,
			second.id(),
		)
		.await
		.unwrap();
	}

	h.relay.drain().await;

	// Custody moved money exactly once — for the first withdrawal only.
	let broadcasts = h.custody.0.lock().unwrap().clone();
	assert_eq!(broadcasts, vec![first.id().raw()], "exactly one broadcast, for the withdrawal whose reserve applied");

	// The second withdrawal's rows are parked (reserve: insufficient funds; broadcast:
	// refused by the guard) — parked, never dispatched, queryable for reconciliation.
	let rows: Vec<(String, bool, bool, Option<String>)> =
		sqlx::query_as("SELECT kind, parked_at IS NOT NULL, dispatched_at IS NOT NULL, last_error FROM outbox WHERE aggregate_id = $1 ORDER BY seq")
			.bind(second.id().raw())
			.fetch_all(&h.pool)
			.await
			.expect("read the second withdrawal's outbox rows");
	assert_eq!(rows.len(), 2, "Requested + Dispatched rows for the second withdrawal");
	assert!(rows[0].1 && !rows[0].2, "the second reserve parked (insufficient funds)");
	assert!(rows[0].3.as_deref().is_some_and(|e| e.contains("insufficient funds")));
	assert!(rows[1].1 && !rows[1].2, "the second broadcast was parked, not executed");
	assert!(
		rows[1].3.as_deref().is_some_and(|e| e.contains("refusing to broadcast")),
		"the refusal reason is recorded for forensics"
	);

	// The claim was never over-spent: exactly the first gross is locked (pending), and
	// nothing spendable remains — but nothing went negative either.
	let claim = h.ledger.balance(&LedgerAccountKey::UserClaim(user)).await.unwrap();
	assert_eq!(claim.available(), 0, "the first reserve locks the full claim; the second locked nothing");

	// The stranded second withdrawal stays `processing` — the reaper alerts on it and
	// reconciliation surfaces both the parked rows and the clearing mismatch; recovery
	// is an operator action (fail → bounded-retry void → park), never an auto-void.
	let stranded = h.withdrawals.find_by_id(second.id()).await.unwrap().unwrap();
	assert_eq!(stranded.state(), WithdrawalState::Processing);
}
