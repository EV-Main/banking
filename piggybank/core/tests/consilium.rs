//! Integration tests for the consilium — real Postgres **and** TigerBeetle (no mocks, per
//! the project rules). They run when `DATABASE_URL` is set and a TigerBeetle replica is
//! reachable (`nix run .#db` + `.#tb`), and skip otherwise.
//!
//! Two pieces of shared state force these tests to run one at a time, and both are
//! deliberate features of the design rather than test friction:
//!   - **at most one consilium may be OPEN per source claim** (the partial unique index
//!     that removes the concurrent-approval race), and every generic test here opens a
//!     holder grant over the one `fee` allocation, and
//!   - **the owner roster is global** — `users.role = 'owner'` has no per-test scope, since
//!     the fund has exactly one set of owners.
//!
//! The generic mechanics — quorum, tokens, votes, expiry, the roster rules — are driven
//! through the HOLDER GRANT (#245): units of the `fee` allocation for a person, the one
//! kind whose effect is self-contained (an issuance row, no order, no rail). The revenue
//! payout those mechanics were first written against is retired and pinned as such.
//!
//! So every test takes [`exclusive_governance`] and starts from [`reset_governance`], which
//! closes any lingering open consilium and clears the roster. That setup is self-healing: a
//! test that panics half-way leaves state behind, and the next test's reset removes it.

use std::sync::Arc;

use domain::{
	auth::AuthSubject,
	balance::{LedgerAccountKey, ServiceId, TransferCode, ValuationId},
	consilium::{ConsiliumId, ConsiliumKind, ConsiliumState, ConsiliumTerms, HolderGrantTerms, ValuationOverrideTerms, VoteDecision},
	error::DomainError,
	issuance::{IdempotencyKey, UnitHolder},
	money::{Nav, Network, Shares, Usdt, WalletAddress},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{consilium as consilium_app, funds as funds_app, issuance as issuance_app, payments as payments_app},
	config::KycGate,
	infrastructure::{
		allocations::PgAllocations, consilium::PgConsilia, custody::StubCustody, deposits::PgDeposits, fee_policy_changes::PgFeePolicyChanges, issuance::PgUnitIssuances, nav::PgNav,
		outflow::PgOutflowPolicy, payments::PgPayments, redemptions::PgRedemptions, relay::Relay, subscriptions::PgSubscriptions, users::PgUsers, withdrawals::PgWithdrawals,
	},
	ports::{
		AllocationRegistry, ConsiliumRepository, LedgerTransfer, NavMarks, PaymentRepository, UnitIssuanceRepository, UserRepository, WithdrawalRepository,
		consilium::{ConsiliumView, MAX_CODE_ATTEMPTS, VoteAudit},
		issuance::StoredIssuance,
		ledger::Ledger,
	},
};
use sqlx::{PgPool, Row};
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

/// A valid BEP20 destination — the retired payout's on-chain target.
const PAYOUT_ADDRESS: &str = "0x52908400098527886E0F7030069857D2E4169EE7";

/// The one rail these tests configure.
const CONFIGURED: [Network; 1] = [Network::Bep20];

const APPROVAL_URL_BASE: &str = "https://example.test/consilium";
const CONSENT_URL_BASE: &str = "https://example.test/consent";

/// Serialises every test in this file — see the module docs.
static GOVERNANCE: std::sync::LazyLock<tokio::sync::Mutex<()>> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

async fn exclusive_governance() -> tokio::sync::MutexGuard<'static, ()> {
	GOVERNANCE.lock().await
}

struct Harness {
	pool: PgPool,
	consilia: Arc<dyn ConsiliumRepository>,
	withdrawals: Arc<dyn WithdrawalRepository>,
	payments: Arc<dyn PaymentRepository>,
	users: Arc<dyn UserRepository>,
	outflow: PgOutflowPolicy,
	allocations: PgAllocations,
	nav: PgNav,
	fee_changes: PgFeePolicyChanges,
	issuances: PgUnitIssuances,
	deposits: PgDeposits,
	subscriptions: PgSubscriptions,
	ledger: Arc<dyn Ledger>,
	relay: Relay,
	notify: Arc<Notify>,
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "consilium test").await?;
	let notify = Arc::new(Notify::new());
	Some(Harness {
		consilia: Arc::new(PgConsilia::new(pool.clone())),
		withdrawals: Arc::new(PgWithdrawals::new(pool.clone())),
		payments: Arc::new(PgPayments::new(pool.clone())),
		users: Arc::new(PgUsers::new(pool.clone())),
		outflow: PgOutflowPolicy::new(pool.clone()),
		allocations: PgAllocations::new(pool.clone()),
		nav: PgNav::new(pool.clone()),
		fee_changes: PgFeePolicyChanges::new(pool.clone()),
		issuances: PgUnitIssuances::new(pool.clone()),
		deposits: PgDeposits::new(pool.clone()),
		subscriptions: PgSubscriptions::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		ledger,
		notify,
		pool,
	})
}

fn ports(h: &Harness) -> consilium_app::ConsiliumPorts<'_> {
	consilium_app::ConsiliumPorts {
		consilia: h.consilia.as_ref(),
		withdrawals: h.withdrawals.as_ref(),
		payments: h.payments.as_ref(),
		users: h.users.as_ref(),
		ledger: h.ledger.as_ref(),
		custody: &StubCustody,
		policy: &h.outflow,
		allocations: &h.allocations,
		nav: &h.nav,
		fee_changes: &h.fee_changes,
		issuances: &h.issuances,
		deposits: &h.deposits,
		addresses: &common::NoAddresses,
		subscriptions: &h.subscriptions,
		relay: &h.notify,
		configured: &CONFIGURED,
		kyc: KycGate::LIFTED,
		approval_url_base: APPROVAL_URL_BASE,
		consent_url_base: CONSENT_URL_BASE,
		// The suite exercises the governance path itself, so it stands in for a wired mailer.
		// `opening_without_a_governance_mailer_is_refused` pins the false case explicitly.
		governance_mail_wired: true,
	}
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn now() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn shares(decimal: &str) -> Shares {
	Shares::parse_decimal(decimal).unwrap()
}

/// A fresh investor to seat as a holder of the `fee` allocation.
async fn grantee(h: &Harness) -> UserId {
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("g{}@example.com", Uuid::new_v4().simple())).unwrap();
	h.users.provision(subject, email, true).await.unwrap().id()
}

/// `units` of the `fee` allocation for a fresh person — the terms every generic test opens.
async fn grant_terms(h: &Harness, units: &str) -> HolderGrantTerms {
	HolderGrantTerms::new(ServiceId::fee(), grantee(h).await, shares(units)).unwrap()
}

/// Clear the governance state this suite shares and guarantee the `fee` allocation prices:
/// close any open consilium (the unique index allows one per source claim), empty the
/// owner roster, and top the `service:fee` claim up.
///
/// Run at the START of every test rather than the end, so a panicking test cannot wedge the
/// ones after it. The top-up is what makes the tests order-independent: a holder grant
/// prices at the allocation's NAV, cash over units, and every executed grant here mints
/// units — without fresh cash behind them a sibling test would leave the price at zero and
/// the next mint refused.
async fn reset_governance(h: &Harness) {
	sqlx::query("UPDATE consilium SET state = 'cancelled', decided_at = now() WHERE state = 'open'")
		.execute(&h.pool)
		.await
		.unwrap();
	// The payment-kind fixture, removed rather than cancelled. `list` reads the whole history
	// and rehydrates every row, so one row this binary cannot parse fails every later test
	// that reads the history — which is the exact failure `0029_consilium_source_claim.sql`
	// describes, and a panicking fixture is enough to leave one behind.
	// The orders those fixtures decide go first: `payment_approval` references the consilium
	// with `ON DELETE RESTRICT`, so a fixture that panicked between opening its order and
	// cleaning up would otherwise wedge every reset after it.
	// The link is a cycle once a consilium has executed (`payment_approval` restricts the
	// consilium's deletion, `executed_payment_id` restricts the order's), so it is broken in
	// order: the orders' ids are collected, the links dropped, the consilia deleted, then
	// the orders and the money facts they drained.
	let orders: Vec<Uuid> = sqlx::query_scalar(
		"SELECT a.payment_id FROM payment_approval a JOIN consilium c ON c.id = a.consilium_id WHERE c.kind = 'payment' \
		 UNION SELECT executed_payment_id FROM consilium WHERE kind = 'payment' AND executed_payment_id IS NOT NULL",
	)
	.fetch_all(&h.pool)
	.await
	.unwrap();
	sqlx::query("DELETE FROM payment_approval WHERE consilium_id IN (SELECT id FROM consilium WHERE kind = 'payment')")
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("DELETE FROM consilium WHERE kind = 'payment'").execute(&h.pool).await.unwrap();
	// A treasury order and the withdrawal executing it reference each other (0052), so both
	// go in one statement, where the foreign keys are checked once, at its end. The
	// withdrawal's reservation stays on the ledger, which nothing in this binary reconciles.
	sqlx::query("WITH executing AS (DELETE FROM withdrawals WHERE payment_id = ANY($1)) DELETE FROM payments WHERE id = ANY($1)")
		.bind(&orders)
		.execute(&h.pool)
		.await
		.unwrap();
	for statement in [
		"DELETE FROM outbox WHERE aggregate = 'payment' AND aggregate_id = ANY($1)",
		"DELETE FROM event_log WHERE aggregate = 'payment' AND aggregate_id = ANY($1)",
	] {
		sqlx::query(statement).bind(&orders).execute(&h.pool).await.unwrap();
	}
	sqlx::query("UPDATE users SET role = 'investor' WHERE role = 'owner'").execute(&h.pool).await.unwrap();
	// The cooling-off clock is global, so a test that exercises it would otherwise freeze
	// every test after it for 48 simulated hours.
	sqlx::query("DELETE FROM governance_roster_change").execute(&h.pool).await.unwrap();
	fund_fee_allocation(h, "100000").await;
}

/// Provision a fresh user and seat them as a fund owner. `concierge_user_id` is set because
/// the mail worker addresses recipients by their id in the plane that owns identities.
async fn owner(h: &Harness) -> UserId {
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("o{}@example.com", Uuid::new_v4().simple())).unwrap();
	let id = h.users.provision(subject, email, true).await.unwrap().id();
	sqlx::query("UPDATE users SET role = 'owner', concierge_user_id = $2 WHERE id = $1")
		.bind(id.raw())
		.bind(Uuid::new_v4())
		.execute(&h.pool)
		.await
		.unwrap();
	id
}

/// Seat `n` owners. The first is the initiator in every test that opens a consilium.
async fn owners(h: &Harness, n: usize) -> Vec<UserId> {
	let mut roster = Vec::with_capacity(n);
	for _ in 0..n {
		roster.push(owner(h).await);
	}
	roster
}

/// Strip a user's seat — what the bridge does when concierge reports a role change.
async fn demote(h: &Harness, user: UserId) {
	sqlx::query("UPDATE users SET role = 'investor' WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
}

/// Credit the `fee` allocation's cash directly, the deposit shape (`Dr wallet / Cr
/// service:fee`). The claim is one per database, so every assertion about it is a DELTA.
async fn fund_fee_allocation(h: &Harness, amount: &str) {
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: LedgerAccountKey::ServiceClaim(ServiceId::fee()),
			amount: usdt(amount).base_units(),
			code: TransferCode::WithdrawFee,
			reference: 0,
		})
		.await
		.unwrap();
}

/// The token and code that were mailed to one seat. Reading them out of the queue is exactly
/// what the owner does when the message lands — the plaintexts live nowhere else.
async fn credentials(h: &Harness, consilium: ConsiliumId, voter: UserId) -> (String, String) {
	let payload: String = sqlx::query_scalar("SELECT payload::text FROM consilium_mail WHERE consilium_id = $1 AND user_id = $2 AND kind IN ('payout_approval', 'payment_approval')")
		.bind(consilium.raw())
		.bind(voter.raw())
		.fetch_one(&h.pool)
		.await
		.expect("every eligible seat is mailed an approval");
	let mail: serde_json::Value = serde_json::from_str(&payload).unwrap();
	let url = mail["approval_url"].as_str().unwrap().to_owned();
	let token = url.rsplit('/').next().unwrap().to_owned();
	(token, mail["code"].as_str().unwrap().to_owned())
}

