//! `payments` bounded context — moving money between two named ends of the platform.
//!
//! # This is an ORDER, not a second saga
//!
//! A [`PaymentOrder`] is **intent + approvals + outcome**. It is deliberately NOT a second
//! money saga beside [`crate::withdrawals::Withdrawal`]: that one owns the
//! `Queued → Processing → Completed | Failed | Cancelled` lifecycle, a `clearing` reserve
//! and the cardinal rule ("never void once the broadcast may have landed"). Folding an
//! on-chain payout's five states into this aggregate would mean either duplicating
//! `WithdrawalState` in a second table or stopping at [`PaymentState::Executed`] while the
//! screen claims the money has arrived when it is still in a queue.
//!
//! So an executed order produces EITHER a withdrawal id OR one ledger fact, and records
//! only which ([`PaymentEffect`]). The machinery behind each stays in the application
//! layer.
//!
//! # The two tiers, and why the caller never picks one
//!
//! [`PaymentTier`] is a pure function of the destination:
//!
//! | destination | tier | effect |
//! | --- | --- | --- |
//! | an external wallet address | [`PaymentTier::External`] (L1) | a [`crate::withdrawals::Withdrawal`] |
//! | an internal claim | [`PaymentTier::Internal`] (L3) | one posted transfer |
//!
//! A caller-supplied tier would be a second statement of a fact the destination already
//! makes, and the interesting failure is the one where the two disagree — a request that
//! names an external address and asks for the internal tier. Deriving it means there is no
//! second statement to disagree with, which is also why the `payments` table has no `tier`
//! column: the projection cannot drift from the terms if it does not exist.
//!
//! # Authorization is a function of the SOURCE
//!
//! > Every payment out of `User(u)` requires `u`'s own consent, at every tier. A payment
//! > out of banking's treasury requires the owners' approval, at every tier.
//!
//! [`PaymentTerms::requirement`] is that rule, and it is total over [`Party`] by
//! construction. There is **no** "an admin may move treasury money alone" cell: a security review found it turns three pre-existing single-actor
//! holes into a treasury-wide one, and deleting it is what makes payments add no new
//! capability to an attacker. Do not reintroduce it.
//!
//! Pure and wasm-safe: ids and the payload hash are supplied by the application layer, no
//! clock, no I/O, no crypto.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{
	balance::{LedgerAccountKey, Party},
	error::DomainError,
	hex32,
	money::{Network, Usdt, WalletAddress},
	push_field,
	scope_operations::{ApprovalField, ApprovalValue, PAYMENT_APPROVAL_SCOPE, PAYMENT_KIND, approval_payload},
	users::UserId,
	withdrawals::WithdrawalId,
};

/// The longest a TEXT field of an approval may be, in bytes (concierge's `ApprovalFieldType`).
pub const APPROVAL_TEXT_MAX_BYTES: usize = 200;

/// Whether a text is one the approval — and the notices built on the same words — would carry:
/// within [`APPROVAL_TEXT_MAX_BYTES`], no control character, no link.
pub fn is_plain_text(value: &str) -> bool {
	let lowered = value.to_lowercase();
	value.len() <= APPROVAL_TEXT_MAX_BYTES && !value.chars().any(char::is_control) && !lowered.contains("://") && !lowered.contains("www.")
}

/// A unique payment-order id (UUID). Minted by the application layer.
pub type PaymentId = Id<PaymentTag>;
/// Phantom tag making [`PaymentId`] a distinct, incompatible identity type.
pub struct PaymentTag;

/// How long an unapproved payment order stays open (72h): a human has to read a notice and
/// act.
pub const TTL_SECS: i64 = 72 * 60 * 60;

/// How long an order stays open while a concierge approval decides it (7 days). The approval
/// is open for up to 72 hours, a passed one waits out its veto delay (24 hours by default, up
/// to 72 by policy), and is then executable for 72 hours; the order must outlive the part of
/// that which comes before its execution. What ends the order earlier is the approval's own end.
pub const CONCIERGE_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// What a decision needs before it can be carried out at the default policy: 72 hours open and
/// a 24-hour veto delay. An order proposed with less window left than this plus
/// [`DECISION_MARGIN_SECS`] could pass and then find itself expired.
pub const DECISION_WINDOW_SECS: i64 = 96 * 60 * 60;

/// Room on top of [`DECISION_WINDOW_SECS`] for the executor's own cadence.
pub const DECISION_MARGIN_SECS: i64 = 2 * 60 * 60;

/// The longest reason an initiator may attach, **in bytes**. Bytes, not characters, because the limit exists to
/// bound what an approval and a column carry, and both count bytes.
pub const MAX_REASON_BYTES: usize = 500;

/// Why this payment is being made — required, and shown verbatim to whoever approves it.
///
/// It is a newtype rather than a bare `String` because it is **hashed into the payload the
/// approver signs**: an unvalidated reason would be a way to put a control character (and
/// therefore a forged line) into the one approval whose whole job is to state the facts of
/// a money move.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PaymentReason(String);

impl PaymentReason {
	pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
		let value: String = raw.into();
		if value.trim().is_empty() {
			return Err(DomainError::Validation("a payment must state a reason".into()));
		}
		if value.len() > MAX_REASON_BYTES {
			return Err(DomainError::Validation(format!("reason exceeds {MAX_REASON_BYTES} bytes")));
		}
		// A control character has no meaning in a reason and every meaning in a plain-text
		// line an approver reads as a fact about the amount.
		if value.chars().any(char::is_control) {
			return Err(DomainError::Validation("reason may not contain control characters".into()));
		}
		Ok(Self(value))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl core::fmt::Display for PaymentReason {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(&self.0)
	}
}

/// Where a payment lands.
///
/// The source is a plain [`Party`] and this type has no `External` *source*: money cannot
/// arrive from an address by anyone's say-so — that is a deposit, which a chain watcher
/// attests. Making an external source unrepresentable is why the two ends have different
/// types rather than one symmetrical one.
/// ADJACENTLY tagged, not internally. [`Party`] is itself tagged on `kind`, and an internal
/// tag would splice the two maps together: the inner `kind` overwrites the outer one, so
/// `Internal(Party::User(_))` serializes as `{"kind":"user",…}` — a value that has lost
/// which VARIANT it is and cannot be read back. `PaymentEvent::Opened` carries these bytes
/// into `event_log`, so a shape that only survives one direction is an audit row nobody can
/// ever load.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "target", rename_all = "snake_case")]
pub enum PaymentDestination {
	/// A claim inside the platform — settled as one posted ledger transfer.
	Internal(Party),
	/// An address on a rail — settled by the ordinary withdrawal saga.
	External { network: Network, address: WalletAddress },
}

impl PaymentDestination {
	/// The tier this destination implies. The ONLY place a tier is decided.
	pub fn tier(&self) -> PaymentTier {
		match self {
			Self::External { .. } => PaymentTier::External,
			Self::Internal(_) => PaymentTier::Internal,
		}
	}

	/// The claim credited, or `None` when the value leaves the platform entirely.
	pub fn claim_key(&self) -> Option<LedgerAccountKey> {
		match self {
			Self::Internal(party) => Some(party.claim_key()),
			Self::External { .. } => None,
		}
	}

	/// The internal party, or `None` for an address.
	pub fn party(&self) -> Option<&Party> {
		match self {
			Self::Internal(party) => Some(party),
			Self::External { .. } => None,
		}
	}
}

