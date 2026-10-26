//! Integration tests for the payments plane — real Postgres, no mocks (project rule).
//! They run when `DATABASE_URL` is set and skip otherwise.
//!
//! Two halves. The first asserts what only the DATABASE states — the partial unique index
//! over the treasury source, its deliberate exemption for an investor's own claim, and the
//! composite foreign keys that keep each seat on the kind of order it decides; none of these
//! has a Rust code path, which is the point of putting them in the schema. The second drives
//! [`PgPayments`] itself, so that every runtime `sqlx::query` in the adapter is executed by a
//! test — the check the compile-time macros would have given, bought back.
//!
//! `payments` carries a database-wide invariant ("one open order per treasury source"), so
//! every test here takes [`exclusive_payments`] and starts from [`reset_payments`]. The reset
//! runs at the START of each test so a panicking one cannot wedge the rest.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use domain::{
	balance::{LedgerAccountKey, Party, TransferCode},
	error::DomainError,
	money::{Network, Usdt},
	payments::{PaymentDestination, PaymentEffect, PaymentId, PaymentOrder, PaymentReason, PaymentState, PaymentTerms},
	scope_operations::MONEY_MOVEMENT_TOPIC,
	users::{Email, UserId},
	withdrawals::WithdrawalId,
};
use piggybank_core::{
	infrastructure::{
		custody::StubCustody,
		payments::{PgPayments, notice_amount},
		relay::Relay,
		scope_notifications::ScopeNotificationWorker,
		users::PgUsers,
	},
	ports::{
		LedgerTransfer, UserRepository,
		payments::{ApprovalSeat, ConsentAudit, ConsentDecision, ConsentInvalidation, ExecutionOutcome, PaymentFeed, PaymentFilter, PaymentRepository, SeatView, SessionSeat},
		scope_notifications::{NoticeError, ScopeNotice, ScopeNotifier},
	},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

static PAYMENTS: std::sync::LazyLock<tokio::sync::Mutex<()>> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

async fn exclusive_payments() -> tokio::sync::MutexGuard<'static, ()> {
	PAYMENTS.lock().await
}

/// Clear everything this suite writes, including the outbox and event-log rows the orders
/// drained. The outbox is shared with the relay suites, so leaving `payment` rows behind
/// would hand a live relay money facts about claims this suite never funded.
async fn reset_payments(pool: &PgPool) {
	refuse_outcome_notices(pool, false).await;
	for statement in [
		"DELETE FROM scope_notification_outbox",
		"DELETE FROM payments",
		"DELETE FROM outbox WHERE aggregate = 'payment'",
		"DELETE FROM event_log WHERE aggregate = 'payment'",
	] {
		sqlx::query(statement).execute(pool).await.expect("clear the payments plane");
	}
}

/// Make every `payment_outcome` INSERT fail — or stop doing so. A `NOT VALID` CHECK refuses
/// new rows only, which is exactly an enqueue failing inside the transition's transaction the
/// way a lock timeout or a constraint drift would. Dropped again by [`reset_payments`], so a
/// test that panics with it on cannot leave the queue refusing.
async fn refuse_outcome_notices(pool: &PgPool, refuse: bool) {
	sqlx::query("ALTER TABLE scope_notification_outbox DROP CONSTRAINT IF EXISTS test_refuses_payment_outcome")
		.execute(pool)
		.await
		.expect("drop the fault");
	if refuse {
		sqlx::query("ALTER TABLE scope_notification_outbox ADD CONSTRAINT test_refuses_payment_outcome CHECK (kind <> 'payment_outcome') NOT VALID")
			.execute(pool)
			.await
			.expect("inject the fault");
	}
}

/// A freshly provisioned investor. Each call mints its own auth subject, so tests never
/// contend for one user row.
async fn an_investor(pool: &PgPool) -> UserId {
	let users = PgUsers::new(pool.clone());
	let tag = Uuid::new_v4();
	let subject = domain::auth::AuthSubject::parse(&format!("payments-test-{tag}")).unwrap();
	let email = Email::parse(&format!("payments-{tag}@example.test")).unwrap();
	let id = users.provision(subject, email, true).await.expect("provision an investor").id();
	mirror_concierge_id(pool, id).await;
	id
}

/// The identity-plane id the bridge mirrors onto a row. A consent notice is addressed by it, so
/// an investor without one cannot be asked for anything and `open` refuses.
async fn mirror_concierge_id(pool: &PgPool, id: UserId) {
	sqlx::query("UPDATE users SET concierge_user_id = $2 WHERE id = $1")
		.bind(id.raw())
		.bind(Uuid::new_v4())
		.execute(pool)
		.await
		.expect("mirror the concierge id");
}

/// Insert one order directly. These tests are about what the SCHEMA refuses, so the rows go
/// in the way a buggy adapter would write them, with nothing in between to launder a violation.
async fn insert_payment(pool: &PgPool, id: Uuid, state: &str, from_kind: &str, from_id: &str, initiator: UserId) -> Result<(), sqlx::Error> {
	sqlx::query(
		"INSERT INTO payments (id, state, from_kind, from_id, to_kind, amount, reason, payload_hash, initiator_user_id, expires_at, decided_at) \
		 VALUES ($1, $2, $3, $4, 'user', '1000', 'a test order', $5, $6, now() + interval '72 hours', CASE WHEN $2 = 'pending' THEN NULL ELSE now() END)",
	)
	.bind(id)
	.bind(state)
	.bind(from_kind)
	.bind(from_id)
	.bind(vec![0u8; 32])
	.bind(initiator.raw())
	.execute(pool)
	.await
	.map(|_| ())
}

/// The whole of the concurrent-approval overdraw defence for banking's own claim: a race
/// that cannot be created does not have to be won. The platform's own money is the treasury,
/// so the keyed source here is `treasury`.
#[tokio::test]
async fn only_one_order_may_be_open_against_the_treasury_claim() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments schema tests");
		return;
	};
	reset_payments(&pool).await;
	let initiator = an_investor(&pool).await;
	let treasury = Party::Treasury.id_str();

	insert_payment(&pool, Uuid::new_v4(), "pending", "treasury", &treasury, initiator)
		.await
		.expect("the first order against the treasury is accepted");

	let second = insert_payment(&pool, Uuid::new_v4(), "pending", "treasury", &treasury, initiator).await;
	assert!(second.is_err(), "a second OPEN order against the treasury claim must be refused");

	// `approved` still holds the source (the reservation is against it), so it counts as open
	// too — and a terminal order releases it.
	sqlx::query("UPDATE payments SET state = 'cancelled', decided_at = now()")
		.execute(&pool)
		.await
		.expect("close the first order");
	insert_payment(&pool, Uuid::new_v4(), "approved", "treasury", &treasury, initiator)
		.await
		.expect("a closed order releases its source claim");
	let third = insert_payment(&pool, Uuid::new_v4(), "pending", "treasury", &treasury, initiator).await;
	assert!(third.is_err(), "an approved order still holds its source claim");

	reset_payments(&pool).await;
}

/// THE EXEMPTION IS THE FEATURE. An investor's claim is already serialized by `lock_claim`
/// and backstopped by TigerBeetle's non-negative flag, so indexing it here would add no
/// safety while letting one pending consent block that investor's every other payment for up
/// to 72h — a denial of service dressed as a guarantee.
#[tokio::test]
async fn an_investor_may_have_several_orders_open_at_once() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments schema tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let source = investor.to_string();

	for _ in 0..3 {
		insert_payment(&pool, Uuid::new_v4(), "pending", "user", &source, investor)
			.await
			.expect("an investor's own claim is deliberately outside the single-open index");
	}

	reset_payments(&pool).await;
}