/// Cast one seat's vote with its real emailed credentials.
async fn vote(h: &Harness, consilium: ConsiliumId, voter: UserId, decision: VoteDecision) -> Result<bool, DomainError> {
	let (token, code) = credentials(h, consilium, voter).await;
	let audit = VoteAudit {
		client_ip: "203.0.113.7".to_owned(),
		user_agent: "itest".to_owned(),
	};
	consilium_app::submit_decision(h.consilia.as_ref(), &token, &code, decision, &audit, now())
		.await
		.map(|outcome| outcome.decided)
}

async fn state_of(h: &Harness, id: ConsiliumId) -> ConsiliumState {
	consilium_app::find(h.consilia.as_ref(), id).await.unwrap().consilium.state()
}

/// Open a holder grant of `units` of `fee` to a fresh person.
async fn open_grant(h: &Harness, initiator: UserId, units: &str) -> ConsiliumView {
	consilium_app::open_holder_grant(&ports(h), initiator, grant_terms(h, units).await, now()).await.unwrap()
}

/// The person a grant consilium names.
fn grantee_of(view: &ConsiliumView) -> UserId {
	match view.consilium.terms() {
		ConsiliumTerms::HolderGrant(terms) => terms.user,
		other => panic!("not a holder grant: {other:?}"),
	}
}

/// Push a consilium and its tokens past their deadline, standing in for 72h passing.
async fn expire_the_window(h: &Harness, id: ConsiliumId) {
	sqlx::query("UPDATE consilium SET expires_at = now() - interval '1 hour' WHERE id = $1")
		.bind(id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("UPDATE consilium_voter SET expires_at = now() - interval '1 hour' WHERE consilium_id = $1")
		.bind(id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// One seat's attempt counter, straight from the column the CHECK constrains.
async fn attempts_of(h: &Harness, consilium: ConsiliumId, voter: UserId) -> i32 {
	sqlx::query_scalar("SELECT attempts FROM consilium_voter WHERE consilium_id = $1 AND user_id = $2")
		.bind(consilium.raw())
		.bind(voter.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

async fn burned_of(h: &Harness, consilium: ConsiliumId, voter: UserId) -> bool {
	sqlx::query_scalar("SELECT burned_at IS NOT NULL FROM consilium_voter WHERE consilium_id = $1 AND user_id = $2")
		.bind(consilium.raw())
		.bind(voter.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

/// Record an owner-roster change `seconds_ago` in the past, the way the lifecycle bridge
/// does when it applies a `ROLE_CHANGED` that adds or removes an owner.
async fn record_roster_change(h: &Harness, user: UserId, from_role: &str, to_role: &str, seconds_ago: i64) {
	sqlx::query("INSERT INTO governance_roster_change (user_id, from_role, to_role, changed_at) VALUES ($1, $2, $3, now() - make_interval(secs => $4))")
		.bind(user.raw())
		.bind(from_role)
		.bind(to_role)
		.bind(seconds_ago as f64)
		.execute(&h.pool)
		.await
		.unwrap();
}

/// How many issuance rows stand for the `fee` allocation — a grant's effect, counted.
async fn grant_count(h: &Harness) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM unit_issuances WHERE service = 'fee'").fetch_one(&h.pool).await.unwrap()
}

#[tokio::test]
async fn the_threshold_is_more_than_half_of_all_owners_end_to_end() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };

	// N=3 — threshold 2 over 2 voters: both peers must agree.
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	assert_eq!(c.consilium.owner_count(), 3);
	assert_eq!(c.consilium.threshold(), 2);
	assert_eq!(c.voters.len(), 2, "the initiator holds no seat");
	assert!(!vote(&h, c.consilium.id(), roster[1], VoteDecision::Approve).await.unwrap());
	assert_eq!(state_of(&h, c.consilium.id()).await, ConsiliumState::Open);
	assert!(vote(&h, c.consilium.id(), roster[2], VoteDecision::Approve).await.unwrap());
	assert_eq!(state_of(&h, c.consilium.id()).await, ConsiliumState::Approved);

	// N=4 — threshold 3 over 3 voters: unanimity among the peers.
	reset_governance(&h).await;
	let roster = owners(&h, 4).await;
	let c = open_grant(&h, roster[0], "500").await;
	assert_eq!(c.consilium.threshold(), 3);
	assert_eq!(c.voters.len(), 3);
	vote(&h, c.consilium.id(), roster[1], VoteDecision::Approve).await.unwrap();
	vote(&h, c.consilium.id(), roster[2], VoteDecision::Approve).await.unwrap();
	assert_eq!(state_of(&h, c.consilium.id()).await, ConsiliumState::Open, "two of three is short");
	vote(&h, c.consilium.id(), roster[3], VoteDecision::Approve).await.unwrap();
	assert_eq!(state_of(&h, c.consilium.id()).await, ConsiliumState::Approved);

	// N=5 — threshold 3 over 4 voters: 3 of 4, so one owner need not answer at all.
	reset_governance(&h).await;
	let roster = owners(&h, 5).await;
	let c = open_grant(&h, roster[0], "500").await;
	assert_eq!(c.consilium.threshold(), 3);
	assert_eq!(c.voters.len(), 4);
	vote(&h, c.consilium.id(), roster[1], VoteDecision::Approve).await.unwrap();
	vote(&h, c.consilium.id(), roster[2], VoteDecision::Approve).await.unwrap();
	assert_eq!(state_of(&h, c.consilium.id()).await, ConsiliumState::Open);
	assert!(vote(&h, c.consilium.id(), roster[3], VoteDecision::Approve).await.unwrap());
	assert_eq!(state_of(&h, c.consilium.id()).await, ConsiliumState::Approved);
	let final_view = consilium_app::find(h.consilia.as_ref(), c.consilium.id()).await.unwrap();
	assert_eq!(final_view.consilium.approvals(), 3);
	assert_eq!(
		final_view.voters.iter().filter(|v| v.decision == VoteDecision::Pending).count(),
		1,
		"the fourth seat never had to answer"
	);
}

#[tokio::test]
async fn a_fund_below_three_owners_cannot_open_a_consilium_at_all() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;

	// Two owners: threshold 2, one eligible voter — arithmetically unreachable, so the
	// request is refused rather than stored as one that could never pass.
	let roster = owners(&h, 2).await;
	let err = consilium_app::open_holder_grant(&ports(&h), roster[0], grant_terms(&h, "500").await, now()).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "expected an explicit refusal, got {err:?}");
	let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium WHERE state = 'open'").fetch_one(&h.pool).await.unwrap();
	assert_eq!(stored, 0, "nothing may be persisted for a quorum that can never be reached");
}

#[tokio::test]
async fn the_initiator_gets_no_token_and_cannot_vote_by_any_path() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();

	// No seat row exists for them — the composite FK plus CHECK in `0025` make one
	// unrepresentable, so there is nothing for a vote to attach to.
	let seats: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium_voter WHERE consilium_id = $1 AND user_id = $2")
		.bind(id.raw())
		.bind(roster[0].raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(seats, 0, "the initiator must have no seat");

	// No approval mail was addressed to them either, so no token was ever minted.
	let mails: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium_mail WHERE consilium_id = $1 AND user_id = $2 AND kind = 'payout_approval'")
		.bind(id.raw())
		.bind(roster[0].raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(mails, 0, "the initiator must never be mailed an approval token");

	// The database refuses to manufacture one, even by direct insert: this is the property
	// that makes "the initiator cannot vote" structural rather than a forgettable check.
	let forged =
		sqlx::query("INSERT INTO consilium_voter (consilium_id, user_id, initiator_user_id, token_hash, code_hash, expires_at) VALUES ($1, $2, $2, $3, $4, now() + interval '1 day')")
			.bind(id.raw())
			.bind(roster[0].raw())
			.bind(vec![9u8; 32])
			.bind(vec![8u8; 32])
			.execute(&h.pool)
			.await;
	assert!(forged.is_err(), "the schema must refuse a seat for the initiator");

	// And the other two peers alone still carry it — the initiator is in the denominator.
	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap();
	assert_eq!(state_of(&h, id).await, ConsiliumState::Approved);
}

#[tokio::test]
async fn only_one_consilium_may_be_open_at_a_time() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let first = open_grant(&h, roster[0], "500").await;

	// This is the whole of the concurrent-approval overdraw defence: two approved requests
	// can never exist to race each other over the same claim.
	let err = consilium_app::open_holder_grant(&ports(&h), roster[0], grant_terms(&h, "100").await, now()).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "expected a conflict, got {err:?}");

	// Closing the first frees the slot again.
	consilium_app::cancel(h.consilia.as_ref(), first.consilium.id(), roster[0], now()).await.unwrap();
	assert_eq!(state_of(&h, first.consilium.id()).await, ConsiliumState::Cancelled);
	let second = open_grant(&h, roster[0], "100").await;
	assert_eq!(second.consilium.state(), ConsiliumState::Open);
	// Votes are not carried over: the reopened request has its own hash and its own seats.
	assert_ne!(second.consilium.payload_hash_hex(), first.consilium.payload_hash_hex());
}

#[tokio::test]
async fn only_the_initiator_may_withdraw_their_own_consilium() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let err = consilium_app::cancel(h.consilia.as_ref(), c.consilium.id(), roster[1], now()).await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)));
	assert_eq!(state_of(&h, c.consilium.id()).await, ConsiliumState::Open);
}

/// Every mail of one kind queued for a consilium as the queue holds it: `(sent, withdrawn,
/// code)` — the code blank where the row carries none, or no longer.
async fn mail_states(h: &Harness, consilium: ConsiliumId, kind: &str) -> Vec<(bool, bool, String)> {
	sqlx::query_as("SELECT sent_at IS NOT NULL, withdrawn_at IS NOT NULL, COALESCE(payload->>'code', '') FROM consilium_mail WHERE consilium_id = $1 AND kind = $2")
		.bind(consilium.raw())
		.bind(kind)
		.fetch_all(&h.pool)
		.await
		.unwrap()
}

/// A consilium withdrawn by its initiator while nothing has left the queue — no worker
/// runs here, which is a relay outage from the queue's side (#342): the seats'
/// invitations (a grant rides the `payment_approval` template) are withdrawn with their
/// secrets blanked, and the `payout_outcome` verdict the withdrawal queues for the
/// initiator and every seat is not.
#[tokio::test]
async fn withdrawing_a_consilium_withdraws_its_undelivered_invitations() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	let invitations = mail_states(&h, id, "payment_approval").await;
	assert_eq!(invitations.len(), 2);
	assert!(invitations.iter().all(|(sent, withdrawn, code)| !sent && !withdrawn && !code.is_empty()), "{invitations:?}");

	consilium_app::cancel(h.consilia.as_ref(), id, roster[0], now()).await.unwrap();
	assert_eq!(state_of(&h, id).await, ConsiliumState::Cancelled);
	let invitations = mail_states(&h, id, "payment_approval").await;
	assert_eq!(invitations.len(), 2);
	assert!(invitations.iter().all(|(sent, withdrawn, code)| !sent && *withdrawn && code.is_empty()), "{invitations:?}");
	let outcomes = mail_states(&h, id, "payout_outcome").await;
	assert_eq!(outcomes.len(), 3, "the initiator and both seats");
	assert!(outcomes.iter().all(|(sent, withdrawn, _)| !sent && !withdrawn), "{outcomes:?}");
}

