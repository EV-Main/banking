//! Postgres adapter for the [`PaymentRepository`] and [`PaymentFeed`] ports.
//!
//! THE TRANSACTION BOUNDARY IS THE METHOD, and it never reaches TigerBeetle. Each method
//! opens one transaction, takes the source claim's advisory lock and/or the order's row lock,
//! applies the aggregate command inside it, and drains the resulting events to `event_log`
//! (always) and `outbox` (only for the two that move money). The relay then issues the
//! transfers after the commit — Write-Last, exactly as every other money aggregate here.
//!
//! [`PaymentRepository::submit`] is the load-bearing method: the attempt counter, the code
//! comparison and the order's transition all happen under ONE `SELECT … FOR UPDATE` on the
//! `payments` row. Counting the attempt *before* comparing (and in the same transaction) is
//! what stops concurrent guesses slipping past the five-attempt ceiling; locking the ORDER
//! rather than the consent row is what stops the answer and the transition landing in two
//! transactions a retry could interleave.
//!
//! Runtime queries (`sqlx::query*`, never the compile-time macros) keep `cargo build`
//! independent of a live database; sqlx 0.9 takes only a `&'static str`, so every column list
//! is spliced in with `concat!` rather than built with `format!`.

use std::collections::HashMap;

use async_trait::async_trait;
use domain::{
	balance::Party,
	consilium::ConsiliumId,
	error::DomainError,
	money::{Network, Usdt, WalletAddress},
	payments::{PaymentApproval, PaymentDestination, PaymentEffect, PaymentEvent, PaymentId, PaymentOrder, PaymentReason, PaymentState, PaymentTerms},
	scope_operations::MONEY_MOVEMENT_TOPIC,
	users::{UserId, mask_email},
	withdrawals::WithdrawalId,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{
	infrastructure::{
		consilium_mailer::{MailSubject, enqueue, withdraw_undelivered_consent},
		outbox, scope_notifications, withdrawals,
	},
	ports::{
		governance_mail::{GovernanceMail, PaymentConsent, PaymentOutcome},
		payments::{
			ApprovalSeat, ConciergeSeatView, ConsentAudit, ConsentDecision, ConsentInvalidation, ConsentInvitation, ConsentOutcome, ConsentView, DIGEST_BYTES, DueApproval, EndDetail,
			ExecutionOutcome, MAX_CODE_ATTEMPTS, PaymentApprovalStore, PaymentFeed, PaymentFilter, PaymentRepository, PaymentView, ReservationStatus, SessionConsentView, already_open,
			consent_not_found,
		},
		scope_notifications::ScopeNotice,
		scope_operations::ApprovalLevel,
	},
};

/// Postgres' unique-violation SQLSTATE — how the single-open-per-source index answers.
const UNIQUE_VIOLATION: &str = "23505";

macro_rules! payment_columns {
	() => {
		"p.id, p.state, p.from_kind, p.from_id, p.to_kind, p.to_id, p.to_network, p.to_address, p.amount, p.reason, p.payload_hash, p.initiator_user_id, \
		 EXTRACT(EPOCH FROM p.created_at)::bigint AS created_at, EXTRACT(EPOCH FROM p.expires_at)::bigint AS expires_at, \
		 EXTRACT(EPOCH FROM p.decided_at)::bigint AS decided_at, p.executed_withdrawal_id, p.failure_reason, p.version"
	};
}

// The two pins ride along with every seat read: the values frozen at open beside the
// subject's CURRENT ones, so `ConsentRow::invalidation` can be asked under whatever lock the
// caller already holds. `subject_token_version` folds both revoke surfaces exactly as
// `issuance_columns!` does — a concierge `SESSIONS_REVOKED` and banking's own `RevokeTokens`
// must each void a consent.
macro_rules! consent_columns {
	() => {
		"c.payment_id, c.subject_user_id, u.email AS subject_email, c.decision, EXTRACT(EPOCH FROM c.decided_at)::bigint AS decided_at, c.notified, c.attempts, \
		 c.code_hash, c.burned_at IS NOT NULL AS burned, c.used_at IS NOT NULL AS used, EXTRACT(EPOCH FROM c.expires_at)::bigint AS token_expires_at, \
		 c.subject_token_version_at_open, c.subject_email_hash_at_open, GREATEST(u.concierge_token_version, u.token_version) AS subject_token_version"
	};
}

macro_rules! consent_of_payment_query {
	() => {
		concat!(
			"SELECT ",
			consent_columns!(),
			" FROM payment_consent c JOIN users u ON u.id = c.subject_user_id WHERE c.payment_id = $1"
		)
	};
}

macro_rules! consent_by_token_query {
	() => {
		concat!(
			"SELECT ",
			consent_columns!(),
			" FROM payment_consent c JOIN users u ON u.id = c.subject_user_id WHERE c.token_hash = $1"
		)
	};
}

pub struct PgPayments {
	pool: PgPool,
}

impl PgPayments {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}

	/// Close one order whose window ran out. `false` means it was decided between the probe
	/// and the lock, which is not a failure.
	async fn expire_one(&self, id: Uuid, at: i64) -> Result<bool, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		withdraw_consent(&mut tx, PaymentId::from_raw(id)).await?;
		let mut order = locked(&mut tx, PaymentId::from_raw(id)).await?;
		if !order.state().is_pending() {
			return Ok(false);
		}
		order.expire(at)?;
		persist(&mut tx, &mut order).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(true)
	}
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

/// Compare two digests without leaking, through timing, how much of a guess was right.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
	a.len() == b.len() && a.ct_eq(b).into()
}

/// Rebuild the aggregate from one `payments` row.
///
/// The requirement, the tier and the effect are all DERIVED here rather than read: each is a
/// pure function of columns the row already carries, so there is no stored copy to disagree
/// with the terms. `to_kind IS NULL` is what says the destination is an address, and that in
/// turn is what says an executed order's effect is a withdrawal.
fn rehydrate(row: &PgRow) -> Result<PaymentOrder, DomainError> {
	let from = Party::from_parts(&text(row, "from_kind")?, opt_text(row, "from_id")?.as_deref())?;
	let to = match opt_text(row, "to_kind")? {
		Some(kind) => PaymentDestination::Internal(Party::from_parts(&kind, opt_text(row, "to_id")?.as_deref())?),
		None => {
			let network = Network::parse(&text(row, "to_network")?)?;
			let address = WalletAddress::parse(network, &text(row, "to_address")?)?;
			PaymentDestination::External { network, address }
		}
	};
	let amount = Usdt::from_base_units(text(row, "amount")?.parse::<u128>().map_err(|_| DomainError::Repository("malformed payment amount".into()))?);
	let terms = PaymentTerms::new(from, to, amount, PaymentReason::new(text(row, "reason")?)?)?;
	let hash_bytes: Vec<u8> = row.try_get("payload_hash").map_err(repo_err)?;
	let payload_hash: [u8; DIGEST_BYTES] = hash_bytes
		.try_into()
		.map_err(|_| DomainError::Repository("payment payload_hash is not a sha-256 digest".into()))?;
	let state = PaymentState::parse(&text(row, "state")?)?;
	let withdrawal: Option<Uuid> = row.try_get("executed_withdrawal_id").map_err(repo_err)?;
	// `Transfer` carries nothing, so "executed with no withdrawal id" IS the ledger effect.
	// The schema's `payments_execution_is_recorded` is what keeps the two cases apart.
	let effect = match (state, withdrawal) {
		(PaymentState::Executed, Some(id)) => Some(PaymentEffect::Withdrawal(WithdrawalId::from_raw(id))),
		(PaymentState::Executed, None) => Some(PaymentEffect::Transfer),
		_ => None,
	};
	Ok(PaymentOrder::rehydrate(
		PaymentId::from_raw(row.try_get("id").map_err(repo_err)?),
		terms,
		payload_hash,
		UserId::from_raw(row.try_get("initiator_user_id").map_err(repo_err)?),
		state,
		row.try_get("created_at").map_err(repo_err)?,
		row.try_get("expires_at").map_err(repo_err)?,
		row.try_get("decided_at").map_err(repo_err)?,
		effect,
		row.try_get("failure_reason").map_err(repo_err)?,
		row.try_get::<i64, _>("version").map_err(repo_err)? as u64,
	))
}

fn text(row: &PgRow, column: &str) -> Result<String, DomainError> {
	row.try_get(column).map_err(repo_err)
}

fn opt_text(row: &PgRow, column: &str) -> Result<Option<String>, DomainError> {
	row.try_get(column).map_err(repo_err)
}

/// The consent seat behind one order, joined to its subject's address.
struct ConsentRow {
	payment_id: Uuid,
	subject: Uuid,
	email: String,
	decision: String,
	decided_at: Option<i64>,
	notified: bool,
	attempts: i32,
	code_hash: Vec<u8>,
	burned: bool,
	used: bool,
	token_expires_at: i64,
	token_version_at_open: i64,
	email_hash_at_open: Vec<u8>,
	/// The subject's current folded revoke floor, read beside the pin.
	token_version: i64,
}

fn consent_of(row: &PgRow) -> Result<ConsentRow, DomainError> {
	Ok(ConsentRow {
		payment_id: row.try_get("payment_id").map_err(repo_err)?,
		subject: row.try_get("subject_user_id").map_err(repo_err)?,
		email: row.try_get("subject_email").map_err(repo_err)?,
		decision: row.try_get("decision").map_err(repo_err)?,
		decided_at: row.try_get("decided_at").map_err(repo_err)?,
		notified: row.try_get("notified").map_err(repo_err)?,
		attempts: row.try_get("attempts").map_err(repo_err)?,
		code_hash: row.try_get("code_hash").map_err(repo_err)?,
		burned: row.try_get("burned").map_err(repo_err)?,
		used: row.try_get("used").map_err(repo_err)?,
		token_expires_at: row.try_get("token_expires_at").map_err(repo_err)?,
		token_version_at_open: row.try_get("subject_token_version_at_open").map_err(repo_err)?,
		email_hash_at_open: row.try_get("subject_email_hash_at_open").map_err(repo_err)?,
		token_version: row.try_get("subject_token_version").map_err(repo_err)?,
	})
}

