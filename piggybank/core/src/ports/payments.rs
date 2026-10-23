//! Persistence + read ports for the [`PaymentOrder`] aggregate.
//!
//! Every method is internally atomic and the method IS the transaction boundary — no caller
//! ever holds one across the port. Two of them carry the weight:
//!
//! - [`PaymentRepository::open`] writes the order, its materialized approval requirement
//!   (a consent seat, or the link to the consilium that will decide it) and the drained
//!   events in ONE transaction, so an order can never exist with no record of what would
//!   authorize it;
//! - [`PaymentRepository::submit`] counts the attempt, compares the code, records the
//!   decision and transitions the order **all under one `SELECT … FOR UPDATE` on the
//!   `payments` row** — not on the consent row, because the order is what the transition
//!   belongs to and locking the seat instead would let the approval and the order move in
//!   two transactions a retry could interleave.
//!
//! **TigerBeetle is never touched here.** A payment's money moves in the relay, from the
//! `Reserved`/`Settled` events these methods drain to the outbox — the one ACID point stays
//! Postgres, exactly as it does for every other money aggregate.

use async_trait::async_trait;
use domain::{
	balance::Party,
	consilium::ConsiliumId,
	error::DomainError,
	payments::{PaymentEffect, PaymentId, PaymentOrder, PaymentState},
	users::UserId,
	withdrawals::WithdrawalId,
};

use super::scope_operations::ApprovalLevel;

/// The digest length every stored secret is reduced to, named so the schema's
/// `octet_length(...) = 32` checks and the Rust side cannot drift apart.
pub const DIGEST_BYTES: usize = 32;

/// How many wrong codes a consent seat may submit before its token burns permanently — and,
/// with it, the payment fails closed. The consilium's ceiling, deliberately: `docs/CONSILIUM.md`
/// § "One specification for both planes" is one specification, not shared code.
pub const MAX_CODE_ATTEMPTS: i32 = 5;

/// What must be written beside a brand-new order to materialize its one approval requirement.
///
/// A sum type rather than two optional fields: an order needs exactly one of these, decided by
/// [`domain::payments::PaymentTerms::requirement`], and a shape that could carry both or
/// neither would let an unauthorizable order commit.
pub enum ApprovalSeat {
	/// The source is fund-owned: the owner quorum that decides this order. The consilium
	/// itself is opened through [`ConsiliumRepository`](super::ConsiliumRepository); this is
	/// only the link, recorded so the order names its authorization from the moment it exists.
	Consilium(ConsiliumId),
	/// The source is one investor's claim: that investor's own emailed consent.
	Consent(ConsentCredential),
	/// The source is fund-owned and the owners decide over a concierge `bank:payment`
	/// approval. Nothing to supply: the repository renders the approval's fields from the
	/// order and stores what it sent, and the executor proposes it.
	Concierge,
	/// The source is one investor's claim and that investor answers inside the wallet host.
	Session(SessionSeat),
}

/// The investor's seat in the wallet host. The repository writes the notice that asks them to answer
/// it in the same transaction as the seat, for the reason a consent mail was: an order nobody
/// was told about looks open for 72 hours and was unanswerable from the first instant.
pub struct SessionSeat {
	pub subject: UserId,
	/// The subject's folded revoke floor at open — [`ConsentCredential::token_version_at_open`].
	pub token_version_at_open: u64,
	/// SHA-256 over the subject's mirrored mailbox at open — [`ConsentCredential::email_hash_at_open`].
	pub email_hash_at_open: [u8; DIGEST_BYTES],
}