#[tokio::test]
async fn five_wrong_codes_burn_the_token_and_a_burned_one_looks_unknown() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	let (token, code) = credentials(&h, id, roster[1]).await;
	let audit = VoteAudit {
		client_ip: String::new(),
		user_agent: String::new(),
	};

	// The invitation is readable while the token is live.
	assert!(consilium_app::invitation(h.consilia.as_ref(), &token, now()).await.is_ok());

	for attempt in 1..MAX_CODE_ATTEMPTS {
		let err = consilium_app::submit_decision(h.consilia.as_ref(), &token, "WRONGCODE1", VoteDecision::Approve, &audit, now())
			.await
			.unwrap_err();
		// A wrong code is NOT the anonymous answer: the token is genuine and its holder
		// deserves to know they mistyped, and how many tries are left. `Validation` is what
		// carries that through the BFF as a 400 — `Forbidden` became an opaque 404, so the
		// count never reached the human and owners burned their own tokens finding out.
		assert!(matches!(err, DomainError::Validation(_)), "attempt {attempt} should be a refusal, got {err:?}");
		assert!(
			err.to_string().contains(&format!("{} attempts remaining", MAX_CODE_ATTEMPTS - attempt)),
			"the refusal must carry the remaining count, got {err:?}"
		);
	}
	// The fifth failure burns it.
	let burned = consilium_app::submit_decision(h.consilia.as_ref(), &token, "WRONGCODE1", VoteDecision::Approve, &audit, now())
		.await
		.unwrap_err();
	assert!(matches!(burned, DomainError::NotFound { .. }));

	// From here the token is indistinguishable from one that never existed — same variant,
	// same (empty) id, on both surfaces, and the CORRECT code no longer helps.
	let unknown_token = "0".repeat(64);
	let burned_read = consilium_app::invitation(h.consilia.as_ref(), &token, now()).await.unwrap_err();
	let unknown_read = consilium_app::invitation(h.consilia.as_ref(), &unknown_token, now()).await.unwrap_err();
	assert_eq!(burned_read.to_string(), unknown_read.to_string(), "a burned token must read exactly like an unknown one");
	let with_real_code = consilium_app::submit_decision(h.consilia.as_ref(), &token, &code, VoteDecision::Approve, &audit, now())
		.await
		.unwrap_err();
	assert_eq!(with_real_code.to_string(), unknown_read.to_string());

	// The burn is recorded, and every owner was told about it.
	let burn_notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium_mail WHERE consilium_id = $1 AND kind = 'token_burned'")
		.bind(id.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(burn_notices, 3, "the whole roster hears about a brute-force attempt");

	// The other peer's seat is untouched — one burned token does not disarm the consilium.
	assert!(consilium_app::invitation(h.consilia.as_ref(), &credentials(&h, id, roster[2]).await.0, now()).await.is_ok());
}

#[tokio::test]
async fn reading_an_invitation_never_costs_an_attempt() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let (token, _) = credentials(&h, c.consilium.id(), roster[1]).await;

	// A corporate mail scanner fetches every URL in a message, often several times. If the
	// read counted against the attempt limit it would burn an owner's vote before they had
	// even opened the mail.
	for _ in 0..(MAX_CODE_ATTEMPTS + 3) {
		let view = consilium_app::invitation(h.consilia.as_ref(), &token, now()).await.unwrap();
		assert_eq!(view.attempts_remaining, MAX_CODE_ATTEMPTS as u32);
		assert_eq!(view.decision, VoteDecision::Pending);
	}
	let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM consilium_voter WHERE consilium_id = $1 AND user_id = $2")
		.bind(c.consilium.id().raw())
		.bind(roster[1].raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(attempts, 0, "the read must be strictly side-effect free");
}

#[tokio::test]
async fn the_same_decision_twice_is_idempotent_and_a_different_one_is_refused() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 5).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();

	assert!(!vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap());
	// A retried request (a double-clicked button, an at-least-once edge) must not error and
	// must not count twice.
	assert!(!vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap());
	let view = consilium_app::find(h.consilia.as_ref(), id).await.unwrap();
	assert_eq!(view.consilium.approvals(), 1, "the repeat is a no-op, not a second vote");

	// Changing your mind is not on offer — the vote is one-shot.
	let err = vote(&h, id, roster[1], VoteDecision::Reject).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "expected a conflict, got {err:?}");
	let view = consilium_app::find(h.consilia.as_ref(), id).await.unwrap();
	assert_eq!(view.consilium.approvals(), 1);
	assert_eq!(view.consilium.rejections(), 0);
}

#[tokio::test]
async fn rejections_close_the_consilium_the_moment_the_threshold_is_unreachable() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };

	// N=5: 4 voters, threshold 3 — the tally can afford exactly one refusal.
	reset_governance(&h).await;
	let roster = owners(&h, 5).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	assert!(!vote(&h, id, roster[1], VoteDecision::Reject).await.unwrap());
	assert_eq!(state_of(&h, id).await, ConsiliumState::Open, "three of the remaining four could still carry it");
	assert!(vote(&h, id, roster[2], VoteDecision::Reject).await.unwrap());
	assert_eq!(state_of(&h, id).await, ConsiliumState::Rejected, "only two voters remain and three are needed");

	// A rejected consilium takes no further votes and can never execute.
	let late = vote(&h, id, roster[3], VoteDecision::Approve).await.unwrap_err();
	assert!(matches!(late, DomainError::NotFound { .. }));
	assert!(consilium_app::execute(&ports(&h), id, now()).await.is_err());

	// N=3 needs both peers, so one refusal ends it at once.
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	assert!(vote(&h, c.consilium.id(), roster[1], VoteDecision::Reject).await.unwrap());
	assert_eq!(state_of(&h, c.consilium.id()).await, ConsiliumState::Rejected);
}

#[tokio::test]
async fn a_voter_who_lost_ownership_stops_counting_toward_quorum() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	// N=5: 4 voters, threshold 3, frozen at open and never recomputed.
	let roster = owners(&h, 5).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();

	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap();
	assert_eq!(consilium_app::find(h.consilia.as_ref(), id).await.unwrap().consilium.approvals(), 2);

	// The first approver loses their seat. Their vote stays on the record but stops being
	// counted, so the roster change made approval HARDER — never easier.
	demote(&h, roster[1]).await;
	let view = consilium_app::find(h.consilia.as_ref(), id).await.unwrap();
	assert_eq!(view.consilium.approvals(), 1, "a vote from a seat that no longer exists does not count");
	assert_eq!(view.consilium.threshold(), 3, "the threshold is frozen — losing owners cannot lower the bar");
	assert!(
		view.voters.iter().any(|v| v.user_id == roster[1] && v.decision == VoteDecision::Approve),
		"the vote is still visible on the record"
	);

	// A third approval now brings the counted tally to only 2, so it stays open.
	vote(&h, id, roster[3], VoteDecision::Approve).await.unwrap();
	assert_eq!(state_of(&h, id).await, ConsiliumState::Open, "two counted approvals cannot reach a threshold of three");

	// The last remaining seat carries it — three approvals from three current owners.
	assert!(vote(&h, id, roster[4], VoteDecision::Approve).await.unwrap());
	assert_eq!(state_of(&h, id).await, ConsiliumState::Approved);
	assert_eq!(consilium_app::find(h.consilia.as_ref(), id).await.unwrap().consilium.approvals(), 3);
}

#[tokio::test]
async fn reaching_quorum_mints_exactly_one_grant_and_executing_twice_mints_no_second() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	let person = grantee_of(&c);
	let before = grant_count(&h).await;
	h.relay.drain().await;
	let held_before = h.ledger.balance(&LedgerAccountKey::UserShares(ServiceId::fee(), person)).await.unwrap().posted;
	let supply_before = h.ledger.balance(&LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await.unwrap().posted;

	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	assert!(vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap());
	// Approval alone mints nothing — the effect is a separate, explicit step.
	assert_eq!(state_of(&h, id).await, ConsiliumState::Approved);
	assert_eq!(grant_count(&h).await, before, "quorum by itself must not mint");

	let executed = consilium_app::execute(&ports(&h), id, now()).await.unwrap();
	assert_eq!(executed.consilium.state(), ConsiliumState::Executed);
	assert_eq!(grant_count(&h).await, before + 1);

	// The issuance is keyed by the consilium, so it is checkable rather than incidental.
	let issuance = executed.consilium.executed_issuance_id().expect("a grant's effect is an issuance");
	let record = h.issuances.find_by_id(issuance).await.unwrap().and_then(StoredIssuance::live).expect("the issuance exists");
	assert_eq!(record.issuance.holder(), &UnitHolder::User(person));
	assert_eq!(record.issuance.service(), &ServiceId::fee());
	assert_eq!(record.issuance.units(), shares("500"));
	let key: String = sqlx::query_scalar("SELECT idempotency_key FROM unit_issuances WHERE id = $1")
		.bind(issuance.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(IdempotencyKey::parse(&key).unwrap(), consilium_app::holder_grant_key(id));

	// Re-executing — the sweeper's retry, or a redelivered call — must be a no-op. This is
	// the difference between an at-least-once execution path and a double mint.
	for _ in 0..3 {
		let again = consilium_app::execute(&ports(&h), id, now()).await.unwrap();
		assert_eq!(again.consilium.executed_issuance_id(), Some(issuance));
	}
	assert_eq!(grant_count(&h).await, before + 1, "a retried execution must never mint a second time");

	// The relay posts the units to the person, and the supply grows by exactly that.
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let held = h.ledger.balance(&LedgerAccountKey::UserShares(ServiceId::fee(), person)).await.unwrap().posted;
	let supply = h.ledger.balance(&LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await.unwrap().posted;
	assert_eq!(held - held_before, shares("500").base_units());
	assert_eq!(supply - supply_before, shares("500").base_units());
}

#[tokio::test]
async fn a_grant_the_allocation_no_longer_admits_lands_in_execution_failed() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;

	// Open while the allocation has room...
	let cap: String = sqlx::query_scalar("SELECT unit_cap FROM allocations WHERE service = 'fee'").fetch_one(&h.pool).await.unwrap();
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap();
	assert_eq!(state_of(&h, id).await, ConsiliumState::Approved);

	// ...then pin its cap under the grant before execution, as an operator sizing it would.
	sqlx::query("UPDATE allocations SET unit_cap = '1' WHERE service = 'fee'").execute(&h.pool).await.unwrap();

	let failed = consilium_app::execute(&ports(&h), id, now()).await.unwrap();
	assert_eq!(failed.consilium.state(), ConsiliumState::ExecutionFailed);
	assert!(
		failed.consilium.failure_reason().unwrap_or_default().contains("cap"),
		"the owners must be able to read WHY: {:?}",
		failed.consilium.failure_reason()
	);
	assert_eq!(failed.consilium.executed_issuance_id(), None);
	// Terminal — nothing retries silently, so the sweeper will not pick it up again.
	assert!(!h.consilia.awaiting_execution().await.unwrap().contains(&id));
	assert!(consilium_app::execute(&ports(&h), id, now()).await.is_err());

	// Put the cap back: the registry row is one per database, shared with every sibling.
	sqlx::query("UPDATE allocations SET unit_cap = $1 WHERE service = 'fee'")
		.bind(cap)
		.execute(&h.pool)
		.await
		.unwrap();
}

#[tokio::test]
async fn an_expired_consilium_can_never_execute_however_late_a_vote_arrives() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	let before = grant_count(&h).await;

	expire_the_window(&h, id).await;

	// The token's own deadline refuses a late vote even BEFORE the sweeper has run, which is
	// what makes this hold in the window between expiry and the next sweep.
	let late = vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap_err();
	assert!(matches!(late, DomainError::NotFound { .. }), "a vote past the deadline must not be accepted");
	assert_eq!(state_of(&h, id).await, ConsiliumState::Open, "still open — only the sweeper closes it");

	// The sweeper closes it, and the vote that never landed cannot have carried it.
	assert_eq!(consilium_app::sweep_expired(h.consilia.as_ref(), now()).await.unwrap(), 1);
	assert_eq!(state_of(&h, id).await, ConsiliumState::Expired);
	// Execution is reachable only from `approved`; expiry only from `open`. No ordering of
	// the two can produce a payout from a dead request.
	assert!(consilium_app::execute(&ports(&h), id, now()).await.is_err());
	assert_eq!(grant_count(&h).await, before, "an expired consilium must move no money");
	assert!(consilium_app::sweep_expired(h.consilia.as_ref(), now()).await.unwrap() == 0, "the sweep is idempotent");
}

