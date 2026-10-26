//! `scope_operations` — money a scope's service moves out of a scope account that is not a fee
//! sweep (`scope/v1/ledger.proto`): payouts into users' compartments, and the operations that
//! wait on an approval before banking carries them out (`RequestPayout`, `RequestTransfer`).
//!
//! A payout is automatic only for an automatic reason and inside the scope's automatic
//! ceilings; anything else is asked for, stored here whole as a [`PendingOperation`], and
//! carried out once, from the stored record, when concierge's approval of it passes. What the
//! approvers are shown — the approval's fields and its `payload_hash` — is rendered from that
//! record and nothing else, so what they approved is what executes.
//!
//! Pure and wasm-safe: ids are minted and digests taken by the application layer.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, LedgerRefusal, SubjectRef, canonical, validate_memo},
	scopes::{ScopeAccountLabel, ScopeId, ScopePolicy},
	users::UserId,
};

/// Most lines one payout or one requested payout may carry (`ledger.proto`). The ledger posts
/// them as one linked chain, well inside TigerBeetle's batch.
pub const MAX_PAYOUT_LINES: usize = 1000;

/// Longest `auth_subject` accepted, in bytes — what `users.auth_subject` was ever given.
pub const MAX_AUTH_SUBJECT: usize = 255;

/// The largest single line a payout may carry without the platform owners deciding it
/// (banking#525, 1000 USDT in 18-decimal base units). A fixed floor rather than a policy
/// column: it is what concierge's `per_line` escalation defaults to, and an operator cannot
/// raise one scope's line past it by editing a row. One recipient must not be able to take a
/// whole operation that stays under the per-operation ceiling.
pub const MAX_PAYOUT_LINE: Usdt = Usdt::from_base_units(1_000_000_000_000_000_000_000);

/// The currency every amount on a `bank:*` approval is denominated in.
pub const APPROVAL_CURRENCY: &str = "USDT";

/// One line as a client asks for it: the user as the identity plane names them, and what
/// they are paid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestedLine {
	pub auth_subject: String,
	pub amount: Usdt,
}

/// Refuses an empty or oversized set, a malformed subject, a subject named twice (one user is
/// one line: two lines for one user would let a split hide the size of a share) and a zero
/// amount. Returns the total.
pub fn check_lines(lines: &[RequestedLine]) -> Result<Usdt, DomainError> {
	if lines.is_empty() || lines.len() > MAX_PAYOUT_LINES {
		return Err(DomainError::Validation(format!("a payout has 1..={MAX_PAYOUT_LINES} lines, got {}", lines.len())));
	}
	let mut subjects: Vec<&str> = Vec::with_capacity(lines.len());
	let mut total = Usdt::ZERO;
	for line in lines {
		let subject = line.auth_subject.as_str();
		if subject.is_empty() || subject.len() > MAX_AUTH_SUBJECT || subject.chars().any(char::is_control) {
			return Err(DomainError::Validation(format!("auth_subject must be 1..{MAX_AUTH_SUBJECT} bytes of plain text")));
		}
		if line.amount.is_zero() {
			return Err(DomainError::Validation(format!("the line for {subject} must be positive")));
		}
		subjects.push(subject);
		total = total.checked_add(line.amount).ok_or_else(|| DomainError::Validation("the payout's total overflows".into()))?;
	}
	subjects.sort_unstable();
	if subjects.windows(2).any(|pair| pair[0] == pair[1]) {
		return Err(DomainError::Validation("one user may appear once in a payout".into()));
	}
	Ok(total)
}

/// The lines as an idempotency key stores them: order-free (sorted), every line length-prefixed,
/// amounts in base units — so a repeat that lists the same lines in another order is the same
/// request, and two different sets never render alike.
pub fn canonical_lines(lines: &[RequestedLine]) -> String {
	let mut rendered: Vec<String> = lines.iter().map(|line| format!("{}|{}", line.auth_subject, line.amount.base_units())).collect();
	rendered.sort_unstable();
	canonical(&rendered.iter().map(String::as_str).collect::<Vec<_>>())
}

/// What `lines_digest` is the SHA-256 of (`ledger.proto`, `PendingOperation.lines_digest`, and
/// concierge's `bank:scope_payout`): each line `<auth_subject>|<canonical amount>`, sorted
/// bytewise, joined by `\n` with no trailing newline. FROZEN: a digest stored on an operation
/// and carried by its approval is compared against this rendering at execution.
pub fn lines_digest_input(lines: &[RequestedLine]) -> String {
	let mut rendered: Vec<String> = lines.iter().map(|line| format!("{}|{}", line.auth_subject, line.amount.to_decimal_string())).collect();
	rendered.sort_unstable();
	rendered.join("\n")
}

/// The largest single line — what an approver must see beside the total, so a distribution
/// cannot hide one outsized share among many small ones.
pub fn max_line(lines: &[RequestedLine]) -> Usdt {
	lines.iter().map(|line| line.amount).max().unwrap_or(Usdt::ZERO)
}

/// Why money leaves a scope account for users' compartments (`PayoutReason`). The first three
/// are a client's automatic reasons; [`Self::Approved`] is banking executing an approved
/// `RequestPayout`, never something a client may name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayoutReason {
	Redemption,
	Book,
	FeeRebate,
	Approved,
}

impl PayoutReason {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Redemption => "redemption",
			Self::Book => "book",
			Self::FeeRebate => "fee_rebate",
			Self::Approved => "approved",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"redemption" => Ok(Self::Redemption),
			"book" => Ok(Self::Book),
			"fee_rebate" => Ok(Self::FeeRebate),
			"approved" => Ok(Self::Approved),
			other => Err(DomainError::Validation(format!("unknown payout reason: {other}"))),
		}
	}
}

/// One line of a payout once banking has resolved the user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutLine {
	pub user: UserId,
	/// The user as the client named them, echoed back and shown in money events.
	pub auth_subject: String,
	pub amount: Usdt,
}

impl PayoutLine {
	fn requested(&self) -> RequestedLine {
		RequestedLine {
			auth_subject: self.auth_subject.clone(),
			amount: self.amount,
		}
	}
}

fn requested_lines(lines: &[PayoutLine]) -> Vec<RequestedLine> {
	lines.iter().map(PayoutLine::requested).collect()
}

