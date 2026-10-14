//! Integration tests for the payments plane — real Postgres, no mocks (project rule).
//! They run when `DATABASE_URL` is set and skip otherwise.
//!
//! Two halves. The first asserts what only the DATABASE states — the partial unique index
//! over fund-owned sources, its deliberate exemption for an investor's own claim, and the
//! composite foreign key that makes a consent seat belong to its payment's source user; none
//! of these has a Rust code path, which is the point of putting them in the schema. The
//! second drives [`PgPayments`] itself, so that every runtime `sqlx::query` in the adapter is
//! executed by a test — the check the compile-time macros would have given, bought back.
//!
//! `payments` carries a database-wide invariant ("one open order per fund-owned source"), so
//! every test here takes [`exclusive_payments`] and starts from [`reset_payments`]. The reset
//! runs at the START of each test so a panicking one cannot wedge the rest.

use std::sync::{
	Arc,
	atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use domain::{
	balance::{LedgerAccountKey, Party, ServiceId, TransferCode},
	consilium::ConsiliumId,
	error::DomainError,
	money::{Network, Usdt},
	payments::{PaymentDestination, PaymentEffect, PaymentId, PaymentOrder, PaymentReason, PaymentState, PaymentTerms},
	users::{Email, UserId},
	withdrawals::WithdrawalId,
};
use piggybank_core::{
	infrastructure::{consilium_mailer::ConsiliumMailer, custody::StubCustody, payments::PgPayments, relay::Relay, users::PgUsers},
	ports::{
		LedgerTransfer, UserRepository,
		governance_mail::{GovernanceMail, GovernanceMailer, MailDeliveryError},
		payments::{ApprovalSeat, ConsentAudit, ConsentCredential, ConsentDecision, ConsentInvalidation, ExecutionOutcome, MAX_CODE_ATTEMPTS, PaymentFeed, PaymentFilter, PaymentRepository},
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
	refuse_outcome_mail(pool, false).await;
	for statement in [
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
async fn refuse_outcome_mail(pool: &PgPool, refuse: bool) {
	sqlx::query("ALTER TABLE consilium_mail DROP CONSTRAINT IF EXISTS test_refuses_payment_outcome")
		.execute(pool)
		.await
		.expect("drop the fault");
	if refuse {
		sqlx::query("ALTER TABLE consilium_mail ADD CONSTRAINT test_refuses_payment_outcome CHECK (kind <> 'payment_outcome') NOT VALID")
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

/// The identity-plane id the bridge mirrors onto a row. A consent mail is addressed by it, so
/// an investor without one cannot be asked for anything — `open` refuses, as pinned below.
async fn mirror_concierge_id(pool: &PgPool, id: UserId) {
	sqlx::query("UPDATE users SET concierge_user_id = $2 WHERE id = $1")
		.bind(id.raw())
		.bind(Uuid::new_v4())
		.execute(pool)
		.await
		.expect("mirror the concierge id");
}

const CONSENT_URL_BASE: &str = "https://example.test/consent";

/// Insert one order directly. These tests are about what the SCHEMA refuses, so the rows go
/// in the way a buggy adapter would write them, with nothing in between to launder a violation.
async fn insert_payment(pool: &PgPool, id: Uuid, state: &str, from_kind: &str, from_id: Option<&str>, to_kind: &str, initiator: UserId) -> Result<(), sqlx::Error> {
	sqlx::query(
		"INSERT INTO payments (id, state, from_kind, from_id, to_kind, amount, reason, payload_hash, initiator_user_id, expires_at, decided_at) \
		 VALUES ($1, $2, $3, $4, $5, '1000', 'a test order', $6, $7, now() + interval '72 hours', CASE WHEN $2 = 'pending' THEN NULL ELSE now() END)",
	)
	.bind(id)
	.bind(state)
	.bind(from_kind)
	.bind(from_id)
	.bind(to_kind)
	.bind(vec![0u8; 32])
	.bind(initiator.raw())
	.execute(pool)
	.await
	.map(|_| ())
}

/// The whole of the concurrent-approval overdraw defence for the fund's own claims: a race
/// that cannot be created does not have to be won. The platform's own money is the `fund`
/// and `fee` allocations (#245), so the keyed source here is `service:fund`; the retired
/// id-less kinds (`piggybank`, `revenue`) are held by the same index through its
/// `NULLS NOT DISTINCT` until the contract migration drops them.
#[tokio::test]
async fn only_one_order_may_be_open_against_a_fund_owned_claim() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments schema tests");
		return;
	};
	reset_payments(&pool).await;
	let initiator = an_investor(&pool).await;

	insert_payment(&pool, Uuid::new_v4(), "pending", "service", Some("fund"), "service", initiator)
		.await
		.expect("the first order against the fund's capital is accepted");

	let second = insert_payment(&pool, Uuid::new_v4(), "pending", "service", Some("fund"), "service", initiator).await;
	assert!(second.is_err(), "a second OPEN order against the same fund-owned claim must be refused");

	// `approved` still holds the source (the reservation is against it), so it counts as open
	// too — and a terminal order releases it.
	sqlx::query("UPDATE payments SET state = 'cancelled', decided_at = now()")
		.execute(&pool)
		.await
		.expect("close the first order");
	insert_payment(&pool, Uuid::new_v4(), "approved", "service", Some("fund"), "service", initiator)
		.await
		.expect("a closed order releases its source claim");
	let third = insert_payment(&pool, Uuid::new_v4(), "pending", "service", Some("fund"), "service", initiator).await;
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
		insert_payment(&pool, Uuid::new_v4(), "pending", "user", Some(&source), "service", investor)
			.await
			.expect("an investor's own claim is deliberately outside the single-open index");
	}

	reset_payments(&pool).await;
}

/// §3's rule — "a payment whose source is `User(u)` requires `u`'s own consent" — made
/// unrepresentable otherwise. `payments.source_user_id` is GENERATED by the database from
/// `from_kind`/`from_id`, so the composite FK cannot be satisfied by a seat naming anyone
/// else, and a fund-owned order (whose generated column is NULL) can have no seat at all.
#[tokio::test]
async fn a_consent_seat_can_only_name_its_payments_own_source_user() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments schema tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let stranger = an_investor(&pool).await;
	let own = Uuid::new_v4();
	let fund_owned = Uuid::new_v4();
	insert_payment(&pool, own, "pending", "user", Some(&investor.to_string()), "service", investor)
		.await
		.expect("open the investor's order");
	insert_payment(&pool, fund_owned, "pending", "service", Some("fee"), "service", investor)
		.await
		.expect("open a fund-owned order");

	assert!(seat(&pool, own, stranger).await.is_err(), "a seat naming another user must be refused");
	assert!(seat(&pool, fund_owned, investor).await.is_err(), "a fund-owned order has no consent subject to seat");
	seat(&pool, own, investor).await.expect("the source user's own seat is accepted");

	reset_payments(&pool).await;
}

/// The mirror image of the test above, for the other requirement. `payments.fund_owned` is
/// GENERATED from `from_kind`, and `payment_approval` carries a composite FK over
/// `(payment_id, fund_owned)` with the second key pinned to TRUE — so an owner quorum cannot
/// be seated over an investor's order by any code path, and a fund-owned order takes one.
#[tokio::test]
async fn a_quorum_seat_can_only_attach_to_a_fund_owned_order() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments schema tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let own = Uuid::new_v4();
	let fund_owned = Uuid::new_v4();
	insert_payment(&pool, own, "pending", "user", Some(&investor.to_string()), "service", investor)
		.await
		.expect("open the investor's order");
	insert_payment(&pool, fund_owned, "pending", "service", Some("fee"), "service", investor)
		.await
		.expect("open a fund-owned order");

	let consilium = a_decided_consilium(&pool, investor).await;
	assert!(
		quorum_seat(&pool, own, consilium).await.is_err(),
		"an owner quorum must not be seated over an investor's own money"
	);
	quorum_seat(&pool, fund_owned, consilium).await.expect("the fund's order takes its quorum seat");

	reset_payments(&pool).await;
	sqlx::query("DELETE FROM consilium WHERE id = $1").bind(consilium.raw()).execute(&pool).await.ok();
}

async fn quorum_seat(pool: &PgPool, payment: Uuid, consilium: ConsiliumId) -> Result<(), sqlx::Error> {
	sqlx::query("INSERT INTO payment_approval (payment_id, consilium_id) VALUES ($1, $2)")
		.bind(payment)
		.bind(consilium.raw())
		.execute(pool)
		.await
		.map(|_| ())
}

async fn seat(pool: &PgPool, payment: Uuid, subject: UserId) -> Result<(), sqlx::Error> {
	sqlx::query(
		"INSERT INTO payment_consent (payment_id, subject_user_id, token_hash, code_hash, expires_at, subject_token_version_at_open, subject_email_hash_at_open) \
		 VALUES ($1, $2, $3, $4, now() + interval '72 hours', 0, $5)",
	)
	.bind(payment)
	.bind(subject.raw())
	.bind(Uuid::new_v4().as_bytes().repeat(2))
	.bind(vec![2u8; 32])
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

/// The digest the adapter compares a submitted code against.
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

/// An order and the seat credential whose plaintext code the test still holds.
fn an_order(from: Party, to: PaymentDestination, initiator: UserId, amount: &str) -> PaymentOrder {
	let terms = PaymentTerms::new(from, to, usdt(amount), PaymentReason::new("a documented reason").unwrap()).unwrap();
	// The application layer takes this hash; the tests only need one that is stable per order.
	let payload_hash = digest(&terms.canonical_bytes());
	PaymentOrder::open(PaymentId::from_raw(Uuid::new_v4()), terms, payload_hash, initiator, now())
}

const CODE: &str = "428913";

/// A seat pinned to the subject AS THE PROJECTION HOLDS THEM NOW — the folded revoke floor
/// and the mirrored address — exactly what the application layer will freeze at open.
async fn a_consent_seat(pool: &PgPool, subject: UserId) -> ApprovalSeat {
	let (token_version, email): (i64, String) = sqlx::query_as("SELECT GREATEST(concierge_token_version, token_version), email FROM users WHERE id = $1")
		.bind(subject.raw())
		.fetch_one(pool)
		.await
		.expect("the subject is mirrored");
	let token = format!("token-{subject}-{}", Uuid::new_v4());
	ApprovalSeat::Consent(ConsentCredential {
		subject,
		token_hash: digest(token.as_bytes()),
		code_hash: digest(CODE.as_bytes()),
		token,
		code: CODE.to_owned(),
		token_version_at_open: token_version as u64,
		email_hash_at_open: digest(email.as_bytes()),
	})
}

fn token_hash_of(seat: &ApprovalSeat) -> [u8; 32] {
	match seat {
		ApprovalSeat::Consent(credential) => credential.token_hash,
		ApprovalSeat::Consilium(_) => unreachable!("this helper mints consent seats"),
	}
}

fn token_of(seat: &ApprovalSeat) -> &str {
	match seat {
		ApprovalSeat::Consent(credential) => &credential.token,
		ApprovalSeat::Consilium(_) => unreachable!("this helper mints consent seats"),
	}
}

/// A consilium row in a TERMINAL state, standing in for the governance record a fund-owned
/// order links to. Terminal on purpose: `consilium_single_open_per_source_idx` covers only
/// the OPEN rows, so this fixture cannot block the governance suite that shares the table.
async fn a_decided_consilium(pool: &PgPool, initiator: UserId) -> ConsiliumId {
	let id = Uuid::new_v4();
	sqlx::query(
		"INSERT INTO consilium (id, kind, state, terms, source_claim, payload_hash, initiator_user_id, owner_count, threshold, expires_at, decided_at) \
		 VALUES ($1, 'valuation_override', 'approved', $2::jsonb, 'service:fixture', $3, $4, 3, 2, now() + interval '72 hours', now())",
	)
	.bind(id)
	.bind(r#"{"service":"fixture","aum":"1"}"#)
	.bind(vec![9u8; 32])
	.bind(initiator.raw())
	.execute(pool)
	.await
	.expect("insert the consilium fixture");
	ConsiliumId::from_raw(id)
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
struct OutcomeMail {
	recipient: Uuid,
	dedupe_key: String,
	payload: serde_json::Value,
}

/// Every consent-outcome notice queued about this order, in recipient order.
async fn outcome_mails(pool: &PgPool, order: PaymentId) -> Vec<OutcomeMail> {
	let rows: Vec<(Uuid, String, String)> =
		sqlx::query_as("SELECT user_id, dedupe_key, payload::text FROM consilium_mail WHERE payment_id = $1 AND kind = 'payment_outcome' ORDER BY user_id")
			.bind(order.raw())
			.fetch_all(pool)
			.await
			.expect("read the outcome mails");
	rows.into_iter()
		.map(|(recipient, dedupe_key, payload)| OutcomeMail {
			recipient,
			dedupe_key,
			payload: serde_json::from_str(&payload).unwrap(),
		})
		.collect()
}

async fn concierge_id_of(pool: &PgPool, user: UserId) -> Uuid {
	sqlx::query_scalar("SELECT concierge_user_id FROM users WHERE id = $1")
		.bind(user.raw())
		.fetch_one(pool)
		.await
		.expect("the user is mirrored")
}

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
fn the_outcome_amount_is_the_consent_decimal_spelled_as_money() {
	use piggybank_core::infrastructure::payments::mail_amount;

	assert_eq!(mail_amount(usdt("1200")), "1200 USDT");
	assert_eq!(mail_amount(usdt("12.50")), "12.5 USDT");
	assert_eq!(mail_amount(usdt("0.000000000000000001")), "0.000000000000000001 USDT");
	for raw in ["1200", "12.50", "0.000000000000000001", "1234567890123.123456789012345678"] {
		assert!(is_money(&mail_amount(usdt(raw))), "{raw} → {}", mail_amount(usdt(raw)));
	}
	let max = mail_amount(Usdt::from_base_units(u128::MAX));
	assert!(is_money(&max), "{max}");
	assert!(max.starts_with(&(u128::MAX / 10u128.pow(18)).to_string()), "{max}");
}

/// Assert the order's outcome notices: one per person in `recipients` (subject first), each
/// under its own key, all naming the SUBJECT in the identity plane and the given pair.
async fn assert_outcome_told(pool: &PgPool, order: PaymentId, subject: UserId, recipients: &[UserId], outcome: &str, reason: &str) -> Vec<OutcomeMail> {
	let mails = outcome_mails(pool, order).await;
	let mut expected: Vec<Uuid> = recipients.iter().map(|user| user.raw()).collect();
	expected.sort();
	assert_eq!(mails.iter().map(|mail| mail.recipient).collect::<Vec<_>>(), expected, "one copy per recipient: {mails:#?}");
	let subject_concierge = concierge_id_of(pool, subject).await.to_string();
	for mail in &mails {
		assert_eq!(mail.dedupe_key, format!("payment:{order}:outcome:{}", mail.recipient), "a key per copy");
		assert_eq!(mail.payload["kind"], "payment_outcome");
		assert_eq!(mail.payload["outcome"], outcome);
		assert_eq!(mail.payload["reason"], reason);
		assert_eq!(mail.payload["subject_user_id"], subject_concierge.as_str(), "every copy names the subject");
		assert_eq!(mail.payload["payment_id"], order.to_string());
		let amount = mail.payload["amount"].as_str().unwrap();
		assert!(is_money(amount), "concierge would refuse the amount {amount:?}");
		assert!(mail.payload.get("approval_url").is_none() && mail.payload.get("code").is_none(), "a notice carries no credential");
	}
	mails
}

/// Opening an order writes it, its seat and its events in one transaction — and puts NOTHING
/// in the outbox, because an order that has not been approved has moved no money.
#[tokio::test]
async fn opening_an_order_materializes_its_consent_seat_and_relays_nothing() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "12.50");
	let id = order.id();
	payments.open(&mut order, a_consent_seat(&pool, investor).await, CONSENT_URL_BASE).await.expect("open the order");

	let view = payments.find(id).await.expect("find the order").expect("the order exists");
	assert_eq!(view.order.state(), PaymentState::Pending);
	assert_eq!(view.order.terms().amount(), usdt("12.50"));
	assert!(view.consilium_id.is_none(), "an investor-sourced order is not decided by a quorum");
	let consent = view.consent.expect("the seat was materialized with the order");
	assert_eq!(consent.subject, investor);
	assert_eq!(consent.decision, ConsentDecision::Pending);
	assert_eq!(consent.attempts_remaining, MAX_CODE_ATTEMPTS as u32);
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
	let payments = PgPayments::new(pool.clone());
	let consilium = a_decided_consilium(&pool, investor).await;

	// An owner quorum over an investor's own money.
	let mut own = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "1.00");
	let refused = payments.open(&mut own, ApprovalSeat::Consilium(consilium), CONSENT_URL_BASE).await;
	assert!(matches!(refused, Err(DomainError::Validation(_))), "a quorum seat over an investor's order: {refused:?}");
	assert!(payments.find(own.id()).await.unwrap().is_none(), "a refused open writes nothing");

	// One investor's consent over the fund's money.
	let mut fund = an_order(
		Party::Service(ServiceId::fee()),
		PaymentDestination::Internal(Party::Service(ServiceId::fund())),
		investor,
		"1.00",
	);
	let refused = payments.open(&mut fund, a_consent_seat(&pool, investor).await, CONSENT_URL_BASE).await;
	assert!(matches!(refused, Err(DomainError::Validation(_))), "a consent seat over a fund-owned order: {refused:?}");
	assert!(payments.find(fund.id()).await.unwrap().is_none());

	// The right kind of seat, naming the wrong investor.
	let mut own = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "1.00");
	let refused = payments.open(&mut own, a_consent_seat(&pool, stranger).await, CONSENT_URL_BASE).await;
	assert!(matches!(refused, Err(DomainError::Validation(_))), "a consent seat naming a stranger: {refused:?}");
	assert!(payments.find(own.id()).await.unwrap().is_none());

	sqlx::query("DELETE FROM consilium WHERE id = $1").bind(consilium.raw()).execute(&pool).await.ok();
	reset_payments(&pool).await;
}