/// §3's rule — "a payment whose source is `User(u)` requires `u`'s own consent" — made
/// unrepresentable otherwise. `payments.source_user_id` is GENERATED by the database from
/// `from_kind`/`from_id`, so the composite FK cannot be satisfied by a seat naming anyone
/// else, and a treasury order (whose generated column is NULL) can have no session seat.
#[tokio::test]
async fn a_session_seat_can_only_name_its_payments_own_source_user() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments schema tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let stranger = an_investor(&pool).await;
	let own = Uuid::new_v4();
	let treasury = Uuid::new_v4();
	insert_payment(&pool, own, "pending", "user", &investor.to_string(), investor)
		.await
		.expect("open the investor's order");
	insert_payment(&pool, treasury, "pending", "treasury", &Party::Treasury.id_str(), investor)
		.await
		.expect("open a treasury order");

	assert!(session_seat_row(&pool, own, stranger).await.is_err(), "a seat naming another user must be refused");
	assert!(session_seat_row(&pool, treasury, investor).await.is_err(), "a treasury order has no consent subject to seat");
	session_seat_row(&pool, own, investor).await.expect("the source user's own seat is accepted");

	reset_payments(&pool).await;
}

/// The mirror image of the test above, for the other requirement. `payments.treasury_source` is
/// GENERATED from `from_kind`, and `payment_concierge_approval` carries a composite FK over
/// `(payment_id, treasury_source)` with the second key pinned to TRUE — so the owners cannot be
/// seated over an investor's order by any code path, and a treasury order takes them.
#[tokio::test]
async fn a_concierge_seat_can_only_attach_to_a_treasury_order() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments schema tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let own = Uuid::new_v4();
	let treasury = Uuid::new_v4();
	insert_payment(&pool, own, "pending", "user", &investor.to_string(), investor)
		.await
		.expect("open the investor's order");
	insert_payment(&pool, treasury, "pending", "treasury", &Party::Treasury.id_str(), investor)
		.await
		.expect("open a treasury order");

	assert!(concierge_seat_row(&pool, own).await.is_err(), "the owners must not be seated over an investor's own money");
	concierge_seat_row(&pool, treasury).await.expect("the treasury's order takes its concierge seat");

	reset_payments(&pool).await;
}

async fn concierge_seat_row(pool: &PgPool, payment: Uuid) -> Result<(), sqlx::Error> {
	sqlx::query("INSERT INTO payment_concierge_approval (payment_id, destination, payload_hash) VALUES ($1, 'an investor', $2)")
		.bind(payment)
		.bind("0".repeat(64))
		.execute(pool)
		.await
		.map(|_| ())
}

async fn session_seat_row(pool: &PgPool, payment: Uuid, subject: UserId) -> Result<(), sqlx::Error> {
	sqlx::query("INSERT INTO payment_session_consent (payment_id, subject_user_id, subject_token_version_at_open, subject_email_hash_at_open) VALUES ($1, $2, 0, $3)")
		.bind(payment)
		.bind(subject.raw())
		.bind(vec![3u8; 32])
		.execute(pool)
		.await
		.map(|_| ())
}

// ---------------------------------------------------------------------------------------
// The adapter. Everything below drives `PgPayments`, so every runtime query it holds is
// executed at least once here — a runtime `sqlx::query` with no test that runs it is an
// unchecked string, which is the one thing the compile-time macros would have caught.
// ---------------------------------------------------------------------------------------

fn digest(bytes: &[u8]) -> [u8; 32] {
	use sha2::{Digest, Sha256};
	Sha256::digest(bytes).into()
}

fn now() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn audit() -> ConsentAudit {
	ConsentAudit {
		client_ip: "203.0.113.7".to_owned(),
		user_agent: "integration-test".to_owned(),
	}
}

fn an_order(from: Party, to: PaymentDestination, initiator: UserId, amount: &str) -> PaymentOrder {
	let terms = PaymentTerms::new(from, to, usdt(amount), PaymentReason::new("a documented reason").unwrap()).unwrap();
	// The application layer takes this hash; the tests only need one that is stable per order.
	let payload_hash = digest(&terms.canonical_bytes());
	PaymentOrder::open(PaymentId::from_raw(Uuid::new_v4()), terms, payload_hash, initiator, now())
}

/// A seat pinned to the subject AS THE PROJECTION HOLDS THEM NOW — the folded revoke floor
/// and the mirrored address — exactly what the application layer will freeze at open.
async fn a_session_seat(pool: &PgPool, subject: UserId) -> ApprovalSeat {
	let (token_version, email): (i64, String) = sqlx::query_as("SELECT GREATEST(concierge_token_version, token_version), email FROM users WHERE id = $1")
		.bind(subject.raw())
		.fetch_one(pool)
		.await
		.expect("the subject is mirrored");
	ApprovalSeat::Session(SessionSeat {
		subject,
		token_version_at_open: token_version as u64,
		email_hash_at_open: digest(email.as_bytes()),
	})
}

/// An approval id concierge would hand back; `approval_id` is unique across seats.
fn an_approval_id() -> String {
	Uuid::new_v4().to_string()
}

/// How many of this order's events reached the outbox. `Opened`, `Approved` and `Executed`
/// are audit trail and must not be there; only `Reserved` and `Settled` move money.
async fn relayed_kinds(pool: &PgPool, order: PaymentId) -> Vec<String> {
	sqlx::query_scalar("SELECT payload::jsonb ->> 'type' FROM outbox WHERE aggregate = 'payment' AND aggregate_id = $1 ORDER BY seq")
		.bind(order.raw())
		.fetch_all(pool)
		.await
		.expect("read the outbox")
}

/// One `payment_outcome` copy as the queue holds it.
#[derive(Debug)]
struct OutcomeNotice {
	recipient: Uuid,
	dedupe_key: String,
	topic: String,
	link: String,
	body: String,
}

/// Every consent-outcome notice queued about this order, in recipient order.
async fn outcome_notices(pool: &PgPool, order: PaymentId) -> Vec<OutcomeNotice> {
	let rows: Vec<(Uuid, String, String, String, String)> =
		sqlx::query_as("SELECT user_id, dedupe_key, topic, link, body FROM scope_notification_outbox WHERE payment_id = $1 AND kind = 'payment_outcome' ORDER BY user_id")
			.bind(order.raw())
			.fetch_all(pool)
			.await
			.expect("read the outcome notices");
	rows.into_iter()
		.map(|(recipient, dedupe_key, topic, link, body)| OutcomeNotice {
			recipient,
			dedupe_key,
			topic,
			link,
			body,
		})
		.collect()
}

const SESSIONS_REVOKED: &str = "the sessions of the account were revoked";
const EMAIL_CHANGED: &str = "the mailbox of the account changed";