/// A unique payout id (UUID). Minted by the application layer.
pub type ScopePayoutId = Id<ScopePayoutTag>;
/// Phantom tag making [`ScopePayoutId`] a distinct, incompatible identity type.
pub struct ScopePayoutTag;

/// Everything a payout names.
#[derive(Clone, Debug)]
pub struct PayoutTerms {
	pub client: ClientId,
	pub scope: ScopeId,
	pub from: ScopeAccountLabel,
	pub lines: Vec<PayoutLine>,
	pub reason: PayoutReason,
	pub subject_ref: SubjectRef,
	pub memo: String,
	/// The approved operation this payout executes; `Some` exactly when the reason is
	/// [`PayoutReason::Approved`].
	pub operation_id: Option<uuid::Uuid>,
}

/// Money paid out of a scope account into users' compartments in the same scope. ONE linked
/// ledger chain, one leg per line, applied by the relay: every line posts or none does.
#[derive(Clone, Debug)]
pub struct ScopePayout {
	id: ScopePayoutId,
	terms: PayoutTerms,
	total: Usdt,
	pending: Vec<ScopePayoutEvent>,
}

impl ScopePayout {
	/// A new payout, raising the chain's event.
	pub fn request(id: ScopePayoutId, terms: PayoutTerms) -> Result<Self, DomainError> {
		let mut payout = Self::restore(id, terms)?;
		payout.pending.push(ScopePayoutEvent::PaidOut {
			payout_id: id,
			scope: payout.terms.scope.clone(),
			from_label: payout.terms.from.clone(),
			lines: payout
				.terms
				.lines
				.iter()
				.map(|line| PaidLine {
					user: line.user,
					amount: line.amount,
				})
				.collect(),
		});
		Ok(payout)
	}

	/// Rebuild from storage (no events). The domain's floor under the use case's checks: never
	/// out of one of banking's own labels, never in `main`, one line per user, an approved
	/// payout always names its operation and an automatic one never does.
	pub fn restore(id: ScopePayoutId, terms: PayoutTerms) -> Result<Self, DomainError> {
		if terms.scope.is_main() {
			return Err(DomainError::Validation("a payout never names main".into()));
		}
		if terms.from.is_reserved() {
			return Err(DomainError::Forbidden(format!("{} is banking's own account", terms.from)));
		}
		let total = check_lines(&requested_lines(&terms.lines))?;
		let mut users: Vec<UserId> = terms.lines.iter().map(|line| line.user).collect();
		users.sort_unstable_by_key(|user| user.raw());
		if users.windows(2).any(|pair| pair[0] == pair[1]) {
			return Err(DomainError::Validation("one user may appear once in a payout".into()));
		}
		match (terms.reason, terms.operation_id) {
			(PayoutReason::Approved, Some(_)) | (PayoutReason::Redemption | PayoutReason::Book | PayoutReason::FeeRebate, None) => {}
			(PayoutReason::Approved, None) => return Err(DomainError::Validation("an approved payout names its operation".into())),
			(PayoutReason::Redemption | PayoutReason::Book | PayoutReason::FeeRebate, Some(_)) => {
				return Err(DomainError::Validation("an automatic payout executes no operation".into()));
			}
		}
		validate_memo(&terms.memo)?;
		Ok(Self {
			id,
			terms,
			total,
			pending: Vec::new(),
		})
	}

	pub fn id(&self) -> ScopePayoutId {
		self.id
	}

	pub fn terms(&self) -> &PayoutTerms {
		&self.terms
	}

	pub fn total(&self) -> Usdt {
		self.total
	}
}

impl Entity for ScopePayout {
	type Id = ScopePayoutId;

	fn id(&self) -> ScopePayoutId {
		self.id
	}
}

impl AggregateRoot for ScopePayout {
	const NAME: &'static str = "scope_payout";
}

/// One leg of a payout's chain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PaidLine {
	pub user: UserId,
	pub amount: Usdt,
}

/// A payout's ledger step; internally tagged so the stored JSON describes itself.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScopePayoutEvent {
	/// Relay, linked: per line in order, post `Dr scope_account:<scope>:<from_label> / Cr
	/// scope_claim:<scope>:<user>` for its amount.
	PaidOut {
		payout_id: ScopePayoutId,
		scope: ScopeId,
		from_label: ScopeAccountLabel,
		lines: Vec<PaidLine>,
	},
}

impl DomainEvent for ScopePayoutEvent {
	const KIND: &'static str = "scope_payouts";
}

impl EmitsEvents for ScopePayout {
	type Event = ScopePayoutEvent;

	fn drain_events(&mut self) -> Vec<ScopePayoutEvent> {
		core::mem::take(&mut self.pending)
	}
}

/// What an automatic payout is checked against, every figure read under the scope's lock.
#[derive(Clone, Copy, Debug)]
pub struct AutoPayoutFacts {
	pub total: Usdt,
	/// The largest single line of the payout.
	pub max_line: Usdt,
	/// What the client paid out automatically in this scope over the last 24 h, `pending` and
	/// `posted`, this one excluded.
	pub paid_24h: Usdt,
	/// What left the scope's accounts without the owners over the last 24 h, by every client:
	/// automatic payouts and scope-level decisions (see [`ScopePolicy::scope_budget_used`]).
	pub scope_out_24h: Usdt,
}

impl ScopePolicy {
	/// Whether a client's `Payout` stays inside the automatic ceilings — per operation and per
	/// rolling 24 h for `(client, scope)` — and inside the scope's own daily budget (D24(4)),
	/// which every client of the scope draws on. All inclusive.
	pub fn admit_auto_payout(&self, facts: AutoPayoutFacts) -> Result<(), LedgerRefusal> {
		if facts.total > self.auto_payout_per_op || facts.max_line > MAX_PAYOUT_LINE {
			return Err(LedgerRefusal::ApprovalRequired);
		}
		// An overflowing sum is past any ceiling a u128 can express.
		let paid = facts.paid_24h.checked_add(facts.total).ok_or(LedgerRefusal::ApprovalRequired)?;
		if paid > self.auto_payout_24h {
			return Err(LedgerRefusal::ApprovalRequired);
		}
		if self.over_scope_budget(facts.scope_out_24h, facts.total) {
			return Err(LedgerRefusal::ApprovalRequired);
		}
		Ok(())
	}