/// The five-attempt ceiling, and what happens at it. A burned consent FAILS THE PAYMENT
/// CLOSED — with one seat there is nobody to escalate to, so the exhausted token is a refusal
/// rather than a detector.
#[tokio::test]
async fn five_wrong_codes_burn_the_consent_and_close_the_order() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let seat = a_consent_seat(&pool, investor).await;
	let token = token_hash_of(&seat);

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "1.00");
	let id = order.id();
	payments.open(&mut order, seat, CONSENT_URL_BASE).await.expect("open the order");

	// Reading the invitation costs no attempt — mail scanners fetch every URL in a message.
	let invitation = payments.invitation(&token, now()).await.expect("the token resolves");
	assert_eq!(invitation.payment_id, id);
	assert_eq!(invitation.attempts_remaining, MAX_CODE_ATTEMPTS as u32);

	for remaining in (1..MAX_CODE_ATTEMPTS).rev() {
		let err = payments
			.submit(&token, "000000", ConsentDecision::Approve, &audit(), now())
			.await
			.expect_err("a wrong code is refused");
		assert!(
			matches!(err, DomainError::Validation(ref message) if message.contains(&format!("{remaining} attempts"))),
			"unexpected refusal: {err:?}"
		);
	}
	let burned = payments
		.submit(&token, "000000", ConsentDecision::Approve, &audit(), now())
		.await
		.expect_err("the fifth wrong code burns");
	assert!(matches!(burned, DomainError::NotFound { .. }), "a burned token answers exactly like an unknown one: {burned:?}");

	let view = payments.find(id).await.unwrap().unwrap();
	assert_eq!(view.order.state(), PaymentState::Rejected, "the burn fails the payment closed");
	assert!(relayed_kinds(&pool, id).await.is_empty(), "a refused order reserves nothing");
	// And the token is now indistinguishable from an unknown one on the read surface too.
	assert!(payments.invitation(&token, now()).await.is_err());
	// The investor opened this order on their own money, so they are told once, not twice.
	assert_outcome_told(&pool, id, investor, &[investor], "TOKEN_BURNED", "WRONG_CODES").await;

	reset_payments(&pool).await;
}