/// Concierge's rule for this kind's amount (`money_amount`, concierge#95): a number of at
/// most 32 characters starting with a digit, one space, an upper-case currency code.
fn is_money(value: &str) -> bool {
	let Some((number, currency)) = value.rsplit_once(' ') else {
		return false;
	};
	number.starts_with(|c: char| c.is_ascii_digit())
		&& number.chars().count() <= 32
		&& number.chars().all(|c| c.is_ascii_digit() || matches!(c, ' ' | '\u{00A0}' | '\u{202F}' | '.' | ',' | '\''))
		&& (2..=10).contains(&currency.len())
		&& currency.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// The amount a notice carries is the consent's exact decimal spelled as money, in every
/// case concierge will see — including the largest amount the ledger can hold, where the
/// smallest fractional digits give way, never the integer part or the currency.
#[test]
fn the_notice_amount_is_the_consent_decimal_spelled_as_money() {
	assert_eq!(notice_amount(usdt("1200")), "1200 USDT");
	assert_eq!(notice_amount(usdt("12.50")), "12.5 USDT");
	assert_eq!(notice_amount(usdt("0.000000000000000001")), "0.000000000000000001 USDT");
	for raw in ["1200", "12.50", "0.000000000000000001", "1234567890123.123456789012345678"] {
		assert!(is_money(&notice_amount(usdt(raw))), "{raw} → {}", notice_amount(usdt(raw)));
	}
	let max = notice_amount(Usdt::from_base_units(u128::MAX));
	assert!(is_money(&max), "{max}");
	assert!(max.starts_with(&(u128::MAX / 10u128.pow(18)).to_string()), "{max}");
}

/// Assert the order's outcome notices: one per person in `recipients`, each under its own key,
/// on the money-movement topic, with nothing to open, saying how much and `because` what.
async fn assert_outcome_told(pool: &PgPool, order: PaymentId, recipients: &[UserId], amount: &str, because: &str) {
	let told = outcome_notices(pool, order).await;
	let mut expected: Vec<Uuid> = recipients.iter().map(|user| user.raw()).collect();
	expected.sort();
	assert_eq!(told.iter().map(|notice| notice.recipient).collect::<Vec<_>>(), expected, "one copy per recipient: {told:#?}");
	for notice in &told {
		assert_eq!(notice.dedupe_key, format!("payment:{order}:outcome:{}", notice.recipient), "a key per copy");
		assert_eq!(notice.topic, MONEY_MOVEMENT_TOPIC);
		assert_eq!(notice.link, "", "an outcome has nothing left to answer");
		let money = notice_amount(usdt(amount));
		assert!(notice.body.contains(&money) && notice.body.contains(because), "{money} / {because}: {}", notice.body);
	}
}

/// Opening an order writes it, its seat and its events in one transaction — and puts NOTHING
/// in the outbox, because an order that has not been approved has moved no money.
#[tokio::test]
async fn opening_an_order_materializes_its_session_seat_and_relays_nothing() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "12.50");
	let id = order.id();
	payments.open(&mut order, a_session_seat(&pool, investor).await).await.expect("open the order");

	let view = payments.find(id).await.expect("find the order").expect("the order exists");
	assert_eq!(view.order.state(), PaymentState::Pending);
	assert_eq!(view.order.terms().amount(), usdt("12.50"));
	let SeatView::Session(consent) = view.seat else {
		panic!("an investor-sourced order is not decided by the owners: {:?}", view.seat);
	};
	assert_eq!(consent.subject, investor);
	assert_eq!(consent.decision, ConsentDecision::Pending);
	assert!(consent.invalidated.is_none());
	assert!(relayed_kinds(&pool, id).await.is_empty(), "a pending order has moved no money");

	reset_payments(&pool).await;
}

/// The seat is minted by the caller and the requirement is read off the terms, so `open` is
/// where the two can first disagree. Each mismatch is refused BEFORE a row is written, by
/// name, rather than surfacing as the foreign-key string the schema would otherwise answer.
#[tokio::test]
async fn open_refuses_a_seat_that_is_not_the_one_the_terms_call_for() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let stranger = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());

	// The owners over an investor's own money.
	let mut own = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "1.00");
	let refused = payments.open(&mut own, ApprovalSeat::Concierge).await;
	assert!(matches!(refused, Err(DomainError::Validation(_))), "a concierge seat over an investor's order: {refused:?}");
	assert!(payments.find(own.id()).await.unwrap().is_none(), "a refused open writes nothing");

	// One investor's consent over the treasury's money.
	let mut treasury = an_order(Party::Treasury, PaymentDestination::Internal(Party::User(recipient)), investor, "1.00");
	let refused = payments.open(&mut treasury, a_session_seat(&pool, investor).await).await;
	assert!(matches!(refused, Err(DomainError::Validation(_))), "a session seat over a treasury order: {refused:?}");
	assert!(payments.find(treasury.id()).await.unwrap().is_none());

	// The right kind of seat, naming the wrong investor.
	let mut own = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "1.00");
	let refused = payments.open(&mut own, a_session_seat(&pool, stranger).await).await;
	assert!(matches!(refused, Err(DomainError::Validation(_))), "a session seat naming a stranger: {refused:?}");
	assert!(payments.find(own.id()).await.unwrap().is_none());

	reset_payments(&pool).await;
}

/// The notice is for a consent that DIED, and for nothing else: an investor who answered
/// (either way), an order that ran out of time or was withdrawn, and a treasury order its
/// owners refused all have their own channel or none, and none of them is this.
#[tokio::test]
async fn an_answered_expired_withdrawn_or_owner_refused_order_sends_no_outcome_notice() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let admin = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let to = || PaymentDestination::Internal(Party::User(recipient));

	// Refused by the investor.
	let mut refused = an_order(Party::User(investor), to(), admin, "1.00");
	payments.open(&mut refused, a_session_seat(&pool, investor).await).await.unwrap();
	payments
		.answer_session_consent(refused.id(), investor, ConsentDecision::Reject, &audit(), now())
		.await
		.expect("a valid refusal");
	assert_eq!(payments.find(refused.id()).await.unwrap().unwrap().order.state(), PaymentState::Rejected);

	// Withdrawn by the admin who opened it.
	let mut withdrawn = an_order(Party::User(investor), to(), admin, "2.00");
	payments.open(&mut withdrawn, a_session_seat(&pool, investor).await).await.unwrap();
	payments.cancel(withdrawn.id(), admin, now()).await.expect("the initiator withdraws it");

	// Left to run out.
	let mut expired = an_order(Party::User(investor), to(), admin, "3.00");
	payments.open(&mut expired, a_session_seat(&pool, investor).await).await.unwrap();
	payments.expire_due(now() + domain::payments::TTL_SECS + 1).await.expect("sweep");
	assert_eq!(payments.find(expired.id()).await.unwrap().unwrap().order.state(), PaymentState::Expired);

	// Refused by the owners.
	let mut treasury = an_order(Party::Treasury, to(), admin, "4.00");
	payments.open(&mut treasury, ApprovalSeat::Concierge).await.unwrap();
	payments.record_rejection(treasury.id(), now()).await.expect("the owners refuse");

	for order in [refused.id(), withdrawn.id(), expired.id(), treasury.id()] {
		assert!(outcome_notices(&pool, order).await.is_empty(), "no consent died on {order}");
	}

	reset_payments(&pool).await;
}

/// The happy path, and the two idempotency rules the retry contract rests on: the same answer
/// again is a no-op, a different one is a conflict.
#[tokio::test]
async fn the_subjects_answer_approves_the_order_and_reserves_its_source() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "3.00");
	let id = order.id();
	payments.open(&mut order, a_session_seat(&pool, investor).await).await.expect("open the order");

	let outcome = payments
		.answer_session_consent(id, investor, ConsentDecision::Approve, &audit(), now())
		.await
		.expect("the subject's answer is accepted");
	assert!(outcome.decided && outcome.approved);
	assert_eq!(outcome.payment.order.state(), PaymentState::Approved);
	assert_eq!(
		relayed_kinds(&pool, id).await,
		vec!["reserved".to_owned()],
		"approval reserves the source, and relays nothing else"
	);

	let repeat = payments
		.answer_session_consent(id, investor, ConsentDecision::Approve, &audit(), now())
		.await
		.expect("a retried answer is a no-op");
	assert!(!repeat.decided, "a repeat must not double-count");
	let contradiction = payments.answer_session_consent(id, investor, ConsentDecision::Reject, &audit(), now()).await;
	assert!(matches!(contradiction, Err(DomainError::Conflict(_))), "a different answer is refused, never an overwrite");

	// Executing it settles the reservation. `Settled` is the second and last event that moves
	// money; `Executed` is audit trail and must not reach the relay.
	payments
		.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Transfer), now())
		.await
		.expect("record the ledger effect");
	assert_eq!(relayed_kinds(&pool, id).await, vec!["reserved".to_owned(), "settled".to_owned()]);
	let view = payments.find(id).await.unwrap().unwrap();
	assert_eq!(view.order.state(), PaymentState::Executed);
	assert!(payments.awaiting_execution().await.unwrap().is_empty(), "an executed order is not awaiting execution");

	reset_payments(&pool).await;
}