	/// Whether `amount` on top of what already left the scope today breaks its daily budget —
	/// the flag a requested payout carries to its approval, which sends it to the owners.
	pub fn over_scope_budget(&self, scope_out_24h: Usdt, amount: Usdt) -> bool {
		scope_out_24h.checked_add(amount).is_none_or(|total| total > self.scope_budget_24h)
	}
}

/// What a `bank:*` approval is about (`ApprovalKindSpec.name`): banking is its only proposer
/// and executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationKind {
	/// A payout to users the client asked for — discretionary, a distribution, or past the
	/// automatic ceilings.
	ScopePayout,
	/// Between two accounts of one scope.
	AccountTransfer,
	/// Into another scope; the platform owners always decide it.
	ScopeTransfer,
}

impl OperationKind {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::ScopePayout => "bank:scope_payout",
			Self::AccountTransfer => "bank:account_transfer",
			Self::ScopeTransfer => "bank:scope_transfer",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"bank:scope_payout" => Ok(Self::ScopePayout),
			"bank:account_transfer" => Ok(Self::AccountTransfer),
			"bank:scope_transfer" => Ok(Self::ScopeTransfer),
			other => Err(DomainError::Validation(format!("unknown operation kind: {other}"))),
		}
	}
}

/// Where a requested operation stands (`PendingOperationState`). Every state but
/// [`Self::AwaitingApproval`] is terminal: an operation is executed at most once, and a
/// rejected or failed one is never carried out later — a retry is a new request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationState {
	AwaitingApproval,
	Executed,
	Rejected,
	Failed,
}

impl OperationState {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::AwaitingApproval => "awaiting_approval",
			Self::Executed => "executed",
			Self::Rejected => "rejected",
			Self::Failed => "failed",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"awaiting_approval" => Ok(Self::AwaitingApproval),
			"executed" => Ok(Self::Executed),
			"rejected" => Ok(Self::Rejected),
			"failed" => Ok(Self::Failed),
			other => Err(DomainError::Validation(format!("unknown operation state: {other}"))),
		}
	}
}

/// A unique operation id (UUID). Minted by the application layer.
pub type OperationId = Id<OperationTag>;
/// Phantom tag making [`OperationId`] a distinct, incompatible identity type.
pub struct OperationTag;

/// What an operation moves, beyond its source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationBody {
	Payout {
		lines: Vec<PayoutLine>,
		/// A distribution to every holder — always the owners' to decide.
		distribution: bool,
		/// It would break the scope's daily budget, decided when it was stored: the approval
		/// carries the flag, and the flag sends it to the owners (D24(4)).
		over_scope_budget: bool,
	},
	Transfer {
		to_scope: ScopeId,
		to: ScopeAccountLabel,
		amount: Usdt,
	},
}

/// Everything a requested operation names.
#[derive(Clone, Debug)]
pub struct OperationTerms {
	pub client: ClientId,
	pub scope: ScopeId,
	pub from: ScopeAccountLabel,
	pub body: OperationBody,
	/// The caller's own ref — never sent to the approval.
	pub subject_ref: SubjectRef,
	pub memo: String,
}

/// Where an operation stands, as stored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationProgress {
	pub state: OperationState,
	/// The concierge approval opened for it; `None` until it is.
	pub approval_id: Option<String>,
	/// The payout or transfer that executed it.
	pub result_id: Option<String>,
	/// Why it was rejected or failed.
	pub failure_reason: Option<String>,
}

impl OperationProgress {
	fn awaiting() -> Self {
		Self {
			state: OperationState::AwaitingApproval,
			approval_id: None,
			result_id: None,
			failure_reason: None,
		}
	}
}

/// A typed value on an approval (`ApprovalFieldValue`). A closed set, as concierge's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalValue {
	Text(String),
	Amount(Usdt),
	Flag(bool),
	/// An `auth_subject`, rendered by concierge as that account's address.
	User(String),
	Count(u64),
}

impl ApprovalValue {
	/// The `ApprovalFieldType` suffix the payload encoding names.
	pub const fn type_name(&self) -> &'static str {
		match self {
			Self::Text(_) => "TEXT",
			Self::Amount(_) => "AMOUNT",
			Self::Flag(_) => "FLAG",
			Self::User(_) => "USER",
			Self::Count(_) => "COUNT",
		}
	}

	/// The value as `payload_hash` encodes it: text verbatim, `<canonical amount> <currency>`,
	/// `true`/`false`, the `auth_subject`, a decimal count.
	pub fn encoded(&self) -> String {
		match self {
			Self::Text(text) | Self::User(text) => text.clone(),
			Self::Amount(amount) => format!("{} {APPROVAL_CURRENCY}", amount.to_decimal_string()),
			Self::Flag(flag) => flag.to_string(),
			Self::Count(count) => count.to_string(),
		}
	}
}

/// The type of an approval field (`ApprovalFieldType`), as the catalog declares it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldType {
	Text,
	Amount,
	Flag,
	User,
	Count,
}

impl ApprovalValue {
	pub const fn field_type(&self) -> FieldType {
		match self {
			Self::Text(_) => FieldType::Text,
			Self::Amount(_) => FieldType::Amount,
			Self::Flag(_) => FieldType::Flag,
			Self::User(_) => FieldType::User,
			Self::Count(_) => FieldType::Count,
		}
	}
}

/// Who decides a kind before escalation (`ApprovalLevel`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KindLevel {
	Platform,
	Scope,
}

/// How a field escalates a scope-level kind to the owners (`EscalationTest`) — the tests
/// banking's kinds use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EscalationTest {
	AmountAbove,
	Rolling24hAmountAbove,
	FlagSet,
}

/// One field of a kind (`ApprovalFieldSpec`).
#[derive(Clone, Copy, Debug)]
pub struct FieldSpec {
	pub key: &'static str,
	pub label: &'static str,
	pub field_type: FieldType,
	pub required: bool,
}

/// One escalation of a kind (`ApprovalEscalation`). Concierge's floor adds its own beside
/// these and caps them, so these may only be as strict or stricter.
#[derive(Clone, Copy, Debug)]
pub struct EscalationSpec {
	pub parameter: &'static str,
	pub field_key: &'static str,
	pub test: EscalationTest,
	/// A canonical USDT amount; empty for FLAG_SET.
	pub default_threshold: &'static str,
	/// `USDT` for the amount tests; empty for FLAG_SET.
	pub currency: &'static str,
}