/// Which plane of the platform a payment crosses. **Derived from the destination, never
/// supplied** — see the module header.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentTier {
	/// L3 — claim to claim.
	Internal,
	/// L1 — off the platform, on a rail.
	External,
}

impl PaymentTier {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Internal => "internal",
			Self::External => "external",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"internal" => Ok(Self::Internal),
			"external" => Ok(Self::External),
			other => Err(DomainError::Validation(format!("unknown payment tier: {other}"))),
		}
	}

	/// Whether a payment on this tier settles as a ledger transfer of its own (L3) or by
	/// handing the money to the withdrawal saga (L1).
	fn settles_on_the_ledger(self) -> bool {
		match self {
			Self::Internal => true,
			Self::External => false,
		}
	}
}

/// What an executed order produced — an identity, never the machinery behind it.
///
/// `Transfer` carries nothing: an L3 settlement's transfer id is
/// `uuid_v5(payment_id, "payment:transfer")`, a pure function of the order, so storing it
/// would be storing a value that can only ever be recomputed and could only ever be wrong.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "effect", content = "id", rename_all = "snake_case")]
pub enum PaymentEffect {
	/// L1 — the withdrawal saga now owns the money.
	Withdrawal(WithdrawalId),
	/// L3 — one balanced posted transfer, already settled.
	Transfer,
}

impl PaymentEffect {
	fn matches(self, tier: PaymentTier) -> bool {
		match self {
			Self::Withdrawal(_) => !tier.settles_on_the_ledger(),
			Self::Transfer => tier.settles_on_the_ledger(),
		}
	}
}

/// What must happen before a payment may execute. Exactly one requirement per order —
/// the source decides which, and nothing composes them.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "requirement", content = "subject", rename_all = "snake_case")]
pub enum PaymentApproval {
	/// The source is banking's treasury: the owners, through a concierge `bank:payment` approval.
	OwnersApproval,
	/// The source is one investor's claim: that investor's own consent.
	SubjectConsent(UserId),
}

impl PaymentApproval {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::OwnersApproval => "owners_approval",
			Self::SubjectConsent(_) => "subject_consent",
		}
	}
}

/// Where a payment order stands.
///
/// `Pending → Approved → Executed | ExecutionFailed` is the happy spine; `Rejected`,
/// `Expired` and `Cancelled` are the three ways it ends before approval. Every one but
/// `Pending` and `Approved` is terminal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentState {
	/// Recorded, awaiting its one approval.
	Pending,
	/// Approved; for L3 the source is reserved. The effect does not exist yet.
	Approved,
	/// The effect exists.
	Executed,
	/// Approved, but the effect could not be created. Terminal — nothing retries silently —
	/// and for L3 the reservation taken at approval is released on the way in.
	ExecutionFailed,
	/// The approver refused, or their consent was voided.
	Rejected,
	/// The window closed with no answer.
	Expired,
	/// The initiator withdrew it.
	Cancelled,
}

impl PaymentState {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Pending => "pending",
			Self::Approved => "approved",
			Self::Executed => "executed",
			Self::ExecutionFailed => "execution_failed",
			Self::Rejected => "rejected",
			Self::Expired => "expired",
			Self::Cancelled => "cancelled",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"pending" => Ok(Self::Pending),
			"approved" => Ok(Self::Approved),
			"executed" => Ok(Self::Executed),
			"execution_failed" => Ok(Self::ExecutionFailed),
			"rejected" => Ok(Self::Rejected),
			"expired" => Ok(Self::Expired),
			"cancelled" => Ok(Self::Cancelled),
			other => Err(DomainError::Validation(format!("unknown payment state: {other}"))),
		}
	}

	pub fn is_pending(self) -> bool {
		matches!(self, Self::Pending)
	}

	/// Whether the order still holds (or may yet take) its source claim — the predicate the
	/// "one open payment per treasury source" index is written over.
	pub fn is_open(self) -> bool {
		matches!(self, Self::Pending | Self::Approved)
	}
}

/// The immutable subject of a payment. There is no edit path: changing anything means
/// cancelling and reopening, and an approval is never carried over.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PaymentTerms {
	from: Party,
	to: PaymentDestination,
	amount: Usdt,
	reason: PaymentReason,
}

impl PaymentTerms {
	/// The domain-separation prefix, included in the digest so a hash over these terms can
	/// never collide with one taken over another subject whose fields happen to encode
	/// alike.
	///
	/// FROZEN. Every `payload_hash` ever stored was taken over an encoding starting with
	/// these bytes; changing one of them invalidates every live approval at once.
	pub const DOMAIN: &'static [u8] = b"banking.v1.PaymentTerms\x00";

	/// Validate the SHAPE of a payment. Four refusals, each for a thing that cannot be
	/// repaired later:
	///
	/// - **`from == to`** — a transfer from a claim to itself moves nothing and would still
	///   consume an approval, an index slot and an owner's attention;
	/// - **an external source** — unrepresentable by type, so there is nothing to check;
	/// - **a zero amount** — TigerBeetle rejects a zero-amount transfer outright, so an
	///   order for one is an approval spent on something that can never execute;
	/// - **an empty or unsendable reason** — see [`PaymentReason`].
	///
	/// A rail-specific shape (the address matching its network, the minimum, on-chain dust)
	/// is NOT re-stated here: [`crate::withdrawals::Withdrawal::request`] is the one
	/// validator of those, and the application layer runs it at open so the answer cannot
	/// drift from what execution will actually do.
	pub fn new(from: Party, to: PaymentDestination, amount: Usdt, reason: PaymentReason) -> Result<Self, DomainError> {
		if to.party() == Some(&from) {
			return Err(DomainError::Validation("a payment cannot name the same claim as both ends".into()));
		}
		if amount.is_zero() {
			return Err(DomainError::Validation("a payment must move a non-zero amount".into()));
		}
		// The treasury fills from the fees banking charges, never from a payment: one
		// paying into it would turn an investor's money into banking's.
		if to.party() == Some(&Party::Treasury) {
			return Err(DomainError::Validation("banking's treasury is not a payment destination".into()));
		}
		if let PaymentDestination::External { network, address } = &to
			&& address.network() != *network
		{
			return Err(DomainError::Validation("payment address is for a different network".into()));
		}
		Ok(Self { from, to, amount, reason })
	}

	pub fn from(&self) -> &Party {
		&self.from
	}

	pub fn to(&self) -> &PaymentDestination {
		&self.to
	}

	pub fn amount(&self) -> Usdt {
		self.amount
	}

	pub fn reason(&self) -> &PaymentReason {
		&self.reason
	}

	/// The tier, derived from the destination. See the module header.
	pub fn tier(&self) -> PaymentTier {
		self.to.tier()
	}

	/// The source, as a human reading an approval request sees it.
	pub fn source_label(&self) -> String {
		party_label(&self.from)
	}

	/// The destination, as that same human sees it. An address is rendered in FULL with its
	/// rail: a truncated address in an approval flow is an invitation to approve the wrong
	/// one.
	pub fn destination_label(&self) -> String {
		match &self.to {
			PaymentDestination::Internal(party) => party_label(party),
			PaymentDestination::External { network, address } => format!("{} on {}", address.as_str(), network.as_str()),
		}
	}

	/// The claim debited — both the "one open payment per source" key and the advisory-lock
	/// target the execution path takes.
	pub fn source_claim(&self) -> LedgerAccountKey {
		self.from.claim_key()
	}