/// The treasury branch: the order is seated on a concierge approval, the approval is
/// recorded by the owners' decision rather than by an investor's answer, and execution is
/// idempotent for the same effect and a conflict for a different one.
#[tokio::test]
async fn a_treasury_order_is_carried_by_its_concierge_approval() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let operator = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());

	let mut order = an_order(Party::Treasury, PaymentDestination::Internal(Party::User(recipient)), operator, "40.00");
	let id = order.id();
	payments.open(&mut order, ApprovalSeat::Concierge).await.expect("open the order");

	let view = payments.find(id).await.unwrap().unwrap();
	let SeatView::Concierge(seat) = &view.seat else {
		panic!("treasury money is never consented to by one investor: {:?}", view.seat);
	};
	assert!(seat.approval_id.is_none(), "nothing proposed yet");
	assert!(view.consent_invalidated().is_none());

	assert_eq!(payments.awaiting_execution().await.unwrap(), Vec::new(), "a pending order is not executable");
	let approval = an_approval_id();
	payments.record_concierge_approval(id, &approval, 0, now()).await.expect("the owners carried it");
	assert_eq!(payments.awaiting_execution().await.unwrap(), vec![id]);
	assert_eq!(relayed_kinds(&pool, id).await, vec!["reserved".to_owned()]);
	// An approval for some OTHER proposal cannot carry this one, whatever its terms say.
	assert!(
		matches!(payments.record_concierge_approval(id, &an_approval_id(), 0, now()).await, Err(DomainError::Conflict(_))),
		"only the seat's own approval may approve the order"
	);
	payments
		.record_concierge_approval(id, &approval, 0, now())
		.await
		.expect("recording the same approval twice is a no-op");
	assert_eq!(relayed_kinds(&pool, id).await, vec!["reserved".to_owned()], "reserved once");

	payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Transfer), now()).await.expect("execute");
	payments
		.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Transfer), now())
		.await
		.expect("the same effect again is the idempotent retry");
	let conflict = payments
		.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Withdrawal(WithdrawalId::from_raw(Uuid::new_v4()))), now())
		.await;
	assert!(matches!(conflict, Err(DomainError::Conflict(_))), "a second, different effect is a conflict, not an overwrite");
	assert_eq!(relayed_kinds(&pool, id).await, vec!["reserved".to_owned(), "settled".to_owned()], "settled once");

	reset_payments(&pool).await;
}

/// The admin feed's filters, the expiry sweep, and the rule that only the operator who opened
/// an order may withdraw it.
#[tokio::test]
async fn the_feed_filters_and_the_sweep_close_what_nobody_answered() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let stranger = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());

	let mut of_investor = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "5.00");
	let investors_order = of_investor.id();
	payments.open(&mut of_investor, a_session_seat(&pool, investor).await).await.expect("open the investor's order");
	let mut of_treasury = an_order(Party::Treasury, PaymentDestination::Internal(Party::User(recipient)), investor, "9.00");
	let treasury_order = of_treasury.id();
	payments.open(&mut of_treasury, ApprovalSeat::Concierge).await.expect("open the treasury's order");

	let all = payments.list(&PaymentFilter::default(), 50).await.expect("the whole history");
	assert_eq!(all.len(), 2);
	let treasury = payments
		.list(
			&PaymentFilter {
				treasury_source: Some(true),
				..PaymentFilter::default()
			},
			50,
		)
		.await
		.unwrap();
	assert_eq!(treasury.iter().map(|view| view.order.id()).collect::<Vec<_>>(), vec![treasury_order]);
	let by_party = payments
		.list(
			&PaymentFilter {
				party: Some(Party::User(investor)),
				state: Some(PaymentState::Pending),
				..PaymentFilter::default()
			},
			50,
		)
		.await
		.unwrap();
	assert_eq!(by_party.iter().map(|view| view.order.id()).collect::<Vec<_>>(), vec![investors_order]);

	let refused = payments.cancel(treasury_order, stranger, now()).await;
	assert!(matches!(refused, Err(DomainError::Forbidden(_))), "only the operator who opened it may withdraw it");
	payments.cancel(treasury_order, investor, now()).await.expect("the initiator may withdraw it");

	// The sweep binds the caller's clock, so "past the deadline" and "expired" answer the same
	// question. One tick past the TTL closes exactly the order still pending.
	let closed = payments.expire_due(now() + domain::payments::TTL_SECS + 1).await.expect("sweep");
	assert_eq!(closed, 1, "the cancelled order is already closed; only the pending one expires");
	assert_eq!(payments.find(investors_order).await.unwrap().unwrap().order.state(), PaymentState::Expired);

	reset_payments(&pool).await;
}

/// Concierge's scope inbox, stood in for at the notifier's port: takes every notice and keeps it.
#[derive(Default)]
struct Inbox(Mutex<Vec<ScopeNotice>>);

#[async_trait]
impl ScopeNotifier for Inbox {
	async fn emit(&self, _: Uuid, notice: &ScopeNotice) -> Result<(), NoticeError> {
		self.0.lock().unwrap().push(notice.clone());
		Ok(())
	}
}

/// The one consent notice an order queued, as the queue holds it: `(sent, withdrawn)`.
async fn consent_notice(pool: &PgPool, order: PaymentId) -> (bool, bool) {
	sqlx::query_as("SELECT sent_at IS NOT NULL, withdrawn_at IS NOT NULL FROM scope_notification_outbox WHERE payment_id = $1 AND kind = 'payment_consent'")
		.bind(order.raw())
		.fetch_one(pool)
		.await
		.expect("exactly one consent notice per order")
}

/// Closing an order without a verdict — the operator withdrawing it, or its window running
/// out — takes back the consent notice the inbox has not taken (#368). Left in the queue, the
/// inbox coming back would ask the investor to answer an order nobody can act on. A notice
/// already delivered stands: the row says sent, never withdrawn.
#[tokio::test]
async fn withdrawing_or_expiring_an_order_withdraws_the_consent_notice_not_yet_delivered() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let inbox = Arc::new(Inbox::default());
	let worker = ScopeNotificationWorker::new(pool.clone(), inbox.clone());

	// The operator withdraws the order before the notice was delivered: it goes with the order.
	let mut first = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "5.00");
	let withdrawn_order = first.id();
	payments.open(&mut first, a_session_seat(&pool, investor).await).await.expect("open the first order");
	assert_eq!(consent_notice(&pool, withdrawn_order).await, (false, false), "queued with the order");
	payments.cancel(withdrawn_order, investor, now()).await.expect("the initiator may withdraw it");
	assert_eq!(consent_notice(&pool, withdrawn_order).await, (false, true), "withdrawn with the order");
	assert_eq!(worker.drain().await.expect("a pass"), 0, "a withdrawn notice is not the next pass's to deliver");

	// The window running out closes the same way.
	let mut second = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "6.00");
	let expired_order = second.id();
	payments.open(&mut second, a_session_seat(&pool, investor).await).await.expect("open the second order");
	let closed = payments.expire_due(now() + domain::payments::TTL_SECS + 1).await.expect("sweep");
	assert_eq!(closed, 1, "the withdrawn order is already closed; only the pending one expires");
	assert_eq!(payments.find(expired_order).await.unwrap().unwrap().order.state(), PaymentState::Expired);
	assert_eq!(consent_notice(&pool, expired_order).await, (false, true));
	assert_eq!(worker.drain().await.expect("a pass"), 0);
	assert!(inbox.0.lock().unwrap().is_empty(), "neither withdrawn notice reached the inbox");

	// Control: a delivered notice stands. Withdrawing the order afterwards does not unsay it.
	let mut third = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "7.00");
	let delivered_order = third.id();
	payments.open(&mut third, a_session_seat(&pool, investor).await).await.expect("open the third order");
	assert_eq!(worker.drain().await.expect("a pass"), 1, "handed over");
	assert_eq!(consent_notice(&pool, delivered_order).await, (true, false));
	payments.cancel(delivered_order, investor, now()).await.expect("the initiator may withdraw it");
	assert_eq!(consent_notice(&pool, delivered_order).await, (true, false), "a delivered notice is left as it is");

	reset_payments(&pool).await;
}