/// One `bank:*` kind as banking publishes it (`ApprovalKindSpec`).
#[derive(Clone, Copy, Debug)]
pub struct KindSpec {
	/// `<namespace>:<name>`. A string and not [`OperationKind`]: `bank:payment` is decided over
	/// a payment order, not a pending operation, and shares only the catalog with them.
	pub kind: &'static str,
	pub label: &'static str,
	pub description: &'static str,
	pub level: KindLevel,
	pub fields: &'static [FieldSpec],
	pub escalations: &'static [EscalationSpec],
}

/// The version of [`BANK_CATALOG`]: unix seconds of the day it last changed. BUMP IT with
/// every change to the catalog — concierge refuses a catalog of the version it stores with
/// other content, and one more than a day ahead of its clock.
pub const BANK_CATALOG_VERSION: u64 = 1_792_368_000;

/// [`BANK_CATALOG`] rendered field by field, one line per item — what a test pins, beside
/// [`BANK_CATALOG_VERSION`], so the catalog cannot change without the version moving.
pub fn bank_catalog_canonical() -> String {
	let mut out = String::new();
	for spec in BANK_CATALOG {
		out.push_str(&format!("kind\t{}\t{}\t{}\t{:?}\n", spec.kind, spec.label, spec.description, spec.level));
		for field in spec.fields {
			out.push_str(&format!("field\t{}\t{}\t{:?}\t{}\n", field.key, field.label, field.field_type, field.required));
		}
		for escalation in spec.escalations {
			out.push_str(&format!(
				"escalation\t{}\t{}\t{:?}\t{}\t{}\n",
				escalation.parameter, escalation.field_key, escalation.test, escalation.default_threshold, escalation.currency
			));
		}
	}
	for topic in BANK_TOPICS {
		out.push_str(&format!("topic\t{}\t{}\t{}\t{}\n", topic.topic, topic.label, topic.description, topic.default_subscribed));
	}
	out
}

/// The notification topic of the money plane's notices to a person about their own money
/// (`bank:money-movement`): a payment out of their claim that awaits their confirmation, and
/// the end of one that died. Published in banking's catalog — concierge accepts an emit on a
/// namespace's topic only once its catalog in force holds it.
pub const MONEY_MOVEMENT_TOPIC: &str = "bank:money-movement";

/// The notification topic of security notices about the wallet (`bank:wallet-security`): a
/// withdrawal requested, a withdrawal to a new address, a mandate granted or revoked. Each
/// carries a "this wasn't me" path to the wallet page.
pub const WALLET_SECURITY_TOPIC: &str = "bank:wallet-security";

/// One notification topic as banking publishes it (`NotificationTopicSpec`).
#[derive(Clone, Copy, Debug)]
pub struct TopicSpec {
	pub topic: &'static str,
	/// Plain text, at most 60 bytes.
	pub label: &'static str,
	/// Plain text, at most 200 bytes.
	pub description: &'static str,
	/// Whether an account that never touched the topic receives it.
	pub default_subscribed: bool,
}

/// The topics banking publishes beside [`BANK_CATALOG`], in the same publication.
pub const BANK_TOPICS: &[TopicSpec] = &[
	TopicSpec {
		topic: MONEY_MOVEMENT_TOPIC,
		label: "Money movement",
		description: "Payments out of your account that wait for your confirmation, and what became of them.",
		// A payment awaiting the owner of the money is the one notice that must not depend on the
		// user having found a settings page; they can still switch it off, and the payment stays
		// answerable in the wallet host either way.
		default_subscribed: true,
	},
	TopicSpec {
		topic: WALLET_SECURITY_TOPIC,
		label: "Wallet security",
		description: "Withdrawals, new withdrawal addresses and mandates on your wallet, with a way to stop what you did not do.",
		// The only warning an account takeover gives the owner; on until they turn it off.
		default_subscribed: true,
	},
];

const fn field(key: &'static str, label: &'static str, field_type: FieldType, required: bool) -> FieldSpec {
	FieldSpec { key, label, field_type, required }
}

const TRANSFER_FIELDS: &[FieldSpec] = &[
	field("service", "Requesting service", FieldType::Text, true),
	field("from_account", "From account", FieldType::Text, true),
	field("to_account", "To account", FieldType::Text, true),
	field("amount", "Amount", FieldType::Amount, true),
];

/// The kind of a payment order decided by the platform owners (`bank:payment`, concierge's
/// `domain::floors`: always PLATFORM). Proposed in [`PAYMENT_APPROVAL_SCOPE`] over
/// `payment:<uuid>`.
pub const PAYMENT_KIND: &str = "bank:payment";

/// The scope a payment is proposed in: platform money belongs to no scope, and concierge
/// resolves a proposal in `main` to the owners.
pub const PAYMENT_APPROVAL_SCOPE: &str = "main";

/// The fields of `bank:payment`: concierge's table names `tier`, `source`, `destination` and
/// `amount`; `reason` rides beside them because it is the sentence the approvers read to
/// learn why the money moves.
const PAYMENT_FIELDS: &[FieldSpec] = &[
	field("tier", "Tier", FieldType::Text, true),
	field("source", "Source", FieldType::Text, true),
	field("destination", "Destination", FieldType::Text, true),
	field("amount", "Amount", FieldType::Amount, true),
	field("reason", "Reason", FieldType::Text, true),
];

