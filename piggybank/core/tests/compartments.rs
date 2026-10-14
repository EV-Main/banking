//! Users' compartments by scope — real Postgres and TigerBeetle. Every query of the
//! compartment store runs here at least once: listing the registry with its policies, the
//! idempotent move record, the rolling 24 h sum, and the relay posting a move.
//!
//! Scopes a test changes (state, policy) are its own, created under a fresh id, so tests
//! sharing this binary's database never see each other's switches. The seeded `invest` and
//! `sa` rows are only read.

use std::sync::Arc;

use domain::{
	auth::AuthSubject,
	balance::{LedgerAccountKey, TransferCode},
	error::DomainError,
	money::{Network, TxRef, Usdt},
	scopes::{Compartment, MoveRefusal, MoveState, ScopeId, ScopeState},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{
		balance as balance_app,
		compartments::{self as compartments_app, CompartmentPorts, MoveError},
		wallet as wallet_app,
	},
	infrastructure::{
		compartments::PgCompartments, deposits::PgDeposits, nav::PgNav, outflow::PgOutflowPolicy, positions::PgFundPositions, reaper::Reaper, redemptions::PgRedemptions, relay::Relay,
		users::PgUsers, withdrawals::PgWithdrawals,
	},
	ports::{
		UserRepository,
		compartments::{CompartmentStore, InFlight, MoveAdmission},
		ledger::{Ledger, LedgerTransfer},
	},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

struct Harness {
	pool: PgPool,
	ledger: Arc<dyn Ledger>,
	store: PgCompartments,
	outflow: PgOutflowPolicy,
	users: PgUsers,
	deposits: PgDeposits,
	relay: Relay,
	notify: Arc<Notify>,
	_serial: tokio::sync::MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "compartment tests").await?;
	let notify = Arc::new(Notify::new());
	Some(Harness {
		store: PgCompartments::new(pool.clone()),
		outflow: PgOutflowPolicy::new(pool.clone()),
		users: PgUsers::new(pool.clone()),
		deposits: PgDeposits::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(piggybank_core::infrastructure::custody::StubCustody), notify.clone()),
		pool,
		ledger,
		notify,
		_serial: serial,
	})
}

fn ports(h: &Harness) -> CompartmentPorts<'_> {
	CompartmentPorts {
		store: &h.store,
		ledger: h.ledger.as_ref(),
		policy: &h.outflow,
		relay: &h.notify,
	}
}

fn usdt(raw: &str) -> Usdt {
	Usdt::parse_decimal(raw).unwrap()
}

fn side(raw: &str) -> Compartment {
	Compartment::parse(raw).unwrap()
}

fn key() -> String {
	Uuid::new_v4().to_string()
}

/// A verified user holding `funds` in `main`, landed on the ledger.
async fn funded_user(h: &Harness, funds: &str) -> UserId {
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("u{}@example.com", Uuid::new_v4().simple())).unwrap();
	let user = h.users.provision(subject, email, true).await.unwrap().id();
	common::set_kyc_level(&h.pool, user, 1).await;
	let tx_ref = TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	balance_app::record_deposit(&h.deposits, &h.notify, tx_ref, user, Network::Bep20, usdt(funds)).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	user
}