/// `Approved` IS A COMMITTED STATE: the reservation landed in the approval's transaction and
/// the settlement would land in the execution's. A failure between the two must give the
/// locked amount back, or a terminal order leaves it in `clearing` with nothing left to post
/// or void it — a withdrawal's fail void, by another name. The treasury is a global singleton
/// shared with every other suite, so every figure here is a DELTA.
#[tokio::test]
async fn a_failed_execution_releases_the_reservation_it_was_holding() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	let Some(ledger) = common::seeded_ledger(&pool, "payments release test").await else {
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone());
	ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: Party::Treasury.claim_key(),
			amount: usdt("100").base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.expect("fund the treasury claim");
	// Whatever an earlier suite left undrained is applied before the snapshots, so the
	// deltas below are this order's alone.
	relay.drain().await;
	let before = ledger.balance(&Party::Treasury.claim_key()).await.unwrap();
	let recipient_before = ledger.balance(&LedgerAccountKey::UserClaim(recipient)).await.unwrap().posted;

	let mut order = an_order(Party::Treasury, PaymentDestination::Internal(Party::User(recipient)), investor, "30");
	let id = order.id();
	payments.open(&mut order, ApprovalSeat::Concierge).await.expect("open the order");
	payments.record_concierge_approval(id, &an_approval_id(), 0, now()).await.expect("the owners carried it");
	relay.drain().await;
	let reserved = ledger.balance(&Party::Treasury.claim_key()).await.unwrap();
	assert_eq!(reserved.locked - before.locked, usdt("30").base_units(), "the approval locked the source");

	payments
		.record_execution(id, ExecutionOutcome::Failed("the ledger refused the settlement".into()), now())
		.await
		.expect("record the failure");
	assert_eq!(
		relayed_kinds(&pool, id).await,
		vec!["reserved".to_owned(), "released".to_owned()],
		"the failure relays exactly one money fact: the release"
	);
	relay.drain().await;

	let released = ledger.balance(&Party::Treasury.claim_key()).await.unwrap();
	assert_eq!(released.posted, before.posted, "nothing was debited");
	assert_eq!(released.locked, before.locked, "the reservation was voided");
	assert_eq!(
		ledger.balance(&LedgerAccountKey::UserClaim(recipient)).await.unwrap().posted,
		recipient_before,
		"the destination saw nothing"
	);
	let parked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox WHERE aggregate = 'payment' AND parked_at IS NOT NULL")
		.fetch_one(&pool)
		.await
		.unwrap();
	assert_eq!(parked, 0, "a parked row would mean a void the ledger refused");
	let view = payments.find(id).await.unwrap().unwrap();
	assert_eq!(view.order.state(), PaymentState::ExecutionFailed);
	assert_eq!(view.order.failure_reason(), Some("the ledger refused the settlement"));
	assert!(payments.awaiting_execution().await.unwrap().is_empty(), "nothing retries a failure silently");

	reset_payments(&pool).await;
}

/// THE ONE TEST THAT PROVES THE MONEY MOVES. Everything above asserts Postgres rows; this
/// drives the whole write path — order, consent, relay, TigerBeetle — and reads the claims
/// back from the ledger that actually owns them.
///
/// It also pins the two-phase shape: after the approval the source is *locked*, not yet
/// debited, so a second approved order against the same claim contends with a reservation
/// rather than with a stale read. Only the settlement posts it.
#[tokio::test]
async fn an_approved_payment_reserves_its_source_and_then_settles_it() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	let Some(ledger) = common::seeded_ledger(&pool, "payments relay test").await else {
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone());

	// Credit the investor's claim the way a deposit does: `Dr wallet:<net> / Cr user:<id>`.
	ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: LedgerAccountKey::UserClaim(investor),
			amount: usdt("100").base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.expect("fund the investor's claim");
	relay.drain().await;

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "30");
	let id = order.id();
	payments.open(&mut order, a_session_seat(&pool, investor).await).await.expect("open the order");
	payments.answer_session_consent(id, investor, ConsentDecision::Approve, &audit(), now()).await.expect("consent");

	relay.drain().await;
	let reserved = ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();
	assert_eq!(reserved.posted, usdt("100").base_units(), "a reservation locks the claim, it does not debit it");
	assert_eq!(reserved.locked, usdt("30").base_units());
	assert_eq!(reserved.available(), usdt("70").base_units());

	let recipient_before = ledger.balance(&LedgerAccountKey::UserClaim(recipient)).await.unwrap().posted;
	payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Transfer), now()).await.expect("execute");
	relay.drain().await;

	let settled = ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();
	assert_eq!(settled.posted, usdt("70").base_units(), "the settlement posts the reservation");
	assert_eq!(settled.locked, 0);
	let recipient_after = ledger.balance(&LedgerAccountKey::UserClaim(recipient)).await.unwrap().posted;
	assert_eq!(recipient_after - recipient_before, usdt("30").base_units());
	// Nothing parked: a parked row here would mean a leg the ledger refused.
	let parked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox WHERE aggregate = 'payment' AND parked_at IS NOT NULL")
		.fetch_one(&pool)
		.await
		.unwrap();
	assert_eq!(parked, 0);

	reset_payments(&pool).await;
}

/// THE TOKEN-VERSION PIN, AT CONSENT. `RevokeTokens` is the investor's (or the operator's)
/// "every session out" — a consent asked for before it must not be answerable after it. The
/// seat fails closed and takes the order with it: with one seat there is nobody to re-issue
/// the request to.
#[tokio::test]
async fn revoking_the_investors_sessions_voids_a_pending_consent() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let users = PgUsers::new(pool.clone());

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "2.00");
	let id = order.id();
	payments.open(&mut order, a_session_seat(&pool, investor).await).await.expect("open the order");
	assert!(payments.find(id).await.unwrap().unwrap().consent_invalidated().is_none(), "the pins hold at open");

	users.revoke_tokens(investor).await.expect("revoke every session");

	let refused = payments.answer_session_consent(id, investor, ConsentDecision::Approve, &audit(), now()).await;
	assert!(
		matches!(refused, Err(DomainError::Conflict(ref why)) if why.contains("revoked")),
		"the answer no longer consents to anything: {refused:?}"
	);
	let view = payments.find(id).await.unwrap().unwrap();
	assert_eq!(view.order.state(), PaymentState::Rejected, "a voided consent fails the payment closed");
	assert!(matches!(view.consent_invalidated(), Some(ConsentInvalidation::SessionsRevoked { .. })));
	assert!(relayed_kinds(&pool, id).await.is_empty(), "nothing was reserved");
	assert_outcome_told(&pool, id, &[investor], "2", SESSIONS_REVOKED).await;
	// A closed order answers like an unknown one.
	assert!(matches!(
		payments.answer_session_consent(id, investor, ConsentDecision::Approve, &audit(), now()).await,
		Err(DomainError::NotFound { .. })
	));
	assert_eq!(outcome_notices(&pool, id).await.len(), 1, "a retried answer tells nobody twice");

	reset_payments(&pool).await;
}

/// THE SAME PIN, AT EXECUTION. Consent and execution can be 72h apart, and a revocation in
/// between must win: the approved order fails closed instead of settling, and — because it
/// was reserved at approval — releases its reservation on the way out.
#[tokio::test]
async fn a_revocation_between_consent_and_execution_fails_the_payment_closed() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let users = PgUsers::new(pool.clone());

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "4.00");
	let id = order.id();
	payments.open(&mut order, a_session_seat(&pool, investor).await).await.expect("open the order");
	let consented = payments
		.answer_session_consent(id, investor, ConsentDecision::Approve, &audit(), now())
		.await
		.expect("consent while the pins hold");
	assert!(consented.approved);
	assert_eq!(payments.awaiting_execution().await.unwrap(), vec![id]);

	users.revoke_tokens(investor).await.expect("revoke every session");

	let refused = payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Transfer), now()).await;
	assert!(
		matches!(refused, Err(DomainError::Conflict(ref why)) if why.contains("revoked")),
		"the settlement is refused: {refused:?}"
	);
	let view = payments.find(id).await.unwrap().unwrap();
	assert_eq!(view.order.state(), PaymentState::ExecutionFailed);
	assert!(view.order.failure_reason().is_some_and(|why| why.contains("revoked")));
	assert_eq!(
		relayed_kinds(&pool, id).await,
		vec!["reserved".to_owned(), "released".to_owned()],
		"the reservation is released, and nothing is settled"
	);
	assert!(payments.awaiting_execution().await.unwrap().is_empty());
	assert_outcome_told(&pool, id, &[investor], "4", SESSIONS_REVOKED).await;
	// The refusal is idempotent: a retry finds a terminal order and is a plain conflict, not a
	// second failure record.
	assert!(matches!(
		payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Transfer), now()).await,
		Err(DomainError::Conflict(_))
	));
	assert_eq!(outcome_notices(&pool, id).await.len(), 1, "nor a second notice");

	reset_payments(&pool).await;
}