/// The minted consent seat. The plaintext token and code travel no further than the mail row
/// [`PaymentRepository::open`] writes from them — in the SAME transaction as the seat, so a
/// concierge outage can never leave an order nobody was asked about, and a mail can never
/// exist for an order that failed to commit. Only their digests are stored on the seat, so a
/// dump of `payment_consent` yields nothing that can consent to anything.
pub struct ConsentCredential {
	pub subject: UserId,
	/// The opaque single-use token that goes in the emailed link.
	pub token: String,
	/// The secret code the subject types on the consent page.
	pub code: String,
	pub token_hash: [u8; DIGEST_BYTES],
	pub code_hash: [u8; DIGEST_BYTES],
	/// The subject's folded revoke floor at open — `GREATEST(concierge_token_version,
	/// token_version)`, the same value [`IssuanceTarget::token_version`](super::IssuanceTarget)
	/// carries, so a revoke on either plane counts. Re-checked at consent and again at
	/// execution, which is what makes `RevokeTokens` void a consent that is already in flight.
	pub token_version_at_open: u64,
	/// SHA-256 over the subject's mirrored `users.email` at open, byte for byte as stored
	/// (already normalized by `Email::parse`). Re-checked at consent and at execution, so
	/// changing the mailbox at the identity provider cannot redirect a live token.
	pub email_hash_at_open: [u8; DIGEST_BYTES],
}

/// The consent seat as a surface renders it.
#[derive(Debug)]
pub struct ConsentView {
	pub subject: UserId,
	pub email: String,
	pub decision: ConsentDecision,
	/// Unix seconds; 0 while pending.
	pub decided_at: i64,
	pub notified: bool,
	pub attempts_remaining: u32,
	/// Why the seat can no longer be answered or executed — one of the pins recorded at open
	/// has moved — or `None` while it still holds. The execution path for an L1 order must
	/// read this BEFORE creating the withdrawal: `record_execution` refuses a moved pin too,
	/// but by then the withdrawal would already exist.
	pub invalidated: Option<ConsentInvalidation>,
}

/// Why a consent seat stopped being trustworthy — which of the two pins frozen at open has
/// moved. Typed rather than a sentence because the cause travels: it is the reason the order
/// records AND the closed word the subject and the initiator are mailed (#238), and a word
/// recovered by parsing the sentence would break the first time the sentence was reworded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsentInvalidation {
	/// The subject's folded revoke floor moved — `RevokeTokens` on either plane.
	SessionsRevoked { at_open: i64, now: i64 },
	/// The subject's mirrored mailbox is not the one the invitation was mailed to.
	EmailChanged,
}

impl ConsentInvalidation {
	/// The closed reason word concierge's `PaymentOutcomeMail` takes for this cause.
	pub fn mail_reason(self) -> &'static str {
		match self {
			Self::SessionsRevoked { .. } => "SESSIONS_REVOKED",
			Self::EmailChanged => "EMAIL_CHANGED",
		}
	}
}

impl core::fmt::Display for ConsentInvalidation {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		match self {
			Self::SessionsRevoked { at_open, now } => write!(
				f,
				"the investor's sessions were revoked after this consent was issued (token version {at_open} at open, {now} now), so the consent is void"
			),
			Self::EmailChanged => f.write_str("the investor's mailbox changed after this consent was issued, so the consent is void"),
		}
	}
}

/// What the emailed investor answered. The consilium's `VoteDecision` is the same three
/// values and is deliberately NOT reused: these are two planes that happen to agree today,
/// and folding them would make a future divergence a refactor of both.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsentDecision {
	Pending,
	Approve,
	Reject,
}

impl ConsentDecision {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Pending => "pending",
			Self::Approve => "approve",
			Self::Reject => "reject",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"pending" => Ok(Self::Pending),
			"approve" => Ok(Self::Approve),
			"reject" => Ok(Self::Reject),
			other => Err(DomainError::Validation(format!("unknown consent decision: {other}"))),
		}
	}
}

/// The investor's seat in the wallet host as a surface renders it.
#[derive(Debug)]
pub struct SessionConsentView {
	pub subject: UserId,
	pub email: String,
	pub decision: ConsentDecision,
	/// Unix seconds; 0 while pending.
	pub decided_at: i64,
	/// Why the seat can no longer be answered or executed, or `None` while both pins hold.
	pub invalidated: Option<ConsentInvalidation>,
}

/// The owners' concierge approval as a surface renders it.
#[derive(Debug)]
pub struct ConciergeSeatView {
	/// Concierge's id; `None` until the executor has opened the proposal.
	pub approval_id: Option<String>,
	pub level: Option<ApprovalLevel>,
	/// SHA-256 hex of the payload banking proposed.
	pub payload_hash: String,
	/// The destination as the owners were shown it.
	pub destination: String,
	/// Unix seconds after which the passed decision is void unexecuted (concierge's
	/// `execute_by`); `None` until the decision was carried to this plane.
	pub execute_by: Option<i64>,
}