/// The kinds banking proposes today — what [`PendingOperation::approval_fields`] and
/// `PaymentTerms::approval_fields` send, field for field. `bank:treasury_surplus` and
/// `bank:compartment_policy` join when banking has a path that opens them — the treasury's
/// spending is closed and a scope's policy has no RPC yet. A catalog is published whole, so
/// a kind left out here is one concierge stops accepting.
pub const BANK_CATALOG: &[KindSpec] = &[
	KindSpec {
		kind: OperationKind::ScopePayout.as_str(),
		label: "Payout from a scope account",
		description: "A payout from a scope account to users' compartments in the scope, requested by the scope's service.",
		level: KindLevel::Scope,
		fields: &[
			field("service", "Requesting service", FieldType::Text, true),
			field("from_account", "From account", FieldType::Text, true),
			field("total", "Total", FieldType::Amount, true),
			field("recipients", "Recipients", FieldType::Count, true),
			field("max_line", "Largest line", FieldType::Amount, true),
			field("recipient", "Recipient", FieldType::User, false),
			field("lines_digest", "Lines digest", FieldType::Text, true),
			field("distribution", "Distribution to every holder", FieldType::Flag, true),
			field("over_scope_budget", "Over the scope's daily budget", FieldType::Flag, true),
		],
		escalations: &[
			EscalationSpec {
				parameter: "per_operation",
				field_key: "total",
				test: EscalationTest::AmountAbove,
				default_threshold: "1000",
				currency: APPROVAL_CURRENCY,
			},
			// banking#525: one recipient's share, whatever the total. `max_line` is already on
			// every approval, so concierge evaluates this without a new field.
			EscalationSpec {
				parameter: "per_line",
				field_key: "max_line",
				test: EscalationTest::AmountAbove,
				default_threshold: "1000",
				currency: APPROVAL_CURRENCY,
			},
			EscalationSpec {
				parameter: "daily",
				field_key: "total",
				test: EscalationTest::Rolling24hAmountAbove,
				default_threshold: "5000",
				currency: APPROVAL_CURRENCY,
			},
			EscalationSpec {
				parameter: "distribution",
				field_key: "distribution",
				test: EscalationTest::FlagSet,
				default_threshold: "",
				currency: "",
			},
			// D24(4): banking counts the scope's one daily budget itself and sets the flag.
			EscalationSpec {
				parameter: "over_scope_budget",
				field_key: "over_scope_budget",
				test: EscalationTest::FlagSet,
				default_threshold: "",
				currency: "",
			},
		],
	},
	KindSpec {
		kind: OperationKind::AccountTransfer.as_str(),
		label: "Transfer between accounts of a scope",
		description: "A transfer between two accounts of one scope, requested by the service that owns both.",
		level: KindLevel::Scope,
		fields: TRANSFER_FIELDS,
		escalations: &[
			EscalationSpec {
				parameter: "per_operation",
				field_key: "amount",
				test: EscalationTest::AmountAbove,
				default_threshold: "1000",
				currency: APPROVAL_CURRENCY,
			},
			EscalationSpec {
				parameter: "daily",
				field_key: "amount",
				test: EscalationTest::Rolling24hAmountAbove,
				default_threshold: "5000",
				currency: APPROVAL_CURRENCY,
			},
		],
	},
	KindSpec {
		kind: PAYMENT_KIND,
		label: "Payment from fund money",
		description: "A payment out of an allocation's pooled claim or banking's treasury, to an internal claim or an address; the platform owners decide it.",
		level: KindLevel::Platform,
		fields: PAYMENT_FIELDS,
		escalations: &[],
	},
	KindSpec {
		kind: OperationKind::ScopeTransfer.as_str(),
		label: "Transfer between scopes",
		description: "A transfer from an account of one scope into an account of another, requested by the service that owns both.",
		level: KindLevel::Platform,
		fields: TRANSFER_FIELDS,
		escalations: &[],
	},
];

/// One field of an approval, by its key in the kind's spec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalField {
	pub key: &'static str,
	pub value: ApprovalValue,
}

/// The bytes concierge's `payload_hash` is the SHA-256 of (its doc, "payload_hash"):
/// `ev-approval-v1\n<kind>\n<scope>\n<subject_ref>\n`, then one `<key>\t<TYPE>\t<value>\n` per
/// field, sorted by key. FROZEN by the other plane: a mismatch here refuses every execution.
pub fn approval_payload(kind: &str, scope: &str, subject_ref: &str, fields: &[ApprovalField]) -> String {
	let mut sorted: Vec<&ApprovalField> = fields.iter().collect();
	sorted.sort_unstable_by_key(|field| field.key);
	let mut out = format!("ev-approval-v1\n{kind}\n{scope}\n{subject_ref}\n");
	for field in sorted {
		out.push_str(field.key);
		out.push('\t');
		out.push_str(field.value.type_name());
		out.push('\t');
		out.push_str(&field.value.encoded());
		out.push('\n');
	}
	out
}

/// An operation a client asked for, stored whole until its approval decides it.
#[derive(Clone, Debug)]
pub struct PendingOperation {
	id: OperationId,
	terms: OperationTerms,
	progress: OperationProgress,
	total: Usdt,
}

impl PendingOperation {
	/// A new request, awaiting its approval.
	pub fn request(id: OperationId, terms: OperationTerms) -> Result<Self, DomainError> {
		Self::restore(id, terms, OperationProgress::awaiting())
	}

	/// Rebuild from storage. The domain's floor under the use case's checks: never out of one
	/// of banking's labels or in `main`, never into a `fee` or `treasury` account, never from
	/// an account into itself.
	pub fn restore(id: OperationId, terms: OperationTerms, progress: OperationProgress) -> Result<Self, DomainError> {
		if terms.scope.is_main() {
			return Err(DomainError::Validation("an operation never names main as its scope".into()));
		}
		if terms.from.is_reserved() {
			return Err(DomainError::Forbidden(format!("{} is banking's own account", terms.from)));
		}
		validate_memo(&terms.memo)?;
		let total = match &terms.body {
			OperationBody::Payout { lines, .. } => {
				let total = check_lines(&requested_lines(lines))?;
				let mut users: Vec<UserId> = lines.iter().map(|line| line.user).collect();
				users.sort_unstable_by_key(|user| user.raw());
				if users.windows(2).any(|pair| pair[0] == pair[1]) {
					return Err(DomainError::Validation("one user may appear once in a payout".into()));
				}
				total
			}
			OperationBody::Transfer { to_scope, to, amount } => {
				if amount.is_zero() {
					return Err(DomainError::Validation("amount must be positive".into()));
				}
				if to_scope.is_main() || to.is_reserved() {
					return Err(DomainError::Forbidden(format!("{to_scope}:{to} is banking's own account")));
				}
				if to_scope == &terms.scope && to == &terms.from {
					return Err(DomainError::Validation("a transfer needs two different accounts".into()));
				}
				*amount
			}
		};
		Ok(Self { id, terms, progress, total })
	}