/// THE MAILBOX PIN. The identity provider changing the investor's address after the order was
/// opened means the person now behind the account may not be the one who was asked — so it
/// must not be able to consent. The address is re-read from the projection the bridge
/// maintains, by the same digest the seat froze at open, and the change arrives the way the
/// first-login upsert applies it.
#[tokio::test]
async fn a_changed_mailbox_voids_a_pending_consent() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let recipient = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let users = PgUsers::new(pool.clone());
	let tag = Uuid::new_v4();
	let subject = || domain::auth::AuthSubject::parse(&format!("payments-test-{tag}")).unwrap();
	let investor = users
		.provision(subject(), Email::parse(&format!("before-{tag}@example.test")).unwrap(), true)
		.await
		.expect("provision the investor")
		.id();
	mirror_concierge_id(&pool, investor).await;
	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), investor, "1.50");
	let id = order.id();
	payments.open(&mut order, a_session_seat(&pool, investor).await).await.expect("open the order");
	// The same investor's money, on an order an admin opened.
	let admin = an_investor(&pool).await;
	let mut admins_order = an_order(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), admin, "2.50");
	payments.open(&mut admins_order, a_session_seat(&pool, investor).await).await.expect("open the admin's order");

	users
		.provision(subject(), Email::parse(&format!("after-{tag}@example.test")).unwrap(), true)
		.await
		.expect("apply the new address");

	let refused = payments.answer_session_consent(id, investor, ConsentDecision::Approve, &audit(), now()).await;
	assert!(
		matches!(refused, Err(DomainError::Conflict(ref why)) if why.contains("mailbox")),
		"a moved mailbox voids the seat: {refused:?}"
	);
	assert_eq!(payments.find(id).await.unwrap().unwrap().order.state(), PaymentState::Rejected);
	// The mailbox the subject's copy would reach is the one that just replaced theirs — maybe
	// unverified, maybe an attacker's. Nobody but staff is told, and here there is no staff.
	assert!(
		outcome_notices(&pool, id).await.is_empty(),
		"the new mailbox is not told: {:#?}",
		outcome_notices(&pool, id).await
	);
	assert!(
		payments
			.answer_session_consent(admins_order.id(), investor, ConsentDecision::Approve, &audit(), now())
			.await
			.is_err()
	);
	assert_eq!(payments.find(admins_order.id()).await.unwrap().unwrap().order.state(), PaymentState::Rejected);
	assert_outcome_told(&pool, admins_order.id(), &[admin], "2.5", EMAIL_CHANGED).await;

	reset_payments(&pool).await;
}

// ---------------------------------------------------------------------------------------
// The use cases. Everything below drives `application::payments` end to end — the seat is
// minted, the notice is queued, the investor answers signed in, and the money is read back
// from the ledger that owns it.
// ---------------------------------------------------------------------------------------

use domain::money::WalletAddress;
use piggybank_core::{
	application::payments as payments_app,
	config::KycGate,
	infrastructure::{operations, outflow::PgOutflowPolicy, withdrawals::PgWithdrawals},
	ports::{WithdrawalRepository, ledger::Ledger},
};

const BEP20_ADDRESS: &str = "0x52908400098527886E0F7030069857D2E4169EE7";

struct App {
	pool: PgPool,
	payments: PgPayments,
	users: PgUsers,
	withdrawals: PgWithdrawals,
	outflow: PgOutflowPolicy,
	ledger: Arc<dyn Ledger>,
	relay: Relay,
	notify: Arc<Notify>,
}

async fn app(skipping: &str) -> Option<App> {
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, skipping).await?;
	let notify = Arc::new(Notify::new());
	Some(App {
		payments: PgPayments::new(pool.clone()),
		users: PgUsers::new(pool.clone()),
		withdrawals: PgWithdrawals::new(pool.clone()),
		outflow: PgOutflowPolicy::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		ledger,
		notify,
		pool,
	})
}

fn ports(a: &App) -> payments_app::PaymentPorts<'_> {
	payments_app::PaymentPorts {
		payments: &a.payments,
		users: &a.users,
		withdrawals: &a.withdrawals,
		ledger: a.ledger.as_ref(),
		custody: &StubCustody,
		policy: &a.outflow,
		relay: &a.notify,
		configured: &[Network::Bep20],
		kyc: KycGate::LIFTED,
	}
}

fn terms(from: Party, to: PaymentDestination, amount: &str) -> PaymentTerms {
	PaymentTerms::new(from, to, usdt(amount), PaymentReason::new("a documented reason").unwrap()).unwrap()
}

fn external() -> PaymentDestination {
	PaymentDestination::External {
		network: Network::Bep20,
		address: WalletAddress::parse(Network::Bep20, BEP20_ADDRESS).unwrap(),
	}
}

/// Credit a claim the way a deposit does: `Dr wallet:<net> / Cr <claim>`.
async fn fund(a: &App, claim: LedgerAccountKey, amount: &str) {
	a.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: claim,
			amount: usdt(amount).base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.expect("fund the claim");
}

/// The external tier out of an investor's claim: the withdrawal is created under the id
/// derived from the order, and a second execution finds it rather than opening another.
#[tokio::test]
async fn an_investors_external_payment_creates_one_withdrawal_under_the_derived_id() {
	let _guard = exclusive_payments().await;
	let Some(a) = app("payments external flow").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;

	let view = payments_app::open(&ports(&a), investor, terms(Party::User(investor), external(), "50"), now())
		.await
		.expect("open the order");
	let id = view.order.id();
	payments_app::answer_consent(&ports(&a), investor, id, ConsentDecision::Approve, &audit(), now())
		.await
		.expect("consent");

	let expected = payments_app::withdrawal_id(id);
	let view = payments_app::find(&a.payments, id).await.unwrap();
	assert_eq!(view.order.state(), PaymentState::Executed, "the consent executed the order inline");
	assert_eq!(view.order.executed_withdrawal_id(), Some(expected));
	let withdrawal = a.withdrawals.find_by_id(expected).await.unwrap().expect("the withdrawal exists under the derived id");
	assert_eq!(withdrawal.user(), Some(investor));
	assert_eq!(withdrawal.amount(), usdt("50"));
	assert_eq!(
		withdrawal.state(),
		domain::withdrawals::WithdrawalState::Queued,
		"a payment's withdrawal is never dispatched on creation, however liquid the rail: the dispatcher re-reads the policy"
	);
	// An L1 order relays nothing of its own: the withdrawal's `Requested` is the only money fact.
	assert!(relayed_kinds(&a.pool, id).await.is_empty());

	for _ in 0..3 {
		assert_eq!(payments_app::execute(&ports(&a), id, now()).await.unwrap().order.executed_withdrawal_id(), Some(expected));
	}
	assert_eq!(
		a.withdrawals.list_by_user(investor).await.unwrap().len(),
		1,
		"a retried execution never opens a second withdrawal"
	);

	a.relay.drain().await;
	reset_payments(&a.pool).await;
}