impl ConsentRow {
	fn view(&self) -> Result<ConsentView, DomainError> {
		Ok(ConsentView {
			subject: UserId::from_raw(self.subject),
			email: self.email.clone(),
			decision: ConsentDecision::parse(&self.decision)?,
			decided_at: self.decided_at.unwrap_or_default(),
			notified: self.notified,
			attempts_remaining: (MAX_CODE_ATTEMPTS - self.attempts).max(0) as u32,
			invalidated: self.invalidation(),
		})
	}

	/// Why this seat can no longer be trusted, or `None` while both pins still hold.
	///
	/// FAIL-CLOSED ON ANY MOVEMENT, not only on an increase: the revoke floor is monotonic
	/// in practice, but a floor that reads LOWER than the one frozen at open means the
	/// projection was rewritten under the seat, and that is not a state to execute out of.
	/// The mailbox is compared by digest rather than by address so the row and the seat
	/// stay comparable without either carrying the other's plaintext.
	fn invalidation(&self) -> Option<ConsentInvalidation> {
		pins_moved(self.token_version_at_open, self.token_version, &self.email_hash_at_open, &self.email)
	}
}

/// Which of the two pins frozen at open has moved, if either — shared by both seats, because a
/// revoke or a mailbox change voids an investor's consent whichever way it was given.
fn pins_moved(token_version_at_open: i64, token_version: i64, email_hash_at_open: &[u8], email: &str) -> Option<ConsentInvalidation> {
	if token_version != token_version_at_open {
		return Some(ConsentInvalidation::SessionsRevoked {
			at_open: token_version_at_open,
			now: token_version,
		});
	}
	if !ct_eq(&digest(email.as_bytes()), email_hash_at_open) {
		return Some(ConsentInvalidation::EmailChanged);
	}
	None
}

macro_rules! session_columns {
	() => {
		"c.payment_id, c.subject_user_id, u.email AS subject_email, c.decision, EXTRACT(EPOCH FROM c.decided_at)::bigint AS decided_at, \
		 c.subject_token_version_at_open, c.subject_email_hash_at_open, GREATEST(u.concierge_token_version, u.token_version) AS subject_token_version"
	};
}

/// The investor's in-cabinet seat behind one order, joined to its subject's address.
struct SessionRow {
	subject: Uuid,
	email: String,
	decision: String,
	decided_at: Option<i64>,
	token_version_at_open: i64,
	email_hash_at_open: Vec<u8>,
	token_version: i64,
}

fn session_of(row: &PgRow) -> Result<SessionRow, DomainError> {
	Ok(SessionRow {
		subject: row.try_get("subject_user_id").map_err(repo_err)?,
		email: row.try_get("subject_email").map_err(repo_err)?,
		decision: row.try_get("decision").map_err(repo_err)?,
		decided_at: row.try_get("decided_at").map_err(repo_err)?,
		token_version_at_open: row.try_get("subject_token_version_at_open").map_err(repo_err)?,
		email_hash_at_open: row.try_get("subject_email_hash_at_open").map_err(repo_err)?,
		token_version: row.try_get("subject_token_version").map_err(repo_err)?,
	})
}

impl SessionRow {
	fn invalidation(&self) -> Option<ConsentInvalidation> {
		pins_moved(self.token_version_at_open, self.token_version, &self.email_hash_at_open, &self.email)
	}

	fn view(&self) -> Result<SessionConsentView, DomainError> {
		Ok(SessionConsentView {
			subject: UserId::from_raw(self.subject),
			email: self.email.clone(),
			decision: ConsentDecision::parse(&self.decision)?,
			decided_at: self.decided_at.unwrap_or_default(),
			invalidated: self.invalidation(),
		})
	}
}

async fn session_of_payment(conn: &mut PgConnection, payment: Uuid, lock: bool) -> Result<Option<SessionRow>, DomainError> {
	let query = if lock {
		concat!(
			"SELECT ",
			session_columns!(),
			" FROM payment_session_consent c JOIN users u ON u.id = c.subject_user_id WHERE c.payment_id = $1 FOR UPDATE OF c"
		)
	} else {
		concat!(
			"SELECT ",
			session_columns!(),
			" FROM payment_session_consent c JOIN users u ON u.id = c.subject_user_id WHERE c.payment_id = $1"
		)
	};
	sqlx::query(query)
		.bind(payment)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.as_ref()
		.map(session_of)
		.transpose()
}

fn concierge_view(row: &PgRow) -> Result<ConciergeSeatView, DomainError> {
	let level: Option<String> = row.try_get("approval_level").map_err(repo_err)?;
	Ok(ConciergeSeatView {
		approval_id: row.try_get("approval_id").map_err(repo_err)?,
		level: level.as_deref().map(ApprovalLevel::parse).transpose()?,
		payload_hash: row.try_get("payload_hash").map_err(repo_err)?,
		destination: row.try_get("destination").map_err(repo_err)?,
	})
}

async fn concierge_of_payment(conn: &mut PgConnection, payment: Uuid) -> Result<Option<ConciergeSeatView>, DomainError> {
	sqlx::query("SELECT approval_id, approval_level, payload_hash, destination FROM payment_concierge_approval WHERE payment_id = $1")
		.bind(payment)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.as_ref()
		.map(concierge_view)
		.transpose()
}