#[tokio::test]
async fn editing_the_terms_after_approval_cannot_spend_the_approval() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap();
	assert_eq!(state_of(&h, id).await, ConsiliumState::Approved);
	let before = grant_count(&h).await;

	// There is no edit RPC, so this is a direct tamper with the stored row — the threat the
	// payload hash exists for. An approval is a signature over those exact terms.
	sqlx::query("UPDATE consilium SET terms = jsonb_set(terms, '{units}', to_jsonb($2::text)) WHERE id = $1")
		.bind(id.raw())
		.bind(shares("4000").base_units().to_string())
		.execute(&h.pool)
		.await
		.unwrap();

	let result = consilium_app::execute(&ports(&h), id, now()).await.unwrap();
	assert_eq!(result.consilium.state(), ConsiliumState::ExecutionFailed);
	assert!(result.consilium.failure_reason().unwrap_or_default().contains("payload hash"));
	assert_eq!(grant_count(&h).await, before, "tampered terms must never reach the money plane");
}

#[tokio::test]
async fn an_impossible_grant_is_refused_at_open_not_after_a_72h_vote() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;

	// A person nobody can sign in as: refused now, rather than after three owners have
	// spent three days approving units nobody could redeem.
	let nobody = HolderGrantTerms::new(ServiceId::fee(), UserId::new(), shares("1")).unwrap();
	let err = consilium_app::open_holder_grant(&ports(&h), roster[0], nobody, now()).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { .. }), "got {err:?}");

	// A product is not granted, and nothing is not a grant — the terms themselves refuse.
	let product = ServiceId::parse("svc-arb").unwrap();
	assert!(matches!(HolderGrantTerms::new(product, roster[1], shares("1")), Err(DomainError::Validation(_))));
	assert!(matches!(HolderGrantTerms::new(ServiceId::fee(), roster[1], Shares::ZERO), Err(DomainError::Validation(_))));

	let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium").fetch_one(&h.pool).await.unwrap();
	let open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium WHERE state = 'open'").fetch_one(&h.pool).await.unwrap();
	assert_eq!(open, 0, "a refused request leaves nothing open (of {stored} historical rows)");
}

/// L-2 of the #245 security review: a holder grant names a person who can actually hold —
/// a mirrored user in good standing. A frozen account is refused at open, and one frozen
/// while the owners were voting fails at execution rather than being seated.
#[tokio::test]
async fn a_holder_grant_to_a_frozen_user_is_refused() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	async fn freeze(h: &Harness, user: UserId) {
		sqlx::query("UPDATE users SET status = 'disabled' WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
	}

	// Frozen before the proposal: refused at the door, nothing opened.
	let frozen = grantee(&h).await;
	freeze(&h, frozen).await;
	let before = grant_count(&h).await;
	let err = consilium_app::open_holder_grant(&ports(&h), roster[0], HolderGrantTerms::new(ServiceId::fee(), frozen, shares("100")).unwrap(), now())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("not active")), "got {err:?}");
	assert!(
		consilium_app::list(h.consilia.as_ref(), 10).await.unwrap().iter().all(|c| !c.consilium.state().is_open()),
		"no consilium was opened"
	);

	// Frozen during the vote: the quorum carries, the mint refuses, the failure is recorded.
	let c = open_grant(&h, roster[0], "100").await;
	let id = c.consilium.id();
	freeze(&h, grantee_of(&c)).await;
	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	assert!(vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap());
	let executed = consilium_app::execute(&ports(&h), id, now()).await.unwrap();
	assert_eq!(executed.consilium.state(), ConsiliumState::ExecutionFailed);
	assert!(
		executed.consilium.failure_reason().unwrap_or_default().contains("not active"),
		"{:?}",
		executed.consilium.failure_reason()
	);
	assert_eq!(grant_count(&h).await, before, "nothing was minted to a frozen account");
}

#[tokio::test]
async fn every_eligible_seat_is_mailed_a_distinct_token_and_code() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 4).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();

	let mut tokens = Vec::new();
	let mut codes = Vec::new();
	for voter in &roster[1..] {
		let (token, code) = credentials(&h, id, *voter).await;
		// 32 random bytes hex; 10 symbols from Crockford base32 minus I, L, O and U.
		assert_eq!(token.len(), 64, "a token is 32 random bytes, well past the 24-byte floor");
		assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
		assert_eq!(code.len(), 10);
		assert!(code.chars().all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c)), "code {code} left the unambiguous alphabet");
		assert!(!code.contains(['I', 'L', 'O', 'U']), "the four misread glyphs must never appear");
		tokens.push(token);
		codes.push(code);
	}
	tokens.sort();
	tokens.dedup();
	codes.sort();
	codes.dedup();
	assert_eq!(tokens.len(), 3, "each seat gets its own token");
	assert_eq!(codes.len(), 3, "each seat gets its own code");

	// Only digests are stored: a dump of the seat table yields nothing that can vote.
	let rows = sqlx::query("SELECT token_hash, code_hash FROM consilium_voter WHERE consilium_id = $1")
		.bind(id.raw())
		.fetch_all(&h.pool)
		.await
		.unwrap();
	assert_eq!(rows.len(), 3);
	for row in &rows {
		let token_hash: Vec<u8> = row.try_get("token_hash").unwrap();
		let code_hash: Vec<u8> = row.try_get("code_hash").unwrap();
		assert_eq!(token_hash.len(), 32);
		assert_eq!(code_hash.len(), 32);
		assert!(!tokens.iter().any(|t| t.as_bytes() == token_hash), "the plaintext token must not be what is stored");
	}

	// One seat's token cannot answer for another seat.
	let (first_token, _) = credentials(&h, id, roster[1]).await;
	let (_, second_code) = credentials(&h, id, roster[2]).await;
	let audit = VoteAudit {
		client_ip: String::new(),
		user_agent: String::new(),
	};
	assert!(
		consilium_app::submit_decision(h.consilia.as_ref(), &first_token, &second_code, VoteDecision::Approve, &audit, now())
			.await
			.is_err(),
		"a code only works with the token it was minted beside"
	);
}

/// PITFALL 10 vs. THE MISTYPED CODE. These are two different secrets and they get two
/// different answers. The TOKEN is protected by indistinguishability — unknown, expired,
/// spent and burned must all read identically, or the endpoint becomes an oracle for which
/// consilia exist. The CODE is protected by the five-attempt ceiling, and someone holding a
/// live token already knows it exists, so naming a wrong code reveals nothing new.
#[tokio::test]
async fn a_wrong_code_and_an_unknown_token_are_told_apart_while_the_dead_token_states_stay_identical() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	let (token, code) = credentials(&h, id, roster[1]).await;
	let audit = VoteAudit {
		client_ip: String::new(),
		user_agent: String::new(),
	};

	let wrong_code = consilium_app::submit_decision(h.consilia.as_ref(), &token, "WRONGCODE1", VoteDecision::Approve, &audit, now())
		.await
		.unwrap_err();
	let unknown_token = consilium_app::submit_decision(h.consilia.as_ref(), &"0".repeat(64), &code, VoteDecision::Approve, &audit, now())
		.await
		.unwrap_err();
	assert!(matches!(wrong_code, DomainError::Validation(_)), "a mistyped code is a 400, not a 404: {wrong_code:?}");
	assert!(
		matches!(unknown_token, DomainError::NotFound { .. }),
		"an unknown token is the anonymous answer: {unknown_token:?}"
	);
	assert_ne!(
		wrong_code.to_string(),
		unknown_token.to_string(),
		"an owner who mistyped must not be told their invitation does not exist"
	);

	// ...while every way a TOKEN can be dead still reads the same.
	let expired = consilium_app::submit_decision(h.consilia.as_ref(), &token, &code, VoteDecision::Approve, &audit, now() + 100 * 3600)
		.await
		.unwrap_err();
	assert_eq!(expired.to_string(), unknown_token.to_string(), "an expired token reads like an unknown one");

	// Spend the seat, then re-read: a spent token is still indistinguishable on the
	// side-effect-free surface.
	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	let spent_read = consilium_app::invitation(h.consilia.as_ref(), &token, now()).await.unwrap_err();
	let unknown_read = consilium_app::invitation(h.consilia.as_ref(), &"0".repeat(64), now()).await.unwrap_err();
	assert_eq!(spent_read.to_string(), unknown_read.to_string(), "a spent token reads like an unknown one");
}

/// THE ATTEMPT COUNTER MUST NOT OVERFLOW ITS OWN CHECK.
///
/// `CHECK (attempts <= 5)` and an unconditional increment are incompatible: a seat that
/// answered correctly on its fifth try sits at exactly 5 un-burned (the correct-code path
/// never burns), and the next POST — a double-click, a gateway retry — pushed it to 6,
/// violated the constraint, and turned that seat's every future request into a 503. Both
/// halves are pinned here: the retry stays idempotent, and the column stays legal.
#[tokio::test]
async fn a_seat_that_already_answered_spends_no_further_attempts() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 5).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	let (token, code) = credentials(&h, id, roster[1]).await;
	let audit = VoteAudit {
		client_ip: String::new(),
		user_agent: String::new(),
	};

	// Walk the seat to the very edge: four wrong guesses, then the correct code.
	for _ in 1..MAX_CODE_ATTEMPTS {
		assert!(
			consilium_app::submit_decision(h.consilia.as_ref(), &token, "WRONGCODE1", VoteDecision::Approve, &audit, now())
				.await
				.is_err()
		);
	}
	assert!(
		consilium_app::submit_decision(h.consilia.as_ref(), &token, &code, VoteDecision::Approve, &audit, now())
			.await
			.is_ok()
	);
	assert_eq!(attempts_of(&h, id, roster[1]).await, MAX_CODE_ATTEMPTS, "the seat sits at the ceiling, correct and un-burned");

	// The double-click. Under the old unconditional increment this wrote 6 and every later
	// request from this seat became a 503 forever.
	for _ in 0..3 {
		let retry = consilium_app::submit_decision(h.consilia.as_ref(), &token, &code, VoteDecision::Approve, &audit, now()).await;
		assert!(retry.is_ok(), "a retried submission from a seat that already answered must stay idempotent, got {retry:?}");
	}
	assert_eq!(
		attempts_of(&h, id, roster[1]).await,
		MAX_CODE_ATTEMPTS,
		"an answered seat is charged nothing, and the column never exceeds its CHECK"
	);
	assert!(!burned_of(&h, id, roster[1]).await, "an answered seat is not burned by its own retries");

	// A wrong code against the answered seat is refused, still without burning it.
	let wrong = consilium_app::submit_decision(h.consilia.as_ref(), &token, "WRONGCODE1", VoteDecision::Approve, &audit, now())
		.await
		.unwrap_err();
	assert!(matches!(wrong, DomainError::Validation(_)), "got {wrong:?}");
	assert!(!burned_of(&h, id, roster[1]).await, "nothing was counted, so nothing can burn");
	assert_eq!(attempts_of(&h, id, roster[1]).await, MAX_CODE_ATTEMPTS);
}

/// The mechanism is inert without a mailer, so it refuses to pretend otherwise: opening a
/// consilium nobody could be sent a token for is a consilium that expires unvotable 72h
/// later, having looked healthy the whole time.
#[tokio::test]
async fn opening_without_a_governance_mailer_is_refused() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let unwired = consilium_app::ConsiliumPorts {
		governance_mail_wired: false,
		..ports(&h)
	};
	let err = consilium_app::open_holder_grant(&unwired, roster[0], grant_terms(&h, "500").await, now()).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("governance mail is not configured"), "the refusal must name the cause: {err}");
}