/// The refusals at OPEN: the treasury as a source (its spending is closed) or as a destination,
/// a source that cannot cover the amount, and an unverified investor.
#[tokio::test]
async fn an_order_that_could_never_execute_is_refused_at_open() {
	let _guard = exclusive_payments().await;
	let Some(a) = app("payments open refusals").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let recipient = an_investor(&a.pool).await;

	for to in [external(), PaymentDestination::Internal(Party::User(recipient))] {
		let err = payments_app::open(&ports(&a), investor, terms(Party::Treasury, to, "5"), now()).await.unwrap_err();
		assert!(
			matches!(err, DomainError::Precondition(ref why) if why == payments_app::TREASURY_SPENDING_CLOSED),
			"the treasury pays nothing out until scope approvals decide its spending: {err:?}"
		);
	}
	let into_treasury = PaymentTerms::new(
		Party::User(investor),
		PaymentDestination::Internal(Party::Treasury),
		usdt("5"),
		PaymentReason::new("a documented reason").unwrap(),
	);
	assert!(
		matches!(into_treasury, Err(DomainError::Validation(_))),
		"an investor's money does not become banking's by payment: {into_treasury:?}"
	);

	// The investor's claim is empty: refused now rather than after 72 hours of consent.
	let err = payments_app::open(
		&ports(&a),
		investor,
		terms(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), "1"),
		now(),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "an uncovered source is refused: {err:?}");

	// An unverified investor cannot route around the floor by paying a verified one who then
	// withdraws: the internal hop is refused for the same reason the external one is.
	fund(&a, LedgerAccountKey::UserClaim(investor), "10").await;
	common::set_kyc_level(&a.pool, investor, 0).await;
	let enforced = payments_app::PaymentPorts {
		kyc: KycGate::ENFORCED,
		..ports(&a)
	};
	let verified = an_investor(&a.pool).await;
	let err = payments_app::open(&enforced, investor, terms(Party::User(investor), PaymentDestination::Internal(Party::User(verified)), "1"), now())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(ref why) if why.contains("verification")), "{err:?}");
	let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments").fetch_one(&a.pool).await.unwrap();
	assert_eq!(stored, 0, "a refused open writes nothing");
	common::set_kyc_level(&a.pool, investor, 1).await;
	payments_app::open(&enforced, investor, terms(Party::User(investor), PaymentDestination::Internal(Party::User(verified)), "1"), now())
		.await
		.expect("verified, the same order opens");

	a.relay.drain().await;
	reset_payments(&a.pool).await;
}

/// The read-only kill-switch holds an APPROVED order the way it holds a queued withdrawal:
/// the attempt is an error, not a recorded failure, so the order is still `approved` when
/// the operator lifts the pause and the sweeper comes back to it.
#[tokio::test]
async fn an_operator_pause_holds_an_approved_order_without_closing_it() {
	let _guard = exclusive_payments().await;
	let Some(a) = app("payments read-only pause").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let recipient = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;

	let id = payments_app::open(
		&ports(&a),
		investor,
		terms(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), "20"),
		now(),
	)
	.await
	.unwrap()
	.order
	.id();
	// The answer through the PORT, so nothing executes inline.
	a.payments.answer_session_consent(id, investor, ConsentDecision::Approve, &audit(), now()).await.unwrap();
	a.relay.drain().await;

	operations::set_read_only(&a.pool, true).await.unwrap();
	let held = payments_app::execute(&ports(&a), id, now()).await;
	let report = payments_app::sweep(&ports(&a), now()).await.unwrap();
	operations::set_read_only(&a.pool, false).await.unwrap();

	assert!(matches!(held, Err(DomainError::Precondition(ref why)) if why.contains("paused")), "{held:?}");
	assert_eq!((report.executed, report.execution_failures), (0, 0), "a pause records nothing");
	assert_eq!(
		payments_app::find(&a.payments, id).await.unwrap().order.state(),
		PaymentState::Approved,
		"still waiting, not failed"
	);
	assert_eq!(a.payments.awaiting_execution().await.unwrap(), vec![id], "the sweeper will come back to it");

	let view = payments_app::execute(&ports(&a), id, now()).await.expect("executes once the pause is lifted");
	assert_eq!(view.order.state(), PaymentState::Executed);

	a.relay.drain().await;
	reset_payments(&a.pool).await;
}

/// A freeze on the subject holds an APPROVED L1 order the same way the pause does. A
/// concierge SUSPENDED is reversible (REINSTATED lifts it) and can land anywhere in the
/// 72-hour window after the consent; admission now refuses it, and that refusal must keep
/// the order `approved` for the next sweep — not close it for good and tell both parties of
/// a refusal over a control the operator meant as a hold.
#[tokio::test]
async fn a_frozen_subject_holds_an_approved_order_without_closing_it() {
	let _guard = exclusive_payments().await;
	let Some(a) = app("payments frozen subject").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;

	let id = payments_app::open(&ports(&a), investor, terms(Party::User(investor), external(), "50"), now())
		.await
		.unwrap()
		.order
		.id();
	// The answer through the PORT, so nothing executes inline and the freeze lands inside the
	// window.
	a.payments.answer_session_consent(id, investor, ConsentDecision::Approve, &audit(), now()).await.unwrap();
	a.relay.drain().await;

	sqlx::query("UPDATE users SET frozen = TRUE WHERE id = $1").bind(investor.raw()).execute(&a.pool).await.unwrap();
	let held = payments_app::execute(&ports(&a), id, now()).await;
	let report = payments_app::sweep(&ports(&a), now()).await.unwrap();
	sqlx::query("UPDATE users SET frozen = FALSE WHERE id = $1").bind(investor.raw()).execute(&a.pool).await.unwrap();

	assert!(matches!(held, Err(DomainError::Precondition(ref why)) if why.contains("frozen")), "{held:?}");
	assert_eq!((report.executed, report.execution_failures), (0, 0), "a freeze records nothing");
	assert_eq!(
		payments_app::find(&a.payments, id).await.unwrap().order.state(),
		PaymentState::Approved,
		"still waiting, not failed"
	);
	assert_eq!(a.payments.awaiting_execution().await.unwrap(), vec![id], "the sweeper will come back to it");
	assert!(
		a.withdrawals.find_by_id(payments_app::withdrawal_id(id)).await.unwrap().is_none(),
		"nothing was queued under the freeze"
	);

	let view = payments_app::execute(&ports(&a), id, now()).await.expect("executes once the subject is reinstated");
	assert_eq!(view.order.state(), PaymentState::Executed);
	assert_eq!(view.order.executed_withdrawal_id(), Some(payments_app::withdrawal_id(id)));

	a.relay.drain().await;
	reset_payments(&a.pool).await;
}

/// THE L1 WINDOW. The execution path checks the pins, then creates the withdrawal, then
/// records the effect — and a revocation can land between the first and the last. The
/// record refuses under the order's lock and, in that same transaction, voids the
/// still-queued withdrawal, so a revoked consent's money never ships.
///
/// On a LIQUID rail on purpose: a self-service withdrawal here would dispatch on creation
/// and be past voiding. A payment's withdrawal is always left `Queued`, which is the whole
/// reason the void below is possible.
#[tokio::test]
async fn a_revocation_inside_the_execution_window_cancels_the_queued_withdrawal() {
	use domain::withdrawals::WithdrawalState;

	let _guard = exclusive_payments().await;
	let Some(a) = app("payments L1 window").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = an_approved_l1_order(&a, investor, investor, "50").await;

	// Stand in for the window: the pins have been read and the withdrawal created, the way
	// `execute` creates it...
	let withdrawal = queue_payment_withdrawal(&a, id, investor, "50").await;
	assert_eq!(
		a.withdrawals.find_by_id(withdrawal).await.unwrap().unwrap().state(),
		WithdrawalState::Queued,
		"queued even though the rail could cover it"
	);
	// ...and the revocation lands before the effect is recorded.
	a.users.revoke_tokens(investor).await.unwrap();

	let refused = a.payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Withdrawal(withdrawal)), now()).await;
	assert!(matches!(refused, Err(DomainError::Conflict(_))), "{refused:?}");
	assert_eq!(payments_app::find(&a.payments, id).await.unwrap().order.state(), PaymentState::ExecutionFailed);
	assert_eq!(
		a.withdrawals.find_by_id(withdrawal).await.unwrap().unwrap().state(),
		WithdrawalState::Cancelled,
		"the withdrawal was voided in the same transaction, so it can never be dispatched"
	);
	assert!(
		a.withdrawals.list_actionable().await.unwrap().iter().all(|queued| queued.id != withdrawal),
		"nothing left for the dispatcher"
	);

	a.relay.drain().await;
	reset_payments(&a.pool).await;
}