/// A burned consent over an order an ADMIN opened tells both of them (#238): the investor
/// whose money it was — someone was guessing at their code — and the staff member who opened
/// the order, who holds no seat and would otherwise find it rejected without a word. Each
/// copy has its own key, because concierge refuses one key for two recipients.
#[tokio::test]
async fn a_burned_consent_tells_the_subject_and_the_initiator_once_each() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let admin = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let seat = a_consent_seat(&pool, investor).await;
	let token = token_hash_of(&seat);

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), admin, "1200.50");
	let id = order.id();
	payments.open(&mut order, seat, CONSENT_URL_BASE).await.expect("open the order");
	assert!(outcome_mails(&pool, id).await.is_empty(), "nothing to tell while the consent stands");

	for _ in 0..MAX_CODE_ATTEMPTS {
		payments
			.submit(&token, "000000", ConsentDecision::Approve, &audit(), now())
			.await
			.expect_err("a wrong code is refused");
	}
	assert_eq!(payments.find(id).await.unwrap().unwrap().order.state(), PaymentState::Rejected);
	let mails = assert_outcome_told(&pool, id, investor, &[investor, admin], "TOKEN_BURNED", "WRONG_CODES").await;
	let payload = &mails[0].payload;
	assert_eq!(payload["amount"], "1200.5 USDT", "the consent's decimal, spelled as money");
	assert_eq!(payload["tier"], "service", "a payment into a product");
	assert_eq!(mails[0].payload, mails[1].payload, "both audiences get the same facts; concierge picks the copy");

	// A burned token answers like an unknown one, so retries change nothing.
	for _ in 0..2 {
		assert!(matches!(
			payments.submit(&token, CODE, ConsentDecision::Approve, &audit(), now()).await,
			Err(DomainError::NotFound { .. })
		));
	}
	assert_eq!(outcome_mails(&pool, id).await.len(), 2, "no duplicate notices");

	reset_payments(&pool).await;
}

/// A burn is the defence, the mail only its report: with the queue refusing the notice, the
/// fifth wrong code still burns the token and rejects the order. Were the two one unit, a
/// broken mailer would roll the burn back — and the attempt with it — handing a guesser
/// unlimited fifth tries.
#[tokio::test]
async fn a_broken_mail_queue_does_not_undo_the_burn() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let admin = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let seat = a_consent_seat(&pool, investor).await;
	let token = token_hash_of(&seat);
	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), admin, "7.00");
	let id = order.id();
	payments.open(&mut order, seat, CONSENT_URL_BASE).await.expect("open the order");

	refuse_outcome_mail(&pool, true).await;
	for _ in 0..MAX_CODE_ATTEMPTS {
		payments
			.submit(&token, "000000", ConsentDecision::Approve, &audit(), now())
			.await
			.expect_err("a wrong code is refused");
	}
	refuse_outcome_mail(&pool, false).await;

	let (attempts, burned): (i32, bool) = sqlx::query_as("SELECT attempts, burned_at IS NOT NULL FROM payment_consent WHERE payment_id = $1")
		.bind(id.raw())
		.fetch_one(&pool)
		.await
		.unwrap();
	assert_eq!((attempts, burned), (MAX_CODE_ATTEMPTS, true), "the burn committed");
	assert_eq!(payments.find(id).await.unwrap().unwrap().order.state(), PaymentState::Rejected);
	assert!(outcome_mails(&pool, id).await.is_empty(), "the refused notice left nothing half-written");
	assert!(
		matches!(payments.submit(&token, CODE, ConsentDecision::Approve, &audit(), now()).await, Err(DomainError::NotFound { .. })),
		"the right code after the burn finds nothing"
	);

	reset_payments(&pool).await;
}