	pub fn id(&self) -> OperationId {
		self.id
	}

	pub fn terms(&self) -> &OperationTerms {
		&self.terms
	}

	pub fn progress(&self) -> &OperationProgress {
		&self.progress
	}

	pub fn state(&self) -> OperationState {
		self.progress.state
	}

	/// The transfer amount, or the sum of the payout lines.
	pub fn total(&self) -> Usdt {
		self.total
	}

	pub fn kind(&self) -> OperationKind {
		match &self.terms.body {
			OperationBody::Payout { .. } => OperationKind::ScopePayout,
			OperationBody::Transfer { to_scope, .. } if to_scope == &self.terms.scope => OperationKind::AccountTransfer,
			OperationBody::Transfer { .. } => OperationKind::ScopeTransfer,
		}
	}

	/// banking's own ref for the approval — never the caller's `subject_ref`.
	pub fn approval_subject_ref(&self) -> String {
		format!("pending_operation:{}", self.id)
	}

	/// What `lines_digest` hashes, for a payout.
	pub fn lines_digest_input(&self) -> Option<String> {
		match &self.terms.body {
			OperationBody::Payout { lines, .. } => Some(lines_digest_input(&requested_lines(lines))),
			OperationBody::Transfer { .. } => None,
		}
	}

	/// The approval's fields, rendered from the stored record alone; `lines_digest` is the
	/// SHA-256 of [`Self::lines_digest_input`], taken by the application (required for a
	/// payout, ignored for a transfer).
	pub fn approval_fields(&self, lines_digest: Option<&str>) -> Result<Vec<ApprovalField>, DomainError> {
		let account = |scope: &ScopeId, label: &ScopeAccountLabel| ApprovalValue::Text(format!("{scope}:{label}"));
		let from = account(&self.terms.scope, &self.terms.from);
		// Who asks is part of what is approved: a decision for one client's request is never
		// one another client's request can match.
		let service = ApprovalField {
			key: "service",
			value: ApprovalValue::Text(self.terms.client.as_str().to_owned()),
		};
		Ok(match &self.terms.body {
			OperationBody::Payout {
				lines,
				distribution,
				over_scope_budget,
			} => {
				let digest = lines_digest.ok_or_else(|| DomainError::Validation("a payout's approval carries its lines_digest".into()))?;
				let requested = requested_lines(lines);
				let mut fields = vec![
					service,
					ApprovalField { key: "from_account", value: from },
					ApprovalField {
						key: "total",
						value: ApprovalValue::Amount(self.total),
					},
					ApprovalField {
						key: "recipients",
						value: ApprovalValue::Count(lines.len() as u64),
					},
					ApprovalField {
						key: "max_line",
						value: ApprovalValue::Amount(max_line(&requested)),
					},
					ApprovalField {
						key: "lines_digest",
						value: ApprovalValue::Text(digest.to_owned()),
					},
					ApprovalField {
						key: "distribution",
						value: ApprovalValue::Flag(*distribution),
					},
					ApprovalField {
						key: "over_scope_budget",
						value: ApprovalValue::Flag(*over_scope_budget),
					},
				];
				// A single recipient is named, so the voters see who; a list is behind its
				// digest and the review page (concierge's spec: "only when there is one line").
				if let [only] = lines.as_slice() {
					fields.push(ApprovalField {
						key: "recipient",
						value: ApprovalValue::User(only.auth_subject.clone()),
					});
				}
				fields
			}
			OperationBody::Transfer { to_scope, to, amount } => vec![
				service,
				ApprovalField { key: "from_account", value: from },
				ApprovalField {
					key: "to_account",
					value: account(to_scope, to),
				},
				ApprovalField {
					key: "amount",
					value: ApprovalValue::Amount(*amount),
				},
			],
		})
	}

	/// The bytes of this operation's `payload_hash` (see [`approval_payload`]).
	pub fn approval_payload(&self, lines_digest: Option<&str>) -> Result<String, DomainError> {
		let fields = self.approval_fields(lines_digest)?;
		Ok(approval_payload(self.kind().as_str(), self.terms.scope.as_str(), &self.approval_subject_ref(), &fields))
	}

	/// Approved and carried out by the payout or transfer `result_id`. Only once.
	pub fn execute(&mut self, result_id: String) -> Result<(), DomainError> {
		self.leave_awaiting(OperationState::Executed)?;
		self.progress.result_id = Some(result_id);
		Ok(())
	}

	/// The approvers said no, or the approval lapsed, or it could not be opened. Only while
	/// awaiting.
	pub fn reject(&mut self, reason: String) -> Result<(), DomainError> {
		self.leave_awaiting(OperationState::Rejected)?;
		self.progress.failure_reason = Some(reason);
		Ok(())
	}

	/// Approved, but could not be carried out. Only while awaiting; terminal.
	pub fn fail(&mut self, reason: String) -> Result<(), DomainError> {
		self.leave_awaiting(OperationState::Failed)?;
		self.progress.failure_reason = Some(reason);
		Ok(())
	}