	/// **The §3 policy, as one total function.**
	///
	/// It reads the SOURCE and nothing else — not the tier, not the destination, not the
	/// initiator's role. An investor's claim is theirs however internal the destination;
	/// banking's treasury is the owners' to spend (D17, D18).
	pub fn requirement(&self) -> PaymentApproval {
		match &self.from {
			Party::User(user) => PaymentApproval::SubjectConsent(*user),
			Party::Treasury => PaymentApproval::OwnersApproval,
		}
	}

	/// What the platform owners are shown of this payment on its `bank:payment` approval, field
	/// for field as the catalog declares them (`BANK_CATALOG`). `destination` is the end as a
	/// person recognises it — the label plus whatever [`PaymentTerms::destination_label`] leaves
	/// out — and is passed in because the recognisable detail is the adapter's to look up.
	///
	/// Refused, not shortened, when a text would not be accepted by the approval — more than
	/// [`APPROVAL_TEXT_MAX_BYTES`], a control character, a link: the approvers must read the
	/// sentence the order carries, and a reason cut to fit would approve something else.
	pub fn approval_fields(&self, destination: &str) -> Result<Vec<ApprovalField>, DomainError> {
		let text = |key: &'static str, value: &str| -> Result<ApprovalField, DomainError> {
			if !is_plain_text(value) {
				return Err(DomainError::Validation(format!(
					"the {key} of a payment decided by the platform owners is at most {APPROVAL_TEXT_MAX_BYTES} bytes of plain text without a link"
				)));
			}
			Ok(ApprovalField {
				key,
				value: ApprovalValue::Text(value.to_owned()),
			})
		};
		Ok(vec![
			text("tier", self.tier().as_str())?,
			text("source", &self.source_label())?,
			text("destination", destination)?,
			ApprovalField {
				key: "amount",
				value: ApprovalValue::Amount(self.amount),
			},
			text("reason", self.reason.as_str())?,
		])
	}

	/// The bytes the payload hash is taken over: a fixed field order with every
	/// variable-length part length-prefixed, so no two distinct terms can encode alike.
	///
	/// `reason` is IN the digest. Leaving it out would bind an approval to an amount and a
	/// destination while the sentence the human actually read stayed free to change — the
	/// one part of the approval that explains why the money is moving.
	pub fn canonical_bytes(&self) -> Vec<u8> {
		let mut out = Vec::with_capacity(Self::DOMAIN.len() + 160);
		out.extend_from_slice(Self::DOMAIN);
		push_field(&mut out, self.from.kind_str().as_bytes());
		push_field(&mut out, self.from.id_str().as_bytes());
		match &self.to {
			PaymentDestination::Internal(party) => {
				push_field(&mut out, b"internal");
				push_field(&mut out, party.kind_str().as_bytes());
				push_field(&mut out, party.id_str().as_bytes());
			}
			PaymentDestination::External { network, address } => {
				push_field(&mut out, b"external");
				push_field(&mut out, network.as_str().as_bytes());
				push_field(&mut out, address.as_str().as_bytes());
			}
		}
		out.extend_from_slice(&self.amount.base_units().to_be_bytes());
		push_field(&mut out, self.reason.as_str().as_bytes());
		out
	}
}

/// One end of a payment, in words rather than as a `(kind, id)` pair.
///
/// IT LIVES HERE, BESIDE THE DIGESTED TERMS, because three surfaces must describe one money
/// move identically — the owners' approval, the subject's confirmation page, and the
/// payments history — and the terms are the only thing all three are bound to. A per-surface
/// formatter is how two surfaces come to disagree about what was approved.
///
/// An investor keeps their id, because "an investor" is not an answer to "which one".
fn party_label(party: &Party) -> String {
	match party {
		Party::User(user) => format!("investor {user}"),
		Party::Treasury => "banking's treasury".to_owned(),
	}
}

/// The `subject_ref` of the `bank:payment` approval over an order: `payment:<lowercase uuid>`,
/// the pattern concierge holds a reference to. Resolved by banking alone.
pub fn approval_subject_ref(id: PaymentId) -> String {
	format!("payment:{}", id.raw())
}

/// The order a `subject_ref` names — only banking's own lowercase spelling, so one order has
/// one reference.
pub fn parse_approval_subject_ref(subject_ref: &str) -> Option<PaymentId> {
	let raw = subject_ref.strip_prefix("payment:")?;
	let id = uuid::Uuid::parse_str(raw).ok().filter(|id| id.to_string() == raw)?;
	Some(PaymentId::from_raw(id))
}

/// The payment-order aggregate. Construct via [`PaymentOrder::open`] (raises
/// [`PaymentEvent::Opened`]) or [`PaymentOrder::rehydrate`] (load from the store, no
/// events).
#[derive(Clone, Debug)]
pub struct PaymentOrder {
	id: PaymentId,
	terms: PaymentTerms,
	payload_hash: [u8; 32],
	initiator: UserId,
	state: PaymentState,
	created_at: i64,
	expires_at: i64,
	decided_at: Option<i64>,
	executed: Option<PaymentEffect>,
	failure_reason: Option<String>,
	version: u64,
	pending: Vec<PaymentEvent>,
}

impl PaymentOrder {
	/// Record the intent. `payload_hash` is the SHA-256 of [`PaymentTerms::canonical_bytes`]
	/// taken by the application layer (the domain stays free of crypto).
	///
	/// The requirement is READ OFF the terms rather than supplied, for the same reason the
	/// tier is: two statements of one fact are two things that can disagree, and the
	/// disagreement here would be a payment approved by the wrong party.
	pub fn open(id: PaymentId, terms: PaymentTerms, payload_hash: [u8; 32], initiator: UserId, created_at: i64) -> Self {
		Self::open_for(id, terms, payload_hash, initiator, created_at, TTL_SECS)
	}

	/// [`Self::open`] with a window of its own. An order the owners decide over a concierge
	/// approval lives [`CONCIERGE_TTL_SECS`]: the approval's own clock (72 hours open, then its
	/// veto delay) is what ends it, and the order's must not run out first, or owners would
	/// vote an approval that the order has already expired under.
	pub fn open_for(id: PaymentId, terms: PaymentTerms, payload_hash: [u8; 32], initiator: UserId, created_at: i64, ttl_secs: i64) -> Self {
		let mut order = Self {
			id,
			payload_hash,
			initiator,
			state: PaymentState::Pending,
			created_at,
			expires_at: created_at.saturating_add(ttl_secs),
			decided_at: None,
			executed: None,
			failure_reason: None,
			version: 1,
			pending: Vec::new(),
			terms,
		};
		order.pending.push(PaymentEvent::Opened {
			payment_id: id,
			terms: order.terms.clone(),
			payload_hash: hex32(&payload_hash),
			initiator,
			requirement: order.terms.requirement(),
			tier: order.terms.tier(),
			expires_at: order.expires_at,
		});
		order
	}

	/// Reconstitute from the store. Raises no events.
	// One parameter per stored column, by design: a struct of the same eleven fields would
	// only move the arity behind a name and cost the compiler's check that the repository
	// filled every one of them.
	#[allow(clippy::too_many_arguments)]
	pub fn rehydrate(
		id: PaymentId,
		terms: PaymentTerms,
		payload_hash: [u8; 32],
		initiator: UserId,
		state: PaymentState,
		created_at: i64,
		expires_at: i64,
		decided_at: Option<i64>,
		executed: Option<PaymentEffect>,
		failure_reason: Option<String>,
		version: u64,
	) -> Self {
		Self {
			id,
			terms,
			payload_hash,
			initiator,
			state,
			created_at,
			expires_at,
			decided_at,
			executed,
			failure_reason,
			version,
			pending: Vec::new(),
		}
	}