/// What a human recognises the receiving end BY, beside the canonical label. The label
/// (`PaymentTerms::destination_label`) is what the digest binds and every surface shares; an
/// investor's masked mailbox or a product's title is the detail that lets an approver tell
/// "investor 8f3e…" from the person they meant. Never digested, and masked by the surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndDetail {
	/// The receiving investor's mirrored address, UNMASKED — every surface masks it.
	Mailbox(String),
	/// The receiving product's title from the allocation registry.
	ProductTitle(String),
}

/// An order and the identity slice a surface needs beside it.
#[derive(Debug)]
pub struct PaymentView {
	pub order: PaymentOrder,
	pub initiator_email: String,
	/// Present exactly when the requirement is the owner quorum.
	pub consilium_id: Option<ConsiliumId>,
	/// Present exactly when the requirement is the subject's emailed consent.
	pub consent: Option<ConsentView>,
	/// Present exactly when the owners decide over a concierge approval.
	pub concierge: Option<ConciergeSeatView>,
	/// Present exactly when the subject answers inside the wallet host.
	pub session: Option<SessionConsentView>,
	/// The receiving end's recognisable detail, when it has one.
	pub destination_detail: Option<EndDetail>,
}

impl PaymentView {
	/// Why the investor's consent — by either seat — no longer stands, if it does not.
	pub fn consent_invalidated(&self) -> Option<ConsentInvalidation> {
		self.consent
			.as_ref()
			.and_then(|consent| consent.invalidated)
			.or_else(|| self.session.as_ref().and_then(|session| session.invalidated))
	}
}

/// What the emailed investor is shown. Deliberately narrower than [`PaymentView`]: the terms
/// they are consenting to and nothing about the operator who proposed it beyond an address.
#[derive(Debug)]
pub struct ConsentInvitation {
	pub payment_id: PaymentId,
	pub state: PaymentState,
	pub order: PaymentOrder,
	pub payload_hash: String,
	pub initiator_email: String,
	pub subject_email: String,
	pub expires_at: i64,
	pub decision: ConsentDecision,
	pub attempts_remaining: u32,
	pub destination_detail: Option<EndDetail>,
}

/// Audit facts the edge supplies with a consent. Recorded, never trusted for authorization.
pub struct ConsentAudit {
	pub client_ip: String,
	pub user_agent: String,
}

/// The result of a consent that was actually accepted.
#[derive(Debug)]
pub struct ConsentOutcome {
	pub payment: PaymentView,
	/// True when this answer carried the order out of `pending`.
	pub decided: bool,
	/// True when that answer was approval — the signal to attempt execution.
	pub approved: bool,
}

/// How an execution attempt ended. Written by [`PaymentRepository::record_execution`].
pub enum ExecutionOutcome {
	Executed(PaymentEffect),
	Failed(String),
	/// The consent behind the order was found void before the effect was recorded. Recorded
	/// as a failure whose reason is the cause, and — unlike any other failure — told by mail
	/// to the audience that cause allows, which is why the cause stays typed this far.
	///
	/// `withdrawal` is the L1 order's derived withdrawal id, `None` for an L2/L3 order. An
	/// earlier attempt may have created it and died before recording the effect; the void
	/// cancels it while it is still `Queued`, as the L1 window does, and records the effect
	/// that exists when it is already past voiding.
	ConsentVoid {
		cause: ConsentInvalidation,
		withdrawal: Option<WithdrawalId>,
	},
}

/// Where an L2/L3 order's reservation stands in the relay — the Read-First the settlement
/// takes before it is recorded, because `Approved` commits the `Reserved` event and the
/// relay applies it afterwards, in its own time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservationStatus {
	/// The relay applied the reserve: its deterministic transfer id is in `saga_steps`.
	Applied,
	/// Still in the outbox, or not yet reached. Try again later.
	Pending,
	/// The ledger refused the reserve and the relay parked the event. The order can never
	/// settle; an operator sees the row in the parked outbox.
	Parked,
}