/// The expiry half of the sweep: the order closes, its undelivered consent notice goes with it,
/// and an answer arriving afterwards finds nothing.
#[tokio::test]
async fn the_sweep_expires_what_nobody_consented_to() {
	let _guard = exclusive_payments().await;
	let Some(a) = app("payments expiry sweep").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let recipient = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "10").await;
	let id = payments_app::open(
		&ports(&a),
		investor,
		terms(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), "1"),
		now(),
	)
	.await
	.unwrap()
	.order
	.id();

	let report = payments_app::sweep(&ports(&a), now() + domain::payments::TTL_SECS + 1).await.unwrap();
	assert_eq!(report.expired, 1);
	assert_eq!(payments_app::find(&a.payments, id).await.unwrap().order.state(), PaymentState::Expired);
	assert_eq!(consent_notice(&a.pool, id).await, (false, true), "the undelivered notice went with the order");
	assert!(
		matches!(
			payments_app::answer_consent(&ports(&a), investor, id, ConsentDecision::Approve, &audit(), now()).await,
			Err(DomainError::NotFound { .. })
		),
		"an expired order answers like an unknown one"
	);

	reset_payments(&a.pool).await;
}

/// Queue the order's own L1 withdrawal under its derived id, as `execute` would have before
/// it failed to record the effect.
async fn queue_payment_withdrawal(a: &App, id: PaymentId, investor: UserId, amount: &str) -> WithdrawalId {
	let withdrawal = payments_app::withdrawal_id(id);
	piggybank_core::application::withdrawals::queue_withdrawal(
		&piggybank_core::application::withdrawals::WithdrawalPorts {
			withdrawals: &a.withdrawals,
			ledger: a.ledger.as_ref(),
			custody: &StubCustody,
			relay: &a.notify,
		},
		&piggybank_core::application::withdrawals::AdmissionGates {
			policy: &a.outflow,
			configured: &[Network::Bep20],
			kyc: KycGate::LIFTED,
		},
		withdrawal,
		investor,
		Network::Bep20,
		WalletAddress::parse(Network::Bep20, BEP20_ADDRESS).unwrap(),
		usdt(amount),
	)
	.await
	.unwrap();
	withdrawal
}

/// An approved L1 order, answered through the port so nothing executed inline.
async fn an_approved_l1_order(a: &App, investor: UserId, initiator: UserId, amount: &str) -> PaymentId {
	let id = payments_app::open(&ports(a), initiator, terms(Party::User(investor), external(), amount), now())
		.await
		.unwrap()
		.order
		.id();
	a.payments.answer_session_consent(id, investor, ConsentDecision::Approve, &audit(), now()).await.unwrap();
	id
}

/// THE CRASHED EXECUTION. `execute` created the order's withdrawal `Queued` and died before
/// recording the effect; the investor's sessions are then revoked; the sweep comes back,
/// reads the void BEFORE looking for a withdrawal and fails the order. That failure must
/// void the withdrawal already there, exactly as the L1 window does — a notice saying no
/// money moved, over a withdrawal the dispatcher then sends, is the worst of both.
#[tokio::test]
async fn a_consent_void_found_at_execution_cancels_the_withdrawal_a_crashed_attempt_left_queued() {
	use domain::withdrawals::WithdrawalState;

	let _guard = exclusive_payments().await;
	let Some(a) = app("payments consent void over a queued withdrawal").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let admin = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = an_approved_l1_order(&a, investor, admin, "40").await;
	let withdrawal = queue_payment_withdrawal(&a, id, investor, "40").await;
	a.users.revoke_tokens(investor).await.unwrap();

	let view = payments_app::execute(&ports(&a), id, now()).await.expect("the void is recorded on the order, not raised");
	assert_eq!(view.order.state(), PaymentState::ExecutionFailed);
	assert_eq!(
		a.withdrawals.find_by_id(withdrawal).await.unwrap().unwrap().state(),
		WithdrawalState::Cancelled,
		"the queued withdrawal is voided with the order"
	);
	assert!(a.withdrawals.list_actionable().await.unwrap().iter().all(|queued| queued.id != withdrawal));
	assert_outcome_told(&a.pool, id, &[investor, admin], "40", SESSIONS_REVOKED).await;

	a.relay.drain().await;
	reset_payments(&a.pool).await;
}

/// The L1 window with the notice queue refusing: the void of the withdrawal and the order's
/// failure still commit. The notice is the report of the defence, never a condition of it.
#[tokio::test]
async fn a_broken_notice_queue_does_not_undo_the_void_in_the_execution_window() {
	use domain::withdrawals::WithdrawalState;

	let _guard = exclusive_payments().await;
	let Some(a) = app("payments L1 window with the notice queue refusing").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = an_approved_l1_order(&a, investor, investor, "30").await;
	let withdrawal = queue_payment_withdrawal(&a, id, investor, "30").await;
	a.users.revoke_tokens(investor).await.unwrap();

	refuse_outcome_notices(&a.pool, true).await;
	let refused = a.payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Withdrawal(withdrawal)), now()).await;
	refuse_outcome_notices(&a.pool, false).await;

	assert!(matches!(refused, Err(DomainError::Conflict(_))), "{refused:?}");
	assert_eq!(payments_app::find(&a.payments, id).await.unwrap().order.state(), PaymentState::ExecutionFailed);
	assert_eq!(a.withdrawals.find_by_id(withdrawal).await.unwrap().unwrap().state(), WithdrawalState::Cancelled);
	assert!(outcome_notices(&a.pool, id).await.is_empty());

	a.relay.drain().await;
	reset_payments(&a.pool).await;
}

/// THE LOSING CREATOR. `execute` has two callers and creates the withdrawal outside the
/// order's lock: A reads a valid consent and goes to create it; the sessions are revoked; B
/// reads the void, finds no withdrawal yet and fails the order; only then does A insert the
/// withdrawal `Queued` and try to record it. A's record is refused — the order is closed —
/// but the withdrawal it created must not be left for the dispatcher to pay: the refusal
/// voids it in the same transaction.
#[tokio::test]
async fn a_withdrawal_created_after_the_order_was_voided_is_cancelled_when_its_record_is_refused() {
	use domain::withdrawals::WithdrawalState;

	let _guard = exclusive_payments().await;
	let Some(a) = app("payments late withdrawal after a void").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let admin = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = an_approved_l1_order(&a, investor, admin, "25").await;
	a.users.revoke_tokens(investor).await.unwrap();

	// B: the void, with nothing created yet.
	let cause = ConsentInvalidation::SessionsRevoked { at_open: 0, now: 1 };
	let view = a
		.payments
		.record_execution(
			id,
			ExecutionOutcome::ConsentVoid {
				cause,
				withdrawal: Some(payments_app::withdrawal_id(id)),
			},
			now(),
		)
		.await
		.expect("the void is recorded");
	assert_eq!(view.order.state(), PaymentState::ExecutionFailed);

	// A: the withdrawal lands after the void, and its record comes in.
	let withdrawal = queue_payment_withdrawal(&a, id, investor, "25").await;
	let refused = a.payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Withdrawal(withdrawal)), now()).await;
	assert!(matches!(refused, Err(DomainError::Conflict(_))), "{refused:?}");

	assert_eq!(
		a.withdrawals.find_by_id(withdrawal).await.unwrap().unwrap().state(),
		WithdrawalState::Cancelled,
		"the late withdrawal is voided, not paid"
	);
	assert!(a.withdrawals.list_actionable().await.unwrap().iter().all(|queued| queued.id != withdrawal));
	assert_eq!(payments_app::find(&a.payments, id).await.unwrap().order.state(), PaymentState::ExecutionFailed);
	assert_outcome_told(&a.pool, id, &[investor, admin], "25", SESSIONS_REVOKED).await;

	a.relay.drain().await;
	reset_payments(&a.pool).await;
}