/// The notice is for a consent that DIED, and for nothing else: an investor who answered
/// (either way), an order that ran out of time or was withdrawn, and a fund-owned order its
/// consilium refused all have their own channel or none, and none of them is this.
#[tokio::test]
async fn an_answered_expired_withdrawn_or_quorum_refused_order_sends_no_outcome_notice() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let admin = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let to = || PaymentDestination::Internal(Party::Service(ServiceId::fee()));

	// Refused by the investor, with the right code.
	let seat = a_consent_seat(&pool, investor).await;
	let token = token_hash_of(&seat);
	let mut refused = an_order(Party::User(investor), to(), admin, "1.00");
	payments.open(&mut refused, seat, CONSENT_URL_BASE).await.unwrap();
	payments.submit(&token, CODE, ConsentDecision::Reject, &audit(), now()).await.expect("a valid refusal");
	assert_eq!(payments.find(refused.id()).await.unwrap().unwrap().order.state(), PaymentState::Rejected);

	// Withdrawn by the admin who opened it.
	let mut withdrawn = an_order(Party::User(investor), to(), admin, "2.00");
	payments.open(&mut withdrawn, a_consent_seat(&pool, investor).await, CONSENT_URL_BASE).await.unwrap();
	payments.cancel(withdrawn.id(), admin, now()).await.expect("the initiator withdraws it");

	// Left to run out.
	let mut expired = an_order(Party::User(investor), to(), admin, "3.00");
	payments.open(&mut expired, a_consent_seat(&pool, investor).await, CONSENT_URL_BASE).await.unwrap();
	payments.expire_due(now() + domain::payments::TTL_SECS + 1).await.expect("sweep");
	assert_eq!(payments.find(expired.id()).await.unwrap().unwrap().order.state(), PaymentState::Expired);

	// Refused by the owners' quorum.
	let consilium = a_decided_consilium(&pool, admin).await;
	let mut fund_owned = an_order(Party::Service(ServiceId::fee()), PaymentDestination::Internal(Party::Service(ServiceId::fund())), admin, "4.00");
	payments.open(&mut fund_owned, ApprovalSeat::Consilium(consilium), CONSENT_URL_BASE).await.unwrap();
	payments.record_rejection(fund_owned.id(), now()).await.expect("the quorum refuses");

	for order in [refused.id(), withdrawn.id(), expired.id(), fund_owned.id()] {
		assert!(outcome_mails(&pool, order).await.is_empty(), "no consent died on {order}");
	}

	sqlx::query("DELETE FROM consilium WHERE id = $1").bind(consilium.raw()).execute(&pool).await.ok();
	reset_payments(&pool).await;
}

/// The happy path, and the two idempotency rules the retry contract rests on: the same answer
/// again is a no-op, a different one is a conflict.
#[tokio::test]
async fn the_right_code_approves_the_order_and_reserves_its_source() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let seat = a_consent_seat(&pool, investor).await;
	let token = token_hash_of(&seat);

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "3.00");
	let id = order.id();
	payments.open(&mut order, seat, CONSENT_URL_BASE).await.expect("open the order");

	let outcome = payments
		.submit(&token, CODE, ConsentDecision::Approve, &audit(), now())
		.await
		.expect("the right code is accepted");
	assert!(outcome.decided && outcome.approved);
	assert_eq!(outcome.payment.order.state(), PaymentState::Approved);
	assert_eq!(
		relayed_kinds(&pool, id).await,
		vec!["reserved".to_owned()],
		"approval reserves the source, and relays nothing else"
	);

	let repeat = payments
		.submit(&token, CODE, ConsentDecision::Approve, &audit(), now())
		.await
		.expect("a retried answer is a no-op");
	assert!(!repeat.decided, "a repeat must not double-count");
	let contradiction = payments.submit(&token, CODE, ConsentDecision::Reject, &audit(), now()).await;
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

/// The fund-owned branch: the order links the consilium that decides it, the approval is
/// recorded by the quorum rather than by a token, and execution is idempotent for the same
/// effect and a conflict for a different one.
#[tokio::test]
async fn a_fund_owned_order_is_carried_by_its_consilium() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let operator = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let consilium = a_decided_consilium(&pool, operator).await;

	let mut order = an_order(
		Party::Service(ServiceId::fee()),
		PaymentDestination::Internal(Party::Service(ServiceId::fund())),
		operator,
		"40.00",
	);
	let id = order.id();
	payments.open(&mut order, ApprovalSeat::Consilium(consilium), CONSENT_URL_BASE).await.expect("open the order");

	let view = payments.find(id).await.unwrap().unwrap();
	assert_eq!(view.consilium_id, Some(consilium));
	assert!(view.consent.is_none(), "fund-owned money is never consented to by one investor");

	assert_eq!(payments.awaiting_execution().await.unwrap(), Vec::new(), "a pending order is not executable");
	// A quorum over some OTHER order cannot carry this one, whatever its terms say.
	let stranger = a_decided_consilium(&pool, operator).await;
	assert!(
		matches!(payments.record_approval(id, stranger, now()).await, Err(DomainError::Conflict(_))),
		"only the linked consilium may approve the order"
	);
	payments.record_approval(id, consilium, now()).await.expect("the quorum carried it");
	assert_eq!(payments.awaiting_execution().await.unwrap(), vec![id]);
	assert_eq!(relayed_kinds(&pool, id).await, vec!["reserved".to_owned()]);
	payments.record_approval(id, consilium, now()).await.expect("recording the same approval twice is a no-op");

	payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Transfer), now()).await.expect("execute");
	let conflict = payments
		.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Withdrawal(WithdrawalId::from_raw(Uuid::new_v4()))), now())
		.await;
	assert!(matches!(conflict, Err(DomainError::Conflict(_))), "a second, different effect is a conflict, not an overwrite");

	reset_payments(&pool).await;
	sqlx::query("DELETE FROM consilium WHERE id = ANY($1)")
		.bind(vec![consilium.raw(), stranger.raw()])
		.execute(&pool)
		.await
		.ok();
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
	let payments = PgPayments::new(pool.clone());

	let mut of_investor = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "5.00");
	let investors_order = of_investor.id();
	payments
		.open(&mut of_investor, a_consent_seat(&pool, investor).await, CONSENT_URL_BASE)
		.await
		.expect("open the investor's order");
	let mut of_fund = an_order(
		Party::Service(ServiceId::fee()),
		PaymentDestination::Internal(Party::Service(ServiceId::fund())),
		investor,
		"9.00",
	);
	let fund_order = of_fund.id();
	payments
		.open(&mut of_fund, ApprovalSeat::Consilium(a_decided_consilium(&pool, investor).await), CONSENT_URL_BASE)
		.await
		.expect("open the fund's order");

	let all = payments.list(&PaymentFilter::default(), 50).await.expect("the whole history");
	assert_eq!(all.len(), 2);
	let fund_owned = payments
		.list(
			&PaymentFilter {
				fund_owned_source: Some(true),
				..PaymentFilter::default()
			},
			50,
		)
		.await
		.unwrap();
	assert_eq!(fund_owned.iter().map(|view| view.order.id()).collect::<Vec<_>>(), vec![fund_order]);
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

	let refused = payments.cancel(fund_order, stranger, now()).await;
	assert!(matches!(refused, Err(DomainError::Forbidden(_))), "only the operator who opened it may withdraw it");
	payments.cancel(fund_order, investor, now()).await.expect("the initiator may withdraw it");

	// The sweep binds the caller's clock, so "past the deadline" and "expired" answer the same
	// question. One tick past the TTL closes exactly the order still pending.
	let closed = payments.expire_due(now() + domain::payments::TTL_SECS + 1).await.expect("sweep");
	assert_eq!(closed, 1, "the cancelled order is already closed; only the pending one expires");
	assert_eq!(payments.find(investors_order).await.unwrap().unwrap().order.state(), PaymentState::Expired);

	sqlx::query("DELETE FROM consilium WHERE initiator_user_id = $1").bind(investor.raw()).execute(&pool).await.ok();
	reset_payments(&pool).await;
}

/// The identity plane's relay, stood in for at the mailer's port: DOWN (every send
/// deferred, as an unreachable concierge is) or UP (every send taken and kept, with the
/// recipient it was addressed to), switched by the test.
#[derive(Default)]
struct SwitchedRelay {
	down: AtomicBool,
	seen: std::sync::Mutex<Vec<(Uuid, GovernanceMail)>>,
}