	fn leave_awaiting(&mut self, to: OperationState) -> Result<(), DomainError> {
		match self.progress.state {
			OperationState::AwaitingApproval => {
				self.progress.state = to;
				Ok(())
			}
			OperationState::Executed | OperationState::Rejected | OperationState::Failed => Err(DomainError::Conflict(format!(
				"operation {} is {}, not awaiting its approval",
				self.id,
				self.progress.state.as_str()
			))),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn usdt(raw: &str) -> Usdt {
		Usdt::parse_decimal(raw).unwrap()
	}

	fn scope(raw: &str) -> ScopeId {
		ScopeId::parse(raw).unwrap()
	}

	fn label(raw: &str) -> ScopeAccountLabel {
		ScopeAccountLabel::parse(raw).unwrap()
	}

	fn line(subject: &str, amount: &str) -> RequestedLine {
		RequestedLine {
			auth_subject: subject.into(),
			amount: usdt(amount),
		}
	}

	fn paid(subject: &str, amount: &str) -> PayoutLine {
		PayoutLine {
			user: UserId::new(),
			auth_subject: subject.into(),
			amount: usdt(amount),
		}
	}

	fn sref() -> SubjectRef {
		SubjectRef::parse("dividend:4f0c2d1e-0000-4000-8000-0123456789ab").unwrap()
	}

	fn policy() -> ScopePolicy {
		ScopePolicy {
			min_kyc_level: 0,
			max_per_move: None,
			max_moved_in_24h: None,
			max_balance: None,
			auto_payout_per_op: usdt("1000"),
			auto_payout_24h: usdt("5000"),
			auto_transfer_per_op: usdt("1000"),
			auto_transfer_24h: usdt("5000"),
			scope_budget_24h: usdt("8000"),
		}
	}

	#[test]
	fn lines_are_one_per_user_positive_and_bounded() {
		assert_eq!(check_lines(&[line("a", "1"), line("b", "2.5")]).unwrap(), usdt("3.5"));
		assert!(check_lines(&[]).is_err());
		assert!(check_lines(&[line("a", "1"), line("a", "2")]).is_err(), "one user twice");
		assert!(check_lines(&[line("a", "0")]).is_err());
		assert!(check_lines(&[line("", "1")]).is_err());
		assert!(check_lines(&[line("a\n", "1")]).is_err());
		let many: Vec<RequestedLine> = (0..=MAX_PAYOUT_LINES).map(|i| line(&format!("u{i}"), "1")).collect();
		assert!(check_lines(&many).is_err());
		assert!(check_lines(&many[..MAX_PAYOUT_LINES]).is_ok());
		let huge = Usdt::from_base_units(u128::MAX);
		assert!(
			check_lines(&[
				RequestedLine {
					auth_subject: "a".into(),
					amount: huge
				},
				line("b", "1")
			])
			.is_err(),
			"an overflowing total"
		);
	}

	#[test]
	fn the_canonical_lines_ignore_order_and_the_digest_input_is_concierges() {
		let ab = [line("b", "2"), line("a", "1.50")];
		let ba = [line("a", "1.5"), line("b", "2")];
		assert_eq!(canonical_lines(&ab), canonical_lines(&ba));
		assert_ne!(canonical_lines(&ab), canonical_lines(&[line("a", "1.5"), line("b", "2.000000000000000001")]));
		assert_eq!(lines_digest_input(&ab), "a|1.5\nb|2", "canonical amounts, sorted, no trailing newline");
		assert_eq!(max_line(&ab), usdt("2"));
	}

	#[test]
	fn an_automatic_payout_stays_inside_the_client_ceilings_and_the_scope_budget() {
		let facts = |total: &str, paid: &str, out: &str| AutoPayoutFacts {
			total: usdt(total),
			max_line: usdt(total),
			paid_24h: usdt(paid),
			scope_out_24h: usdt(out),
		};
		let p = policy();
		assert_eq!(p.admit_auto_payout(facts("1000", "4000", "7000")), Ok(()), "every ceiling is inclusive");
		assert_eq!(p.admit_auto_payout(facts("1000.000000000000000001", "0", "0")), Err(LedgerRefusal::ApprovalRequired));
		assert_eq!(p.admit_auto_payout(facts("1", "5000", "0")), Err(LedgerRefusal::ApprovalRequired));
		assert_eq!(
			p.admit_auto_payout(facts("1", "0", "8000")),
			Err(LedgerRefusal::ApprovalRequired),
			"the scope's budget binds every client"
		);
		// One line past the ceiling needs the owners although the total is inside every other one.
		let wide = AutoPayoutFacts {
			total: usdt("1000"),
			max_line: usdt("1000.000000000000000001"),
			paid_24h: usdt("0"),
			scope_out_24h: usdt("0"),
		};
		assert_eq!(p.admit_auto_payout(wide), Err(LedgerRefusal::ApprovalRequired));
		assert!(p.over_scope_budget(Usdt::from_base_units(u128::MAX), usdt("1")), "an overflowing sum is over budget");
		assert!(!p.over_scope_budget(usdt("7999"), usdt("1")));
	}

	fn payout_terms(reason: PayoutReason, operation_id: Option<uuid::Uuid>) -> PayoutTerms {
		PayoutTerms {
			client: ClientId::parse("invest").unwrap(),
			scope: scope("invest"),
			from: label("alloc:x"),
			lines: vec![paid("a", "1"), paid("b", "2")],
			reason,
			subject_ref: sref(),
			memo: String::new(),
			operation_id,
		}
	}

	#[test]
	fn a_payout_raises_one_chain_and_refuses_banking_labels_and_mixed_reasons() {
		let mut payout = ScopePayout::request(ScopePayoutId::new(), payout_terms(PayoutReason::Redemption, None)).unwrap();
		assert_eq!(payout.total(), usdt("3"));
		let events = payout.drain_events();
		let [ScopePayoutEvent::PaidOut { lines, .. }] = events.as_slice() else {
			panic!("{events:?}")
		};
		assert_eq!(lines.len(), 2);
		let json = serde_json::to_value(&events[0]).unwrap();
		assert_eq!(json["type"], "paid_out");
		assert!(
			ScopePayout::request(
				ScopePayoutId::new(),
				PayoutTerms {
					from: label("fee"),
					..payout_terms(PayoutReason::Book, None)
				}
			)
			.is_err()
		);
		assert!(ScopePayout::request(ScopePayoutId::new(), payout_terms(PayoutReason::Approved, None)).is_err());
		assert!(ScopePayout::request(ScopePayoutId::new(), payout_terms(PayoutReason::Book, Some(uuid::Uuid::new_v4()))).is_err());
		assert!(ScopePayout::request(ScopePayoutId::new(), payout_terms(PayoutReason::Approved, Some(uuid::Uuid::new_v4()))).is_ok());
		let user = UserId::new();
		let twice = PayoutTerms {
			lines: vec![
				PayoutLine {
					user,
					auth_subject: "a".into(),
					amount: usdt("1"),
				},
				PayoutLine {
					user,
					auth_subject: "b".into(),
					amount: usdt("1"),
				},
			],
			..payout_terms(PayoutReason::Book, None)
		};
		assert!(ScopePayout::request(ScopePayoutId::new(), twice).is_err(), "two subjects resolving to one user");
	}

	fn transfer_op(to_scope: &str, to: &str) -> Result<PendingOperation, DomainError> {
		PendingOperation::request(
			OperationId::new(),
			OperationTerms {
				client: ClientId::parse("invest").unwrap(),
				scope: scope("invest"),
				from: label("alloc:x"),
				body: OperationBody::Transfer {
					to_scope: scope(to_scope),
					to: label(to),
					amount: usdt("1200"),
				},
				subject_ref: sref(),
				memo: "rebalance".into(),
			},
		)
	}

	#[test]
	fn an_operation_names_its_kind_by_where_the_money_goes() {
		assert_eq!(transfer_op("invest", "alloc:y").unwrap().kind(), OperationKind::AccountTransfer);
		assert_eq!(transfer_op("sa", "alloc:y").unwrap().kind(), OperationKind::ScopeTransfer);
		assert!(transfer_op("invest", "alloc:x").is_err(), "into itself");
		assert!(transfer_op("invest", "fee").is_err(), "a fee collector is never a destination");
		assert!(transfer_op("main", "treasury").is_err());
		for kind in [OperationKind::ScopePayout, OperationKind::AccountTransfer, OperationKind::ScopeTransfer] {
			assert_eq!(OperationKind::parse(kind.as_str()).unwrap(), kind);
		}
		for state in [OperationState::AwaitingApproval, OperationState::Executed, OperationState::Rejected, OperationState::Failed] {
			assert_eq!(OperationState::parse(state.as_str()).unwrap(), state);
		}
	}

	#[test]
	fn the_approval_payload_is_concierges_encoding_over_the_stored_record() {
		let op = transfer_op("sa", "alloc:y").unwrap();
		let payload = op.approval_payload(None).unwrap();
		let subject = format!("pending_operation:{}", op.id());
		assert_eq!(
			payload,
			format!(
				"ev-approval-v1\nbank:scope_transfer\ninvest\n{subject}\namount\tAMOUNT\t1200 USDT\nfrom_account\tTEXT\tinvest:alloc:x\nservice\tTEXT\tinvest\nto_account\tTEXT\tsa:alloc:y\n"
			)
		);
		assert!(!payload.contains("dividend:"), "the caller's own ref never reaches the approval");
	}

	fn payout_op(lines: Vec<PayoutLine>) -> PendingOperation {
		PendingOperation::request(
			OperationId::new(),
			OperationTerms {
				client: ClientId::parse("invest").unwrap(),
				scope: scope("invest"),
				from: label("alloc:x"),
				body: OperationBody::Payout {
					lines,
					distribution: false,
					over_scope_budget: true,
				},
				subject_ref: sref(),
				memo: String::new(),
			},
		)
		.unwrap()
	}

	#[test]
	fn a_payouts_approval_names_one_recipient_and_carries_its_digest_and_budget_flag() {
		let one = payout_op(vec![paid("sub-1", "10")]);
		assert!(one.approval_fields(None).is_err(), "a payout's approval needs its digest");
		let fields = one.approval_fields(Some("d")).unwrap();
		let value = |key: &str| fields.iter().find(|field| field.key == key).map(|field| field.value.clone());
		assert_eq!(value("recipient"), Some(ApprovalValue::User("sub-1".into())));
		assert_eq!(value("over_scope_budget"), Some(ApprovalValue::Flag(true)));
		assert_eq!(value("lines_digest"), Some(ApprovalValue::Text("d".into())));
		let two = payout_op(vec![paid("a", "1"), paid("b", "9")]);
		let fields = two.approval_fields(Some("d")).unwrap();
		assert!(fields.iter().all(|field| field.key != "recipient"), "a list is behind its digest");
		assert!(fields.contains(&ApprovalField {
			key: "max_line",
			value: ApprovalValue::Amount(usdt("9"))
		}));
		assert_eq!(two.lines_digest_input().unwrap(), "a|1\nb|9");
		let payload = two.approval_payload(Some("d")).unwrap();
		let keys: Vec<&str> = payload.lines().skip(4).map(|line| line.split('\t').next().unwrap()).collect();
		let mut sorted = keys.clone();
		sorted.sort_unstable();
		assert_eq!(keys, sorted, "fields are encoded sorted by key");
	}

	/// Every field an operation's approval carries is declared by its kind with that type, and
	/// every required field is always carried — else concierge refuses the proposal.
	#[test]
	fn the_catalog_declares_exactly_what_the_approvals_carry() {
		let spec = |kind: OperationKind| BANK_CATALOG.iter().find(|spec| spec.kind == kind.as_str()).expect("kind published");
		let ops = [
			payout_op(vec![paid("a", "1")]),
			payout_op(vec![paid("a", "1"), paid("b", "2")]),
			transfer_op("invest", "alloc:y").unwrap(),
			transfer_op("sa", "alloc:y").unwrap(),
		];
		for op in ops {
			let spec = spec(op.kind());
			let fields = op.approval_fields(Some("d")).unwrap();
			for sent in &fields {
				let declared = spec
					.fields
					.iter()
					.find(|field| field.key == sent.key)
					.unwrap_or_else(|| panic!("{} undeclared in {:?}", sent.key, spec.kind));
				assert_eq!(declared.field_type, sent.value.field_type(), "{}", sent.key);
			}
			for required in spec.fields.iter().filter(|field| field.required) {
				assert!(fields.iter().any(|sent| sent.key == required.key), "{} not sent for {:?}", required.key, spec.kind);
			}
			for escalation in spec.escalations {
				let tested = spec.fields.iter().find(|field| field.key == escalation.field_key).unwrap();
				assert!(tested.required, "an escalation over an optional field can be switched off");
			}
		}
		assert!(BANK_CATALOG.iter().filter(|spec| spec.level == KindLevel::Platform).all(|spec| spec.escalations.is_empty()));
	}

	#[test]
	fn an_operation_leaves_awaiting_once_and_never_comes_back() {
		let mut op = transfer_op("invest", "alloc:y").unwrap();
		op.execute("t-1".into()).unwrap();
		assert_eq!(op.state(), OperationState::Executed);
		assert!(op.execute("t-2".into()).is_err(), "one approval, one execution");
		assert!(op.reject("late".into()).is_err());
		let mut failed = transfer_op("invest", "alloc:y").unwrap();
		failed.fail("insufficient_funds".into()).unwrap();
		assert!(failed.execute("t".into()).is_err(), "a failed operation is never carried out later");
		assert_eq!(failed.progress().failure_reason.as_deref(), Some("insufficient_funds"));
	}
}