/// A scope of this test's own, active, with the default policy.
async fn own_scope(h: &Harness) -> ScopeId {
	let id = ScopeId::parse(&format!("t_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	sqlx::query("INSERT INTO scopes (id, display_name) VALUES ($1, 'Test scope')")
		.bind(id.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("INSERT INTO scope_policies (scope) VALUES ($1)").bind(id.as_str()).execute(&h.pool).await.unwrap();
	id
}

/// Set one of the policy's TEXT amount columns.
async fn set_policy(h: &Harness, scope: &ScopeId, column: &str, value: &str) {
	// `column` is one of this file's literals, never input.
	let sql = format!("UPDATE scope_policies SET {column} = $2, updated_at = now() WHERE scope = $1");
	sqlx::query(sqlx::AssertSqlSafe(sql)).bind(scope.as_str()).bind(value).execute(&h.pool).await.unwrap();
}

async fn set_state(h: &Harness, scope: &ScopeId, state: ScopeState) {
	sqlx::query("UPDATE scopes SET state = $2 WHERE id = $1")
		.bind(scope.as_str())
		.bind(state.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
}

async fn posted(h: &Harness, key: &LedgerAccountKey) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(key).await.unwrap().posted)
}

async fn move_funds(h: &Harness, user: UserId, from: &str, to: &str, amount: &str, idempotency_key: &str) -> Result<piggybank_core::ports::compartments::RecordedMove, MoveError> {
	compartments_app::move_funds(&ports(h), user, side(from), side(to), usdt(amount), idempotency_key).await
}

fn refusal(result: Result<piggybank_core::ports::compartments::RecordedMove, MoveError>) -> MoveRefusal {
	match result {
		Err(MoveError::Refused(refusal)) => refusal,
		other => panic!("expected a refusal, got {other:?}"),
	}
}

#[tokio::test]
async fn the_seeded_scopes_list_after_main_with_the_default_ceilings() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "5").await;
	let (views, kyc_level) = compartments_app::list_compartments(&ports(&h), user).await.unwrap();
	assert_eq!(kyc_level, 1);
	assert_eq!(views[0].compartment, Compartment::Main, "main comes first");
	assert_eq!(views[0].total, usdt("5"));
	assert!(views[0].scope.is_none(), "main has no policy");
	for seeded in ["invest", "sa"] {
		let view = views.iter().find(|v| v.compartment.as_str() == seeded).unwrap_or_else(|| panic!("{seeded} is not listed"));
		assert_eq!(view.total, Usdt::ZERO, "never entered reads zero");
		let scope = view.scope.as_ref().unwrap();
		assert_eq!(scope.state, ScopeState::Active);
		assert_eq!(scope.policy.min_kyc_level, 0);
		assert_eq!(scope.policy.max_balance, None);
		assert_eq!(
			(scope.policy.auto_payout_per_op, scope.policy.auto_payout_24h, scope.policy.auto_transfer_24h),
			(usdt("1000"), usdt("5000"), usdt("5000"))
		);
	}
	let treasury: Option<String> = sqlx::query_scalar("SELECT owner_client FROM scope_accounts WHERE scope = 'main' AND label = 'treasury'")
		.fetch_optional(&h.pool)
		.await
		.unwrap();
	assert_eq!(treasury.as_deref(), Some("banking"), "the treasury is banking's own account");
}

#[tokio::test]
async fn money_moves_into_a_compartment_and_back_on_the_ledger() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "100").await;
	let main = LedgerAccountKey::UserClaim(user);
	let sa = LedgerAccountKey::ScopeClaim(user, ScopeId::parse("sa").unwrap());

	let moved = move_funds(&h, user, "main", "sa", "30", &key()).await.unwrap();
	assert_eq!(moved.movement.amount(), usdt("30"));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!((posted(&h, &main).await, posted(&h, &sa).await), (usdt("70"), usdt("30")));

	move_funds(&h, user, "sa", "main", "12.5", &key()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!((posted(&h, &main).await, posted(&h, &sa).await), (usdt("82.5"), usdt("17.5")));

	let (views, _) = compartments_app::list_compartments(&ports(&h), user).await.unwrap();
	let listed = views.iter().find(|v| v.compartment.as_str() == "sa").unwrap();
	assert_eq!((listed.available, listed.held, listed.total), (usdt("17.5"), Usdt::ZERO, usdt("17.5")));

	// A move is net-zero on the cash invariant and its money is a named part of the claims.
	let inv = h.ledger.cash_invariant().await.unwrap();
	assert!(inv.balanced(), "custody != claims: {inv:?}");
	assert!(inv.scope_claims >= usdt("17.5").base_units(), "compartments are counted: {inv:?}");
	assert_eq!(inv.unclassified(), 0, "no compartment reads as an unknown kind: {inv:?}");
}

#[tokio::test]
async fn a_repeated_key_returns_the_original_move_and_a_reused_one_conflicts() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "50").await;
	let k = key();
	let first = move_funds(&h, user, "main", "sa", "10", &k).await.unwrap();
	let again = move_funds(&h, user, "main", "sa", "10", &k).await.unwrap();
	assert_eq!(again.movement.id(), first.movement.id(), "a repeat is the original");
	assert_eq!(again.created_at, first.created_at);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, &LedgerAccountKey::UserClaim(user)).await, usdt("40"), "moved once");

	let reused = move_funds(&h, user, "main", "sa", "11", &k).await;
	assert!(matches!(reused, Err(MoveError::Domain(DomainError::Conflict(_)))), "{reused:?}");
	// The store answers a taken key with the original too, without recording a second move.
	let mut racing = domain::scopes::CompartmentMove::request(domain::scopes::CompartmentMoveId::new(), user, side("main"), side("sa"), usdt("10")).unwrap();
	match h.store.record_move(&mut racing, &k, &Refuse).await.unwrap() {
		piggybank_core::ports::compartments::MoveOutcome::Existing(original) => assert_eq!(original.movement.id(), first.movement.id()),
		other => panic!("a taken key was recorded again: {other:?}"),
	}
}