#[async_trait]
impl GovernanceMailer for SwitchedRelay {
	async fn send(&self, recipient: Uuid, _dedupe_key: &str, mail: &GovernanceMail) -> Result<(), MailDeliveryError> {
		if self.down.load(Ordering::SeqCst) {
			return Err(MailDeliveryError::Deferred("governance mail relay: status: Unavailable".into()));
		}
		self.seen.lock().unwrap().push((recipient, mail.clone()));
		Ok(())
	}
}

/// The queue is shared by every suite on this database and drained in batches of 100 by id,
/// so a test that runs the mailer first retires the backlog the others left behind.
async fn quiet_queue(pool: &PgPool) {
	sqlx::query("UPDATE consilium_mail SET sent_at = now() WHERE sent_at IS NULL AND withdrawn_at IS NULL")
		.execute(pool)
		.await
		.expect("retire the backlog");
}

/// The one consent row an order queued, as the queue holds it: `(sent, withdrawn, code,
/// approval_url)`.
async fn consent_mail(pool: &PgPool, order: PaymentId) -> (bool, bool, String, String) {
	sqlx::query_as(
		"SELECT sent_at IS NOT NULL, withdrawn_at IS NOT NULL, payload ->> 'code', payload ->> 'approval_url' \
		 FROM consilium_mail WHERE payment_id = $1 AND kind = 'payment_consent'",
	)
	.bind(order.raw())
	.fetch_one(pool)
	.await
	.expect("exactly one consent mail per order")
}

async fn consent_notified(pool: &PgPool, order: PaymentId) -> bool {
	sqlx::query_scalar("SELECT notified FROM payment_consent WHERE payment_id = $1")
		.bind(order.raw())
		.fetch_one(pool)
		.await
		.expect("the seat row")
}

/// Stand in for a deferred row's backoff having run out, so the next pass asks the relay again.
async fn let_the_backoff_run(pool: &PgPool, order: PaymentId) {
	sqlx::query("UPDATE consilium_mail SET next_attempt_at = NULL WHERE payment_id = $1")
		.bind(order.raw())
		.execute(pool)
		.await
		.expect("clear the backoff");
}

/// Closing an order without a verdict — the operator withdrawing it, or its window running
/// out — takes back the consent invitation the relay has not taken, with its token and code
/// blanked (#368). Left in the queue, the relay coming back would ask the investor to consent
/// to an order nobody can act on, with a token that still resolves. A consent already
/// delivered stands: the row says sent, never withdrawn, and its secrets were stripped at
/// delivery.
#[tokio::test]
async fn withdrawing_or_expiring_an_order_withdraws_the_consent_mail_the_relay_has_not_taken() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	quiet_queue(&pool).await;
	let investor = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let relay = Arc::new(SwitchedRelay::default());
	let mailer = ConsiliumMailer::new(pool.clone(), relay.clone());

	// Relay down: the invitation is queued with its secrets, and every pass defers it.
	relay.down.store(true, Ordering::SeqCst);
	let mut first = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "5.00");
	let withdrawn_order = first.id();
	let seat = a_consent_seat(&pool, investor).await;
	let token = token_of(&seat).to_owned();
	payments.open(&mut first, seat, CONSENT_URL_BASE).await.expect("open the first order");
	let (sent, withdrawn, code, url) = consent_mail(&pool, withdrawn_order).await;
	assert_eq!((sent, withdrawn), (false, false));
	assert_eq!(code, CODE, "queued with the plaintext code, to be delivered");
	assert!(url.ends_with(&token), "queued with the token, to be delivered");
	assert_eq!(mailer.drain().await.expect("a pass"), 0, "the relay is down: deferred, not delivered");
	assert!(!consent_notified(&pool, withdrawn_order).await);

	// The operator withdraws the order: the invitation goes with it, secrets and all.
	payments.cancel(withdrawn_order, investor, now()).await.expect("the initiator may withdraw it");
	let (sent, withdrawn, code, url) = consent_mail(&pool, withdrawn_order).await;
	assert!(!sent, "never delivered");
	assert!(withdrawn, "withdrawn with the order");
	assert_eq!((code.as_str(), url.as_str()), ("", ""), "a credential nobody will ever be handed is blanked");
	assert!(!consent_notified(&pool, withdrawn_order).await, "nobody was told");
	assert_eq!(
		piggybank_core::infrastructure::consilium_mailer::pending_count(&pool).await.unwrap(),
		0,
		"a withdrawn mail is not owed"
	);

	// The relay comes back: a withdrawn row is not the next pass's to deliver.
	relay.down.store(false, Ordering::SeqCst);
	let_the_backoff_run(&pool, withdrawn_order).await;
	assert_eq!(mailer.drain().await.expect("a pass"), 0);
	assert!(relay.seen.lock().unwrap().is_empty(), "the withdrawn invitation never reached the relay");

	// The window running out closes the same way.
	relay.down.store(true, Ordering::SeqCst);
	let mut second = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "6.00");
	let expired_order = second.id();
	payments
		.open(&mut second, a_consent_seat(&pool, investor).await, CONSENT_URL_BASE)
		.await
		.expect("open the second order");
	assert_eq!(mailer.drain().await.expect("a pass"), 0);
	let closed = payments.expire_due(now() + domain::payments::TTL_SECS + 1).await.expect("sweep");
	assert_eq!(closed, 1, "the withdrawn order is already closed; only the pending one expires");
	assert_eq!(payments.find(expired_order).await.unwrap().unwrap().order.state(), PaymentState::Expired);
	let (sent, withdrawn, code, url) = consent_mail(&pool, expired_order).await;
	assert_eq!((sent, withdrawn), (false, true));
	assert_eq!((code.as_str(), url.as_str()), ("", ""));
	assert!(!consent_notified(&pool, expired_order).await);
	assert_eq!(piggybank_core::infrastructure::consilium_mailer::pending_count(&pool).await.unwrap(), 0);
	relay.down.store(false, Ordering::SeqCst);
	let_the_backoff_run(&pool, expired_order).await;
	assert_eq!(mailer.drain().await.expect("a pass"), 0);
	assert!(relay.seen.lock().unwrap().is_empty());

	// Control: a delivered invitation stands. The relay took it, the seat says notified, and
	// withdrawing the order afterwards neither takes the mail back nor unsays that.
	let mut third = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "7.00");
	let delivered_order = third.id();
	payments
		.open(&mut third, a_consent_seat(&pool, investor).await, CONSENT_URL_BASE)
		.await
		.expect("open the third order");
	assert_eq!(mailer.drain().await.expect("a pass"), 1, "the relay is up: handed over");
	assert!(matches!(relay.seen.lock().unwrap().as_slice(), [(_, GovernanceMail::PaymentConsent(_))]));
	assert!(consent_notified(&pool, delivered_order).await, "the seat is notified once concierge has taken the message");
	let (sent, withdrawn, code, _) = consent_mail(&pool, delivered_order).await;
	assert_eq!((sent, withdrawn), (true, false));
	assert_eq!(code, "", "redacted at delivery");
	payments.cancel(delivered_order, investor, now()).await.expect("the initiator may withdraw it");
	let (sent, withdrawn, _, _) = consent_mail(&pool, delivered_order).await;
	assert_eq!((sent, withdrawn), (true, false), "a delivered mail is left as it is");
	assert!(consent_notified(&pool, delivered_order).await);

	reset_payments(&pool).await;
}