/// What an admin screen filters the payments table by. Every field is additive and `None`
/// means "no restriction", so the empty filter is the whole history.
#[derive(Default)]
pub struct PaymentFilter {
	pub state: Option<PaymentState>,
	/// Either end. Most payments have no user on either side (`Piggybank → Revenue`), which
	/// is exactly why the admin screen cannot be served by the per-user operation feed.
	pub party: Option<Party>,
	/// Only orders whose source is fund-owned (the governance surface), when true.
	pub fund_owned_source: Option<bool>,
}

/// The refusal for a second open order against one fund-owned source claim — the partial
/// unique index's answer, and the pre-check's, in the same words: which of the two spoke
/// is an accident of timing the caller has no business telling apart.
pub fn already_open() -> DomainError {
	DomainError::Conflict("a payment is already open against this claim — close it before opening another".into())
}

/// The single response every unusable consent token produces, whatever made it unusable —
/// unknown, expired, spent, burned, or attached to an order that has closed. A caller cannot
/// tell which they hit, so the surface cannot be used to probe for live tokens.
pub fn consent_not_found() -> DomainError {
	DomainError::NotFound {
		entity: "consent",
		id: String::new(),
	}
}

#[async_trait]
pub trait PaymentRepository: Send + Sync {
	/// Persist a brand-new order together with its materialized approval requirement, in one
	/// transaction. Takes the source claim's write lock ([`lock_claim`](crate::infrastructure::outbox::lock_claim))
	/// first: a payment spends the same claims withdrawals and subscriptions do, and a
	/// spender that skips the lock silently stops the serialization covering the others.
	///
	/// Refuses with [`DomainError::Conflict`] when another order is already open against the
	/// same fund-owned source (the partial unique index is what actually enforces it).
	///
	/// For a consent seat the invitation mail is queued in this same transaction, addressed
	/// to the subject's identity-plane id; `consent_url_base` is what the emailed link is
	/// built on. Refused when the subject has no mirrored concierge id — there would be no
	/// safe address to send to, and an order nobody can be asked about must not exist.
	async fn open(&self, order: &mut PaymentOrder, seat: ApprovalSeat, consent_url_base: &str) -> Result<(), DomainError>;

	/// Load one order in full (no lock; for queries).
	async fn find(&self, id: PaymentId) -> Result<Option<PaymentView>, DomainError>;

	/// Whether an order is already open (`pending` or `approved`) against `source`. A read
	/// the fund-owned open path takes BEFORE seating its consilium: the unique index would
	/// refuse the order anyway, but only after a quorum had been opened and every owner
	/// mailed — and then withdrawn and every owner mailed again. Advisory, not the guard:
	/// the index is the guard.
	async fn has_open_against(&self, source: &Party) -> Result<bool, DomainError>;

	/// Record that the owner quorum carried this order — the consilium branch's counterpart
	/// to [`Self::submit`]. Applies [`PaymentOrder::approve`] under the order's row lock, so
	/// the reservation event and the state change commit together. Idempotent.
	///
	/// `consilium` must be the quorum this order was opened under (the `payment_approval`
	/// link), checked under the same lock: a consilium over one order must not be able to
	/// approve another, whatever its terms happen to say.
	async fn record_approval(&self, id: PaymentId, consilium: ConsiliumId, at: i64) -> Result<PaymentView, DomainError>;

	/// Record that the owner quorum refused, or that the order was withdrawn by its initiator.
	/// Under the row lock; idempotent on an already-closed order.
	async fn record_rejection(&self, id: PaymentId, at: i64) -> Result<PaymentView, DomainError>;

	/// Withdraw an order the caller opened, under the row lock. Refuses a caller who is not
	/// the initiator; idempotent on an already-cancelled one.
	async fn cancel(&self, id: PaymentId, by: UserId, at: i64) -> Result<PaymentView, DomainError>;