/// THE COOLING-OFF PERIOD, FIRST HALF: a fresh roster change refuses a NEW proposal.
///
/// This does not stop a majority from seizing the roster — nothing can; a majority owns the
/// roster by definition. It stops the seizure and the payout from being one uninterrupted
/// motion, which is the part an auditor or a remaining honest owner can actually act on.
#[tokio::test]
async fn a_recent_owner_roster_change_freezes_new_proposals() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 4).await;

	// A seat changed hands an hour ago.
	record_roster_change(&h, roster[3], "investor", "owner", 3600).await;
	let err = consilium_app::open_holder_grant(&ports(&h), roster[0], grant_terms(&h, "500").await, now()).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("cooling-off"), "the refusal must name the cooling-off period: {err}");
	assert!(err.to_string().contains("lifts in"), "and say when it lifts: {err}");

	// Once the window has passed, the same proposal opens normally.
	sqlx::query("DELETE FROM governance_roster_change").execute(&h.pool).await.unwrap();
	record_roster_change(&h, roster[3], "investor", "owner", consilium_app::ROSTER_COOLING_OFF_SECS + 60).await;
	assert!(
		consilium_app::open_holder_grant(&ports(&h), roster[0], grant_terms(&h, "500").await, now()).await.is_ok(),
		"a settled roster does not block a proposal forever"
	);
}

/// THE COOLING-OFF PERIOD, SECOND HALF: a change landing on an ALREADY-OPEN proposal voids
/// it. Without this the window is trivially straddled — open the request first, seize the
/// roster after, and the freeze on new proposals never applies.
#[tokio::test]
async fn an_owner_roster_change_voids_a_request_that_was_already_open() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 5).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	assert_eq!(state_of(&h, id).await, ConsiliumState::Open);

	// The roster moves while the request is live.
	record_roster_change(&h, roster[4], "owner", "investor", 0).await;
	let voided = h.consilia.void_open_for_roster_change(now(), now()).await.unwrap();
	assert_eq!(voided, 1, "the open request is voided");
	assert_eq!(state_of(&h, id).await, ConsiliumState::Cancelled);

	// The audience is told WHY, so the delay reads as the deliberate control it is.
	let told: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium_mail WHERE consilium_id = $1 AND payload::text LIKE '%roster changed%'")
		.bind(id.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert!(told > 0, "the owners are told why their request disappeared");

	// Idempotent: a second sweep voids nothing more.
	assert_eq!(h.consilia.void_open_for_roster_change(now(), now()).await.unwrap(), 0);
}

/// PITFALL 1 AT THE MOMENT THAT SPENDS MONEY. The stored `approved` state is a fact about
/// the tally when the carrying vote landed; execution can happen much later. An owner who
/// loses their seat in between must take their approval with them — otherwise the invariant
/// holds everywhere except the one place it matters.
#[tokio::test]
async fn an_approval_invalidated_by_a_roster_change_is_refused_at_execution() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	// N=3: threshold 2, two eligible voters — so both must approve, and losing either one
	// drops the live tally below the frozen bar.
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	assert!(vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap());
	assert_eq!(state_of(&h, id).await, ConsiliumState::Approved);

	// Simulate the crash-before-execution window, then remove a seat that had approved.
	sqlx::query("UPDATE consilium SET state = 'approved', executed_issuance_id = NULL WHERE id = $1")
		.bind(id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	demote(&h, roster[2]).await;

	let before = grant_count(&h).await;
	let view = consilium_app::execute(&ports(&h), id, now()).await.unwrap();
	assert_eq!(view.consilium.state(), ConsiliumState::ExecutionFailed, "a stale quorum must not mint");
	assert!(
		view.consilium.failure_reason().unwrap_or_default().contains("still held by current owners"),
		"the reason must name the roster change: {:?}",
		view.consilium.failure_reason()
	);
	assert_eq!(grant_count(&h).await, before, "nothing was minted");
}

/// THE TWO-CALLER RACE. The inline execute after the carrying vote and the sweeper both see
/// "no issuance under this key" and both try to mint. One wins on the `(service, key)`
/// unique index; the loser must record the issuance that ACTUALLY EXISTS, not a phantom
/// failure. Recording `Failed` there is a lie that sticks: the owners are mailed a failure,
/// `awaiting_execution` never returns the consilium again, and the units are posted anyway.
#[tokio::test]
async fn two_concurrent_executions_agree_on_one_grant_and_neither_records_a_phantom_failure() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap();
	assert!(vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap());

	// Put it back to `approved` with no issuance recorded, the state a crash between the
	// verdict and the mint leaves behind — then drive BOTH callers at once. The carrying
	// vote's inline execution already minted; its row and the events it drained (the relay
	// has not run) are removed so the race is over a genuinely absent row.
	let minted: Option<Uuid> = sqlx::query_scalar("SELECT executed_issuance_id FROM consilium WHERE id = $1")
		.bind(id.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	sqlx::query("UPDATE consilium SET state = 'approved', executed_issuance_id = NULL WHERE id = $1")
		.bind(id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	if let Some(minted) = minted {
		for statement in [
			"DELETE FROM outbox WHERE aggregate_id = $1",
			"DELETE FROM event_log WHERE aggregate_id = $1",
			"DELETE FROM unit_issuances WHERE id = $1",
		] {
			sqlx::query(statement).bind(minted).execute(&h.pool).await.unwrap();
		}
	}

	let before = grant_count(&h).await;
	let (a, b) = (ports(&h), ports(&h));
	let (first, second) = tokio::join!(consilium_app::execute(&a, id, now()), consilium_app::execute(&b, id, now()));
	let first = first.expect("the first execution must not error");
	let second = second.expect("the second execution must not error");

	for view in [&first, &second] {
		assert_eq!(
			view.consilium.state(),
			ConsiliumState::Executed,
			"a lost race is not a failure — reason: {:?}",
			view.consilium.failure_reason()
		);
	}
	assert_eq!(grant_count(&h).await, before + 1, "exactly one issuance, however many callers raced");
	assert_eq!(first.consilium.executed_issuance_id(), second.consilium.executed_issuance_id());
	assert_eq!(state_of(&h, id).await, ConsiliumState::Executed);
}

/// THE SPEC TABLE IN docs/CONSILIUM.md, PINNED. Both planes implement an emailed-token flow
/// and had already drifted on these two points before they were decided. This is banking's
/// side of the table, asserted rather than assumed.
#[tokio::test]
async fn the_shared_token_specification_holds_on_this_side() {
	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 5).await;
	let c = open_grant(&h, roster[0], "500").await;
	let id = c.consilium.id();
	let (token, code) = credentials(&h, id, roster[1]).await;
	let audit = VoteAudit {
		client_ip: String::new(),
		user_agent: String::new(),
	};

	// "Reading an invitation costs nothing."
	assert!(consilium_app::invitation(h.consilia.as_ref(), &token, now()).await.is_ok());
	assert_eq!(attempts_of(&h, id, roster[1]).await, 0, "reading must never consume an attempt");

	// "Correct code: the attempt counter is NOT reset — a token that has been guessed at
	// stays closer to burning."
	assert!(
		consilium_app::submit_decision(h.consilia.as_ref(), &token, "WRONGCODE1", VoteDecision::Approve, &audit, now())
			.await
			.is_err()
	);
	assert!(
		consilium_app::submit_decision(h.consilia.as_ref(), &token, "WRONGCODE2", VoteDecision::Approve, &audit, now())
			.await
			.is_err()
	);
	assert_eq!(attempts_of(&h, id, roster[1]).await, 2);
	assert!(
		consilium_app::submit_decision(h.consilia.as_ref(), &token, &code, VoteDecision::Approve, &audit, now())
			.await
			.is_ok()
	);
	// 3, not 2: the successful submission is itself a counted attempt, and — the point of
	// the rule — the two guesses before it are NOT forgiven. A reset here would let an
	// attacker refresh the budget at will against any seat whose code they eventually find.
	assert_eq!(attempts_of(&h, id, roster[1]).await, 3, "a correct code does not reset the counter");

	// "Token against an already-closed request: refused BEFORE an attempt is counted, so a
	// mail scanner cannot spend a human's attempt budget."
	let (other_token, _) = credentials(&h, id, roster[2]).await;
	let before = attempts_of(&h, id, roster[2]).await;
	consilium_app::cancel(h.consilia.as_ref(), id, roster[0], now()).await.unwrap();
	let closed = consilium_app::submit_decision(h.consilia.as_ref(), &other_token, "WRONGCODE1", VoteDecision::Approve, &audit, now())
		.await
		.unwrap_err();
	assert!(matches!(closed, DomainError::NotFound { .. }), "a closed request answers anonymously: {closed:?}");
	assert_eq!(attempts_of(&h, id, roster[2]).await, before, "a closed request must not charge an attempt");
}

/// THE SECOND KIND, END TO END. A payment out of the fund's pooled capital opens its own
/// consilium inside `payments::open`, every eligible seat is mailed a PAYMENT approval (not
/// the payout template, which would name the wrong claim and the wrong rail), the carrying
/// vote records the approval on the order and chains its execution — deferred until the
/// relay has applied the reservation — and the governance history still reads with a payout
/// sitting beside it.
#[tokio::test]
async fn a_payment_consilium_is_opened_mailed_carried_and_leaves_the_history_readable() {
	use domain::{
		balance::Party,
		payments::{PaymentDestination, PaymentReason, PaymentState, PaymentTerms},
	};

	let _guard = exclusive_governance().await;
	let Some(h) = harness().await else {
		eprintln!("DATABASE_URL unset — skipping the consilium suite");
		return;
	};
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	// Another kind in the same table (a grant over `fee`), so the assertion below is "the
	// history still reads" — and the payment below spends `fund`, so the two do not queue.
	let grant = open_grant(&h, roster[0], "500").await;
	// The fund allocation's capital, credited the deposit way, so the solvency pre-check
	// passes.
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: LedgerAccountKey::ServiceClaim(ServiceId::fund()),
			amount: usdt("1000").base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.unwrap();
	// Apply whatever an earlier test left in the outbox BEFORE the snapshots, so the deltas
	// below measure this order's two legs and nothing else.
	h.relay.drain().await;
	let fee_before = h.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted;
	let fund_locked_before = h.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fund())).await.unwrap().locked;

	let payment_terms = PaymentTerms::new(
		Party::Service(ServiceId::fund()),
		PaymentDestination::Internal(Party::Service(ServiceId::fee())),
		usdt("250"),
		PaymentReason::new("settle the quarterly management fee").unwrap(),
	)
	.unwrap();
	let order = payments_app::open(&ports(&h).payment_ports(), roster[0], payment_terms, now()).await.expect("open the payment");
	let payment_id = order.order.id();
	let consilium = order.consilium_id.expect("fund-owned money is decided by the quorum");
	assert!(order.consent.is_none());

	let loaded = h.consilia.find(consilium).await.unwrap().expect("the payment consilium reads back");
	assert_eq!(loaded.consilium.kind(), domain::consilium::ConsiliumKind::Payment);
	assert_eq!(
		loaded.consilium.source_claim(),
		LedgerAccountKey::ServiceClaim(ServiceId::fund()),
		"NOT `service:fee`: the per-source index keys on the order's claim"
	);
	assert_eq!(loaded.voters.len(), 2, "the initiator holds no seat");
	let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM consilium_mail WHERE consilium_id = $1 ORDER BY kind")
		.bind(consilium.raw())
		.fetch_all(&h.pool)
		.await
		.unwrap();
	assert_eq!(kinds, vec!["payment_approval".to_owned(); 2], "each seat is asked with the PAYMENT template");
	let payload: String = sqlx::query_scalar("SELECT payload::text FROM consilium_mail WHERE consilium_id = $1 LIMIT 1")
		.bind(consilium.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	let mail: serde_json::Value = serde_json::from_str(&payload).unwrap();
	assert_eq!(mail["payment_id"], payment_id.to_string());
	assert_eq!(mail["tier"], "service");
	assert_eq!(mail["source"], "the fund allocation");

	// A second order against the same source is refused — and the consilium it would have
	// opened is withdrawn with it rather than left collecting votes over nothing.
	let open_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium WHERE state = 'open'").fetch_one(&h.pool).await.unwrap();
	let duplicate = PaymentTerms::new(
		Party::Service(ServiceId::fund()),
		PaymentDestination::Internal(Party::Service(ServiceId::fee())),
		usdt("1"),
		PaymentReason::new("again").unwrap(),
	)
	.unwrap();
	assert!(matches!(
		payments_app::open(&ports(&h).payment_ports(), roster[0], duplicate, now()).await,
		Err(DomainError::Conflict(_))
	));
	let open_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium WHERE state = 'open'").fetch_one(&h.pool).await.unwrap();
	assert_eq!(open_after, open_before, "the compensating cancel closed the orphan consilium");

	// The quorum carries; the carrying vote approves the order and reserves the source.
	assert!(!vote(&h, consilium, roster[1], VoteDecision::Approve).await.unwrap());
	assert!(vote(&h, consilium, roster[2], VoteDecision::Approve).await.unwrap());
	// The vote's inline execution recorded the approval; the settlement waits for the relay.
	let executed = consilium_app::execute(&ports(&h), consilium, now()).await.unwrap();
	assert_eq!(executed.consilium.state(), ConsiliumState::Executed);
	assert_eq!(executed.consilium.executed_payment_id(), Some(payment_id));
	let order = h.payments.find(payment_id).await.unwrap().unwrap();
	assert_eq!(order.order.state(), PaymentState::Approved, "reserved, not yet settled: the relay has not run");
	h.relay.drain().await;
	// `service:fund` is one claim per database, shared with every sibling, so this is a DELTA.
	assert_eq!(
		h.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fund())).await.unwrap().locked - fund_locked_before,
		usdt("250").base_units()
	);

	let report = payments_app::sweep(&ports(&h).payment_ports(), now()).await.unwrap();
	assert_eq!(report.executed, 1);
	assert_eq!(h.payments.find(payment_id).await.unwrap().unwrap().order.state(), PaymentState::Executed);
	h.relay.drain().await;
	assert_eq!(
		h.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted - fee_before,
		usdt("250").base_units()
	);

	// The history read — the one 0029 said a second kind would break if the vocabularies
	// ever differed in size.
	let history = consilium_app::list(h.consilia.as_ref(), 50).await.expect("a second kind must not break the history read");
	assert!(history.iter().any(|view| view.consilium.id() == consilium));
	assert!(history.iter().any(|view| view.consilium.id() == grant.consilium.id()));
}