	/// The order's one requirement is satisfied — the owners' approval passed, or the subject
	/// consented. Idempotent.
	///
	/// For an L3 order this is also where the source is **reserved**: a pending
	/// `Dr <source> / Cr clearing`, exactly as a withdrawal reserves, so N concurrent
	/// approved payments physically cannot overdraw one claim. An L1 order raises no
	/// reservation here — its withdrawal takes the identical reserve when it is created,
	/// and two of them would lock the amount twice.
	pub fn approve(&mut self, at: i64) -> Result<(), DomainError> {
		if self.state == PaymentState::Approved {
			return Ok(());
		}
		self.require_pending(PaymentState::Approved)?;
		self.state = PaymentState::Approved;
		self.decided_at = Some(at);
		self.raise(PaymentEvent::ApprovalRecorded {
			payment_id: self.id,
			requirement: self.terms.requirement(),
			at,
		});
		self.raise(PaymentEvent::Approved { payment_id: self.id, at });
		if self.terms.tier().settles_on_the_ledger() {
			self.raise(PaymentEvent::Reserved {
				payment_id: self.id,
				from: self.terms.from.clone(),
				amount: self.terms.amount,
				at,
			});
		}
		Ok(())
	}

	/// The approver refused, or their consent was voided. Terminal. Idempotent.
	pub fn reject(&mut self, at: i64) -> Result<(), DomainError> {
		if self.enter_verdict(PaymentState::Rejected, at)? {
			self.raise(PaymentEvent::Rejected { payment_id: self.id, at });
		}
		Ok(())
	}

	/// The window closed with no answer. Refuses to expire early, so a clock skew cannot
	/// cut an approver's window short. Idempotent.
	pub fn expire(&mut self, at: i64) -> Result<(), DomainError> {
		if self.state.is_pending() && at < self.expires_at {
			return Err(DomainError::Conflict("payment has not expired yet".into()));
		}
		if self.enter_verdict(PaymentState::Expired, at)? {
			self.raise(PaymentEvent::Expired { payment_id: self.id, at });
		}
		Ok(())
	}

	/// The initiator withdrew it. Ownership is checked by the caller. Idempotent.
	pub fn cancel(&mut self, at: i64) -> Result<(), DomainError> {
		if self.enter_verdict(PaymentState::Cancelled, at)? {
			self.raise(PaymentEvent::Cancelled { payment_id: self.id, at });
		}
		Ok(())
	}

	/// The effect now exists. Written exactly once — a repeat naming the SAME effect is the
	/// idempotent retry an at-least-once execution path depends on, and one naming a
	/// different effect is a conflict rather than a silent overwrite.
	///
	/// An L3 order raises its [`PaymentEvent::Settled`] here, immediately before
	/// `Executed`. The reservation raised by [`Self::approve`] committed in its own, earlier
	/// transaction; the relay is single-worker and strictly ordered, so it always applies
	/// before the settlement that posts it.
	pub fn mark_executed(&mut self, effect: PaymentEffect, at: i64) -> Result<(), DomainError> {
		if !effect.matches(self.terms.tier()) {
			return Err(DomainError::Conflict(format!("a {} payment cannot be executed as this effect", self.terms.tier().as_str())));
		}
		if self.state == PaymentState::Executed {
			return if self.executed == Some(effect) {
				Ok(())
			} else {
				Err(DomainError::Conflict("payment already executed a different effect".into()))
			};
		}
		if self.state != PaymentState::Approved {
			return Err(DomainError::Conflict(format!("payment is {}, not executable", self.state.as_str())));
		}
		self.state = PaymentState::Executed;
		self.executed = Some(effect);
		if let (PaymentEffect::Transfer, Some(to)) = (effect, self.terms.to.party().cloned()) {
			self.raise(PaymentEvent::Settled {
				payment_id: self.id,
				from: self.terms.from.clone(),
				to,
				amount: self.terms.amount,
				at,
			});
		}
		self.raise(PaymentEvent::Executed { payment_id: self.id, effect, at });
		Ok(())
	}

	/// The approved payment could not be executed. Terminal: nothing retries silently, and
	/// the reason is what the initiator reads.
	///
	/// An L3 order RELEASES ITS RESERVATION here. `Approved` is a real committed state
	/// for those tiers — the reservation is raised by [`Self::approve`] in one transaction
	/// and the settlement by [`Self::mark_executed`] in another, so an execution that fails
	/// between them (a re-check refusing a stale consent, say) would otherwise leave the
	/// amount locked in `clearing` under a terminal order that nothing will ever post or
	/// void. [`PaymentEvent::Released`] is the void, exactly as a failed withdrawal voids
	/// its own reservation; it is raised before the audit fact so the relay applies it in
	/// that order. An L1 order reserved nothing and releases nothing.
	pub fn mark_execution_failed(&mut self, reason: String, at: i64) -> Result<(), DomainError> {
		if self.state == PaymentState::ExecutionFailed {
			return Ok(());
		}
		if self.state != PaymentState::Approved {
			return Err(DomainError::Conflict(format!("payment is {}, not executable", self.state.as_str())));
		}
		self.state = PaymentState::ExecutionFailed;
		self.failure_reason = Some(reason.clone());
		if self.terms.tier().settles_on_the_ledger() {
			self.raise(PaymentEvent::Released {
				payment_id: self.id,
				from: self.terms.from.clone(),
				amount: self.terms.amount,
				at,
			});
		}
		self.raise(PaymentEvent::ExecutionFailed { payment_id: self.id, reason, at });
		Ok(())
	}

	fn require_pending(&self, target: PaymentState) -> Result<(), DomainError> {
		if self.state.is_pending() {
			Ok(())
		} else {
			Err(DomainError::Conflict(format!("payment is {}, not {}", self.state.as_str(), target.as_str())))
		}
	}

	/// The shared body of the three verdicts reachable from `pending`: idempotent on
	/// repeat, a conflict from any other state.
	///
	/// Returns whether the transition actually happened, so the CALLER raises its own event.
	/// Deriving the event from `target` here would need a `match` over every
	/// [`PaymentState`], and the only honest arm for the four states that are not verdicts
	/// is one that cannot happen — a shape where mistyping a caller's target silently files
	/// a cancellation as an expiry.
	fn enter_verdict(&mut self, target: PaymentState, at: i64) -> Result<bool, DomainError> {
		if self.state == target {
			return Ok(false);
		}
		self.require_pending(target)?;
		self.state = target;
		self.decided_at = Some(at);
		Ok(true)
	}

	fn raise(&mut self, event: PaymentEvent) {
		self.version = self.version.saturating_add(1);
		self.pending.push(event);
	}

	pub fn id(&self) -> PaymentId {
		self.id
	}

	/// The bytes concierge's `payload_hash` for this order's `bank:payment` approval is the
	/// SHA-256 of (`scope_operations::approval_payload`), over fields rendered with
	/// `destination`. The application layer takes the digest, the domain stays free of crypto.
	pub fn approval_payload(&self, destination: &str) -> Result<String, DomainError> {
		let fields = self.terms.approval_fields(destination)?;
		Ok(approval_payload(PAYMENT_KIND, PAYMENT_APPROVAL_SCOPE, &approval_subject_ref(self.id), &fields))
	}