	/// Resolve an emailed consent token to its invitation. **Strictly side-effect free** —
	/// mail scanners issue automatic requests for every URL in a message, so this must not
	/// count an attempt, spend a token, or record anything.
	async fn invitation(&self, token_hash: &[u8; DIGEST_BYTES], at: i64) -> Result<ConsentInvitation, DomainError>;

	/// Answer a consent: count the attempt, compare the code in constant time, record the
	/// decision and transition the order — one transaction, one lock, taken on the `payments`
	/// row. A repeat of the same answer is an idempotent no-op; a different one is refused.
	///
	/// Burning the token (five wrong codes) **fails the payment closed**. With one seat there
	/// is no second party to escalate to, so the burn is a refusal, not a detector.
	///
	/// A seat whose pins have moved since open (the subject's sessions revoked, or their
	/// mailbox changed) is refused with [`DomainError::Conflict`] before the code is compared,
	/// and the order is rejected with it — fail-closed, and terminal for the same one-seat
	/// reason.
	async fn submit(&self, token_hash: &[u8; DIGEST_BYTES], code: &str, decision: ConsentDecision, audit: &ConsentAudit, at: i64) -> Result<ConsentOutcome, DomainError>;

	/// Expire every pending order past its deadline. Returns how many closed.
	async fn expire_due(&self, at: i64) -> Result<usize, DomainError>;

	/// Orders that are approved but have no effect yet — what an execution retry picks up
	/// after a crash between the approval and the money. An `execution_failed` one is
	/// deliberately NOT here: nothing retries silently.
	async fn awaiting_execution(&self) -> Result<Vec<PaymentId>, DomainError>;

	/// Whether the relay has applied this order's reservation (`reserve_tid` is its
	/// deterministic transfer id, `uuid_v5(payment_id, "payment:reserve")`). Read from the
	/// relay's own record, `saga_steps` and the parked outbox — not from the ledger, which
	/// cannot say "refused" — so the settlement is never recorded over a reserve that
	/// never landed.
	async fn reservation_status(&self, id: PaymentId, reserve_tid: u128) -> Result<ReservationStatus, DomainError>;

	/// Record how the execution attempt ended, under the row lock. Writing the effect is
	/// idempotent for the same effect and a conflict for a different one — which is what lets
	/// the caller re-read by the deterministic id and believe the row rather than the error.
	///
	/// An `Executed` outcome over a consent seat whose pins have moved since open is NOT
	/// recorded: the order is moved to `execution_failed` (releasing an L2/L3 reservation)
	/// and the call returns [`DomainError::Conflict`] naming why. See
	/// [`ConsentView::invalidated`] for the L1 path, which must check before the withdrawal
	/// exists — and for the window between that check and this call, a withdrawal the
	/// refused effect names is CANCELLED in this same transaction while it is still queued,
	/// so a revocation that lands after the check still stops the money. A withdrawal that
	/// has already been dispatched cannot be voided (the broadcast may have landed), so the
	/// effect is recorded as it is and the pin movement is logged rather than lied about.
	async fn record_execution(&self, id: PaymentId, outcome: ExecutionOutcome, at: i64) -> Result<PaymentView, DomainError>;

	/// Record that the concierge approval decided this order in favour. The counterpart of
	/// [`Self::record_approval`] for the concierge seat, and idempotent in the same way: an
	/// order already `approved` or `executed` is returned as it stands, so a decision served
	/// twice approves once and reserves once.
	///
	/// `approval_id` must be the one the seat recorded when the proposal was opened; a seat
	/// that has none yet (the answer to the proposal was lost) takes this one. A different id
	/// is a [`DomainError::Conflict`]: one order, one approval. Under the order's row lock.
	///
	/// `execute_by` is the decision's own deadline (0: unknown) and is stored with the seat; an
	/// order whose own window ended (`expires_at` at or before `at`) is not approved.
	async fn record_concierge_approval(&self, id: PaymentId, approval_id: &str, execute_by: i64, at: i64) -> Result<PaymentView, DomainError>;