/// The owners' mails name the RECIPIENT, a duplicate order is refused before a quorum is
/// seated, and a quorum's refusal closes the order it was over in the same transaction.
#[tokio::test]
async fn a_refused_payment_consilium_closes_its_order_and_its_mails_name_the_recipient() {
	use domain::{
		balance::Party,
		payments::{PaymentDestination, PaymentReason, PaymentState, PaymentTerms},
		users::mask_email,
	};

	let _guard = exclusive_governance().await;
	let Some(h) = harness().await else {
		eprintln!("DATABASE_URL unset — skipping the consilium suite");
		return;
	};
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	h.relay.drain().await;
	// The receiving investor, whose masked mailbox is what the owners must be shown.
	let mailbox = format!("recipient-{}@example.com", Uuid::new_v4().simple());
	let recipient = h
		.users
		.provision(AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap(), Email::parse(&mailbox).unwrap(), true)
		.await
		.unwrap()
		.id();
	let payment_terms = PaymentTerms::new(
		Party::Service(ServiceId::fee()),
		PaymentDestination::Internal(Party::User(recipient)),
		usdt("10"),
		PaymentReason::new("a referral bonus").unwrap(),
	)
	.unwrap();
	let order = payments_app::open(&ports(&h).payment_ports(), roster[0], payment_terms.clone(), now())
		.await
		.expect("open the payment");
	let payment_id = order.order.id();
	let consilium = order.consilium_id.expect("decided by the quorum");

	let payloads: Vec<String> = sqlx::query_scalar("SELECT payload::text FROM consilium_mail WHERE consilium_id = $1 AND kind = 'payment_approval'")
		.bind(consilium.raw())
		.fetch_all(&h.pool)
		.await
		.unwrap();
	assert_eq!(payloads.len(), 2);
	for payload in &payloads {
		let mail: serde_json::Value = serde_json::from_str(payload).unwrap();
		let destination = mail["destination"].as_str().unwrap();
		assert!(destination.contains(&mask_email(&mailbox)), "the owners are told who receives: {destination}");
		assert!(!destination.contains(&mailbox), "…but never the unmasked address: {destination}");
	}

	// A second order against the same source is refused BEFORE a quorum is seated: no
	// consilium row, no approval mails, no withdrawal notices.
	let mails_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium_mail").fetch_one(&h.pool).await.unwrap();
	let consilia_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium").fetch_one(&h.pool).await.unwrap();
	let duplicate = payments_app::open(&ports(&h).payment_ports(), roster[0], payment_terms, now()).await;
	assert!(matches!(duplicate, Err(DomainError::Conflict(ref why)) if why.contains("already open")), "{duplicate:?}");
	let mails_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium_mail").fetch_one(&h.pool).await.unwrap();
	let consilia_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium").fetch_one(&h.pool).await.unwrap();
	assert_eq!((mails_after, consilia_after), (mails_before, consilia_before), "the refusal touched nothing");

	// One rejection makes the threshold (2 of 3) unreachable: the consilium closes, and the
	// order closes with it rather than sitting pending until the sweeper expires it.
	assert!(vote(&h, consilium, roster[1], VoteDecision::Reject).await.unwrap());
	assert_eq!(state_of(&h, consilium).await, ConsiliumState::Rejected);
	assert_eq!(h.payments.find(payment_id).await.unwrap().unwrap().order.state(), PaymentState::Rejected);
	let outcome: String = sqlx::query_scalar("SELECT payload::text FROM consilium_mail WHERE consilium_id = $1 AND kind = 'payout_outcome' LIMIT 1")
		.bind(consilium.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	let mail: serde_json::Value = serde_json::from_str(&outcome).unwrap();
	assert_eq!(mail["outcome"], "REJECTED");
	assert!(mail["destination"].as_str().unwrap().contains(&mask_email(&mailbox)), "{outcome}");
	// The verdict over a payment stays the payment tuple: concierge renders exactly one
	// description per outcome, and the fund-and-mark one belongs to a valuation override.
	assert_eq!(mail["tier"], "internal", "{outcome}");
	assert_eq!(mail["amount"], "10", "{outcome}");
	assert_eq!(mail["reason"], "a referral bonus", "{outcome}");
	assert_eq!(mail["fund"], "", "a payment verdict describes no fund: {outcome}");
	assert!(matches!(mail["mark"].as_str(), None | Some("")), "a payment verdict records no mark: {outcome}");
	let expired = h.payments.expire_due(now() + domain::payments::TTL_SECS + 1).await.unwrap();
	assert_eq!(expired, 0, "the order was closed with the verdict, so nothing is left for the sweeper to expire");
}

/// THE RE-READ AFTER A REFUSED APPROVAL ASKS FOR THE POSITIVE FACT.
///
/// `execute_payment` re-reads the order when `record_approval` refuses, because the refusal
/// may be the loser of a two-caller race over an order the other caller has already
/// approved. An earlier draft took "no longer pending" as that proof — but an order the
/// initiator withdrew, the sweeper expired or a burned seat rejected is not pending either,
/// and each of those was filed as `Executed`: a consilium recorded as having authorized money
/// that never moved. Only `approved` (or a state past it) proves the approval landed; every
/// other closer must come back as `Failed`, naming the order's state.
///
/// Driven directly rather than through `execute` so each closer's outcome can be read off
/// the returned value rather than off the consilium row it would be recorded on.
#[tokio::test]
async fn a_refused_approval_is_believed_unless_the_order_is_actually_approved() {
	use domain::{
		balance::{Party, ServiceId},
		consilium::ConsiliumEffect,
		payments::{PaymentDestination, PaymentId, PaymentOrder, PaymentReason, PaymentState, PaymentSubject, PaymentTerms},
	};
	use piggybank_core::ports::{consilium::ExecutionOutcome, payments::ApprovalSeat};
	use sha2::{Digest, Sha256};

	let _guard = exclusive_governance().await;
	let Some(h) = harness().await else {
		eprintln!("DATABASE_URL unset — skipping the consilium suite");
		return;
	};
	reset_governance(&h).await;
	let initiator = owner(&h).await;

	/// The order, its subject and the approved payment consilium that links to it.
	async fn a_linked_order(h: &Harness, initiator: UserId, from: Party, to: PaymentDestination) -> (PaymentSubject, ConsiliumId) {
		let terms = PaymentTerms::new(from, to, usdt("250"), PaymentReason::new("settle the quarterly management fee").unwrap()).unwrap();
		let subject = PaymentSubject {
			payment_id: PaymentId::from_raw(Uuid::new_v4()),
			terms,
		};
		let consilium = ConsiliumId::from_raw(Uuid::new_v4());
		sqlx::query(
			"INSERT INTO consilium (id, kind, state, terms, source_claim, payload_hash, initiator_user_id, owner_count, threshold, expires_at, decided_at) \
			 VALUES ($1, 'payment', 'approved', $2::jsonb, $3, $4, $5, 3, 2, now() + interval '72 hours', now())",
		)
		.bind(consilium.raw())
		.bind(serde_json::to_string(&subject).unwrap())
		.bind(subject.terms.source_claim().logical_key())
		.bind(Sha256::digest(domain::consilium::ConsiliumTerms::Payment(subject.clone()).canonical_bytes()).as_slice())
		.bind(initiator.raw())
		.execute(&h.pool)
		.await
		.expect("insert the approved payment consilium");
		let mut order = PaymentOrder::open(
			subject.payment_id,
			subject.terms.clone(),
			Sha256::digest(subject.terms.canonical_bytes()).into(),
			initiator,
			now(),
		);
		h.payments.open(&mut order, ApprovalSeat::Consilium(consilium), CONSENT_URL_BASE).await.expect("open the order");
		(subject, consilium)
	}

	// The money facts go too: a `reserved` row left in the outbox would be applied by the
	// next test's relay drain against the global `fund` claim and skew every delta after it.
	async fn remove(h: &Harness, subject: &PaymentSubject, consilium: ConsiliumId) {
		sqlx::query("DELETE FROM payments WHERE id = $1").bind(subject.payment_id.raw()).execute(&h.pool).await.unwrap();
		sqlx::query("DELETE FROM consilium WHERE id = $1").bind(consilium.raw()).execute(&h.pool).await.unwrap();
		for statement in [
			"DELETE FROM outbox WHERE aggregate = 'payment' AND aggregate_id = $1",
			"DELETE FROM event_log WHERE aggregate = 'payment' AND aggregate_id = $1",
		] {
			sqlx::query(statement).bind(subject.payment_id.raw()).execute(&h.pool).await.unwrap();
		}
	}

	// One order per closer, each over its own fund-owned source so none queues behind
	// another on the single-open-per-source index.
	let closers: [(&str, Party, PaymentDestination); 3] = [
		("rejected", Party::Service(ServiceId::fund()), PaymentDestination::Internal(Party::Service(ServiceId::fee()))),
		("cancelled", Party::Service(ServiceId::fee()), PaymentDestination::Internal(Party::Service(ServiceId::fund()))),
		(
			"expired",
			Party::Service(ServiceId::parse("alpha").unwrap()),
			PaymentDestination::Internal(Party::Service(ServiceId::fee())),
		),
	];
	for (closer, from, to) in closers {
		let (subject, consilium) = a_linked_order(&h, initiator, from, to).await;
		let id = subject.payment_id;
		let expected = match closer {
			"rejected" => {
				h.payments.record_rejection(id, now()).await.unwrap();
				PaymentState::Rejected
			}
			"cancelled" => {
				h.payments.cancel(id, initiator, now()).await.unwrap();
				PaymentState::Cancelled
			}
			_ => {
				h.payments.expire_due(now() + domain::payments::TTL_SECS + 1).await.unwrap();
				PaymentState::Expired
			}
		};
		assert_eq!(h.payments.find(id).await.unwrap().unwrap().order.state(), expected);

		let view = h.consilia.find(consilium).await.unwrap().unwrap();
		match consilium_app::execute_payment(&ports(&h), &view.consilium, subject.clone(), now()).await.unwrap() {
			ExecutionOutcome::Failed(why) => assert!(why.contains(closer), "the refusal names the order's state: {why}"),
			ExecutionOutcome::Executed(_) => panic!("a consilium over a {closer} order was filed as having authorized it"),
		}
		// And the attempt moved nothing.
		assert_eq!(h.payments.find(id).await.unwrap().unwrap().order.state(), expected);
		remove(&h, &subject, consilium).await;
	}

	// THE POSITIVE CONTROL: an approval that did land — recorded by the other caller before
	// this one got the row lock — is believed, and so is a repeat, because `record_approval`
	// is idempotent on an approved order.
	let (subject, consilium) = a_linked_order(&h, initiator, Party::Service(ServiceId::fund()), PaymentDestination::Internal(Party::Service(ServiceId::fee()))).await;
	h.payments.record_approval(subject.payment_id, consilium, now()).await.unwrap();
	let view = h.consilia.find(consilium).await.unwrap().unwrap();
	for _ in 0..2 {
		match consilium_app::execute_payment(&ports(&h), &view.consilium, subject.clone(), now()).await.unwrap() {
			ExecutionOutcome::Executed(ConsiliumEffect::Payment(id)) => assert_eq!(id, subject.payment_id),
			ExecutionOutcome::Executed(
				ConsiliumEffect::Withdrawal(_) | ConsiliumEffect::Valuation(_) | ConsiliumEffect::FeePolicy(_) | ConsiliumEffect::Issuance(_) | ConsiliumEffect::Subscription(_),
			) => {
				panic!("a payment consilium produces neither a withdrawal, a mark, a mint nor a seed and schedules no change")
			}
			ExecutionOutcome::Failed(why) => panic!("an approved order must be believed: {why}"),
		}
	}
	remove(&h, &subject, consilium).await;
}

/// ONE ADMINISTRATOR CANNOT MOVE THE FEE ALLOCATION'S MONEY WITHOUT A QUORUM (#245).
///
/// Every door into `service:fee` is walked: a payment out of it opens only with the owners'
/// consilium seated and executes on nothing short of the threshold — one approval, however
/// senior, leaves it pending; the operator's `IssueUnits` and `RetireUnits` refuse the
/// reserved allocation outright; and a holder is seated only by an executed holder grant,
/// after which the units are theirs, the supply grew by exactly them, and nobody else's
/// units moved. Cash reaches the chain from `fee` only through a holder's redemption onto
/// their own claim (`ownership_fee`), never through a payment to an address.
#[tokio::test]
async fn one_admin_cannot_move_the_fee_allocation_without_a_quorum() {
	use domain::{
		balance::Party,
		payments::{PaymentDestination, PaymentReason, PaymentState, PaymentTerms},
	};

	let _lock = exclusive_governance().await;
	let Some(h) = harness().await else { return };
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let admin = roster[0];
	let person = grantee(&h).await;
	h.relay.drain().await;
	let fee = ServiceId::fee();

	// (1) A payment out of `fee` is the owners' to release: one approval is not a quorum.
	let terms = PaymentTerms::new(
		Party::Service(fee.clone()),
		PaymentDestination::Internal(Party::User(person)),
		usdt("10"),
		PaymentReason::new("a bonus").unwrap(),
	)
	.unwrap();
	let order = payments_app::open(&ports(&h).payment_ports(), admin, terms, now()).await.expect("open the payment");
	let consilium = order.consilium_id.expect("fund-owned money is decided by the quorum");
	assert!(order.consent.is_none());
	assert!(!vote(&h, consilium, roster[1], VoteDecision::Approve).await.unwrap(), "one approval must not carry it");
	assert_eq!(state_of(&h, consilium).await, ConsiliumState::Open);
	assert!(
		matches!(consilium_app::execute(&ports(&h), consilium, now()).await, Err(DomainError::Conflict(_))),
		"an open consilium is not executable"
	);
	assert_eq!(h.payments.find(order.order.id()).await.unwrap().unwrap().order.state(), PaymentState::Pending);
	assert!(h.payments.awaiting_execution().await.unwrap().is_empty(), "nothing is executable on one approval");
	// The initiator's own vote is not a path either: they hold no seat and were mailed no token.
	let seats: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consilium_voter WHERE consilium_id = $1 AND user_id = $2")
		.bind(consilium.raw())
		.bind(admin.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(seats, 0, "the initiator must have no seat");
	consilium_app::cancel(h.consilia.as_ref(), consilium, admin, now()).await.unwrap();

	// (2) Nor is a payment to an address: an allocation's cash never leaves by an order.
	let external = PaymentDestination::External {
		network: Network::Bep20,
		address: WalletAddress::parse(Network::Bep20, PAYOUT_ADDRESS).unwrap(),
	};
	let terms = PaymentTerms::new(Party::Service(fee.clone()), external, usdt("10"), PaymentReason::new("a draw").unwrap()).unwrap();
	let err = payments_app::open(&ports(&h).payment_ports(), admin, terms, now()).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref why) if why.contains("redemption")), "got {err:?}");

	// (3) The operator's mint and burn refuse the reserved allocation by name.
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &h.nav,
		relay: &h.notify,
	};
	let request = |key: &str| issuance_app::IssueUnitsRequest {
		service: fee.clone(),
		holder: UnitHolder::User(person),
		units: shares("500"),
		cost_basis: None,
		idempotency_key: IdempotencyKey::parse(key).unwrap(),
	};
	let err = issuance_app::issue_units(&fund_ports, &h.issuances, h.users.as_ref(), request("admin-mints"), now())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(ref why) if why.contains("holder grant")), "got {err:?}");
	let retire = issuance_app::RetireUnitsRequest {
		service: fee.clone(),
		holder: UnitHolder::User(person),
		units: shares("1"),
		cost_basis: None,
		idempotency_key: IdempotencyKey::parse("admin-burns").unwrap(),
		force: true,
	};
	let err = issuance_app::retire_units(&fund_ports, &h.issuances, h.users.as_ref(), retire, now()).await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)), "got {err:?}");
	assert_eq!(
		h.ledger.balance(&LedgerAccountKey::UserShares(fee.clone(), person)).await.unwrap().posted,
		0,
		"nothing was minted by hand"
	);

	// (4) After the quorum, the grant mints to the person and to nobody else: the supply
	// grows by exactly the grant, so every other holder is diluted and nothing more.
	let other = grantee(&h).await;
	let quote_before = funds_app::nav_of(&h.nav, h.ledger.as_ref(), &fee).await.unwrap();
	let supply_before = h.ledger.balance(&LedgerAccountKey::SharesOutstanding(fee.clone())).await.unwrap().posted;
	let cash_before = h.ledger.balance(&LedgerAccountKey::ServiceClaim(fee.clone())).await.unwrap().posted;
	let grant = HolderGrantTerms::new(fee.clone(), person, shares("500")).unwrap();
	let c = consilium_app::open_holder_grant(&ports(&h), admin, grant, now()).await.unwrap();
	let id = c.consilium.id();
	assert!(!vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap());
	assert!(
		matches!(consilium_app::execute(&ports(&h), id, now()).await, Err(DomainError::Conflict(_))),
		"one approval mints nothing"
	);
	assert_eq!(h.ledger.balance(&LedgerAccountKey::UserShares(fee.clone(), person)).await.unwrap().posted, 0);
	assert!(vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap());
	assert_eq!(state_of(&h, id).await, ConsiliumState::Approved);
	let executed = consilium_app::execute(&ports(&h), id, now()).await.unwrap();
	assert_eq!(executed.consilium.state(), ConsiliumState::Executed);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(
		h.ledger.balance(&LedgerAccountKey::UserShares(fee.clone(), person)).await.unwrap().posted,
		shares("500").base_units()
	);
	assert_eq!(
		h.ledger.balance(&LedgerAccountKey::UserShares(fee.clone(), other)).await.unwrap().posted,
		0,
		"nobody else was seated"
	);
	assert_eq!(
		h.ledger.balance(&LedgerAccountKey::SharesOutstanding(fee.clone())).await.unwrap().posted - supply_before,
		shares("500").base_units(),
		"the supply grew by the grant alone"
	);
	assert_eq!(
		h.ledger.balance(&LedgerAccountKey::ServiceClaim(fee.clone())).await.unwrap().posted,
		cash_before,
		"a grant moves no cash"
	);
	// The price is the same value over more units — the dilution and nothing else. (An
	// empty allocation quotes the seed price, so the direction is only meaningful once
	// somebody already held units.)
	let quote_after = funds_app::nav_of(&h.nav, h.ledger.as_ref(), &fee).await.unwrap();
	assert_eq!(quote_after.aum, quote_before.aum, "the allocation is worth what it was");
	let supply_after = Shares::from_base_units(supply_before + shares("500").base_units());
	assert_eq!(quote_after.nav, Nav::from_aum(quote_after.aum.unwrap(), supply_after).unwrap());
	if supply_before > 0 {
		assert!(quote_after.nav <= quote_before.nav, "dilution never raises the price");
	}
}