#[tokio::test]
async fn a_move_the_source_cannot_cover_is_refused_before_anything_is_recorded() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "5").await;
	assert_eq!(refusal(move_funds(&h, user, "main", "sa", "5.01", &key()).await), MoveRefusal::InsufficientFunds);
	assert_eq!(
		refusal(move_funds(&h, user, "sa", "main", "1", &key()).await),
		MoveRefusal::InsufficientFunds,
		"an empty compartment"
	);
	let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM compartment_moves WHERE user_id = $1")
		.bind(user.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(recorded, 0);
}

#[tokio::test]
async fn malformed_and_unknown_moves_are_refused() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "5").await;
	let invalid = |r: Result<_, MoveError>| matches!(r, Err(MoveError::Domain(DomainError::Validation(_))));
	assert!(invalid(move_funds(&h, user, "sa", "invest", "1", &key()).await), "never scope to scope");
	assert!(invalid(move_funds(&h, user, "main", "main", "1", &key()).await));
	assert!(invalid(move_funds(&h, user, "main", "sa", "1", "").await), "a key is required");
	assert!(invalid(move_funds(&h, user, "main", "sa", "1", &"k".repeat(129)).await));
	let unknown = move_funds(&h, user, "main", "nowhere", "1", &key()).await;
	assert!(matches!(unknown, Err(MoveError::Domain(DomainError::NotFound { entity: "scope", .. }))), "{unknown:?}");
}

#[tokio::test]
async fn a_frozen_or_closed_scope_takes_nothing_in_but_lets_money_out() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h).await;
	let user = funded_user(&h, "20").await;
	move_funds(&h, user, "main", scope.as_str(), "10", &key()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	set_state(&h, &scope, ScopeState::Frozen).await;
	assert_eq!(refusal(move_funds(&h, user, "main", scope.as_str(), "1", &key()).await), MoveRefusal::ScopeFrozen);
	move_funds(&h, user, scope.as_str(), "main", "4", &key()).await.expect("out of a frozen scope");

	set_state(&h, &scope, ScopeState::Closed).await;
	assert_eq!(refusal(move_funds(&h, user, "main", scope.as_str(), "1", &key()).await), MoveRefusal::ScopeClosed);
	move_funds(&h, user, scope.as_str(), "main", "6", &key()).await.expect("out of a closed scope");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Emptied, the closed scope drops off the list; a stranger never saw it.
	let (views, _) = compartments_app::list_compartments(&ports(&h), user).await.unwrap();
	assert!(views.iter().all(|v| v.compartment.as_str() != scope.as_str()), "an empty closed scope is listed");
}

#[tokio::test]
async fn a_frozen_user_moves_nothing_in_but_takes_their_money_back_to_main() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "20").await;
	move_funds(&h, user, "main", "sa", "5", &key()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	sqlx::query("UPDATE users SET frozen = true WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
	assert_eq!(refusal(move_funds(&h, user, "main", "sa", "1", &key()).await), MoveRefusal::UserFrozen);
	// The money never leaves the platform: back to `main` goes on (wallet.proto).
	move_funds(&h, user, "sa", "main", "5", &key()).await.expect("out of a compartment while frozen");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, &LedgerAccountKey::UserClaim(user)).await, usdt("20"));
}