/// The initiator's address. A missing row is an error, not an empty string: every order has
/// an initiator by FK, so `None` here means the projection is broken — and returning `""`
/// would let a surface render a money move proposed by nobody.
async fn email_of(conn: &mut PgConnection, id: UserId) -> Result<String, DomainError> {
	sqlx::query_scalar::<_, String>("SELECT email FROM users WHERE id = $1")
		.bind(id.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.ok_or_else(|| DomainError::Repository(format!("payment initiator {id} has no mirrored user row")))
}

async fn consilium_of(conn: &mut PgConnection, payment: Uuid) -> Result<Option<ConsiliumId>, DomainError> {
	let id: Option<Uuid> = sqlx::query_scalar("SELECT consilium_id FROM payment_approval WHERE payment_id = $1")
		.bind(payment)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
	Ok(id.map(ConsiliumId::from_raw))
}

/// The receiving end's recognisable detail — an investor's mirrored mailbox, a product's
/// title — or `None` for a singleton claim, an address, or a row that is not there (a
/// product deregistered after the order was opened still has a label; it just has no title).
/// Crate-visible so the consilium adapter states the same detail on the owners' mails.
pub(crate) async fn detail_of(conn: &mut PgConnection, to: &PaymentDestination) -> Result<Option<EndDetail>, DomainError> {
	Ok(match to.party() {
		Some(Party::User(user)) => sqlx::query_scalar::<_, String>("SELECT email FROM users WHERE id = $1")
			.bind(user.raw())
			.fetch_optional(&mut *conn)
			.await
			.map_err(repo_err)?
			.map(EndDetail::Mailbox),
		Some(Party::Service(service)) => sqlx::query_scalar::<_, String>("SELECT title FROM allocations WHERE service = $1")
			.bind(service.as_str())
			.fetch_optional(&mut *conn)
			.await
			.map_err(repo_err)?
			.map(EndDetail::ProductTitle),
		// Never a destination; the label says all there is.
		Some(Party::Treasury) | None => None,
	})
}

/// The destination as a MAIL states it: the canonical label, plus the recognisable detail
/// when there is one. The label alone is what the digest binds; the detail is what keeps
/// "investor 8f3e…" from being approved for the wrong person. A mailbox is masked here
/// because this string goes to someone who is not its owner.
pub(crate) fn mail_destination(terms: &PaymentTerms, detail: Option<&EndDetail>) -> String {
	let label = terms.destination_label();
	match detail {
		Some(EndDetail::Mailbox(email)) => format!("{label} ({})", mask_email(email)),
		Some(EndDetail::ProductTitle(title)) => format!("{label} ({title})"),
		None => label,
	}
}

/// Which way an investor's consent was being asked for: by a mailed token, or in the cabinet.
/// The notice that tells the people a dead consent concerns goes the way the consent went.
#[derive(Clone, Copy)]
enum SeatKind {
	Mailed,
	Session,
}

/// The `kind` of the notice that asks the investor to answer, and the one thing withdrawn
/// when the order closes before they do.
const CONSENT_NOTICE_KIND: &str = "payment_consent";
const OUTCOME_NOTICE_KIND: &str = "payment_outcome";

/// Where in the cabinet an order is answered. A path and not a URL: concierge pins it to its
/// own origin, so this plane never mints a host.
fn answer_link(payment: PaymentId) -> String {
	format!("/cabinet/wallet/action/payment/{}", payment.raw())
}

/// How a consent died without being answered — the pair concierge's `PaymentOutcomeMail`
/// takes, and a closed one on both sides: a burn is only ever wrong codes.
#[derive(Clone, Copy)]
enum ConsentEnd {
	Burned,
	Invalidated(ConsentInvalidation),
}

impl ConsentEnd {
	fn words(self) -> (&'static str, &'static str) {
		match self {
			Self::Burned => ("TOKEN_BURNED", "WRONG_CODES"),
			Self::Invalidated(cause) => ("INVALIDATED", cause.mail_reason()),
		}
	}
}

/// The amount as the payment-outcome mail must spell it: money and nothing else — the exact
/// decimal the consent invitation showed, one space, the currency code.
///
/// Concierge caps the number at 32 characters. A canonical 18-decimal amount only reaches
/// that past thirteen integer digits, and then the digits dropped are the smallest
/// fractional ones — the mail is notice about an order that moved nothing, not a record of
/// it, and an amount the relay refuses would be a mail nobody gets.
pub fn mail_amount(amount: Usdt) -> String {
	const MAX_NUMBER: usize = 32;
	let mut number = amount.to_decimal_string();
	if number.len() > MAX_NUMBER {
		number.truncate(MAX_NUMBER);
		if number.contains('.') {
			number.truncate(number.trim_end_matches('0').trim_end_matches('.').len());
		}
	}
	format!("{number} USDT")
}

/// Tell the people a dead consent concerns that the order died with it (#238), on the
/// caller's transaction so the notice commits with the rejection — but NEVER as a condition
/// of it. The transition this reports is the defence (a burn, a void, a cancelled
/// withdrawal); were a failed enqueue allowed to roll it back, a broken mail queue would
/// undo a burn and its attempt, and hand a guesser unlimited fifth tries. So the notice is
/// written inside a SAVEPOINT: on any failure only the savepoint is rolled back, the failure
/// is logged at error, and the caller commits the transition as if nothing had been mailed.
///
/// One key per copy, because concierge refuses a key reused for another recipient; the same
/// key on a replayed write is the no-op `enqueue` makes it.
async fn announce_consent_end(conn: &mut PgConnection, order: &PaymentOrder, end: ConsentEnd, seat: SeatKind) {
	let mut savepoint = match sqlx::Connection::begin(&mut *conn).await {
		Ok(savepoint) => savepoint,
		Err(err) => {
			tracing::error!(payment_id = %order.id(), "payments: could not open the savepoint for the consent-outcome notice; nobody is mailed: {err}");
			return;
		}
	};
	let queued = match seat {
		SeatKind::Mailed => queue_consent_end(&mut savepoint, order, end).await,
		SeatKind::Session => queue_session_end(&mut savepoint, order, end).await,
	};
	match queued {
		Ok(()) =>
			if let Err(err) = savepoint.commit().await {
				tracing::error!(payment_id = %order.id(), "payments: could not release the savepoint of the consent-outcome notice: {err}");
			},
		Err(err) => {
			tracing::error!(payment_id = %order.id(), "payments: the consent-outcome notice could not be queued; the order is closed regardless and nobody is mailed: {err}");
			// An error left the savepoint aborted, and Postgres refuses every later statement
			// of the transaction until it is rolled back — the commit of the transition
			// included. If even the rollback fails, that commit fails loudly instead.
			if let Err(err) = savepoint.rollback().await {
				tracing::error!(payment_id = %order.id(), "payments: could not roll back the savepoint of the consent-outcome notice: {err}");
			}
		}
	}
}

/// Who is told is decided by how the consent died:
///
/// - the SUBJECT, unless their mailbox is the thing that changed — then the address a copy
///   would reach is the one that just replaced theirs, possibly unverified, possibly an
///   attacker's, and it is not told that a payment out of this account was stopped;
/// - the INITIATOR, when that is somebody else — staff who opened the order and would
///   otherwise find it closed without a word.
///
/// Every copy names the SUBJECT by their identity-plane id, and concierge sends it only to
/// that person or to an admin/owner. The id is resolved here; `open` refused to seat a consent
/// without it and the bridge only ever fills a missing one in, so its absence is a broken
/// projection — logged, and nothing is queued, because a copy that cannot name its subject
/// is one concierge refuses.
async fn queue_consent_end(conn: &mut PgConnection, order: &PaymentOrder, end: ConsentEnd) -> Result<(), DomainError> {
	let PaymentApproval::SubjectConsent(subject) = order.requirement() else {
		return Ok(());
	};
	let mut recipients = Vec::with_capacity(2);
	if !matches!(end, ConsentEnd::Invalidated(ConsentInvalidation::EmailChanged)) {
		recipients.push(subject);
	}
	if order.initiator() != subject {
		recipients.push(order.initiator());
	}
	if recipients.is_empty() {
		return Ok(());
	}
	let subject_concierge_id: Option<Uuid> = sqlx::query_scalar("SELECT concierge_user_id FROM users WHERE id = $1")
		.bind(subject.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.flatten();
	let Some(subject_concierge_id) = subject_concierge_id else {
		tracing::error!(payment_id = %order.id(), %subject, "payments: the consent ended but its subject has no mirrored identity-plane id; nobody is mailed");
		return Ok(());
	};
	let detail = detail_of(conn, order.terms().to()).await?;
	let (outcome, reason) = end.words();
	let mail = GovernanceMail::PaymentOutcome(PaymentOutcome {
		subject_user_id: subject_concierge_id.to_string(),
		outcome: outcome.to_owned(),
		reason: reason.to_owned(),
		tier: order.tier().as_str().to_owned(),
		source: order.terms().source_label(),
		destination: mail_destination(order.terms(), detail.as_ref()),
		amount: mail_amount(order.terms().amount()),
		payment_id: order.id().to_string(),
	});
	for recipient in recipients {
		let key = format!("payment:{}:outcome:{recipient}", order.id());
		enqueue(conn, MailSubject::Payment(order.id().raw()), recipient.raw(), &key, &mail).await?;
	}
	Ok(())
}

/// The in-cabinet twin of [`queue_consent_end`], with the same audiences for the same reasons:
/// the subject, unless their mailbox is what changed, and the initiator when that is somebody
/// else. Only the invalidations exist here — there is no code to burn.
async fn queue_session_end(conn: &mut PgConnection, order: &PaymentOrder, end: ConsentEnd) -> Result<(), DomainError> {
	let PaymentApproval::SubjectConsent(subject) = order.requirement() else {
		return Ok(());
	};
	let ConsentEnd::Invalidated(cause) = end else {
		return Ok(());
	};
	let detail = detail_of(conn, order.terms().to()).await?;
	let what = format!("{} to {}", mail_amount(order.terms().amount()), mail_destination(order.terms(), detail.as_ref()));
	let why = match cause {
		ConsentInvalidation::SessionsRevoked { .. } => "the sessions of the account were revoked after the payment was opened",
		ConsentInvalidation::EmailChanged => "the mailbox of the account changed after the payment was opened",
	};
	let mut notices = Vec::with_capacity(2);
	if !matches!(cause, ConsentInvalidation::EmailChanged) {
		notices.push((subject, format!("The payment of {what} out of your account was stopped because {why}. Nothing was moved.")));
	}
	if order.initiator() != subject {
		notices.push((order.initiator(), format!("The payment of {what} that you opened was stopped because {why}. Nothing was moved.")));
	}
	for (recipient, body) in notices {
		let notice = ScopeNotice {
			user: recipient,
			topic: MONEY_MOVEMENT_TOPIC.to_owned(),
			kind: OUTCOME_NOTICE_KIND.to_owned(),
			title: "A payment was stopped".to_owned(),
			body,
			link: String::new(),
			dedupe_key: format!("payment:{}:outcome:{recipient}", order.id()),
		};
		scope_notifications::enqueue(conn, &notice, Some(order.id().raw())).await?;
	}
	Ok(())
}

/// Why the order's investor consent — mailed or given in the cabinet — no longer stands, read
/// under whatever lock the caller holds. `None` for an order with neither seat (a consilium's
/// or an approval's) and for a seat whose pins hold.
async fn seat_invalidation(conn: &mut PgConnection, payment: PaymentId) -> Result<Option<ConsentInvalidation>, DomainError> {
	if let Some(seat) = consent_of_payment(conn, payment.raw()).await? {
		return Ok(seat.invalidation());
	}
	Ok(session_of_payment(conn, payment.raw(), false).await?.and_then(|seat| seat.invalidation()))
}

async fn consent_of_payment(conn: &mut PgConnection, payment: Uuid) -> Result<Option<ConsentRow>, DomainError> {
	sqlx::query(consent_of_payment_query!())
		.bind(payment)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.as_ref()
		.map(consent_of)
		.transpose()
}

/// Assemble the view an order's surface needs. One extra round trip each for the initiator's
/// address, the consilium link and the consent seat — `find` is a single-row screen, and
/// `list` below does the same work set-at-a-time instead.
async fn view_of(conn: &mut PgConnection, order: PaymentOrder) -> Result<PaymentView, DomainError> {
	let initiator_email = email_of(conn, order.initiator()).await?;
	let consilium_id = consilium_of(conn, order.id().raw()).await?;
	let consent = consent_of_payment(conn, order.id().raw()).await?.as_ref().map(ConsentRow::view).transpose()?;
	let concierge = concierge_of_payment(conn, order.id().raw()).await?;
	let session = session_of_payment(conn, order.id().raw(), false).await?.as_ref().map(SessionRow::view).transpose()?;
	let destination_detail = detail_of(conn, order.terms().to()).await?;
	Ok(PaymentView {
		order,
		initiator_email,
		consilium_id,
		consent,
		concierge,
		session,
		destination_detail,
	})
}

/// The `payment_approval` link, read under whatever lock the caller holds, so a quorum can
/// only ever carry the order it was opened for.
async fn require_linked(conn: &mut PgConnection, payment: PaymentId, consilium: ConsiliumId) -> Result<(), DomainError> {
	match consilium_of(conn, payment.raw()).await? {
		Some(linked) if linked == consilium => Ok(()),
		Some(_) => Err(DomainError::Conflict("this consilium does not decide this payment".into())),
		None => Err(DomainError::Conflict("this payment is not decided by a consilium".into())),
	}
}

/// Reject a still-pending order on the caller's open transaction — the cascade of a
/// consilium verdict that is not an approval. Crate-visible so the consilium adapter closes
/// the order in the SAME transaction that records the quorum's refusal, expiry or
/// withdrawal: an order left `pending` under a dead consilium would sit for the rest of its
/// 72 hours looking answerable, and the sweeper would then expire what was in fact refused.
///
/// A no-op on an order that is already decided: the initiator's own withdrawal reaches the
/// order first and the consilium second, and the second must not fail over the first.
/// LOCK ORDER: the consilium row, then the order's — the only place the two nest, and
/// always in this direction.
pub(crate) async fn reject_on(conn: &mut PgConnection, id: PaymentId, at: i64) -> Result<PaymentOrder, DomainError> {
	let mut order = locked(conn, id).await?;
	if order.state().is_pending() {
		order.reject(at)?;
		persist(conn, &mut order).await?;
	}
	Ok(order)
}

/// Share-lock the consent subject's `users` row before the pins are read.
///
/// A revoke or a mailbox change is an UPDATE on `users`, held as a row lock until its
/// transaction commits. A pin read that does not wait on that lock can see the OLD version,
/// commit the effect, and be overtaken by a revocation that was already issued. `FOR SHARE`
/// waits for the writer and reads what it wrote. Taken AFTER the order's lock, as every
/// lock here is, so the `payments` row stays the first lock and the order total. A no-op
/// for a consilium- or concierge-decided order, which has no consent seat to join.
async fn lock_subject(conn: &mut PgConnection, payment: PaymentId) -> Result<(), DomainError> {
	sqlx::query(
		"SELECT 1 FROM users u WHERE u.id IN (SELECT subject_user_id FROM payment_consent WHERE payment_id = $1 UNION SELECT subject_user_id FROM payment_session_consent WHERE payment_id = $1) \
		 FOR SHARE OF u",
	)
	.bind(payment.raw())
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	Ok(())
}

/// Take back the consent invitation a closing order still has queued (#368): an investor
/// reached once the relay is back would be asked to consent to an order nobody can act on,
/// with a token that still resolves. Runs BEFORE [`locked`] in every path that closes an
/// order without a verdict — the worker flips the seat's `notified` while it holds the
/// invitation's row, so this must not wait on that row with the order held
/// ([`withdraw_undelivered_consent`]). Nothing of the transition is decided yet: an order
/// found already decided under the lock has lost nothing but an invitation to a decision
/// already made, and a rolled-back transition rolls this back with it. A no-op for a
/// fund-owned order, whose seat is a consilium's and queues no consent mail.
async fn withdraw_consent(conn: &mut PgConnection, id: PaymentId) -> Result<(), DomainError> {
	// The in-cabinet seat's invitation is a notice, not a mail, and carries no secret to blank;
	// it goes the same way for the same reason. It waits on no seat row, so unlike the mail it
	// cannot deadlock with the worker whatever lock the caller holds.
	let notices = scope_notifications::withdraw_undelivered(conn, id.raw(), CONSENT_NOTICE_KIND).await?;
	let withdrawn = withdraw_undelivered_consent(conn, id.raw()).await? + notices;
	// `debug`, not `info`: the transaction is still open here and may yet roll back (a
	// cancel refused as a stranger's, an expiry racing a consent), so this is what was
	// ATTEMPTED, not what happened.
	if withdrawn > 0 {
		tracing::debug!(payment_id = %id, withdrawn, "payments: withdrawing the undelivered consent invitation with the closing order");
	}
	Ok(())
}

/// Load an order `FOR UPDATE` — the opening move of every transition here. The `payments` row
/// is always locked first, so the transitions cannot deadlock each other; the one thing taken
/// ahead of it is the consent invitation a closing path withdraws, which no transition
/// touches once it holds the order.
async fn locked(conn: &mut PgConnection, id: PaymentId) -> Result<PaymentOrder, DomainError> {
	let row = sqlx::query(concat!("SELECT ", payment_columns!(), " FROM payments p WHERE p.id = $1 FOR UPDATE"))
		.bind(id.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.ok_or_else(|| DomainError::NotFound {
			entity: "payment",
			id: id.to_string(),
		})?;
	rehydrate(&row)
}

/// Persist a transition and drain its events.
///
/// The `relay` flag is asked of each EVENT rather than of the call: an order raises money
/// facts (`Reserved`, `Settled`) and audit facts (`Opened`, `Approved`, `Executed`, …) in the
/// same unit of work, and `PaymentEvent::relays` is the one place that answers which is which.
async fn persist(conn: &mut PgConnection, order: &mut PaymentOrder) -> Result<(), DomainError> {
	let affected = sqlx::query("UPDATE payments SET state = $2, decided_at = to_timestamp($3), executed_withdrawal_id = $4, failure_reason = $5, version = $6 WHERE id = $1")
		.bind(order.id().raw())
		.bind(order.state().as_str())
		.bind(order.decided_at().map(|at| at as f64))
		.bind(order.executed_withdrawal_id().map(|id| id.raw()))
		.bind(order.failure_reason())
		.bind(order.version() as i64)
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?
		.rows_affected();
	if affected != 1 {
		return Err(DomainError::Repository("payment row vanished under lock".into()));
	}
	outbox::drain_to_outbox_by(conn, order, PaymentEvent::relays).await
}

/// Refuse a seat that is not the one the order's terms call for.
///
/// The requirement is read off the SOURCE and the seat is minted by the caller, so the two
/// are two statements of one fact — and the interesting failure is where they disagree: an
/// owner quorum seated over an investor's money, or one investor's consent over the fund's.
/// The schema refuses both too (each seat table carries a composite FK into `payments`), but
/// a foreign-key error is an infrastructure string; this names the mismatch before a row is
/// written.
fn require_seat_matches(seat: &ApprovalSeat, requirement: PaymentApproval) -> Result<(), DomainError> {
	match (seat, requirement) {
		(ApprovalSeat::Consilium(_) | ApprovalSeat::Concierge, PaymentApproval::OwnerConsilium) => Ok(()),
		(ApprovalSeat::Consent(credential), PaymentApproval::SubjectConsent(subject)) if credential.subject == subject => Ok(()),
		(ApprovalSeat::Session(session), PaymentApproval::SubjectConsent(subject)) if session.subject == subject => Ok(()),
		(ApprovalSeat::Consent(_) | ApprovalSeat::Session(_), PaymentApproval::SubjectConsent(_)) =>
			Err(DomainError::Validation("the consent seat names an investor other than the order's source".into())),
		(ApprovalSeat::Consilium(_) | ApprovalSeat::Concierge, PaymentApproval::SubjectConsent(_)) =>
			Err(DomainError::Validation("an investor's own money is decided by that investor's consent, not by the owners".into())),
		(ApprovalSeat::Consent(_) | ApprovalSeat::Session(_), PaymentApproval::OwnerConsilium) =>
			Err(DomainError::Validation("fund-owned money is decided by the owners, not by one investor's consent".into())),
	}
}

/// The destination as the owners are shown it on the approval: the label and the recognisable
/// detail, falling back to the label alone when the detail would not fit the approval's text
/// field (a long product title) or would be refused in it (a link).
fn approval_destination(terms: &PaymentTerms, detail: Option<&EndDetail>) -> String {
	let full = mail_destination(terms, detail);
	let lowered = full.to_lowercase();
	if full.len() <= domain::payments::APPROVAL_TEXT_MAX_BYTES && !lowered.contains("://") && !lowered.contains("www.") && !full.chars().any(char::is_control) {
		full
	} else {
		terms.destination_label()
	}
}

/// The `(kind, id)` column pair for a destination, plus the address pair. Exactly one side is
/// populated, which is what `payments_destination_is_coherent` states to the database.
fn destination_columns(to: &PaymentDestination) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
	match to {
		PaymentDestination::Internal(party) => (Some(party.kind_str().to_owned()), Some(party.id_str()), None, None),
		PaymentDestination::External { network, address } => (None, None, Some(network.as_str().to_owned()), Some(address.as_str().to_owned())),
	}
}

#[async_trait]
impl PaymentRepository for PgPayments {
	async fn open(&self, order: &mut PaymentOrder, seat: ApprovalSeat, consent_url_base: &str) -> Result<(), DomainError> {
		require_seat_matches(&seat, order.requirement())?;
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// A payment spends the same claims a withdrawal or a subscription does. Skipping this
		// would not fail loudly — it would silently stop the serialization those two rely on
		// from covering the writer that was added last.
		outbox::lock_claim(&mut tx, &order.source_claim()).await?;
		let (to_kind, to_id, to_network, to_address) = destination_columns(order.terms().to());
		let inserted = sqlx::query(
			"INSERT INTO payments (id, state, from_kind, from_id, to_kind, to_id, to_network, to_address, amount, reason, payload_hash, initiator_user_id, expires_at, version) \
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, to_timestamp($13), $14)",
		)
		.bind(order.id().raw())
		.bind(order.state().as_str())
		.bind(order.terms().from().kind_str())
		.bind(order.terms().from().id_str())
		.bind(to_kind)
		.bind(to_id)
		.bind(to_network)
		.bind(to_address)
		.bind(order.terms().amount().base_units().to_string())
		.bind(order.terms().reason().as_str())
		.bind(order.payload_hash().as_slice())
		.bind(order.initiator().raw())
		.bind(order.expires_at() as f64)
		.bind(order.version() as i64)
		.execute(&mut *tx)
		.await;
		if let Err(sqlx::Error::Database(err)) = &inserted
			&& err.code().as_deref() == Some(UNIQUE_VIOLATION)
		{
			// The partial unique index spoke: one open order per fund-owned source claim. A
			// refusal, not a retry — the operator has a live request to finish or withdraw.
			return Err(already_open());
		}
		inserted.map_err(repo_err)?;

		match seat {
			ApprovalSeat::Consilium(consilium) => {
				// The composite FK on (payment_id, fund_owned) refuses a quorum seat over an
				// investor's order — the mirror of the consent seat's FK below — so the check
				// at the top is the first statement of §3 and the schema is the second.
				sqlx::query("INSERT INTO payment_approval (payment_id, consilium_id) VALUES ($1, $2)")
					.bind(order.id().raw())
					.bind(consilium.raw())
					.execute(&mut *tx)
					.await
					.map_err(repo_err)?;
			}
			ApprovalSeat::Consent(credential) => {
				// The composite FK on (payment_id, subject_user_id) refuses a seat naming
				// anyone but this order's own source user, so §3's rule is the database's
				// statement rather than a check this insert is trusted to have made.
				sqlx::query(
					"INSERT INTO payment_consent (payment_id, subject_user_id, token_hash, code_hash, expires_at, subject_token_version_at_open, subject_email_hash_at_open) \
					 VALUES ($1, $2, $3, $4, to_timestamp($5), $6, $7)",
				)
				.bind(order.id().raw())
				.bind(credential.subject.raw())
				.bind(credential.token_hash.as_slice())
				.bind(credential.code_hash.as_slice())
				.bind(order.expires_at() as f64)
				.bind(credential.token_version_at_open as i64)
				.bind(credential.email_hash_at_open.as_slice())
				.execute(&mut *tx)
				.await
				.map_err(repo_err)?;

				// THE INVITATION, IN THIS SAME TRANSACTION. An order that commits with no
				// record of the mail asking its subject is the silent stall an unmailed
				// consilium is: it looks open for 72h and was unanswerable from the first
				// instant. The recipient is addressed by their id in the plane that OWNS
				// identities — concierge refuses the mail unless that id is the payment's
				// subject — and without the mirrored id there is no safe address at all.
				let subject_concierge_id: Option<Uuid> = sqlx::query_scalar("SELECT concierge_user_id FROM users WHERE id = $1")
					.bind(credential.subject.raw())
					.fetch_optional(&mut *tx)
					.await
					.map_err(repo_err)?
					.flatten();
				let Some(subject_concierge_id) = subject_concierge_id else {
					return Err(DomainError::Conflict(
						"the investor whose consent this payment needs has no mirrored identity-plane id, so no consent mail could be addressed to them".into(),
					));
				};
				let initiator_email = email_of(&mut tx, order.initiator()).await?;
				let detail = detail_of(&mut tx, order.terms().to()).await?;
				let mail = GovernanceMail::PaymentConsent(PaymentConsent {
					payment_id: order.id().to_string(),
					subject_user_id: subject_concierge_id.to_string(),
					initiator_email,
					tier: order.tier().as_str().to_owned(),
					source: order.terms().source_label(),
					destination: mail_destination(order.terms(), detail.as_ref()),
					amount: order.terms().amount().to_decimal_string(),
					reason: order.terms().reason().as_str().to_owned(),
					payload_hash: order.payload_hash_hex(),
					expires_at: order.expires_at(),
					approval_url: format!("{}/{}", consent_url_base.trim_end_matches('/'), credential.token),
					code: credential.code,
				});
				let key = format!("payment:{}:consent:{}", order.id(), credential.subject);
				enqueue(&mut tx, MailSubject::Payment(order.id().raw()), credential.subject.raw(), &key, &mail).await?;
			}
			ApprovalSeat::Concierge => {
				// The destination is rendered NOW and stored with the hash: the approval is
				// proposed, and the decision checked, from these two, and the detail (a mailbox,
				// a title) may change while the order waits.
				let detail = detail_of(&mut tx, order.terms().to()).await?;
				let destination = approval_destination(order.terms(), detail.as_ref());
				let payload = order.approval_payload(&destination)?;
				sqlx::query("INSERT INTO payment_concierge_approval (payment_id, destination, payload_hash) VALUES ($1, $2, $3)")
					.bind(order.id().raw())
					.bind(&destination)
					.bind(hex_digest(payload.as_bytes()))
					.execute(&mut *tx)
					.await
					.map_err(repo_err)?;
			}
			ApprovalSeat::Session(session) => {
				sqlx::query("INSERT INTO payment_session_consent (payment_id, subject_user_id, subject_token_version_at_open, subject_email_hash_at_open) VALUES ($1, $2, $3, $4)")
					.bind(order.id().raw())
					.bind(session.subject.raw())
					.bind(session.token_version_at_open as i64)
					.bind(session.email_hash_at_open.as_slice())
					.execute(&mut *tx)
					.await
					.map_err(repo_err)?;
				// The notice that asks them, in this same transaction (see the mailed seat). The
				// notice is addressed through the mirror at send time; the check that the mirror
				// exists is made here all the same, because an investor concierge has never
				// signed in cannot open the cabinet to answer.
				let mirrored: Option<Uuid> = sqlx::query_scalar("SELECT concierge_user_id FROM users WHERE id = $1")
					.bind(session.subject.raw())
					.fetch_optional(&mut *tx)
					.await
					.map_err(repo_err)?
					.flatten();
				if mirrored.is_none() {
					return Err(DomainError::Conflict(
						"the investor whose confirmation this payment needs has no mirrored identity-plane id, so they could not be told to answer it".into(),
					));
				}
				let detail = detail_of(&mut tx, order.terms().to()).await?;
				let notice = ScopeNotice {
					user: session.subject,
					topic: MONEY_MOVEMENT_TOPIC.to_owned(),
					kind: CONSENT_NOTICE_KIND.to_owned(),
					title: "Confirm a payment from your account".to_owned(),
					body: format!(
						"A payment of {} from your account to {} waits for your confirmation. Reason: {}. Open the cabinet to confirm or decline it; it lapses on its own if you do nothing.",
						mail_amount(order.terms().amount()),
						mail_destination(order.terms(), detail.as_ref()),
						order.terms().reason(),
					),
					link: answer_link(order.id()),
					dedupe_key: format!("payment:{}:consent:{}", order.id(), session.subject),
				};
				scope_notifications::enqueue(&mut tx, &notice, Some(order.id().raw())).await?;
			}
		}

		outbox::drain_to_outbox_by(&mut tx, order, PaymentEvent::relays).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(())
	}

	async fn find(&self, id: PaymentId) -> Result<Option<PaymentView>, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		let Some(row) = sqlx::query(concat!("SELECT ", payment_columns!(), " FROM payments p WHERE p.id = $1"))
			.bind(id.raw())
			.fetch_optional(&mut *conn)
			.await
			.map_err(repo_err)?
		else {
			return Ok(None);
		};
		Ok(Some(view_of(&mut conn, rehydrate(&row)?).await?))
	}

	async fn has_open_against(&self, source: &Party) -> Result<bool, DomainError> {
		sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM payments WHERE from_kind = $1 AND from_id IS NOT DISTINCT FROM $2 AND state IN ('pending', 'approved'))")
			.bind(source.kind_str())
			.bind(source.id_str())
			.fetch_one(&self.pool)
			.await
			.map_err(repo_err)
	}

	async fn record_approval(&self, id: PaymentId, consilium: ConsiliumId, at: i64) -> Result<PaymentView, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let mut order = locked(&mut tx, id).await?;
		require_linked(&mut tx, id, consilium).await?;
		order.approve(at)?;
		persist(&mut tx, &mut order).await?;
		let view = view_of(&mut tx, order).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(view)
	}

	async fn record_rejection(&self, id: PaymentId, at: i64) -> Result<PaymentView, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let order = reject_on(&mut tx, id, at).await?;
		let view = view_of(&mut tx, order).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(view)
	}

	async fn cancel(&self, id: PaymentId, by: UserId, at: i64) -> Result<PaymentView, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		withdraw_consent(&mut tx, id).await?;
		let mut order = locked(&mut tx, id).await?;
		if order.initiator() != by {
			return Err(DomainError::Forbidden("only the operator who opened this payment may withdraw it".into()));
		}
		order.cancel(at)?;
		persist(&mut tx, &mut order).await?;
		let view = view_of(&mut tx, order).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(view)
	}

	async fn invitation(&self, token_hash: &[u8; DIGEST_BYTES], at: i64) -> Result<ConsentInvitation, DomainError> {
		// STRICTLY read-only. Gmail, Outlook SafeLinks and corporate gateways fetch every URL
		// in a message; were this to count an attempt or spend the token, a scanner would burn
		// an investor's consent before they ever opened the mail.
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		let Some(seat) = sqlx::query(consent_by_token_query!())
			.bind(token_hash.as_slice())
			.fetch_optional(&mut *conn)
			.await
			.map_err(repo_err)?
			.as_ref()
			.map(consent_of)
			.transpose()?
		else {
			return Err(consent_not_found());
		};
		let order = order_of(&mut conn, seat.payment_id).await?;
		// Unknown, expired, burned, spent and closed all leave by the same door, so this
		// surface cannot be used to work out which one a given token hit.
		if seat.burned || seat.used || seat.token_expires_at <= at || !order.state().is_pending() {
			return Err(consent_not_found());
		}
		let initiator_email = email_of(&mut conn, order.initiator()).await?;
		let detail = detail_of(&mut conn, order.terms().to()).await?;
		Ok(invitation_of(order, initiator_email, &seat, ConsentDecision::Pending, detail))
	}

	async fn submit(&self, token_hash: &[u8; DIGEST_BYTES], code: &str, decision: ConsentDecision, audit: &ConsentAudit, at: i64) -> Result<ConsentOutcome, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// LOCK ORDER: THE PAYMENT ROW, ALWAYS, AND FIRST. Every other transition here goes
		// through `locked()`. Resolving the token with an UNLOCKED read first, then taking the
		// order's lock, then re-reading the seat under it is what keeps that order total —
		// a single statement driving from `payment_consent` would lock the seat first and two
		// concurrent submissions would deadlock. The re-read is what makes the unlocked probe
		// safe: every value the decision below turns on is taken under the lock.
		let Some(probe) = sqlx::query_scalar::<_, Uuid>("SELECT payment_id FROM payment_consent WHERE token_hash = $1")
			.bind(token_hash.as_slice())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?
		else {
			return Err(consent_not_found());
		};
		let mut order = locked(&mut tx, PaymentId::from_raw(probe)).await?;
		let Some(mut seat) = sqlx::query(concat!(consent_by_token_query!(), " FOR UPDATE OF c"))
			.bind(token_hash.as_slice())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?
			.as_ref()
			.map(consent_of)
			.transpose()?
		else {
			return Err(consent_not_found());
		};
		let already = ConsentDecision::parse(&seat.decision)?;

		// A burned token and an expired one are indistinguishable from an unknown one. The
		// token's OWN deadline is checked here rather than trusting the sweep to have run, so
		// "an expired order can never execute" holds even for an answer that arrives late on
		// one the sweeper has not reached yet.
		if seat.burned || seat.token_expires_at <= at {
			return Err(consent_not_found());
		}
		if !order.state().is_pending() && already == ConsentDecision::Pending {
			return Err(consent_not_found());
		}

		// THE PINS, RE-CHECKED BEFORE THE CODE IS EVEN COMPARED. A revoked session or a
		// moved mailbox voids the seat whatever the code says, so no attempt is charged and no
		// answer is recorded; the order fails closed instead — with one seat there is nobody
		// to re-issue it to — and the holder is told why rather than shown the opaque door,
		// because holding a live token already proves the seat exists.
		if already == ConsentDecision::Pending
			&& let Some(cause) = seat.invalidation()
		{
			order.reject(at)?;
			persist(&mut tx, &mut order).await?;
			announce_consent_end(&mut tx, &order, ConsentEnd::Invalidated(cause), SeatKind::Mailed).await;
			tx.commit().await.map_err(repo_err)?;
			return Err(DomainError::Conflict(cause.to_string()));
		}

		let correct = ct_eq(&digest(code.as_bytes()), &seat.code_hash);

		// ONLY A SEAT THAT CAN STILL CHANGE SOMETHING PAYS FOR A GUESS. A seat that has
		// already answered is charged nothing: its decision is recorded and immutable, so
		// there is nothing left for an attempt to protect — and charging it would push a seat
		// that answered on its fifth try to 6, violate `CHECK (attempts <= 5)` and turn every
		// later retry of an idempotent request into a 503.
		if already == ConsentDecision::Pending {
			// Counted BEFORE comparing, in this same transaction: comparing first and counting
			// after would let N concurrent guesses all read the same pre-increment counter.
			// `LEAST` clamps as defence in depth, and `RETURNING` keeps the Rust value and the
			// column identical rather than mirroring the arithmetic in two places.
			seat.attempts = sqlx::query_scalar("UPDATE payment_consent SET attempts = LEAST(attempts + 1, $2) WHERE payment_id = $1 RETURNING attempts")
				.bind(seat.payment_id)
				.bind(MAX_CODE_ATTEMPTS)
				.fetch_one(&mut *tx)
				.await
				.map_err(repo_err)?;

			if !correct {
				if seat.attempts >= MAX_CODE_ATTEMPTS {
					// THE BURN FAILS THE PAYMENT CLOSED. With one seat there is no other party
					// to alert, so the exhausted token is a refusal of the money move, not a
					// detector that escalates to somebody who can approve instead.
					sqlx::query("UPDATE payment_consent SET burned_at = now() WHERE payment_id = $1")
						.bind(seat.payment_id)
						.execute(&mut *tx)
						.await
						.map_err(repo_err)?;
					order.reject(at)?;
					persist(&mut tx, &mut order).await?;
					announce_consent_end(&mut tx, &order, ConsentEnd::Burned, SeatKind::Mailed).await;
					tx.commit().await.map_err(repo_err)?;
					// From here on this token answers exactly like an unknown one.
					return Err(consent_not_found());
				}
				// The attempt must survive the refusal, so this commits rather than rolling back.
				tx.commit().await.map_err(repo_err)?;
				let remaining = MAX_CODE_ATTEMPTS - seat.attempts;
				// `Validation` (→ INVALID_ARGUMENT → HTTP 400) and NOT `Forbidden`, which the
				// BFF folds into the same opaque 404 an unknown token gets — under that mapping
				// "incorrect code, N remaining" never reaches the human, who then retries until
				// they burn their own consent. It reveals nothing: holding a live token already
				// proves the token exists, and the five-attempt ceiling is what guards the code.
				return Err(DomainError::Validation(format!("incorrect code — {remaining} attempts remaining")));
			}
		} else if !correct {
			// An answered seat, wrong code: nothing was counted, so nothing can burn.
			return Err(DomainError::Validation("incorrect code".to_owned()));
		}

		if already != ConsentDecision::Pending {
			// A retried submission resolves to its idempotent answer; a DIFFERENT one is
			// refused rather than silently overwriting a recorded decision.
			if already != decision {
				return Err(DomainError::Conflict("this payment has already been answered".into()));
			}
			let view = view_of(&mut tx, order).await?;
			tx.commit().await.map_err(repo_err)?;
			return Ok(ConsentOutcome {
				payment: view,
				decided: false,
				approved: false,
			});
		}

		sqlx::query(
			// `payload_hash` is captured on the seat, not just on the order: the order's own
			// hash is re-verified at execution, but nothing else records WHICH terms the
			// investor personally signed. With it an auditor can prove the terms shown at the
			// moment of that consent are the terms that were paid.
			"UPDATE payment_consent SET decision = $2, decided_at = to_timestamp($3), used_at = to_timestamp($3), client_ip = $4, user_agent = $5, payload_hash = $6 WHERE payment_id = $1",
		)
		.bind(seat.payment_id)
		.bind(decision.as_str())
		.bind(at as f64)
		.bind(&audit.client_ip)
		.bind(&audit.user_agent)
		.bind(order.payload_hash().as_slice())
		.execute(&mut *tx)
		.await
		.map_err(repo_err)?;

		match decision {
			ConsentDecision::Approve => order.approve(at)?,
			ConsentDecision::Reject => order.reject(at)?,
			// Unreachable: the surface answers with a decision, never with the absence of one.
			// Stated as a refusal rather than a panic because the value crosses a wire.
			ConsentDecision::Pending => return Err(DomainError::Validation("a consent must approve or reject".into())),
		}
		let approved = order.state() == PaymentState::Approved;
		persist(&mut tx, &mut order).await?;
		let view = view_of(&mut tx, order).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(ConsentOutcome {
			payment: view,
			decided: true,
			approved,
		})
	}

	/// ONE CLOCK, AND NO ROW CAN KILL THE SWEEP. The selection binds the caller's `at` rather
	/// than reading Postgres' `now()`, so the row set and the transition answer the same
	/// question; a per-row failure warns and continues, so one wedged order cannot keep the
	/// rest of the queue from closing.
	async fn expire_due(&self, at: i64) -> Result<usize, DomainError> {
		let due: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM payments WHERE state = 'pending' AND expires_at <= to_timestamp($1)")
			.bind(at)
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		let mut closed = 0;
		for id in due {
			match self.expire_one(id, at).await {
				Ok(true) => closed += 1,
				Ok(false) => {}
				Err(err) => tracing::warn!(payment_id = %id, "payments: could not expire (will retry next sweep): {err}"),
			}
		}
		Ok(closed)
	}

	async fn awaiting_execution(&self) -> Result<Vec<PaymentId>, DomainError> {
		// `execution_failed` is deliberately absent: nothing retries silently.
		let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM payments WHERE state = 'approved' ORDER BY created_at")
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(ids.into_iter().map(PaymentId::from_raw).collect())
	}

	async fn reservation_status(&self, id: PaymentId, reserve_tid: u128) -> Result<ReservationStatus, DomainError> {
		let applied: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM saga_steps WHERE tb_transfer_id = $1)")
			.bind(&reserve_tid.to_be_bytes()[..])
			.fetch_one(&self.pool)
			.await
			.map_err(repo_err)?;
		if applied {
			return Ok(ReservationStatus::Applied);
		}
		let parked: bool =
			sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM outbox WHERE aggregate = 'payment' AND aggregate_id = $1 AND parked_at IS NOT NULL AND payload::jsonb ->> 'type' = 'reserved')")
				.bind(id.raw())
				.fetch_one(&self.pool)
				.await
				.map_err(repo_err)?;
		Ok(if parked { ReservationStatus::Parked } else { ReservationStatus::Pending })
	}

	async fn record_execution(&self, id: PaymentId, outcome: ExecutionOutcome, at: i64) -> Result<PaymentView, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let mut order = locked(&mut tx, id).await?;
		// Only an order still `approved` is at stake — a repeat over a terminal order is the
		// idempotent retry and must stay one: no second void, no second notice.
		let void = match &outcome {
			// THE PINS AGAIN, AT THE MOMENT THAT SPENDS THE MONEY. Consent and execution can
			// be 72h apart; a `RevokeTokens` in between must not be overtaken by an execution
			// the investor authorized before it. The failure is committed and then reported
			// as an error, so a caller that only checks for `Ok` cannot mistake it for success.
			ExecutionOutcome::Executed(effect) if order.state() == PaymentState::Approved => {
				lock_subject(&mut tx, id).await?;
				match seat_invalidation(&mut tx, id).await? {
					Some(cause) => {
						let withdrawal = match effect {
							PaymentEffect::Withdrawal(withdrawal) => Some(*withdrawal),
							PaymentEffect::Transfer => None,
						};
						Some((cause, withdrawal, true))
					}
					None => None,
				}
			}
			// The caller found the void before recording anything — but an earlier attempt
			// may have created the withdrawal and died, so it is voided here all the same.
			ExecutionOutcome::ConsentVoid { cause, withdrawal } if order.state() == PaymentState::Approved => Some((*cause, *withdrawal, false)),
			_ => None,
		};
		if let Some((cause, withdrawal, raise)) = void {
			// THE L1 WINDOW. The execution path reads `invalidated` before it creates the
			// withdrawal, but a revocation can land between that read and this lock; by then
			// the withdrawal exists and, left alone, ships. Voiding it HERE, under the order's
			// lock and in the failure's own transaction, is what closes the window — the one
			// place two aggregates move together, because the alternative is money leaving
			// under a consent that no longer stands. A payment's withdrawal is always created
			// `Queued` (never dispatched on creation) precisely so that the void is possible.
			if let Some(withdrawal) = withdrawal {
				match withdrawals::cancel_on(&mut tx, withdrawal).await {
					// Absent NOW: nothing to void here. That does not mean no attempt will create
					// it — the other caller of `execute` may be creating it right now, outside
					// this lock — and that late withdrawal is voided when its record arrives
					// over the closed order (below).
					Ok(_) | Err(DomainError::NotFound { .. }) => {}
					// Past `Queued` the broadcast may have landed and the cardinal rule forbids
					// the void. The effect then EXISTS whatever the pins say, and the honest
					// record is that it does; a failure written over a shipped withdrawal would
					// be the lie that sticks — and would mail that nothing moved. Logged at
					// error so an operator sees the one case the pins could not stop.
					Err(DomainError::Conflict(state)) => {
						tracing::error!(payment_id = %id, %withdrawal, why = %cause, "payments: the consent pins moved after the withdrawal was already dispatched ({state}); recording the effect that exists");
						order.mark_executed(PaymentEffect::Withdrawal(withdrawal), at)?;
						persist(&mut tx, &mut order).await?;
						let view = view_of(&mut tx, order).await?;
						tx.commit().await.map_err(repo_err)?;
						return Ok(view);
					}
					Err(err) => return Err(err),
				}
			}
			order.mark_execution_failed(cause.to_string(), at)?;
			persist(&mut tx, &mut order).await?;
			let seat = if session_of_payment(&mut tx, id.raw(), false).await?.is_some() {
				SeatKind::Session
			} else {
				SeatKind::Mailed
			};
			announce_consent_end(&mut tx, &order, ConsentEnd::Invalidated(cause), seat).await;
			if raise {
				tx.commit().await.map_err(repo_err)?;
				return Err(DomainError::Conflict(cause.to_string()));
			}
			let view = view_of(&mut tx, order).await?;
			tx.commit().await.map_err(repo_err)?;
			return Ok(view);
		}
		// THE LOSING CREATOR. `execute` has two callers (the inline one after a consent, and
		// the sweeper) and creates the withdrawal outside this lock. One can find the order
		// approved and go to create it while the other fails the order — on a void found
		// with nothing created yet, or any other refusal — and commits first. The late
		// withdrawal then arrives `Queued` under an order that will never record it, and
		// left alone the dispatcher pays it. Its record is refused as before, but the refusal
		// first voids it, in this same transaction. Only a CLOSED order is judged here: an
		// executed one keeps its idempotent answer below, and an approved one records.
		if let ExecutionOutcome::Executed(PaymentEffect::Withdrawal(withdrawal)) = outcome
			&& matches!(
				order.state(),
				PaymentState::ExecutionFailed | PaymentState::Rejected | PaymentState::Expired | PaymentState::Cancelled
			) {
			match withdrawals::cancel_on(&mut tx, withdrawal).await {
				Ok(_) | Err(DomainError::NotFound { .. }) => {}
				// Already dispatched: nothing may void it now, and the order's record says it
				// did not execute. The one case the lock order cannot close — logged at error so
				// an operator reconciles it by hand.
				Err(DomainError::Conflict(state)) => {
					tracing::error!(payment_id = %id, %withdrawal, order_state = order.state().as_str(), "payments: a withdrawal created for an order that had already closed was dispatched before it could be voided ({state})");
				}
				Err(err) => return Err(err),
			}
			tx.commit().await.map_err(repo_err)?;
			return Err(DomainError::Conflict(format!("payment is {}, not executable", order.state().as_str())));
		}
		match outcome {
			ExecutionOutcome::Executed(effect) => order.mark_executed(effect, at)?,
			ExecutionOutcome::Failed(reason) => order.mark_execution_failed(reason, at)?,
			ExecutionOutcome::ConsentVoid { cause, .. } => order.mark_execution_failed(cause.to_string(), at)?,
		}
		persist(&mut tx, &mut order).await?;
		let view = view_of(&mut tx, order).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(view)
	}

	async fn record_concierge_approval(&self, id: PaymentId, approval_id: &str, at: i64) -> Result<PaymentView, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let mut order = locked(&mut tx, id).await?;
		let known: Option<Option<String>> = sqlx::query_scalar("SELECT approval_id FROM payment_concierge_approval WHERE payment_id = $1 FOR UPDATE")
			.bind(id.raw())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		match known {
			None => return Err(DomainError::Conflict("this payment is not decided by a concierge approval".into())),
			Some(Some(known)) if known != approval_id => return Err(DomainError::Conflict("another approval decides this payment".into())),
			Some(Some(_)) => {}
			Some(None) => {
				sqlx::query("UPDATE payment_concierge_approval SET approval_id = $2 WHERE payment_id = $1")
					.bind(id.raw())
					.bind(approval_id)
					.execute(&mut *tx)
					.await
					.map_err(repo_err)?;
			}
		}
		// A decision served again after it was acted on: approved once, reserved once. An order
		// that ended otherwise is refused by `approve` — a late decision never reaches money.
		if !matches!(order.state(), PaymentState::Approved | PaymentState::Executed) {
			order.approve(at)?;
			persist(&mut tx, &mut order).await?;
		}
		let view = view_of(&mut tx, order).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(view)
	}

	async fn answer_session_consent(&self, id: PaymentId, subject: UserId, decision: ConsentDecision, audit: &ConsentAudit, at: i64) -> Result<ConsentOutcome, DomainError> {
		if decision == ConsentDecision::Pending {
			return Err(DomainError::Validation("a consent must approve or reject".into()));
		}
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// The order's lock first, then the subject's row (`lock_subject`), as everywhere here.
		let mut order = match locked(&mut tx, id).await {
			Ok(order) => order,
			Err(DomainError::NotFound { .. }) => return Err(consent_not_found()),
			Err(err) => return Err(err),
		};
		lock_subject(&mut tx, id).await?;
		// Another investor's order, one with no such seat and one that does not exist leave by
		// the same door, so the surface cannot be used to find other people's payments.
		let Some(seat) = session_of_payment(&mut tx, id.raw(), true).await?.filter(|seat| seat.subject == subject.raw()) else {
			return Err(consent_not_found());
		};
		let already = ConsentDecision::parse(&seat.decision)?;
		if already != ConsentDecision::Pending {
			// A retried answer resolves to its idempotent result; a DIFFERENT one is refused
			// rather than silently overwriting a recorded decision.
			if already != decision {
				return Err(DomainError::Conflict("this payment has already been answered".into()));
			}
			let view = view_of(&mut tx, order).await?;
			tx.commit().await.map_err(repo_err)?;
			return Ok(ConsentOutcome {
				payment: view,
				decided: false,
				approved: false,
			});
		}
		// The order's own deadline is checked here rather than trusting the sweep to have run,
		// so an expired order can never be approved by an answer that arrives late.
		if !order.state().is_pending() || order.expires_at() <= at {
			return Err(consent_not_found());
		}
		// THE PINS, BEFORE THE ANSWER COUNTS. A revoked session or a moved mailbox voids the
		// seat whatever the click says: the order fails closed and both audiences are told.
		if let Some(cause) = seat.invalidation() {
			order.reject(at)?;
			persist(&mut tx, &mut order).await?;
			announce_consent_end(&mut tx, &order, ConsentEnd::Invalidated(cause), SeatKind::Session).await;
			scope_notifications::withdraw_undelivered(&mut tx, id.raw(), CONSENT_NOTICE_KIND).await?;
			tx.commit().await.map_err(repo_err)?;
			return Err(DomainError::Conflict(cause.to_string()));
		}
		sqlx::query("UPDATE payment_session_consent SET decision = $2, decided_at = to_timestamp($3), client_ip = $4, user_agent = $5, payload_hash = $6 WHERE payment_id = $1")
			.bind(id.raw())
			.bind(decision.as_str())
			.bind(at as f64)
			.bind(&audit.client_ip)
			.bind(&audit.user_agent)
			.bind(order.payload_hash().as_slice())
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		match decision {
			ConsentDecision::Approve => order.approve(at)?,
			ConsentDecision::Reject => order.reject(at)?,
			ConsentDecision::Pending => unreachable!("refused above"),
		}
		let approved = order.state() == PaymentState::Approved;
		persist(&mut tx, &mut order).await?;
		// Answered: the invitation that has not left the queue yet is moot.
		scope_notifications::withdraw_undelivered(&mut tx, id.raw(), CONSENT_NOTICE_KIND).await?;
		let view = view_of(&mut tx, order).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(ConsentOutcome {
			payment: view,
			decided: true,
			approved,
		})
	}

	async fn awaiting_session_consent(&self, subject: UserId) -> Result<Vec<PaymentView>, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		let ids: Vec<Uuid> = sqlx::query_scalar(
			"SELECT p.id FROM payments p JOIN payment_session_consent c ON c.payment_id = p.id \
			 WHERE c.subject_user_id = $1 AND c.decision = 'pending' AND p.state = 'pending' AND p.expires_at > now() ORDER BY p.created_at DESC",
		)
		.bind(subject.raw())
		.fetch_all(&mut *conn)
		.await
		.map_err(repo_err)?;
		let mut views = Vec::with_capacity(ids.len());
		for id in ids {
			let order = order_of(&mut conn, id).await?;
			views.push(view_of(&mut conn, order).await?);
		}
		Ok(views)
	}

	async fn find_for_session_subject(&self, id: PaymentId, subject: UserId) -> Result<Option<PaymentView>, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		let owned: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM payment_session_consent WHERE payment_id = $1 AND subject_user_id = $2)")
			.bind(id.raw())
			.bind(subject.raw())
			.fetch_one(&mut *conn)
			.await
			.map_err(repo_err)?;
		if !owned {
			return Ok(None);
		}
		let order = order_of(&mut conn, id.raw()).await?;
		Ok(Some(view_of(&mut conn, order).await?))
	}
}