/// A registered, open fund with `units` outstanding held by `holder`, marked once at
/// NAV 1.0 by an operator — the state a valuation override is proposed against.
async fn marked_fund(h: &Harness, holder: UserId, units: &str) -> ServiceId {
	use domain::allocations::{Allocation, AllocationIcon, AllocationId};
	let service = ServiceId::parse(&format!("svc-{}", Uuid::new_v4())).unwrap();
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "Arbitrage seat", "", AllocationIcon::default()).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	h.allocations.open(&service).await.unwrap();
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::UserShares(service.clone(), holder),
			credit: LedgerAccountKey::SharesOutstanding(service.clone()),
			amount: Shares::parse_decimal(units).unwrap().base_units(),
			code: TransferCode::ShareMint,
			reference: 0,
		})
		.await
		.unwrap();
	funds_app::record_valuation(&h.nav, h.ledger.as_ref(), ValuationId::new(), service.clone(), usdt(units), "itest")
		.await
		.unwrap();
	service
}

fn override_terms(service: &ServiceId, aum: &str) -> ValuationOverrideTerms {
	ValuationOverrideTerms {
		service: service.clone(),
		aum: usdt(aum),
	}
}

/// banking#232 end to end: the guarded post refuses a +900% mark and there is no flag to
/// lift it; the same mark put to the owners is mailed, voted, and executed through the
/// shared writer with the initiator as `posted_by` — which then blocks the initiator's own
/// redemption. A retried execution appends nothing, and the history stays readable.
#[tokio::test]
async fn a_valuation_override_is_the_only_way_past_the_move_guard_and_binds_its_proposer() {
	let _guard = exclusive_governance().await;
	let Some(h) = harness().await else {
		eprintln!("DATABASE_URL unset — skipping the consilium suite");
		return;
	};
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let service = marked_fund(&h, roster[0], "100").await;

	// The direct post is capped, and names the way past it.
	let err = funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), usdt("1000"), &roster[0].to_string(), now())
		.await
		.unwrap_err();
	assert!(matches!(&err, DomainError::Validation(reason) if reason.contains("valuation-override consilium")), "got {err:?}");

	// The open gates: a fund the registry does not know, and one with nothing outstanding.
	let unregistered = ServiceId::parse("svc-nobody").unwrap();
	assert!(matches!(
		consilium_app::open_valuation_override(&ports(&h), roster[0], override_terms(&unregistered, "1"), now()).await,
		Err(DomainError::NotFound { entity: "allocation", .. })
	));
	let empty = {
		use domain::allocations::{Allocation, AllocationIcon, AllocationId};
		let service = ServiceId::parse(&format!("svc-{}", Uuid::new_v4())).unwrap();
		let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "empty", "", AllocationIcon::default()).unwrap();
		h.allocations.register(&mut allocation).await.unwrap();
		service
	};
	assert!(matches!(
		consilium_app::open_valuation_override(&ports(&h), roster[0], override_terms(&empty, "1"), now()).await,
		Err(DomainError::Validation(_))
	));

	let opened = consilium_app::open_valuation_override(&ports(&h), roster[0], override_terms(&service, "1000"), now())
		.await
		.unwrap();
	let id = opened.consilium.id();
	assert_eq!(opened.consilium.kind(), ConsiliumKind::ValuationOverride);
	assert_eq!(
		opened.consilium.source_claim(),
		LedgerAccountKey::ServiceClaim(service.clone()),
		"one open override per fund, keyed on its claim"
	);
	assert_eq!(opened.voters.len(), 2, "the initiator holds no seat");
	// A second override on the same fund is refused while this one is open.
	assert!(matches!(
		consilium_app::open_valuation_override(&ports(&h), roster[1], override_terms(&service, "900"), now()).await,
		Err(DomainError::Conflict(_))
	));

	// The owners are rung through the borrowed payment template, with the labels spelled out.
	let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM consilium_mail WHERE consilium_id = $1 ORDER BY kind")
		.bind(id.raw())
		.fetch_all(&h.pool)
		.await
		.unwrap();
	assert_eq!(kinds, vec!["payment_approval".to_owned(); 2]);
	let payload: String = sqlx::query_scalar("SELECT payload::text FROM consilium_mail WHERE consilium_id = $1 LIMIT 1")
		.bind(id.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	let mail: serde_json::Value = serde_json::from_str(&payload).unwrap();
	assert_eq!(mail["source"], format!("Arbitrage seat ({service}) — NAV valuation"));
	assert_eq!(mail["destination"], "AUM 1000 USDT");
	assert_eq!(mail["amount"], "1000");
	// The invitation carries the real terms, not the mail's labels.
	let (token, _) = credentials(&h, id, roster[1]).await;
	let invitation = consilium_app::invitation(h.consilia.as_ref(), &token, now()).await.unwrap();
	assert_eq!(invitation.terms, ConsiliumTerms::ValuationOverride(override_terms(&service, "1000")));

	// Carried by both peers; executed through the shared writer, guard not consulted.
	assert!(!vote(&h, id, roster[1], VoteDecision::Approve).await.unwrap());
	assert!(vote(&h, id, roster[2], VoteDecision::Approve).await.unwrap());
	let executed = consilium_app::execute(&ports(&h), id, now()).await.unwrap();
	assert_eq!(executed.consilium.state(), ConsiliumState::Executed);
	let mark = consilium_app::valuation_id(id);
	assert_eq!(executed.consilium.executed_valuation_id(), Some(mark));
	assert!(executed.consilium.executed_withdrawal_id().is_none() && executed.consilium.executed_payment_id().is_none());
	let current = h.nav.current(&service).await.unwrap().unwrap();
	assert_eq!(current.nav, Nav::parse_decimal("10").unwrap(), "AUM 1000 over 100 live units");
	assert_eq!(current.posted_by, roster[0].to_string(), "the proposer is the poster");
	assert_eq!(h.nav.find(mark).await.unwrap().map(|v| v.aum), Some(usdt("1000")));

	// A retried execution names the same mark and appends nothing.
	let again = consilium_app::execute(&ports(&h), id, now()).await.unwrap();
	assert_eq!(again.consilium.executed_valuation_id(), Some(mark));
	let marks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fund_valuations WHERE service = $1")
		.bind(service.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(marks, 2, "the operator's first mark and the owners' one");

	// The proposer is bound by the cooldown exactly as a direct poster would be.
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &h.nav,
		relay: &h.notify,
	};
	let err = funds_app::request_redemption(
		&fund_ports,
		&PgRedemptions::new(h.pool.clone()),
		roster[0],
		service.clone(),
		Shares::parse_decimal("1").unwrap(),
		now(),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(_)), "got {err:?}");

	// The history read — the third kind must not break it either.
	let history = consilium_app::list(h.consilia.as_ref(), 50).await.expect("a third kind must not break the history read");
	assert!(history.iter().any(|view| view.consilium.id() == id));
}