#[tokio::test]
async fn the_scope_policy_gates_money_in_and_never_money_out() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h).await;
	let s = scope.as_str();
	let user = funded_user(&h, "1000").await;

	sqlx::query("UPDATE scope_policies SET min_kyc_level = 2 WHERE scope = $1")
		.bind(s)
		.execute(&h.pool)
		.await
		.unwrap();
	assert_eq!(refusal(move_funds(&h, user, "main", s, "1", &key()).await), MoveRefusal::KycLevel);
	let view = compartments_app::get_scope_policy(&ports(&h), user, &scope).await.unwrap();
	assert!(!view.caller_eligible, "level 1 under a floor of 2");
	common::set_kyc_level(&h.pool, user, 2).await;
	assert!(compartments_app::get_scope_policy(&ports(&h), user, &scope).await.unwrap().caller_eligible);

	set_policy(&h, &scope, "max_per_move", &usdt("50").base_units().to_string()).await;
	assert_eq!(refusal(move_funds(&h, user, "main", s, "50.5", &key()).await), MoveRefusal::MaxPerMove);
	move_funds(&h, user, "main", s, "50", &key()).await.unwrap();

	set_policy(&h, &scope, "max_moved_in_24h", &usdt("80").base_units().to_string()).await;
	move_funds(&h, user, "main", s, "30", &key()).await.unwrap();
	// 50 + 30, both still pending in the control plane: the 24 h figure counts them.
	assert_eq!(refusal(move_funds(&h, user, "main", s, "0.01", &key()).await), MoveRefusal::MaxMovedIn24h);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// Moving out is never limited by policy, and does not reduce the 24 h figure.
	move_funds(&h, user, s, "main", "70", &key()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(refusal(move_funds(&h, user, "main", s, "0.01", &key()).await), MoveRefusal::MaxMovedIn24h);

	sqlx::query("UPDATE scope_policies SET max_moved_in_24h = NULL, max_balance = $2 WHERE scope = $1")
		.bind(s)
		.bind(usdt("25").base_units().to_string())
		.execute(&h.pool)
		.await
		.unwrap();
	// 10 in the compartment now; 15 more reaches the cap, a cent past it does not.
	assert_eq!(refusal(move_funds(&h, user, "main", s, "15.01", &key()).await), MoveRefusal::MaxBalance);
	move_funds(&h, user, "main", s, "15", &key()).await.unwrap();
	// Not on the ledger yet, but promised: the cap counts the pending move in.
	assert_eq!(refusal(move_funds(&h, user, "main", s, "0.01", &key()).await), MoveRefusal::MaxBalance);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, &LedgerAccountKey::ScopeClaim(user, scope.clone())).await, usdt("25"));
}

#[tokio::test]
async fn main_has_no_policy_and_an_unknown_scope_none_either() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "1").await;
	for scope in [ScopeId::main(), ScopeId::parse("nowhere").unwrap()] {
		let err = compartments_app::get_scope_policy(&ports(&h), user, &scope).await.unwrap_err();
		assert!(matches!(err, DomainError::NotFound { entity: "scope", .. }), "{scope}: {err:?}");
	}
}

/// An admission that refuses everything — proves the store answers a taken key before it
/// asks.
struct Refuse;

#[async_trait::async_trait]
impl MoveAdmission for Refuse {
	async fn admit(&self, _: &InFlight) -> Result<(), MoveError> {
		Err(MoveError::Refused(MoveRefusal::InsufficientFunds))
	}
}

async fn state_of(h: &Harness, id: Uuid) -> (String, Option<String>) {
	sqlx::query_as("SELECT state, failure_reason FROM compartment_moves WHERE id = $1")
		.bind(id)
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

#[tokio::test]
async fn a_move_is_recorded_pending_and_stamped_posted_by_the_relay() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "10").await;
	let moved = move_funds(&h, user, "main", "sa", "4", &key()).await.unwrap();
	assert_eq!(moved.state, MoveState::Pending);
	assert!(moved.created_at > 0);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(state_of(&h, moved.movement.id().raw()).await, ("posted".to_owned(), None));
}

/// Two moves racing for the same money: the second is refused at once, because the first
/// is counted while it is still in flight — no parking, no relay failure.
#[tokio::test]
async fn the_loser_of_two_moves_is_refused_at_once_not_parked() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "100").await;
	move_funds(&h, user, "main", "sa", "60", &key()).await.unwrap();
	assert_eq!(refusal(move_funds(&h, user, "main", "invest", "60", &key()).await), MoveRefusal::InsufficientFunds);
	move_funds(&h, user, "main", "invest", "40", &key())
		.await
		.expect("what is left after the first is still spendable");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, &LedgerAccountKey::UserClaim(user)).await, Usdt::ZERO);
}