/// `Approved` IS A COMMITTED STATE: the reservation landed in the approval's transaction and
/// the settlement would land in the execution's. A failure between the two must give the
/// locked amount back, or a terminal order leaves it in `clearing` with nothing left to post
/// or void it — a withdrawal's fail void, by another name. `fund` is a global singleton
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
	let payments = PgPayments::new(pool.clone());
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone());
	ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: LedgerAccountKey::ServiceClaim(ServiceId::fund()),
			amount: usdt("100").base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.expect("fund the fund's own claim");
	// Whatever an earlier suite left undrained is applied before the snapshots, so the
	// deltas below are this order's alone.
	relay.drain().await;
	let before = ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fund())).await.unwrap();
	let revenue_before = ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted;

	let mut order = an_order(Party::Service(ServiceId::fund()), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "30");
	let id = order.id();
	let consilium = a_decided_consilium(&pool, investor).await;
	payments.open(&mut order, ApprovalSeat::Consilium(consilium), CONSENT_URL_BASE).await.expect("open the order");
	payments.record_approval(id, consilium, now()).await.expect("the quorum carried it");
	relay.drain().await;
	let reserved = ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fund())).await.unwrap();
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

	let released = ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fund())).await.unwrap();
	assert_eq!(released.posted, before.posted, "nothing was debited");
	assert_eq!(released.locked, before.locked, "the reservation was voided");
	assert_eq!(
		ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted,
		revenue_before,
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

	sqlx::query("DELETE FROM consilium WHERE initiator_user_id = $1").bind(investor.raw()).execute(&pool).await.ok();
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

	let seat = a_consent_seat(&pool, investor).await;
	let token = token_hash_of(&seat);
	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "30");
	let id = order.id();
	payments.open(&mut order, seat, CONSENT_URL_BASE).await.expect("open the order");
	payments.submit(&token, CODE, ConsentDecision::Approve, &audit(), now()).await.expect("consent");

	relay.drain().await;
	let reserved = ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();
	assert_eq!(reserved.posted, usdt("100").base_units(), "a reservation locks the claim, it does not debit it");
	assert_eq!(reserved.locked, usdt("30").base_units());
	assert_eq!(reserved.available(), usdt("70").base_units());

	let revenue_before = ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted;
	payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Transfer), now()).await.expect("execute");
	relay.drain().await;

	let settled = ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();
	assert_eq!(settled.posted, usdt("70").base_units(), "the settlement posts the reservation");
	assert_eq!(settled.locked, 0);
	// `fee` is a global singleton shared with every other suite, so this is a DELTA.
	let revenue_after = ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted;
	assert_eq!(revenue_after - revenue_before, usdt("30").base_units());
	// Nothing parked: a parked row here would mean a leg the ledger refused.
	let parked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox WHERE aggregate = 'payment' AND parked_at IS NOT NULL")
		.fetch_one(&pool)
		.await
		.unwrap();
	assert_eq!(parked, 0);

	reset_payments(&pool).await;
}

/// THE TOKEN-VERSION PIN, AT CONSENT. `RevokeTokens` is the investor's (or the operator's)
/// "every session out" — a consent mailed before it must not be spendable after it, however
/// right the code. The seat fails closed and takes the order with it: with one seat there is
/// nobody to re-issue the request to.
#[tokio::test]
async fn revoking_the_investors_sessions_voids_a_pending_consent() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let investor = an_investor(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let users = PgUsers::new(pool.clone());
	let seat = a_consent_seat(&pool, investor).await;
	let token = token_hash_of(&seat);

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "2.00");
	let id = order.id();
	payments.open(&mut order, seat, CONSENT_URL_BASE).await.expect("open the order");
	assert!(payments.find(id).await.unwrap().unwrap().consent.unwrap().invalidated.is_none(), "the pins hold at open");

	users.revoke_tokens(investor).await.expect("revoke every session");

	let refused = payments.submit(&token, CODE, ConsentDecision::Approve, &audit(), now()).await;
	assert!(
		matches!(refused, Err(DomainError::Conflict(ref why)) if why.contains("revoked")),
		"the right code no longer consents to anything: {refused:?}"
	);
	let view = payments.find(id).await.unwrap().unwrap();
	assert_eq!(view.order.state(), PaymentState::Rejected, "a voided consent fails the payment closed");
	assert!(
		view.consent
			.unwrap()
			.invalidated
			.is_some_and(|cause| matches!(cause, ConsentInvalidation::SessionsRevoked { .. }))
	);
	assert_eq!(
		payments.find(id).await.unwrap().unwrap().consent.unwrap().attempts_remaining,
		MAX_CODE_ATTEMPTS as u32,
		"no attempt was charged"
	);
	assert!(relayed_kinds(&pool, id).await.is_empty(), "nothing was reserved");
	assert_outcome_told(&pool, id, investor, &[investor], "INVALIDATED", "SESSIONS_REVOKED").await;
	// A closed order's token answers like an unknown one on both surfaces.
	assert!(payments.invitation(&token, now()).await.is_err());
	assert!(matches!(
		payments.submit(&token, CODE, ConsentDecision::Approve, &audit(), now()).await,
		Err(DomainError::NotFound { .. })
	));
	assert_eq!(outcome_mails(&pool, id).await.len(), 1, "a retried answer tells nobody twice");

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
	let payments = PgPayments::new(pool.clone());
	let users = PgUsers::new(pool.clone());
	let seat = a_consent_seat(&pool, investor).await;
	let token = token_hash_of(&seat);

	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "4.00");
	let id = order.id();
	payments.open(&mut order, seat, CONSENT_URL_BASE).await.expect("open the order");
	let consented = payments
		.submit(&token, CODE, ConsentDecision::Approve, &audit(), now())
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
	assert_outcome_told(&pool, id, investor, &[investor], "INVALIDATED", "SESSIONS_REVOKED").await;
	// The refusal is idempotent: a retry finds a terminal order and is a plain conflict, not a
	// second failure record.
	assert!(matches!(
		payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Transfer), now()).await,
		Err(DomainError::Conflict(_))
	));
	assert_eq!(outcome_mails(&pool, id).await.len(), 1, "nor a second notice");

	reset_payments(&pool).await;
}

/// THE MAILBOX PIN. The identity provider changing the investor's address after the consent
/// mail went out means the token in flight was delivered to an address that is no longer
/// theirs — so it must not be able to consent. The address is re-read from the projection
/// the bridge maintains, by the same digest the seat froze at open.
#[tokio::test]
async fn a_changed_mailbox_voids_a_pending_consent() {
	let _guard = exclusive_payments().await;
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping payments adapter tests");
		return;
	};
	reset_payments(&pool).await;
	let payments = PgPayments::new(pool.clone());
	let users = PgUsers::new(pool.clone());
	let tag = Uuid::new_v4();
	let subject = || domain::auth::AuthSubject::parse(&format!("payments-test-{tag}")).unwrap();
	let investor = users
		.provision(subject(), Email::parse(&format!("before-{tag}@example.test")).unwrap(), true)
		.await
		.expect("provision the investor")
		.id();
	// Without a mirrored identity-plane id there is nobody to address the consent mail to,
	// and `open` refuses rather than seating a consent nobody will ever receive.
	let mut unaddressed = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "1.50");
	let refused = payments.open(&mut unaddressed, a_consent_seat(&pool, investor).await, CONSENT_URL_BASE).await;
	assert!(matches!(refused, Err(DomainError::Conflict(_))), "an investor with no concierge id cannot be asked: {refused:?}");
	assert!(payments.find(unaddressed.id()).await.unwrap().is_none(), "a refused open writes nothing");
	mirror_concierge_id(&pool, investor).await;
	let seat = a_consent_seat(&pool, investor).await;
	let token = token_hash_of(&seat);
	let mut order = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), investor, "1.50");
	let id = order.id();
	payments.open(&mut order, seat, CONSENT_URL_BASE).await.expect("open the order");
	// The same investor's money, on an order an admin opened.
	let admin = an_investor(&pool).await;
	let admins_seat = a_consent_seat(&pool, investor).await;
	let admins_token = token_hash_of(&admins_seat);
	let mut admins_order = an_order(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), admin, "2.50");
	payments.open(&mut admins_order, admins_seat, CONSENT_URL_BASE).await.expect("open the admin's order");

	// The provider reports a new address behind the same subject — the path the first-login
	// upsert takes for an existing row.
	users
		.provision(subject(), Email::parse(&format!("after-{tag}@example.test")).unwrap(), true)
		.await
		.expect("apply the new address");

	let refused = payments.submit(&token, CODE, ConsentDecision::Approve, &audit(), now()).await;
	assert!(
		matches!(refused, Err(DomainError::Conflict(ref why)) if why.contains("mailbox")),
		"a moved mailbox voids the seat: {refused:?}"
	);
	assert_eq!(payments.find(id).await.unwrap().unwrap().order.state(), PaymentState::Rejected);
	// The mailbox the subject's copy would reach is the one that just replaced theirs — maybe
	// unverified, maybe an attacker's. Nobody but staff is told, and here there is no staff.
	assert!(outcome_mails(&pool, id).await.is_empty(), "the new mailbox is not told: {:#?}", outcome_mails(&pool, id).await);
	assert!(payments.submit(&admins_token, CODE, ConsentDecision::Approve, &audit(), now()).await.is_err());
	assert_eq!(payments.find(admins_order.id()).await.unwrap().unwrap().order.state(), PaymentState::Rejected);
	assert_outcome_told(&pool, admins_order.id(), investor, &[admin], "INVALIDATED", "EMAIL_CHANGED").await;

	reset_payments(&pool).await;
}