	pub fn terms(&self) -> &PaymentTerms {
		&self.terms
	}

	pub fn tier(&self) -> PaymentTier {
		self.terms.tier()
	}

	pub fn requirement(&self) -> PaymentApproval {
		self.terms.requirement()
	}

	pub fn source_claim(&self) -> LedgerAccountKey {
		self.terms.source_claim()
	}

	pub fn payload_hash(&self) -> [u8; 32] {
		self.payload_hash
	}

	/// The hash as the wire shows it.
	pub fn payload_hash_hex(&self) -> String {
		hex32(&self.payload_hash)
	}

	pub fn initiator(&self) -> UserId {
		self.initiator
	}

	pub fn state(&self) -> PaymentState {
		self.state
	}

	pub fn created_at(&self) -> i64 {
		self.created_at
	}

	pub fn expires_at(&self) -> i64 {
		self.expires_at
	}

	pub fn decided_at(&self) -> Option<i64> {
		self.decided_at
	}

	pub fn effect(&self) -> Option<PaymentEffect> {
		self.executed
	}

	/// The executed effect NARROWED to a withdrawal — the projection the `payments` table's
	/// `executed_withdrawal_id` column wants. An L3 settlement leaves that column NULL,
	/// which is why this returns an `Option` over the variant rather than over the effect.
	pub fn executed_withdrawal_id(&self) -> Option<WithdrawalId> {
		// Exhaustive on purpose: a third effect must not silently fall through to `None` and
		// leave the column blank on a row that did execute.
		match self.executed? {
			PaymentEffect::Withdrawal(id) => Some(id),
			PaymentEffect::Transfer => None,
		}
	}

	pub fn failure_reason(&self) -> Option<&str> {
		self.failure_reason.as_deref()
	}

	pub fn version(&self) -> u64 {
		self.version
	}
}

/// Facts raised by the [`PaymentOrder`] aggregate.
///
/// **Only [`PaymentEvent::Reserved`], [`PaymentEvent::Settled`] and
/// [`PaymentEvent::Released`] reach the outbox** ([`PaymentEvent::relays`]); everything else
/// is an audit fact drained with `relay = false`. In particular
/// [`PaymentEvent::Executed`] must NOT be relayed: for an L1 order the money is already
/// moved by the [`crate::withdrawals::WithdrawalEvent::Requested`] sitting in the outbox,
/// and two relayed events over one payment means two reservations against one claim.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaymentEvent {
	Opened {
		payment_id: PaymentId,
		terms: PaymentTerms,
		payload_hash: String,
		initiator: UserId,
		requirement: PaymentApproval,
		tier: PaymentTier,
		expires_at: i64,
	},
	ApprovalRecorded {
		payment_id: PaymentId,
		requirement: PaymentApproval,
		at: i64,
	},
	Approved {
		payment_id: PaymentId,
		at: i64,
	},
	/// L3 only — reserve the gross against the source claim into `clearing` (relay:
	/// pending `Dr <source> / Cr clearing`).
	Reserved {
		payment_id: PaymentId,
		from: Party,
		amount: Usdt,
		at: i64,
	},
	/// L3 only — post the reservation, then move the gross out of `clearing` into the
	/// destination claim.
	Settled {
		payment_id: PaymentId,
		from: Party,
		to: Party,
		amount: Usdt,
		at: i64,
	},
	Executed {
		payment_id: PaymentId,
		effect: PaymentEffect,
		at: i64,
	},
	/// L3 only — the execution failed after the reservation landed: void the pending
	/// `Dr <source> / Cr clearing`, returning the amount to the source in full (relay).
	Released {
		payment_id: PaymentId,
		from: Party,
		amount: Usdt,
		at: i64,
	},
	ExecutionFailed {
		payment_id: PaymentId,
		reason: String,
		at: i64,
	},
	Rejected {
		payment_id: PaymentId,
		at: i64,
	},
	Expired {
		payment_id: PaymentId,
		at: i64,
	},
	Cancelled {
		payment_id: PaymentId,
		at: i64,
	},
}

impl PaymentEvent {
	/// Whether this fact moves money and therefore belongs in the `outbox`.
	///
	/// Exhaustive with no `_` arm: a new event must be a deliberate answer to "does the
	/// relay act on this?", because answering it wrong in the `true` direction double-moves
	/// money and in the `false` direction strands it.
	pub fn relays(&self) -> bool {
		match self {
			Self::Reserved { .. } | Self::Settled { .. } | Self::Released { .. } => true,
			Self::Opened { .. }
			| Self::ApprovalRecorded { .. }
			| Self::Approved { .. }
			| Self::Executed { .. }
			| Self::ExecutionFailed { .. }
			| Self::Rejected { .. }
			| Self::Expired { .. }
			| Self::Cancelled { .. } => false,
		}
	}
}

impl DomainEvent for PaymentEvent {
	const KIND: &'static str = "payments";
}

impl Entity for PaymentOrder {
	type Id = PaymentId;

	fn id(&self) -> PaymentId {
		self.id
	}
}

impl AggregateRoot for PaymentOrder {
	const NAME: &'static str = "payment";
}

impl EmitsEvents for PaymentOrder {
	type Event = PaymentEvent;