/// The ledger refusing a move — another flow took the money between the check and the
/// relay — fails it for good: nothing moves, nothing parks, and an operator replaying the
/// event later does not carry it out.
#[tokio::test]
async fn a_move_the_ledger_refuses_fails_for_good() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "10").await;
	let moved = move_funds(&h, user, "main", "sa", "10", &key()).await.unwrap();
	// Spend the claim out from under the queued move, straight on the ledger.
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::UserClaim(user),
			credit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			amount: usdt("5").base_units(),
			code: TransferCode::Withdraw,
			reference: 0,
		})
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let id = moved.movement.id().raw();
	assert_eq!(state_of(&h, id).await, ("failed".to_owned(), Some("insufficient_funds".to_owned())));
	let parked: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE aggregate_id = $1 AND parked_at IS NOT NULL")
		.bind(id)
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(parked, 0, "a refused move is an outcome, not an event for an operator");

	// Top the claim back up and replay the event as an unpark would: still nothing moves.
	balance_app::record_deposit(
		&h.deposits,
		&h.notify,
		TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap(),
		user,
		Network::Bep20,
		usdt("5"),
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	sqlx::query("UPDATE outbox SET dispatched_at = NULL WHERE aggregate_id = $1")
		.bind(id)
		.execute(&h.pool)
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, &LedgerAccountKey::UserClaim(user)).await, usdt("10"), "the failed move stayed failed");
	assert_eq!(posted(&h, &LedgerAccountKey::ScopeClaim(user, ScopeId::parse("sa").unwrap())).await, Usdt::ZERO);
}

/// A move whose event a relay dispatched without posting (an older build that did not know
/// the kind) is found by the reaper and replayed — once, whatever the number of sweeps.
#[tokio::test]
async fn the_reaper_replays_a_move_whose_event_was_lost() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "10").await;
	let moved = move_funds(&h, user, "main", "sa", "3", &key()).await.unwrap();
	let id = moved.movement.id().raw();
	sqlx::query("UPDATE outbox SET dispatched_at = now() WHERE aggregate_id = $1")
		.bind(id)
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("UPDATE compartment_moves SET updated_at = now() - interval '1 hour' WHERE id = $1")
		.bind(id)
		.execute(&h.pool)
		.await
		.unwrap();
	let reaper = Reaper::new(
		h.pool.clone(),
		Arc::new(PgWithdrawals::new(h.pool.clone())),
		Arc::new(PgRedemptions::new(h.pool.clone())),
		h.notify.clone(),
	)
	.with_move_redrive_after(std::time::Duration::from_secs(60));
	assert!(reaper.sweep().await.unwrap().redriven_compartment_moves >= 1);
	assert_eq!(reaper.sweep().await.unwrap().redriven_compartment_moves, 0, "a replay waits for its own event");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(state_of(&h, id).await.0, "posted");
	assert_eq!(posted(&h, &LedgerAccountKey::ScopeClaim(user, ScopeId::parse("sa").unwrap())).await, usdt("3"));
	// Replaying a posted move again cannot post it twice: the transfer id is the move's.
	sqlx::query("UPDATE compartment_moves SET state = 'pending', updated_at = now() - interval '1 hour' WHERE id = $1")
		.bind(id)
		.execute(&h.pool)
		.await
		.unwrap();
	reaper.sweep().await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, &LedgerAccountKey::ScopeClaim(user, ScopeId::parse("sa").unwrap())).await, usdt("3"), "posted once");
	assert_eq!(state_of(&h, id).await.0, "posted");
}

/// GetWallet and the balance RPCs add the compartments to `main`.
#[tokio::test]
async fn the_wallet_total_counts_the_compartments() {
	let Some(h) = harness().await else { return };
	let user = funded_user(&h, "30").await;
	move_funds(&h, user, "main", "sa", "12", &key()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let held = compartments_app::holdings(&h.store, h.ledger.as_ref(), user).await.unwrap();
	assert_eq!((held.main.posted, held.compartments.posted), (usdt("18"), usdt("12")));
	let wallet = wallet_app::get_wallet(
		&wallet_app::WalletPorts {
			ledger: h.ledger.as_ref(),
			positions: &PgFundPositions::new(h.pool.clone()),
			nav: &PgNav::new(h.pool.clone()),
			deposit_addresses: &common::NoAddresses,
			users: &h.users,
			compartments: &h.store,
		},
		&[],
		piggybank_core::config::KycGate::LIFTED,
		user,
	)
	.await
	.unwrap()
	.balance;
	assert_eq!((wallet.available, wallet.in_compartments, wallet.total), (usdt("18"), usdt("12"), usdt("30")));
}