#[async_trait]
impl PaymentApprovalStore for PgPayments {
	async fn due_to_propose(&self, limit: u32) -> Result<Vec<DueApproval>, DomainError> {
		self.due(
			"SELECT s.payment_id, s.attempts FROM payment_concierge_approval s JOIN payments p ON p.id = s.payment_id \
			 WHERE s.approval_id IS NULL AND p.state = 'pending' AND s.next_attempt_at <= now() ORDER BY s.created_at LIMIT $1",
			limit,
		)
		.await
	}

	async fn due_to_check(&self, limit: u32) -> Result<Vec<DueApproval>, DomainError> {
		self.due(
			"SELECT s.payment_id, s.attempts FROM payment_concierge_approval s JOIN payments p ON p.id = s.payment_id \
			 WHERE s.approval_id IS NOT NULL AND p.state = 'pending' AND s.next_attempt_at <= now() ORDER BY s.created_at LIMIT $1",
			limit,
		)
		.await
	}

	async fn due_to_report(&self, limit: u32) -> Result<Vec<PaymentId>, DomainError> {
		let ids: Vec<Uuid> = sqlx::query_scalar(
			"SELECT s.payment_id FROM payment_concierge_approval s JOIN payments p ON p.id = s.payment_id \
			 WHERE s.approval_id IS NOT NULL AND s.reported_at IS NULL AND p.state IN ('executed', 'execution_failed') ORDER BY s.created_at LIMIT $1",
		)
		.bind(i64::from(limit))
		.fetch_all(&self.pool)
		.await
		.map_err(repo_err)?;
		Ok(ids.into_iter().map(PaymentId::from_raw).collect())
	}

