//! Persistence + read ports for the [`PaymentOrder`] aggregate.
//!
//! Every method is internally atomic and the method IS the transaction boundary — no caller
//! ever holds one across the port. Two of them carry the weight:
//!
//! - [`PaymentRepository::open`] writes the order, its materialized approval requirement
//!   (the investor's session seat, or the concierge approval's) and the drained events in ONE
//!   transaction, so an order can never exist with no record of what would authorize it;
//! - [`PaymentRepository::answer_session_consent`] re-reads the pins, records the decision
//!   and transitions the order **all under one `SELECT … FOR UPDATE` on the `payments` row**
//!   — not on the seat row, because the order is what the transition belongs to and locking
//!   the seat instead would let the approval and the order move in two transactions a retry
//!   could interleave.
//!
//! **TigerBeetle is never touched here.** A payment's money moves in the relay, from the
//! `Reserved`/`Settled` events these methods drain to the outbox — the one ACID point stays
//! Postgres, exactly as it does for every other money aggregate.

use async_trait::async_trait;
use domain::{
	balance::Party,
	error::DomainError,
	payments::{PaymentEffect, PaymentId, PaymentOrder, PaymentState},
	users::UserId,
	withdrawals::WithdrawalId,
};

use super::scope_operations::ApprovalLevel;

/// The digest length every stored hash is reduced to, named so the schema's
/// `octet_length(...) = 32` checks and the Rust side cannot drift apart.
pub const DIGEST_BYTES: usize = 32;

/// What must be written beside a brand-new order to materialize its one approval requirement.
///
/// A sum type rather than two optional fields: an order needs exactly one of these, decided by
/// [`domain::payments::PaymentTerms::requirement`], and a shape that could carry both or
/// neither would let an unauthorizable order commit.
pub enum ApprovalSeat {
	/// The source is the treasury and the owners decide over a concierge `bank:payment`
	/// approval. Nothing to supply: the repository renders the approval's fields from the
	/// order and stores what it sent, and the executor proposes it.
	Concierge,
	/// The source is one investor's claim and that investor answers inside the wallet host.
	Session(SessionSeat),
}

/// The investor's seat in the wallet host. The repository writes the notice that asks them to
/// answer it in the same transaction as the seat: an order nobody was told about looks open
/// for 72 hours and was unanswerable from the first instant.
pub struct SessionSeat {
	pub subject: UserId,
	/// The subject's folded revoke floor at open — `GREATEST(concierge_token_version,
	/// token_version)`, the same value [`IssuanceTarget::token_version`](super::IssuanceTarget)
	/// carries, so a revoke on either plane counts. Re-checked at the answer and again at
	/// execution, which is what makes `RevokeTokens` void a consent that is already in flight.
	pub token_version_at_open: u64,
	/// SHA-256 over the subject's mirrored `users.email` at open, byte for byte as stored
	/// (already normalized by `Email::parse`). Re-checked at the answer and at execution, so
	/// changing the mailbox at the identity provider cannot redirect a pending consent.
	pub email_hash_at_open: [u8; DIGEST_BYTES],
}

/// Why a consent seat stopped being trustworthy — which of the two pins frozen at open has
/// moved. Typed rather than a sentence because the cause travels: it is the reason the order
/// records AND what the notices to the subject and the initiator say (#238).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsentInvalidation {
	/// The subject's folded revoke floor moved — `RevokeTokens` on either plane.
	SessionsRevoked { at_open: i64, now: i64 },
	/// The subject's mirrored mailbox is not the one recorded at open.
	EmailChanged,
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

/// What the investor answered.
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
/// investor's masked mailbox is the detail that lets an approver tell "investor 8f3e…" from
/// the person they meant. Never digested, and masked by the surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndDetail {
	/// The receiving investor's mirrored address, UNMASKED — every surface masks it.
	Mailbox(String),
}

/// An order and the identity slice a surface needs beside it.
#[derive(Debug)]
pub struct PaymentView {
	pub order: PaymentOrder,
	pub initiator_email: String,
	pub seat: SeatView,
	/// The receiving end's recognisable detail, when it has one.
	pub destination_detail: Option<EndDetail>,
}

/// The one seat that decides an order, as a surface renders it — [`ApprovalSeat`] once written.
#[derive(Debug)]
pub enum SeatView {
	/// The owners decide over a concierge approval.
	Concierge(ConciergeSeatView),
	/// The subject answers inside the wallet host.
	Session(SessionConsentView),
}