	/// The investor's own answer to an order seated on [`ApprovalSeat::Session`], given signed
	/// in. Under the order's lock it re-reads the pins, records the answer and moves the order,
	/// exactly as [`Self::submit`] does for the emailed seat — without a code to compare, since
	/// the session proved who is asking.
	///
	/// `subject` must be the seat's subject: another user's order is [`consent_not_found`], the
	/// same door as an order that does not exist. A repeat of the same answer is an idempotent
	/// no-op; a different one is a [`DomainError::Conflict`]. A seat whose pins have moved is
	/// refused with a Conflict and the order is rejected with it.
	async fn answer_session_consent(&self, id: PaymentId, subject: UserId, decision: ConsentDecision, audit: &ConsentAudit, at: i64) -> Result<ConsentOutcome, DomainError>;

	/// The orders waiting for `subject`'s answer inside the wallet host, newest first.
	async fn awaiting_session_consent(&self, subject: UserId) -> Result<Vec<PaymentView>, DomainError>;

	/// One order as its subject in the wallet host may see it; anyone else's, or an order with no such
	/// seat, is `None`.
	async fn find_for_session_subject(&self, id: PaymentId, subject: UserId) -> Result<Option<PaymentView>, DomainError>;
}

/// An order whose approval the executor has something to do about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DueApproval {
	pub id: PaymentId,
	/// How many times the executor put it off.
	pub attempts: u32,
}

/// The executor's side of [`ApprovalSeat::Concierge`]: which orders it has to open, watch,
/// report and withdraw, and the notes it leaves on the seat. Kept apart from
/// [`PaymentRepository`] so the surfaces that open and answer orders do not carry the
/// executor's bookkeeping, and the other way round.
#[async_trait]
pub trait PaymentApprovalStore: Send + Sync {
	/// Pending orders whose proposal was never opened and which were written more than
	/// `older_than_secs` ago, oldest first. The proposal is opened by the request that wrote the
	/// order, with the proof of who asked; an order still without one is a request that died
	/// between the two, and nothing may open it later without that proof.
	async fn unproposed(&self, older_than_secs: u32, limit: u32) -> Result<Vec<DueApproval>, DomainError>;

	/// Pending orders with an opened approval whose turn to be asked about has come.
	async fn due_to_check(&self, limit: u32) -> Result<Vec<DueApproval>, DomainError>;

	/// Orders that executed or failed and whose outcome concierge has not been told.
	async fn due_to_report(&self, limit: u32) -> Result<Vec<PaymentId>, DomainError>;

	/// Orders that were cancelled or expired while their proposal was live and not yet
	/// withdrawn.
	async fn due_to_withdraw(&self, limit: u32) -> Result<Vec<(PaymentId, String)>, DomainError>;

	/// The proposal was opened as `approval_id` (resolved to `level`; `None` when concierge
	/// did not say). A repeat with the same id is a no-op, a different id a
	/// [`DomainError::Conflict`].
	async fn opened(&self, id: PaymentId, approval_id: &str, level: Option<ApprovalLevel>) -> Result<(), DomainError>;

	/// Try again no sooner than `delay_secs` from now; counts one more attempt.
	async fn postpone(&self, id: PaymentId, delay_secs: u32, why: &str) -> Result<(), DomainError>;

	/// Concierge was told how the order ended, or refused to take it (`refused` says why and is
	/// kept on the seat): either way nothing more is to be reported.
	async fn reported(&self, id: PaymentId, refused: Option<&str>) -> Result<(), DomainError>;

	/// The proposal of a dead order was taken back, or found already closed.
	async fn withdrawn(&self, id: PaymentId) -> Result<(), DomainError>;
}

/// The admin payments screen's read model.
///
/// A **query-side** port, separate from [`PaymentRepository`] for the reason
/// [`OperationFeed`](super::OperationFeed) is separate from the aggregates it reads: it owns
/// nothing and writes nothing. It exists as its own trait rather than another method on the
/// repository so the screen's permission and paging can be granted without handing out the
/// write surface with them.
#[async_trait]
pub trait PaymentFeed: Send + Sync {
	/// The payment history matching `filter`, newest first, capped at `limit`. Nothing is
	/// ever deleted, so rejected, expired and failed orders stay in it — the audit record is
	/// half the point of the screen.
	async fn list(&self, filter: &PaymentFilter, limit: i64) -> Result<Vec<PaymentView>, DomainError>;
}