	async fn due_to_withdraw(&self, limit: u32) -> Result<Vec<(PaymentId, String)>, DomainError> {
		let rows = sqlx::query(
			"SELECT s.payment_id, s.approval_id FROM payment_concierge_approval s JOIN payments p ON p.id = s.payment_id \
			 WHERE s.approval_id IS NOT NULL AND s.withdrawn_at IS NULL AND s.reported_at IS NULL AND p.state IN ('cancelled', 'expired') ORDER BY s.created_at LIMIT $1",
		)
		.bind(i64::from(limit))
		.fetch_all(&self.pool)
		.await
		.map_err(repo_err)?;
		rows.iter()
			.map(|row| Ok((PaymentId::from_raw(row.try_get("payment_id").map_err(repo_err)?), row.try_get("approval_id").map_err(repo_err)?)))
			.collect()
	}

	async fn opened(&self, id: PaymentId, approval_id: &str, level: Option<ApprovalLevel>) -> Result<(), DomainError> {
		let updated = sqlx::query(
			"UPDATE payment_concierge_approval SET approval_id = $2, approval_level = $3, next_attempt_at = now(), last_error = NULL \
			 WHERE payment_id = $1 AND (approval_id IS NULL OR approval_id = $2)",
		)
		.bind(id.raw())
		.bind(approval_id)
		.bind(level.map(ApprovalLevel::as_str))
		.execute(&self.pool)
		.await
		.map_err(repo_err)?
		.rows_affected();
		if updated == 1 {
			return Ok(());
		}
		let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM payment_concierge_approval WHERE payment_id = $1)")
			.bind(id.raw())
			.fetch_one(&self.pool)
			.await
			.map_err(repo_err)?;
		Err(if exists {
			DomainError::Conflict(format!("payment {id} already has another approval"))
		} else {
			DomainError::NotFound {
				entity: "payment approval",
				id: id.to_string(),
			}
		})
	}

	async fn postpone(&self, id: PaymentId, delay_secs: u32, why: &str) -> Result<(), DomainError> {
		sqlx::query("UPDATE payment_concierge_approval SET attempts = attempts + 1, next_attempt_at = now() + make_interval(secs => $2), last_error = $3 WHERE payment_id = $1")
			.bind(id.raw())
			.bind(f64::from(delay_secs))
			.bind(why)
			.execute(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(())
	}

	async fn reported(&self, id: PaymentId) -> Result<(), DomainError> {
		sqlx::query("UPDATE payment_concierge_approval SET reported_at = COALESCE(reported_at, now()) WHERE payment_id = $1")
			.bind(id.raw())
			.execute(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(())
	}

	async fn withdrawn(&self, id: PaymentId) -> Result<(), DomainError> {
		sqlx::query("UPDATE payment_concierge_approval SET withdrawn_at = COALESCE(withdrawn_at, now()) WHERE payment_id = $1")
			.bind(id.raw())
			.execute(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(())
	}
}

impl PgPayments {
	async fn due(&self, query: &'static str, limit: u32) -> Result<Vec<DueApproval>, DomainError> {
		let rows = sqlx::query(query).bind(i64::from(limit)).fetch_all(&self.pool).await.map_err(repo_err)?;
		rows.iter()
			.map(|row| {
				Ok(DueApproval {
					id: PaymentId::from_raw(row.try_get("payment_id").map_err(repo_err)?),
					attempts: row.try_get::<i32, _>("attempts").map_err(repo_err)?.max(0) as u32,
				})
			})
			.collect()
	}
}

#[async_trait]
impl PaymentFeed for PgPayments {
	/// THREE QUERIES, NOT `3N`. The consilium links and the consent seats for the whole page
	/// are fetched once each with `= ANY($1)` and joined in memory, rather than a round trip
	/// per row — the mistake `ConsiliumRepository::list` had to be fixed for.
	async fn list(&self, filter: &PaymentFilter, limit: i64) -> Result<Vec<PaymentView>, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		let (party_kind, party_id) = match &filter.party {
			Some(party) => (Some(party.kind_str().to_owned()), Some(party.id_str())),
			None => (None, None),
		};
		// Every predicate is written as "the filter is absent OR it matches", so one prepared
		// statement serves every combination and no SQL is ever assembled from a value.
		let rows = sqlx::query(concat!(
			"SELECT ",
			payment_columns!(),
			" FROM payments p WHERE ($1::text IS NULL OR p.state = $1) \
			   AND ($2::text IS NULL OR (p.from_kind = $2 AND p.from_id IS NOT DISTINCT FROM $3) OR (p.to_kind = $2 AND p.to_id IS NOT DISTINCT FROM $3)) \
			   AND ($4::bool IS NULL OR (p.from_kind <> 'user') = $4) \
			 ORDER BY p.created_at DESC LIMIT $5"
		))
		.bind(filter.state.map(PaymentState::as_str))
		.bind(party_kind)
		.bind(party_id)
		.bind(filter.fund_owned_source)
		.bind(limit)
		.fetch_all(&mut *conn)
		.await
		.map_err(repo_err)?;
		if rows.is_empty() {
			return Ok(Vec::new());
		}

		let orders = rows.iter().map(rehydrate).collect::<Result<Vec<_>, DomainError>>()?;
		let ids: Vec<Uuid> = orders.iter().map(|order| order.id().raw()).collect();
		let initiators: Vec<Uuid> = orders.iter().map(|order| order.initiator().raw()).collect();

		let mut consilium_by_payment = HashMap::new();
		for row in &sqlx::query("SELECT payment_id, consilium_id FROM payment_approval WHERE payment_id = ANY($1)")
			.bind(&ids)
			.fetch_all(&mut *conn)
			.await
			.map_err(repo_err)?
		{
			let payment: Uuid = row.try_get("payment_id").map_err(repo_err)?;
			consilium_by_payment.insert(payment, ConsiliumId::from_raw(row.try_get::<Uuid, _>("consilium_id").map_err(repo_err)?));
		}

		let mut consent_by_payment = HashMap::new();
		for row in &sqlx::query(concat!(
			"SELECT ",
			consent_columns!(),
			" FROM payment_consent c JOIN users u ON u.id = c.subject_user_id WHERE c.payment_id = ANY($1)"
		))
		.bind(&ids)
		.fetch_all(&mut *conn)
		.await
		.map_err(repo_err)?
		{
			let seat = consent_of(row)?;
			consent_by_payment.insert(seat.payment_id, seat.view()?);
		}

		let mut concierge_by_payment = HashMap::new();
		for row in &sqlx::query("SELECT payment_id, approval_id, approval_level, payload_hash, destination FROM payment_concierge_approval WHERE payment_id = ANY($1)")
			.bind(&ids)
			.fetch_all(&mut *conn)
			.await
			.map_err(repo_err)?
		{
			concierge_by_payment.insert(row.try_get::<Uuid, _>("payment_id").map_err(repo_err)?, concierge_view(row)?);
		}

		let mut session_by_payment = HashMap::new();
		for row in &sqlx::query(concat!(
			"SELECT ",
			session_columns!(),
			" FROM payment_session_consent c JOIN users u ON u.id = c.subject_user_id WHERE c.payment_id = ANY($1)"
		))
		.bind(&ids)
		.fetch_all(&mut *conn)
		.await
		.map_err(repo_err)?
		{
			session_by_payment.insert(row.try_get::<Uuid, _>("payment_id").map_err(repo_err)?, session_of(row)?.view()?);
		}

		// One users read serves both the initiators' addresses and the receiving investors'
		// (the destination detail); one allocations read serves the receiving products'.
		let mut user_ids = initiators;
		let mut services: Vec<String> = Vec::new();
		for order in &orders {
			match order.terms().to().party() {
				Some(Party::User(user)) => user_ids.push(user.raw()),
				Some(Party::Service(service)) => services.push(service.as_str().to_owned()),
				Some(Party::Treasury) | None => {}
			}
		}
		let mut email_by_user = HashMap::new();
		for row in &sqlx::query("SELECT id, email FROM users WHERE id = ANY($1)")
			.bind(&user_ids)
			.fetch_all(&mut *conn)
			.await
			.map_err(repo_err)?
		{
			email_by_user.insert(row.try_get::<Uuid, _>("id").map_err(repo_err)?, row.try_get::<String, _>("email").map_err(repo_err)?);
		}
		let mut title_by_service = HashMap::new();
		if !services.is_empty() {
			for row in &sqlx::query("SELECT service, title FROM allocations WHERE service = ANY($1)")
				.bind(&services)
				.fetch_all(&mut *conn)
				.await
				.map_err(repo_err)?
			{
				title_by_service.insert(row.try_get::<String, _>("service").map_err(repo_err)?, row.try_get::<String, _>("title").map_err(repo_err)?);
			}
		}

		orders
			.into_iter()
			.map(|order| {
				let id = order.id().raw();
				let initiator_email = email_by_user
					.get(&order.initiator().raw())
					.cloned()
					.ok_or_else(|| DomainError::Repository(format!("payment initiator {} has no mirrored user row", order.initiator())))?;
				let destination_detail = match order.terms().to().party() {
					Some(Party::User(user)) => email_by_user.get(&user.raw()).cloned().map(EndDetail::Mailbox),
					Some(Party::Service(service)) => title_by_service.get(service.as_str()).cloned().map(EndDetail::ProductTitle),
					Some(Party::Treasury) | None => None,
				};
				Ok(PaymentView {
					consilium_id: consilium_by_payment.get(&id).copied(),
					consent: consent_by_payment.remove(&id),
					concierge: concierge_by_payment.remove(&id),
					session: session_by_payment.remove(&id),
					initiator_email,
					destination_detail,
					order,
				})
			})
			.collect()
	}
}

/// Load an order without a lock, by raw id — the read half of the token path.
async fn order_of(conn: &mut PgConnection, id: Uuid) -> Result<PaymentOrder, DomainError> {
	let row = sqlx::query(concat!("SELECT ", payment_columns!(), " FROM payments p WHERE p.id = $1"))
		.bind(id)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.ok_or_else(consent_not_found)?;
	rehydrate(&row)
}

fn invitation_of(order: PaymentOrder, initiator_email: String, seat: &ConsentRow, decision: ConsentDecision, destination_detail: Option<EndDetail>) -> ConsentInvitation {
	ConsentInvitation {
		payment_id: order.id(),
		state: order.state(),
		payload_hash: order.payload_hash_hex(),
		expires_at: order.expires_at(),
		order,
		initiator_email,
		subject_email: seat.email.clone(),
		decision,
		attempts_remaining: (MAX_CODE_ATTEMPTS - seat.attempts).max(0) as u32,
		destination_detail,
	}
}

/// Lowercase hex SHA-256, the spelling concierge's `payload_hash` has.
pub(crate) fn hex_digest(bytes: &[u8]) -> String {
	hex::encode(digest(bytes))
}

pub(crate) fn digest(bytes: &[u8]) -> [u8; DIGEST_BYTES] {
	use sha2::{Digest, Sha256};
	Sha256::digest(bytes).into()
}