/// The owners hear how a valuation-override consilium ended, and that a token burned on one
/// of its seats, as a MARK — the fund line the fee-terms mails use and the AUM the mark
/// records — and not as a payment (banking#340): the borrowed payment tuple announced
/// "Payment …" over a number that moves no money, and concierge renders exactly one
/// description per outcome, refusing a mark next to a tier. The fixed wording of what the
/// override is for rides the verdict only; a burn notice is an alert about a brute-force
/// attempt, not the request.
#[tokio::test]
async fn the_owners_are_mailed_the_verdict_and_the_burn_of_a_valuation_override_as_a_mark() {
	let _guard = exclusive_governance().await;
	let Some(h) = harness().await else {
		eprintln!("DATABASE_URL unset — skipping the consilium suite");
		return;
	};
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let service = marked_fund(&h, roster[0], "100").await;

	// Refused by one peer: the verdict reaches the initiator and both seats.
	let opened = consilium_app::open_valuation_override(&ports(&h), roster[0], override_terms(&service, "1000"), now())
		.await
		.unwrap();
	let id = opened.consilium.id();
	assert!(vote(&h, id, roster[1], VoteDecision::Reject).await.unwrap());
	assert_eq!(state_of(&h, id).await, ConsiliumState::Rejected);
	let outcomes = outcome_mails(&h, id, "payout_outcome").await;
	assert_eq!(outcomes.len(), 3, "the initiator and every seat hear the verdict");
	for mail in &outcomes {
		assert_eq!(mail["outcome"], "REJECTED");
		assert_mark_description(mail, &service, "Valuation beyond the NAV-move guard; executing records the mark regardless of the guard.");
	}

	// A token burned on the next override over the same fund: the whole roster is warned,
	// over the same mark.
	let opened = consilium_app::open_valuation_override(&ports(&h), roster[0], override_terms(&service, "1000"), now())
		.await
		.unwrap();
	let id = opened.consilium.id();
	let (token, _) = credentials(&h, id, roster[1]).await;
	let audit = VoteAudit {
		client_ip: "203.0.113.7".to_owned(),
		user_agent: "itest".to_owned(),
	};
	for _ in 0..MAX_CODE_ATTEMPTS {
		consilium_app::submit_decision(h.consilia.as_ref(), &token, "WRONGCODE1", VoteDecision::Approve, &audit, now())
			.await
			.unwrap_err();
	}
	let burns = outcome_mails(&h, id, "token_burned").await;
	assert_eq!(burns.len(), 3, "the whole roster hears about a brute-force attempt");
	for mail in &burns {
		assert_eq!(mail["outcome"], "TOKEN_BURNED");
		assert!(mail["detail"].as_str().unwrap().contains(&roster[1].to_string()), "the seat is named: {}", mail["detail"]);
		assert_mark_description(mail, &service, "");
	}
	assert_eq!(state_of(&h, id).await, ConsiliumState::Open, "one burned token does not disarm the consilium");
}

/// The outcome mails of one kind queued for a consilium, as the worker will read them.
async fn outcome_mails(h: &Harness, consilium: ConsiliumId, kind: &str) -> Vec<serde_json::Value> {
	sqlx::query_scalar::<_, String>("SELECT payload::text FROM consilium_mail WHERE consilium_id = $1 AND kind = $2 ORDER BY user_id")
		.bind(consilium.raw())
		.bind(kind)
		.fetch_all(&h.pool)
		.await
		.unwrap()
		.iter()
		.map(|payload| serde_json::from_str(payload).unwrap())
		.collect()
}

/// What an outcome or burn mail over a valuation override says — the fund the fee-terms
/// mails name it by, the AUM the mark records, and nothing of a payout's, a payment's or a
/// fee change's. `reason` is the wording the mail is expected to carry: what the override
/// is for on a verdict, none on a burn notice.
fn assert_mark_description(mail: &serde_json::Value, service: &ServiceId, reason: &str) {
	assert_eq!(mail["fund"], format!("Arbitrage seat ({service})"), "the title, and the slug it is known by: {mail}");
	assert_eq!(mail["mark"], "AUM 1000 USDT", "the AUM the mark records: {mail}");
	assert_eq!(mail["reason"], reason, "what the override is for rides the verdict, never the burn notice");
	for empty in ["network", "address", "amount", "tier", "source", "destination"] {
		assert_eq!(mail[empty], "", "a mark names no rail and no payment: {empty}");
	}
	assert!(mail["current"].is_null() && mail["proposed"].is_null(), "a mark proposes no fee terms: {mail}");
}

/// banking's treasury (D17) collects the withdrawal fees, and pays nothing out until its
/// spending is decided by concierge's scope approvals (`bank:treasury_surplus`, D24): an
/// order out of it — into a claim on the platform or out to an address — is refused at
/// open, before any owner is asked.
#[tokio::test]
async fn the_treasury_takes_fees_in_and_pays_nothing_out() {
	use domain::{
		balance::Party,
		money::WalletAddress,
		payments::{PaymentDestination, PaymentReason, PaymentTerms},
	};

	let _guard = exclusive_governance().await;
	let Some(h) = harness().await else {
		eprintln!("DATABASE_URL unset — skipping the consilium suite");
		return;
	};
	reset_governance(&h).await;
	let roster = owners(&h, 3).await;
	let treasury = Party::Treasury.claim_key();
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: treasury.clone(),
			amount: usdt("300").base_units(),
			code: TransferCode::WithdrawFee,
			reference: 0,
		})
		.await
		.unwrap();
	h.relay.drain().await;
	let consilia_before: i64 = sqlx::query_scalar("SELECT count(*) FROM consilium").fetch_one(&h.pool).await.unwrap();
	let address = WalletAddress::parse(Network::Bep20, "0x52908400098527886E0F7030069857D2E4169EE7").unwrap();
	for to in [
		PaymentDestination::Internal(Party::Service(ServiceId::fee())),
		PaymentDestination::External { network: Network::Bep20, address },
	] {
		let terms = PaymentTerms::new(Party::Treasury, to, usdt("50"), PaymentReason::new("pay the auditors").unwrap()).unwrap();
		let refused = payments_app::open(&ports(&h).payment_ports(), roster[0], terms, now()).await;
		assert!(
			matches!(&refused, Err(DomainError::Precondition(reason)) if reason == payments_app::TREASURY_SPENDING_CLOSED),
			"{refused:?}"
		);
	}
	let consilia_after: i64 = sqlx::query_scalar("SELECT count(*) FROM consilium").fetch_one(&h.pool).await.unwrap();
	assert_eq!(consilia_after, consilia_before, "no owner was asked");
	let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM payments WHERE from_kind = 'treasury'").fetch_one(&h.pool).await.unwrap();
	assert_eq!(orders, 0, "nothing recorded");
	assert_eq!(h.ledger.balance(&treasury).await.unwrap().available(), usdt("300").base_units(), "the fees stay put");

	// Nobody pays INTO the treasury.
	assert!(
		PaymentTerms::new(
			Party::Service(ServiceId::fee()),
			PaymentDestination::Internal(Party::Treasury),
			usdt("1"),
			PaymentReason::new("no").unwrap()
		)
		.is_err()
	);
}