	fn drain_events(&mut self) -> Vec<PaymentEvent> {
		core::mem::take(&mut self.pending)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const NOW: i64 = 1_700_000_000;

	fn investor() -> Party {
		Party::User(UserId::from_raw(uuid::Uuid::from_u128(7)))
	}

	fn treasury() -> Party {
		Party::Treasury
	}

	fn reason() -> PaymentReason {
		PaymentReason::new("quarterly rebalance").unwrap()
	}

	fn usdt(units: &str) -> Usdt {
		Usdt::parse_decimal(units).unwrap()
	}

	fn address() -> WalletAddress {
		WalletAddress::parse(Network::Bep20, "0x52908400098527886E0F7030069857D2E4169EE7").unwrap()
	}

	fn external() -> PaymentDestination {
		PaymentDestination::External {
			network: Network::Bep20,
			address: address(),
		}
	}

	fn terms(from: Party, to: PaymentDestination) -> PaymentTerms {
		PaymentTerms::new(from, to, usdt("100"), reason()).unwrap()
	}

	fn opened(from: Party, to: PaymentDestination) -> PaymentOrder {
		let mut order = PaymentOrder::open(PaymentId::new(), terms(from, to), [7u8; 32], UserId::new(), NOW);
		order.drain_events();
		order
	}

	#[test]
	fn the_tier_is_read_off_the_destination_and_never_supplied() {
		// The table in the module header, asserted rather than described.
		assert_eq!(PaymentDestination::Internal(investor()).tier(), PaymentTier::Internal);
		assert_eq!(external().tier(), PaymentTier::External);
		// And the effect each tier admits: only the external tier hands the money to the
		// withdrawal saga, so only it may record a withdrawal id.
		assert!(!PaymentTier::External.settles_on_the_ledger());
		assert!(PaymentTier::Internal.settles_on_the_ledger());
		for tier in [PaymentTier::Internal, PaymentTier::External] {
			assert_eq!(PaymentTier::parse(tier.as_str()).unwrap(), tier);
		}
		assert!(PaymentTier::parse("l1").is_err());
		assert!(PaymentTier::parse("service").is_err());
	}

	#[test]
	fn treasury_money_needs_the_owners_and_a_users_money_needs_their_consent_at_every_tier() {
		// THE §3 MATRIX. The source decides, and the tier does not enter into it — which is
		// the whole content of the rule, and the cell an earlier draft got wrong.
		let user = UserId::new();
		let destinations = || vec![PaymentDestination::Internal(investor()), PaymentDestination::Internal(Party::User(UserId::new())), external()];
		for to in destinations() {
			let terms = PaymentTerms::new(treasury(), to, usdt("5"), reason()).unwrap();
			assert_eq!(
				terms.requirement(),
				PaymentApproval::OwnersApproval,
				"treasury money at the {} tier must need the owners",
				terms.tier().as_str()
			);
		}
		for to in destinations() {
			let terms = PaymentTerms::new(Party::User(user), to, usdt("5"), reason()).unwrap();
			assert_eq!(
				terms.requirement(),
				PaymentApproval::SubjectConsent(user),
				"a user's own money at the {} tier must need that user's consent",
				terms.tier().as_str()
			);
		}
		// There is NO cell in which an admin alone may move treasury money: every source
		// resolves to one of exactly two requirements, and neither is "the initiator".
	}

	#[test]
	fn the_treasury_pays_out_but_is_never_paid_into() {
		let terms = terms(treasury(), external());
		assert_eq!(terms.requirement(), PaymentApproval::OwnersApproval);
		assert!(PaymentTerms::new(treasury(), PaymentDestination::Internal(investor()), usdt("5"), reason()).is_ok());
		for from in [investor(), Party::User(UserId::new())] {
			assert!(PaymentTerms::new(from, PaymentDestination::Internal(treasury()), usdt("5"), reason()).is_err());
		}
	}

	#[test]
	fn the_source_claim_is_the_partys_own_claim() {
		let user = UserId::new();
		assert_eq!(terms(Party::User(user), external()).source_claim(), LedgerAccountKey::UserClaim(user));
		assert_eq!(
			terms(treasury(), external()).source_claim(),
			LedgerAccountKey::ScopeAccount(crate::scopes::ScopeId::main(), crate::scopes::ScopeAccountLabel::treasury())
		);
	}

	#[test]
	fn the_parties_are_named_out_loud() {
		// The label is what the owners' approval, the confirmation page and the history all
		// render — one wording per party, decided here.
		assert_eq!(terms(treasury(), external()).source_label(), "banking's treasury");
		let user = UserId::new();
		assert_eq!(terms(Party::User(user), external()).source_label(), format!("investor {user}"));
		assert_eq!(terms(treasury(), PaymentDestination::Internal(Party::User(user))).destination_label(), format!("investor {user}"));
	}

	#[test]
	fn the_constructor_refuses_every_shape_that_can_never_execute() {
		// from == to: nothing moves, yet an approval and an index slot are spent.
		let err = PaymentTerms::new(investor(), PaymentDestination::Internal(investor()), usdt("5"), reason()).unwrap_err();
		assert!(matches!(err, DomainError::Validation(_)));
		let user = UserId::new();
		assert!(PaymentTerms::new(Party::User(user), PaymentDestination::Internal(Party::User(user)), usdt("5"), reason()).is_err());
		// Zero: TigerBeetle rejects a zero-amount transfer, so this order could only park.
		assert!(PaymentTerms::new(investor(), external(), Usdt::ZERO, reason()).is_err());
		// An address for another rail.
		let mismatched = PaymentDestination::External {
			network: Network::Trc20,
			address: address(),
		};
		assert!(PaymentTerms::new(investor(), mismatched, usdt("5"), reason()).is_err());
		// The two ends are still allowed to be different users, and a different service.
		assert!(PaymentTerms::new(Party::User(user), PaymentDestination::Internal(Party::User(UserId::new())), usdt("5"), reason()).is_ok());
	}

	#[test]
	fn a_reason_is_required_bounded_in_bytes_and_free_of_control_characters() {
		assert!(PaymentReason::new("").is_err());
		assert!(PaymentReason::new("   ").is_err());
		assert!(PaymentReason::new("x".repeat(MAX_REASON_BYTES)).is_ok());
		assert!(PaymentReason::new("x".repeat(MAX_REASON_BYTES + 1)).is_err());
		// BYTES, not chars: the limit bounds what a column and an approval carry, and both count
		// bytes. A 2-byte character therefore costs two, so half as many of them fit.
		let multibyte = "д".repeat(MAX_REASON_BYTES / 2 + 1);
		assert!(multibyte.chars().count() <= MAX_REASON_BYTES, "still well inside a character-counted limit");
		assert!(PaymentReason::new(multibyte).is_err());
		// A newline in the reason forges a line in the plain text of the approval.
		assert!(PaymentReason::new("paid\nAmount: 1000000").is_err());
		assert!(PaymentReason::new("tab\there").is_err());
	}

	#[test]
	fn the_canonical_encoding_separates_fields_and_carries_the_reason() {
		let a = PaymentTerms::new(investor(), external(), usdt("100"), PaymentReason::new("ab").unwrap()).unwrap();
		let b = PaymentTerms::new(investor(), external(), usdt("100"), PaymentReason::new("a").unwrap()).unwrap();
		// Length prefixes: two reasons that concatenate alike must not encode alike.
		assert_ne!(a.canonical_bytes(), b.canonical_bytes());
		assert_eq!(a.canonical_bytes(), a.canonical_bytes());
		// THE REASON IS IN THE DIGEST. Without this an approval would bind to the amount and
		// the destination while the sentence the human read stayed free to change.
		let reworded = PaymentTerms::new(investor(), external(), usdt("100"), PaymentReason::new("something else entirely").unwrap()).unwrap();
		assert_ne!(a.canonical_bytes(), reworded.canonical_bytes());
		// So are the amount and both ends.
		let dearer = PaymentTerms::new(investor(), external(), usdt("101"), PaymentReason::new("ab").unwrap()).unwrap();
		assert_ne!(a.canonical_bytes(), dearer.canonical_bytes());
		let elsewhere = PaymentTerms::new(treasury(), external(), usdt("100"), PaymentReason::new("ab").unwrap()).unwrap();
		assert_ne!(a.canonical_bytes(), elsewhere.canonical_bytes());
		// The domain prefix opens every encoding, so a digest over these terms can never be
		// mistaken for one over another subject.
		assert!(a.canonical_bytes().starts_with(PaymentTerms::DOMAIN));
		assert_eq!(PaymentTerms::DOMAIN, b"banking.v1.PaymentTerms\x00");
	}

	#[test]
	fn an_internal_destination_cannot_encode_as_an_external_one() {
		// The discriminator is length-prefixed inside the encoding, so no arrangement of a
		// party id can make an internal payment hash like a payment to an address.
		let internal = PaymentTerms::new(treasury(), PaymentDestination::Internal(investor()), usdt("1"), reason()).unwrap();
		let out = PaymentTerms::new(treasury(), external(), usdt("1"), reason()).unwrap();
		assert_ne!(internal.canonical_bytes(), out.canonical_bytes());
	}

	#[test]
	fn an_internal_payment_reserves_on_approval_and_settles_on_execution() {
		let mut order = opened(treasury(), PaymentDestination::Internal(investor()));
		order.approve(NOW).unwrap();
		let events = order.drain_events();
		assert!(matches!(events[0], PaymentEvent::ApprovalRecorded { .. }));
		assert!(matches!(events[1], PaymentEvent::Approved { .. }));
		// The reserve is what makes N concurrent approved payments unable to overdraw one
		// claim — and it is the ONLY approval-time fact the relay acts on.
		assert!(matches!(events[2], PaymentEvent::Reserved { .. }));
		assert_eq!(events.iter().filter(|e| e.relays()).count(), 1);

		order.mark_executed(PaymentEffect::Transfer, NOW + 1).unwrap();
		let events = order.drain_events();
		// Settled BEFORE Executed: the relay drains in strict order, and the settlement is
		// what posts the reservation raised above.
		assert!(matches!(events[0], PaymentEvent::Settled { .. }));
		assert!(matches!(events[1], PaymentEvent::Executed { .. }));
		assert!(events[0].relays());
		assert!(!events[1].relays(), "Executed must never reach the outbox");
		assert_eq!(order.state(), PaymentState::Executed);
		assert_eq!(order.effect(), Some(PaymentEffect::Transfer));
		assert_eq!(order.executed_withdrawal_id(), None);
	}

	#[test]
	fn an_external_payment_reserves_nothing_and_relays_nothing() {
		// TWO RESERVES AGAINST ONE CLAIM IS THE BUG THIS PREVENTS. An L1 payment's money is
		// moved by the withdrawal it creates, whose own `Requested` event is already in the
		// outbox taking the identical `Dr <source> / Cr clearing` pending.
		let mut order = opened(investor(), external());
		order.approve(NOW).unwrap();
		let events = order.drain_events();
		assert!(!events.iter().any(|e| matches!(e, PaymentEvent::Reserved { .. })));
		assert_eq!(events.iter().filter(|e| e.relays()).count(), 0);

		let withdrawal = WithdrawalId::new();
		order.mark_executed(PaymentEffect::Withdrawal(withdrawal), NOW + 1).unwrap();
		let events = order.drain_events();
		assert!(!events.iter().any(|e| matches!(e, PaymentEvent::Settled { .. })));
		assert_eq!(events.iter().filter(|e| e.relays()).count(), 0, "an L1 payment relays nothing at all");
		assert_eq!(order.executed_withdrawal_id(), Some(withdrawal));
	}

	#[test]
	fn an_effect_that_does_not_match_the_tier_is_refused() {
		// The tier says which effect is possible; recording the other one would leave the
		// money in one plane and the record in the other.
		let mut internal = opened(treasury(), PaymentDestination::Internal(investor()));
		internal.approve(NOW).unwrap();
		assert!(matches!(
			internal.mark_executed(PaymentEffect::Withdrawal(WithdrawalId::new()), NOW),
			Err(DomainError::Conflict(_))
		));

		let mut out = opened(investor(), external());
		out.approve(NOW).unwrap();
		assert!(matches!(out.mark_executed(PaymentEffect::Transfer, NOW), Err(DomainError::Conflict(_))));
	}

	#[test]
	fn execution_is_recorded_once_and_retries_are_no_ops() {
		let mut order = opened(investor(), external());
		order.approve(NOW).unwrap();
		order.drain_events();
		let withdrawal = WithdrawalId::new();
		order.mark_executed(PaymentEffect::Withdrawal(withdrawal), NOW).unwrap();
		order.drain_events();
		// The deterministic id makes a retry name the same withdrawal, so it is a no-op —
		// the difference between an at-least-once execution path and a double payment.
		order.mark_executed(PaymentEffect::Withdrawal(withdrawal), NOW + 5).unwrap();
		assert!(order.drain_events().is_empty());
		// A different effect is a conflict, never a silent overwrite.
		assert!(matches!(
			order.mark_executed(PaymentEffect::Withdrawal(WithdrawalId::new()), NOW + 6),
			Err(DomainError::Conflict(_))
		));
		// And a failure can no longer be recorded over a completed execution.
		assert!(order.mark_execution_failed("too late".into(), NOW + 7).is_err());
	}

	#[test]
	fn approval_is_idempotent_and_only_reachable_from_pending() {
		let mut order = opened(treasury(), PaymentDestination::Internal(investor()));
		order.approve(NOW).unwrap();
		order.drain_events();
		order.approve(NOW + 1).unwrap();
		assert!(order.drain_events().is_empty(), "a repeat approval raises nothing — and reserves nothing twice");

		let mut cancelled = opened(treasury(), PaymentDestination::Internal(investor()));
		cancelled.cancel(NOW).unwrap();
		assert!(matches!(cancelled.approve(NOW + 1), Err(DomainError::Conflict(_))));
	}

	#[test]
	fn a_refused_approval_ends_the_payment_dead() {
		let mut order = opened(Party::User(UserId::new()), external());
		order.reject(NOW).unwrap();
		assert_eq!(order.state(), PaymentState::Rejected);
		order.drain_events();
		order.reject(NOW + 1).unwrap();
		assert!(order.drain_events().is_empty());
		assert!(order.approve(NOW + 2).is_err(), "a rejected payment can never be approved afterwards");
		assert!(order.mark_executed(PaymentEffect::Withdrawal(WithdrawalId::new()), NOW + 3).is_err());
	}

	#[test]
	fn cancel_and_expire_are_idempotent_mutually_exclusive_and_never_early() {
		let mut cancelled = opened(investor(), external());
		cancelled.cancel(NOW).unwrap();
		cancelled.drain_events();
		cancelled.cancel(NOW + 1).unwrap();
		assert!(cancelled.drain_events().is_empty());
		assert!(cancelled.expire(NOW + TTL_SECS + 1).is_err());

		let mut expired = opened(investor(), external());
		// Refuses to cut the window short, so a clock skew cannot void a live approval.
		assert!(matches!(expired.expire(NOW + 10), Err(DomainError::Conflict(_))));
		expired.expire(NOW + TTL_SECS).unwrap();
		assert_eq!(expired.state(), PaymentState::Expired);
		expired.expire(NOW + TTL_SECS + 99).unwrap();
		assert!(expired.cancel(NOW).is_err());
		// An expired order is not executable by any ordering of the transitions.
		assert!(expired.mark_executed(PaymentEffect::Withdrawal(WithdrawalId::new()), NOW + TTL_SECS + 1).is_err());
	}

	#[test]
	fn execution_failure_is_terminal_and_states_why() {
		let mut order = opened(investor(), external());
		order.approve(NOW).unwrap();
		order.drain_events();
		order.mark_execution_failed("payout exceeds the fund's available revenue".into(), NOW).unwrap();
		assert_eq!(order.state(), PaymentState::ExecutionFailed);
		assert_eq!(order.failure_reason(), Some("payout exceeds the fund's available revenue"));
		let events = order.drain_events();
		// An L1 order reserved nothing, so it has nothing to release: the failure is audit only.
		assert!(matches!(events.as_slice(), [PaymentEvent::ExecutionFailed { .. }]));
		assert_eq!(events.iter().filter(|e| e.relays()).count(), 0);
		order.mark_execution_failed("again".into(), NOW + 1).unwrap();
		assert!(order.drain_events().is_empty());
		// Nothing retries silently: a failure cannot become an execution.
		assert!(order.mark_executed(PaymentEffect::Withdrawal(WithdrawalId::new()), NOW + 2).is_err());
	}

	/// `Approved` IS A COMMITTED STATE for the ledger tiers: the reservation and the
	/// settlement land in different transactions. A failure between them must give the
	/// locked amount back, or a terminal order strands it in `clearing` forever.
	#[test]
	fn an_internal_payment_that_fails_to_execute_releases_its_reservation() {
		for to in [PaymentDestination::Internal(investor()), PaymentDestination::Internal(Party::User(UserId::new()))] {
			let mut order = opened(treasury(), to);
			order.approve(NOW).unwrap();
			order.drain_events();
			order.mark_execution_failed("the consent was voided".into(), NOW + 1).unwrap();
			let events = order.drain_events();
			// The void FIRST, then the audit fact — the relay drains in strict order and the
			// money fact is the one it acts on.
			assert!(matches!(events[0], PaymentEvent::Released { .. }), "at the {} tier", order.tier().as_str());
			assert!(matches!(events[1], PaymentEvent::ExecutionFailed { .. }));
			assert!(events[0].relays());
			assert!(!events[1].relays());
			assert_eq!(events.iter().filter(|e| e.relays()).count(), 1, "exactly one money fact: the release");
			let PaymentEvent::Released { from, amount, .. } = &events[0] else { unreachable!() };
			assert_eq!(from, &treasury());
			assert_eq!(*amount, usdt("100"));
			assert_eq!(order.state(), PaymentState::ExecutionFailed);
			assert!(!order.state().is_open(), "a failed order no longer holds its source");
			// Idempotent: a second failure releases nothing twice.
			order.mark_execution_failed("again".into(), NOW + 2).unwrap();
			assert!(order.drain_events().is_empty());
		}
	}

	#[test]
	fn the_version_moves_on_every_fact() {
		let mut order = opened(treasury(), PaymentDestination::Internal(investor()));
		let at_open = order.version();
		order.approve(NOW).unwrap();
		// Three facts: the approval record, the verdict, and the reservation.
		assert_eq!(order.version(), at_open + 3);
	}

	#[test]
	fn states_round_trip_and_reject_junk() {
		for state in [
			PaymentState::Pending,
			PaymentState::Approved,
			PaymentState::Executed,
			PaymentState::ExecutionFailed,
			PaymentState::Rejected,
			PaymentState::Expired,
			PaymentState::Cancelled,
		] {
			assert_eq!(PaymentState::parse(state.as_str()).unwrap(), state);
		}
		assert!(PaymentState::parse("settled").is_err());
		// The index predicate: exactly the two states that still hold a source claim.
		assert!(PaymentState::Pending.is_open());
		assert!(PaymentState::Approved.is_open());
		for closed in [
			PaymentState::Executed,
			PaymentState::ExecutionFailed,
			PaymentState::Rejected,
			PaymentState::Expired,
			PaymentState::Cancelled,
		] {
			assert!(!closed.is_open(), "{} still claims its source", closed.as_str());
		}
	}

	#[test]
	fn events_round_trip_through_json() {
		let mut order = PaymentOrder::open(PaymentId::new(), terms(treasury(), external()), [0xab; 32], UserId::new(), NOW);
		let event = order.drain_events().pop().unwrap();
		let json = serde_json::to_string(&event).unwrap();
		let back: PaymentEvent = serde_json::from_str(&json).unwrap();
		let PaymentEvent::Opened {
			payload_hash, tier, requirement, ..
		} = back
		else {
			panic!("expected Opened")
		};
		assert_eq!(payload_hash, "ab".repeat(32));
		assert_eq!(tier, PaymentTier::External);
		assert_eq!(requirement, PaymentApproval::OwnersApproval);
	}

	/// BOTH DESTINATION SHAPES, BOTH DIRECTIONS.
	///
	/// The external one round-tripped under an internal tag by luck — its fields are
	/// `network`/`address`, which do not collide with `kind`. `Internal(Party)` does collide:
	/// [`Party`] carries its own `kind`, and the spliced map silently dropped the outer tag,
	/// so `{"kind":"internal"}` went in and `{"kind":"revenue"}` came out — unreadable. These
	/// bytes are audit (`event_log`), so a one-way encoding is a row nobody can ever read.
	#[test]
	fn every_destination_shape_survives_a_json_round_trip() {
		for destination in [external(), PaymentDestination::Internal(investor()), PaymentDestination::Internal(Party::User(UserId::new()))] {
			let terms = terms(treasury(), destination.clone());
			let json = serde_json::to_string(&terms).unwrap();
			let back: PaymentTerms = serde_json::from_str(&json).unwrap_or_else(|err| panic!("{destination:?} must read back: {err} — from {json}"));
			assert_eq!(back, terms);
			// The digest is taken over `canonical_bytes`, never over serde, so the two must
			// agree about what a round trip preserves.
			assert_eq!(back.canonical_bytes(), terms.canonical_bytes());
		}
	}
	/// The approval carries exactly the fields `bank:payment` declares, with the types it
	/// declares them in — else concierge refuses the proposal and no payment is ever decided.
	#[test]
	fn a_payment_approval_carries_what_the_catalog_declares() {
		use crate::scope_operations::{BANK_CATALOG, FieldType, PAYMENT_KIND};
		let spec = BANK_CATALOG.iter().find(|spec| spec.kind == PAYMENT_KIND).expect("bank:payment is published");
		let fields = terms(treasury(), external()).approval_fields("0x5290 on bep20").unwrap();
		for sent in &fields {
			let declared = spec.fields.iter().find(|field| field.key == sent.key).unwrap_or_else(|| panic!("{} undeclared", sent.key));
			assert_eq!(declared.field_type, sent.value.field_type(), "{}", sent.key);
		}
		for required in spec.fields.iter().filter(|field| field.required) {
			assert!(fields.iter().any(|sent| sent.key == required.key), "{} not sent", required.key);
		}
		assert_eq!(spec.fields.iter().find(|field| field.key == "amount").unwrap().field_type, FieldType::Amount);
	}

	/// A reason the approval would not carry is refused when the order opens, not silently cut:
	/// the owners decide over the whole sentence.
	#[test]
	fn a_text_the_approval_cannot_carry_is_refused_not_shortened() {
		let long = PaymentTerms::new(treasury(), external(), usdt("5"), PaymentReason::new("x".repeat(APPROVAL_TEXT_MAX_BYTES + 1)).unwrap()).unwrap();
		assert!(matches!(long.approval_fields("a place"), Err(DomainError::Validation(_))));
		let exact = PaymentTerms::new(treasury(), external(), usdt("5"), PaymentReason::new("x".repeat(APPROVAL_TEXT_MAX_BYTES)).unwrap()).unwrap();
		assert!(exact.approval_fields("a place").is_ok());
		for reason in ["see https://example.test/invoice", "pay via WWW.example.test"] {
			let linked = PaymentTerms::new(treasury(), external(), usdt("5"), PaymentReason::new(reason).unwrap()).unwrap();
			assert!(linked.approval_fields("a place").is_err(), "{reason}");
		}
		assert!(terms(treasury(), external()).approval_fields("a\nplace").is_err(), "a control character in a destination");
	}

	#[test]
	fn the_approval_subject_ref_is_the_lowercase_form_and_nothing_else() {
		let id = PaymentId::new();
		assert_eq!(parse_approval_subject_ref(&approval_subject_ref(id)), Some(id));
		assert_eq!(parse_approval_subject_ref(&format!("payment:{}", id.raw().to_string().to_uppercase())), None);
		assert_eq!(parse_approval_subject_ref(&format!("pending_operation:{}", id.raw())), None);
		assert_eq!(parse_approval_subject_ref("payment:nope"), None);
	}
}