impl PaymentView {
	/// Why the investor's consent no longer stands, if it does not.
	pub fn consent_invalidated(&self) -> Option<ConsentInvalidation> {
		match &self.seat {
			SeatView::Session(session) => session.invalidated,
			SeatView::Concierge(_) => None,
		}
	}
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
	/// as a failure whose reason is the cause, and — unlike any other failure — told to the
	/// audience that cause allows, which is why the cause stays typed this far.
	///
	/// `withdrawal` is the L1 order's derived withdrawal id, `None` for an L3 order. An
	/// earlier attempt may have created it and died before recording the effect; the void
	/// cancels it while it is still `Queued`, as the L1 window does, and records the effect
	/// that exists when it is already past voiding.
	ConsentVoid {
		cause: ConsentInvalidation,
		withdrawal: Option<WithdrawalId>,
	},
}

/// Where an L3 order's reservation stands in the relay — the Read-First the settlement
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
	/// Either end.
	pub party: Option<Party>,
	/// Only orders whose source is banking's treasury (the owners' surface), when true.
	pub treasury_source: Option<bool>,
}

/// The refusal for a second open order against the treasury — the partial
/// unique index's answer, and the pre-check's, in the same words: which of the two spoke
/// is an accident of timing the caller has no business telling apart.
pub fn already_open() -> DomainError {
	DomainError::Conflict("a payment is already open against this claim — close it before opening another".into())
}

/// The single response for an order the caller cannot answer, whatever the reason — not
/// theirs, not seated on their answer, or not there. A caller cannot tell which they hit, so
/// the surface cannot be used to find other people's payments.
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
	/// first: a payment spends the same claims withdrawals and moves do, and a
	/// spender that skips the lock silently stops the serialization covering the others.
	///
	/// Refuses with [`DomainError::Conflict`] when another order is already open against the
	/// same treasury source (the partial unique index is what actually enforces it).
	///
	/// For a session seat the notice asking the subject is queued in this same transaction.
	/// Refused when the subject has no mirrored concierge id — they could not be told, and an
	/// order nobody can be asked about must not exist.
	async fn open(&self, order: &mut PaymentOrder, seat: ApprovalSeat) -> Result<(), DomainError>;

	/// Load one order in full (no lock; for queries).
	async fn find(&self, id: PaymentId) -> Result<Option<PaymentView>, DomainError>;

	/// Whether an order is already open (`pending` or `approved`) against `source`. A read
	/// the treasury open path takes before writing, so the ordinary refusal reads as one.
	/// Advisory, not the guard: the index is the guard.
	async fn has_open_against(&self, source: &Party) -> Result<bool, DomainError>;

	/// Record that the owners' approval did not pass, or that the order could not be proposed.
	/// Under the row lock; idempotent on an already-closed order.
	async fn record_rejection(&self, id: PaymentId, at: i64) -> Result<PaymentView, DomainError>;

	/// Withdraw an order the caller opened, under the row lock. Refuses a caller who is not
	/// the initiator; idempotent on an already-cancelled one.
	async fn cancel(&self, id: PaymentId, by: UserId, at: i64) -> Result<PaymentView, DomainError>;

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
	/// An `Executed` outcome over a session seat whose pins have moved since open is NOT
	/// recorded: the order is moved to `execution_failed` (releasing an L3 reservation)
	/// and the call returns [`DomainError::Conflict`] naming why. See
	/// [`SessionConsentView::invalidated`] for the L1 path, which must check before the
	/// withdrawal exists — and for the window between that check and this call, a withdrawal the
	/// refused effect names is CANCELLED in this same transaction while it is still queued,
	/// so a revocation that lands after the check still stops the money. A withdrawal that
	/// has already been dispatched cannot be voided (the broadcast may have landed), so the
	/// effect is recorded as it is and the pin movement is logged rather than lied about.
	async fn record_execution(&self, id: PaymentId, outcome: ExecutionOutcome, at: i64) -> Result<PaymentView, DomainError>;

	/// Record that the concierge approval decided this order in favour. Idempotent: an order
	/// already `approved` or `executed` is returned as it stands, so a decision served twice
	/// approves once and reserves once.
	///
	/// `approval_id` must be the one the seat recorded when the proposal was opened; a seat
	/// that has none yet (the answer to the proposal was lost) takes this one. A different id
	/// is a [`DomainError::Conflict`]: one order, one approval. Under the order's row lock.
	///
	/// `execute_by` is the decision's own deadline (0: unknown) and is stored with the seat; an
	/// order whose own window ended (`expires_at` at or before `at`) is not approved.
	async fn record_concierge_approval(&self, id: PaymentId, approval_id: &str, execute_by: i64, at: i64) -> Result<PaymentView, DomainError>;

	/// The investor's own answer to an order seated on [`ApprovalSeat::Session`], given signed
	/// in. Under the order's lock it re-reads the pins, records the answer and moves the order;
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