// ---------------------------------------------------------------------------------------
// The use cases. Everything below drives `application::payments` end to end — the seat is
// minted, the mail is queued, the consent is answered by the credentials read out of that
// queue, and the money is read back from the ledger that owns it.
// ---------------------------------------------------------------------------------------

use domain::money::WalletAddress;
use piggybank_core::{
	application::payments as payments_app,
	config::KycGate,
	infrastructure::{allocations::PgAllocations, consilium::PgConsilia, operations, outflow::PgOutflowPolicy, withdrawals::PgWithdrawals},
	ports::{WithdrawalRepository, ledger::Ledger},
};

const APPROVAL_URL_BASE: &str = "https://example.test/approve";
const BEP20_ADDRESS: &str = "0x52908400098527886E0F7030069857D2E4169EE7";

struct App {
	pool: PgPool,
	payments: PgPayments,
	consilia: PgConsilia,
	users: PgUsers,
	withdrawals: PgWithdrawals,
	outflow: PgOutflowPolicy,
	allocations: PgAllocations,
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
		consilia: PgConsilia::new(pool.clone()),
		users: PgUsers::new(pool.clone()),
		withdrawals: PgWithdrawals::new(pool.clone()),
		outflow: PgOutflowPolicy::new(pool.clone()),
		allocations: PgAllocations::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		ledger,
		notify,
		pool,
	})
}

fn ports(a: &App) -> payments_app::PaymentPorts<'_> {
	payments_app::PaymentPorts {
		payments: &a.payments,
		consilia: &a.consilia,
		users: &a.users,
		withdrawals: &a.withdrawals,
		ledger: a.ledger.as_ref(),
		custody: &StubCustody,
		policy: &a.outflow,
		allocations: &a.allocations,
		relay: &a.notify,
		configured: &[Network::Bep20],
		kyc: KycGate::LIFTED,
		approval_url_base: APPROVAL_URL_BASE,
		consent_url_base: CONSENT_URL_BASE,
		governance_mail_wired: true,
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

/// The token and code mailed to the subject — read out of the queue exactly as the investor
/// reads them off the message, because the plaintexts live nowhere else.
async fn consent_credentials(pool: &PgPool, payment: PaymentId) -> (String, String) {
	let payload: String = sqlx::query_scalar("SELECT payload::text FROM consilium_mail WHERE payment_id = $1 AND kind = 'payment_consent'")
		.bind(payment.raw())
		.fetch_one(pool)
		.await
		.expect("the consent mail was queued with the order");
	let mail: serde_json::Value = serde_json::from_str(&payload).unwrap();
	let url = mail["approval_url"].as_str().unwrap().to_owned();
	assert!(url.starts_with(CONSENT_URL_BASE), "the link is built on the consent base: {url}");
	(url.rsplit('/').next().unwrap().to_owned(), mail["code"].as_str().unwrap().to_owned())
}

/// Open → consent mail queued → consent → execute, at the internal tier, with the money read
/// back from TigerBeetle. The settlement is recorded only once the relay has applied the
/// reservation — the inline execute after the consent finds it pending and records nothing.
#[tokio::test]
async fn an_investors_internal_payment_is_consented_to_by_mail_and_settles_once_reserved() {
	let _guard = exclusive_payments().await;
	let Some(a) = app("payments consent flow").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let revenue_before = a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted;

	let view = payments_app::open(
		&ports(&a),
		investor,
		terms(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), "30"),
		now(),
	)
	.await
	.expect("open the order");
	let id = view.order.id();
	assert_eq!(view.order.state(), PaymentState::Pending);
	let consent = view.consent.expect("a consent seat");
	assert!(!consent.notified, "notified means handed to concierge, not queued");
	assert!(consent.invalidated.is_none());
	// The queue row names the PAYMENT and the subject's identity-plane id, so the relay can
	// refuse it unless the addressee is the subject.
	let (subject_id, kind): (String, String) = sqlx::query_as("SELECT payload ->> 'subject_user_id', kind FROM consilium_mail WHERE payment_id = $1")
		.bind(id.raw())
		.fetch_one(&a.pool)
		.await
		.unwrap();
	assert_eq!(kind, "payment_consent");
	let concierge_id: Uuid = sqlx::query_scalar("SELECT concierge_user_id FROM users WHERE id = $1")
		.bind(investor.raw())
		.fetch_one(&a.pool)
		.await
		.unwrap();
	assert_eq!(subject_id, concierge_id.to_string());

	let (token, code) = consent_credentials(&a.pool, id).await;
	let invitation = payments_app::invitation(&a.payments, &token, now()).await.expect("the token resolves");
	assert_eq!(invitation.payment_id, id);
	let outcome = payments_app::submit_consent(&ports(&a), &token, &code, ConsentDecision::Approve, &audit(), now())
		.await
		.expect("the right code consents");
	assert!(outcome.decided && outcome.approved);
	// The inline execute found the reservation still in the outbox: nothing recorded.
	let view = payments_app::find(&a.payments, id).await.unwrap();
	assert_eq!(view.order.state(), PaymentState::Approved, "no settlement over a reserve that has not landed");
	assert_eq!(relayed_kinds(&a.pool, id).await, vec!["reserved".to_owned()]);

	a.relay.drain().await;
	let reserved = a.ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();
	assert_eq!(reserved.locked, usdt("30").base_units());

	// The sweeper's half: the reserve has landed, so the settlement is recorded now.
	let report = payments_app::sweep(&ports(&a), now()).await.unwrap();
	assert_eq!((report.executed, report.deferred, report.execution_failures), (1, 0, 0));
	assert_eq!(payments_app::find(&a.payments, id).await.unwrap().order.state(), PaymentState::Executed);
	a.relay.drain().await;
	let settled = a.ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();
	assert_eq!(settled.posted, usdt("70").base_units());
	assert_eq!(settled.locked, 0);
	assert_eq!(
		a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted - revenue_before,
		usdt("30").base_units()
	);
	// Idempotent: a repeat execution records nothing new.
	assert_eq!(payments_app::execute(&ports(&a), id, now()).await.unwrap().order.state(), PaymentState::Executed);

	reset_payments(&a.pool).await;
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
	let (token, code) = consent_credentials(&a.pool, id).await;
	payments_app::submit_consent(&ports(&a), &token, &code, ConsentDecision::Approve, &audit(), now())
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

/// The refusals at OPEN: an external payment from a source the withdrawal saga cannot pay
/// from, a source that cannot cover the amount, and an unwired mailer.
#[tokio::test]
async fn an_order_that_could_never_execute_is_refused_at_open() {
	let _guard = exclusive_payments().await;
	let Some(a) = app("payments open refusals").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;

	let err = payments_app::open(&ports(&a), investor, terms(Party::Service(ServiceId::fund()), external(), "5"), now())
		.await
		.unwrap_err();
	assert!(
		matches!(err, DomainError::Validation(_)),
		"the saga has no account for the fund allocation's pooled capital: {err:?}"
	);
	let err = payments_app::open(
		&ports(&a),
		investor,
		terms(Party::Service(domain::balance::ServiceId::parse("alpha").unwrap()), external(), "5"),
		now(),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "nor for a product's pooled funds: {err:?}");

	// The investor's claim is empty: refused now rather than after 72 hours of consent.
	let err = payments_app::open(
		&ports(&a),
		investor,
		terms(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), "1"),
		now(),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "an uncovered source is refused: {err:?}");

	// A product nobody registered: its claim would take the money, and nobody could reach it.
	let unregistered = PaymentDestination::Internal(Party::Service(domain::balance::ServiceId::parse("no-such-product").unwrap()));
	let err = payments_app::open(&ports(&a), investor, terms(Party::User(investor), unregistered, "1"), now())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref why) if why.contains("no product")), "{err:?}");

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
	common::set_kyc_level(&a.pool, investor, 1).await;
	payments_app::open(&enforced, investor, terms(Party::User(investor), PaymentDestination::Internal(Party::User(verified)), "1"), now())
		.await
		.expect("verified, the same order opens");
	reset_payments(&a.pool).await;

	let unwired = payments_app::PaymentPorts {
		governance_mail_wired: false,
		..ports(&a)
	};
	let err = payments_app::open(
		&unwired,
		investor,
		terms(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), "1"),
		now(),
	)
	.await
	.unwrap_err();
	assert!(err.to_string().contains("governance mail is not configured"), "{err}");

	let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments").fetch_one(&a.pool).await.unwrap();
	assert_eq!(stored, 0, "a refused open writes nothing");
	a.relay.drain().await;
}

/// A revocation between consent and execution: the execution path reads the moved pin
/// BEFORE creating anything, fails the order closed and releases the reservation.
#[tokio::test]
async fn a_revocation_after_consent_fails_execution_closed_and_releases_the_reserve() {
	let _guard = exclusive_payments().await;
	let Some(a) = app("payments revoked consent").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let before = a.ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();

	let id = payments_app::open(
		&ports(&a),
		investor,
		terms(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), "20"),
		now(),
	)
	.await
	.unwrap()
	.order
	.id();
	let (token, code) = consent_credentials(&a.pool, id).await;
	// The consent through the PORT, so nothing executes inline and the window is real.
	a.payments.submit(&digest(token.as_bytes()), &code, ConsentDecision::Approve, &audit(), now()).await.unwrap();
	a.relay.drain().await;
	a.users.revoke_tokens(investor).await.unwrap();

	let view = payments_app::execute(&ports(&a), id, now()).await.expect("the refusal is recorded on the order, not raised");
	assert_eq!(view.order.state(), PaymentState::ExecutionFailed);
	assert!(view.order.failure_reason().is_some_and(|why| why.contains("revoked")), "{:?}", view.order.failure_reason());
	assert_eq!(relayed_kinds(&a.pool, id).await, vec!["reserved".to_owned(), "released".to_owned()]);
	assert_outcome_told(&a.pool, id, investor, &[investor], "INVALIDATED", "SESSIONS_REVOKED").await;
	// A second caller carrying the same void — the sweeper racing the inline execute — finds
	// the order already failed and tells nobody again.
	let cause = ConsentInvalidation::SessionsRevoked { at_open: 0, now: 1 };
	a.payments
		.record_execution(id, ExecutionOutcome::ConsentVoid { cause, withdrawal: None }, now())
		.await
		.expect("idempotent");
	assert_eq!(outcome_mails(&a.pool, id).await.len(), 1);
	a.relay.drain().await;
	let after = a.ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();
	assert_eq!((after.posted, after.locked), (before.posted, before.locked), "the reservation was given back");
	assert!(a.payments.awaiting_execution().await.unwrap().is_empty(), "nothing retries a failure");

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
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;

	let id = payments_app::open(
		&ports(&a),
		investor,
		terms(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), "20"),
		now(),
	)
	.await
	.unwrap()
	.order
	.id();
	let (token, code) = consent_credentials(&a.pool, id).await;
	a.payments.submit(&digest(token.as_bytes()), &code, ConsentDecision::Approve, &audit(), now()).await.unwrap();
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
/// the order `approved` for the next sweep — not close it for good and mail a refusal to
/// both parties over a control the operator meant as a hold.
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
	let (token, code) = consent_credentials(&a.pool, id).await;
	// The consent through the PORT, so nothing executes inline and the freeze lands
	// inside the window.
	a.payments.submit(&digest(token.as_bytes()), &code, ConsentDecision::Approve, &audit(), now()).await.unwrap();
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
	let id = payments_app::open(&ports(&a), investor, terms(Party::User(investor), external(), "50"), now())
		.await
		.unwrap()
		.order
		.id();
	let (token, code) = consent_credentials(&a.pool, id).await;
	a.payments.submit(&digest(token.as_bytes()), &code, ConsentDecision::Approve, &audit(), now()).await.unwrap();

	// Stand in for the window: the pins have been read and the withdrawal created, the way
	// `execute` creates it...
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
		usdt("50"),
	)
	.await
	.unwrap();
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

/// The expiry half of the sweep, and the initiator's withdrawal closing the consilium with it.
#[tokio::test]
async fn the_sweep_expires_what_nobody_consented_to() {
	let _guard = exclusive_payments().await;
	let Some(a) = app("payments expiry sweep").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "10").await;
	let id = payments_app::open(
		&ports(&a),
		investor,
		terms(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), "1"),
		now(),
	)
	.await
	.unwrap()
	.order
	.id();

	// Read as the investor would have off a message delivered BEFORE the window ran out: the
	// sweep blanks the queued copy (#368), so afterwards there is nothing left to read.
	let (token, code) = consent_credentials(&a.pool, id).await;

	let report = payments_app::sweep(&ports(&a), now() + domain::payments::TTL_SECS + 1).await.unwrap();
	assert_eq!(report.expired, 1);
	assert_eq!(payments_app::find(&a.payments, id).await.unwrap().order.state(), PaymentState::Expired);
	let (sent, withdrawn, queued_code, queued_url) = consent_mail(&a.pool, id).await;
	assert_eq!((sent, withdrawn), (false, true), "the undelivered invitation went with the order");
	assert_eq!((queued_code.as_str(), queued_url.as_str()), ("", ""), "and its secrets with it");
	assert!(
		matches!(
			payments_app::submit_consent(&ports(&a), &token, &code, ConsentDecision::Approve, &audit(), now()).await,
			Err(DomainError::NotFound { .. })
		),
		"an expired order's token answers like an unknown one"
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

/// An approved L1 order, its consent given through the port so nothing executed inline.
async fn an_approved_l1_order(a: &App, investor: UserId, initiator: UserId, amount: &str) -> PaymentId {
	let id = payments_app::open(&ports(a), initiator, terms(Party::User(investor), external(), amount), now())
		.await
		.unwrap()
		.order
		.id();
	let (token, code) = consent_credentials(&a.pool, id).await;
	a.payments.submit(&digest(token.as_bytes()), &code, ConsentDecision::Approve, &audit(), now()).await.unwrap();
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
	assert_outcome_told(&a.pool, id, investor, &[investor, admin], "INVALIDATED", "SESSIONS_REVOKED").await;

	a.relay.drain().await;
	reset_payments(&a.pool).await;
}

/// The L1 window with the mail queue refusing: the void of the withdrawal and the order's
/// failure still commit. The notice is the report of the defence, never a condition of it.
#[tokio::test]
async fn a_broken_mail_queue_does_not_undo_the_void_in_the_execution_window() {
	use domain::withdrawals::WithdrawalState;

	let _guard = exclusive_payments().await;
	let Some(a) = app("payments L1 window with the mail queue refusing").await else { return };
	reset_payments(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = an_approved_l1_order(&a, investor, investor, "30").await;
	let withdrawal = queue_payment_withdrawal(&a, id, investor, "30").await;
	a.users.revoke_tokens(investor).await.unwrap();

	refuse_outcome_mail(&a.pool, true).await;
	let refused = a.payments.record_execution(id, ExecutionOutcome::Executed(PaymentEffect::Withdrawal(withdrawal)), now()).await;
	refuse_outcome_mail(&a.pool, false).await;

	assert!(matches!(refused, Err(DomainError::Conflict(_))), "{refused:?}");
	assert_eq!(payments_app::find(&a.payments, id).await.unwrap().order.state(), PaymentState::ExecutionFailed);
	assert_eq!(a.withdrawals.find_by_id(withdrawal).await.unwrap().unwrap().state(), WithdrawalState::Cancelled);
	assert!(outcome_mails(&a.pool, id).await.is_empty());

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
	assert_outcome_told(&a.pool, id, investor, &[investor, admin], "INVALIDATED", "SESSIONS_REVOKED").await;

	a.relay.drain().await;
	reset_payments(&a.pool).await;
}
