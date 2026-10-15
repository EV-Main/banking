//! `scope_ledger` bounded context — what a scope's own service may do with money in its scope
//! (`scope/v1/ledger.proto`, `ScopeLedgerService`).
//!
//! The caller is a **client** — another backend (invest, the SA panel), identified by its mTLS
//! identity — and what it may touch is decided here, from banking's own configuration and
//! never from anything the client says: a [`Grant`] `(client, scope, label_prefix)` lets it
//! open accounts under that prefix in that scope, and an account debits only for the client
//! that opened it. banking's own labels ([`ScopeAccountLabel::is_reserved`]) are out of every
//! client's reach, whatever a grant says.
//!
//! Pure and wasm-safe: ids are minted by the application layer, no clock, no I/O.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{
	error::DomainError,
	money::{Usdt, contains_url},
	scopes::{ScopeAccountLabel, ScopeId, ScopePolicy},
	users::UserId,
};

/// banking's own client id: the owner of `(main, treasury)` and of every `(<scope>, fee)`.
const BANKING: &str = "banking";

/// A service client's id, `[a-z][a-z0-9_-]{0,63}` — what `scope_accounts.owner_client` holds.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClientId(String);

impl ClientId {
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		let bytes = raw.as_bytes();
		let shaped =
			!bytes.is_empty() && bytes.len() <= 64 && bytes[0].is_ascii_lowercase() && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'));
		if !shaped {
			return Err(DomainError::Validation(format!("client id must match [a-z][a-z0-9_-]{{0,63}}: {raw:?}")));
		}
		Ok(Self(raw.to_owned()))
	}

	/// banking itself, the owner of its reserved accounts. Never a caller.
	pub fn banking() -> Self {
		Self(BANKING.to_owned())
	}

	pub fn is_banking(&self) -> bool {
		self.0 == BANKING
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl TryFrom<String> for ClientId {
	type Error = DomainError;

	fn try_from(raw: String) -> Result<Self, DomainError> {
		Self::parse(&raw)
	}
}

impl From<ClientId> for String {
	fn from(id: ClientId) -> Self {
		id.0
	}
}

impl core::fmt::Display for ClientId {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(&self.0)
	}
}

/// One client's standing in one scope: the labels it may open there, and whether it may touch
/// users' compartments (Reserve, Charge, Capture into a user, Payout — not served yet).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
	pub client: ClientId,
	pub scope: ScopeId,
	pub label_prefix: String,
	pub compartments: bool,
}

impl Grant {
	fn covers(&self, label: &ScopeAccountLabel) -> bool {
		label.as_str().starts_with(&self.label_prefix)
	}
}

/// Every grant banking's configuration holds, checked once when it is loaded so that what
/// the RPCs consult is already safe to consult.
#[derive(Clone, Debug, Default)]
pub struct Grants {
	grants: Vec<Grant>,
}

impl Grants {
	/// Refuses, by the first one found: a grant to `banking` (it would make a client the owner
	/// of banking's own accounts), a grant in `main` (banking's alone), an empty or malformed
	/// prefix (an empty one covers every label, `fee` included), and two prefixes in one scope
	/// where one starts with the other — either client could then open a label the other's
	/// prefix also covers, and which of them owns it would depend on who asked first.
	pub fn new(grants: Vec<Grant>) -> Result<Self, DomainError> {
		for (i, grant) in grants.iter().enumerate() {
			if grant.client.is_banking() {
				return Err(DomainError::Validation(format!("grant #{i}: client_id \"banking\" is banking's own and cannot be granted")));
			}
			if grant.scope.is_main() {
				return Err(DomainError::Validation(format!(
					"grant #{i} ({}): scope main is banking's own and cannot be granted",
					grant.client
				)));
			}
			// The prefix of a label is itself a label when it is not empty, so the label rule
			// is the prefix rule.
			if grant.label_prefix.is_empty() || ScopeAccountLabel::parse(&grant.label_prefix).is_err() {
				return Err(DomainError::Validation(format!(
					"grant #{i} ({} in {}): label_prefix must match [a-z0-9_:.-]{{1,64}}, got {:?}",
					grant.client, grant.scope, grant.label_prefix
				)));
			}
			for (j, other) in grants.iter().enumerate().take(i) {
				if other.scope == grant.scope && (other.label_prefix.starts_with(&grant.label_prefix) || grant.label_prefix.starts_with(&other.label_prefix)) {
					return Err(DomainError::Validation(format!(
						"grants #{j} ({} {:?}) and #{i} ({} {:?}) overlap in scope {}: a label one prefix covers must not be coverable by another",
						other.client, other.label_prefix, grant.client, grant.label_prefix, grant.scope
					)));
				}
			}
		}
		Ok(Self { grants })
	}

	/// What `client` holds in `scope`; empty when it holds nothing there.
	pub fn in_scope<'a>(&'a self, client: &'a ClientId, scope: &'a ScopeId) -> impl Iterator<Item = &'a Grant> {
		self.grants.iter().filter(move |grant| &grant.client == client && &grant.scope == scope)
	}

	/// Whether `client` may open `label` in `scope`: one of its grants there covers it, and it
	/// is not one of banking's own labels.
	pub fn may_open(&self, client: &ClientId, scope: &ScopeId, label: &ScopeAccountLabel) -> bool {
		!label.is_reserved() && self.in_scope(client, scope).any(|grant| grant.covers(label))
	}

	pub fn is_empty(&self) -> bool {
		self.grants.is_empty()
	}

	/// Every scope `client` holds a grant in, each once, in configuration order.
	pub fn scopes_of(&self, client: &ClientId) -> Vec<ScopeId> {
		let mut scopes: Vec<ScopeId> = Vec::new();
		for grant in self.grants.iter().filter(|grant| &grant.client == client) {
			if !scopes.contains(&grant.scope) {
				scopes.push(grant.scope.clone());
			}
		}
		scopes
	}

	/// Whether one of `client`'s grants in `scope` lets it touch users' compartments there.
	pub fn compartments(&self, client: &ClientId, scope: &ScopeId) -> bool {
		self.in_scope(client, scope).any(|grant| grant.compartments)
	}
}

/// What a user's mandate lets a client do with their compartment
/// (`banking.v1.MandateOperation`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MandateOperation {
	Reserve,
	Charge,
	BookFill,
}

/// Which client a TLS client certificate is: the SHA-256 of the certificate's DER bytes,
/// lowercase hex, mapped statically to a [`ClientId`] by banking's configuration — the only
/// way a caller of the scope ledger gets an identity (`ledger.proto`, GRANTS). A client may
/// carry several fingerprints, so a certificate can be rotated make-before-break.
#[derive(Clone, Debug, Default)]
pub struct ClientDirectory {
	by_fingerprint: Vec<(String, ClientId)>,
}

impl ClientDirectory {
	/// Refuses a fingerprint that is not 64 lowercase hex digits, one listed twice (two
	/// clients behind one certificate would be one identity), a client with none, and
	/// `banking` (it would be a caller that owns banking's own accounts).
	pub fn new(clients: Vec<(ClientId, Vec<String>)>) -> Result<Self, DomainError> {
		let mut by_fingerprint: Vec<(String, ClientId)> = Vec::new();
		for (client, fingerprints) in clients {
			if client.is_banking() {
				return Err(DomainError::Validation("client_id \"banking\" is banking's own and cannot be a caller".into()));
			}
			if fingerprints.is_empty() {
				return Err(DomainError::Validation(format!("client {client} has no certificate fingerprint")));
			}
			for fingerprint in fingerprints {
				if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
					return Err(DomainError::Validation(format!(
						"client {client}: cert_sha256 must be 64 lowercase hex digits, got {fingerprint:?}"
					)));
				}
				if let Some((_, other)) = by_fingerprint.iter().find(|(known, _)| *known == fingerprint) {
					return Err(DomainError::Validation(format!("cert_sha256 {fingerprint} is listed for both {other} and {client}")));
				}
				by_fingerprint.push((fingerprint, client.clone()));
			}
		}
		Ok(Self { by_fingerprint })
	}

	/// The client whose certificate has this fingerprint.
	pub fn client_for(&self, fingerprint: &str) -> Option<&ClientId> {
		self.by_fingerprint.iter().find(|(known, _)| known == fingerprint).map(|(_, client)| client)
	}

	pub fn knows(&self, client: &ClientId) -> bool {
		self.by_fingerprint.iter().any(|(_, known)| known == client)
	}
}

/// Why the scope ledger refused a call — each one the whole wire status message, which
/// clients branch on with the code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerRefusal {
	/// The caller holds no grant for this — the scope, or the label in it (PERMISSION_DENIED).
	NotGranted,
	/// A debit of an account the caller does not own, or of one of banking's own
	/// (PERMISSION_DENIED). An account that does not exist is the same answer: a debit names
	/// an account the caller says is its own, and whether someone else's exists is not its
	/// business.
	NotAccountOwner,
	/// The scope is frozen: no new debit (FAILED_PRECONDITION).
	ScopeFrozen,
	/// The source cannot cover it right now (FAILED_PRECONDITION).
	InsufficientFunds,
	/// Past an automatic ceiling, or not an automatic move at all: ask with `RequestTransfer`
	/// (FAILED_PRECONDITION).
	ApprovalRequired,
	/// The hold is captured, voided or expired — or past its expiry and about to be
	/// (FAILED_PRECONDITION).
	HoldNotActive,
	/// The user is frozen: no new debit of their compartment, and no capture of a hold on
	/// it into another user's (FAILED_PRECONDITION).
	UserFrozen,
	/// No live mandate of the user covers this for the caller (FAILED_PRECONDITION).
	MandateRequired,
	/// The invoice is paid or expired, or past its expiry (FAILED_PRECONDITION).
	InvoiceNotOpen,
}

impl LedgerRefusal {
	pub const fn reason(self) -> &'static str {
		match self {
			Self::NotGranted => "not_granted",
			Self::NotAccountOwner => "not_account_owner",
			Self::ScopeFrozen => "scope_frozen",
			Self::InsufficientFunds => "insufficient_funds",
			Self::ApprovalRequired => "approval_required",
			Self::HoldNotActive => "hold_not_active",
			Self::UserFrozen => "user_frozen",
			Self::MandateRequired => "mandate_required",
			Self::InvoiceNotOpen => "invoice_not_open",
		}
	}

	/// Whether the refusal is about who the caller is rather than about the state of things.
	pub const fn is_permission(self) -> bool {
		match self {
			Self::NotGranted | Self::NotAccountOwner => true,
			Self::ScopeFrozen | Self::InsufficientFunds | Self::ApprovalRequired | Self::HoldNotActive | Self::UserFrozen | Self::MandateRequired | Self::InvoiceNotOpen => false,
		}
	}
}

/// The caller's reference for the business object behind a call: a type, a colon, a lowercase
/// UUID (`subscription:4f0c…`), at most 128 bytes — concierge's form exactly, so it can travel
/// into money events either plane reads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct SubjectRef(String);

impl SubjectRef {
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		let refuse = || DomainError::Validation(format!("subject_ref must be <type>:<lowercase uuid>, at most 128 bytes: {raw:?}"));
		if raw.len() > 128 {
			return Err(refuse());
		}
		let (kind, uuid) = raw.split_once(':').ok_or_else(refuse)?;
		let kind_shaped = kind.as_bytes().first().is_some_and(u8::is_ascii_lowercase) && kind.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
		if !kind_shaped || !is_lowercase_uuid(uuid) {
			return Err(refuse());
		}
		Ok(Self(raw.to_owned()))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

fn is_lowercase_uuid(raw: &str) -> bool {
	let bytes = raw.as_bytes();
	bytes.len() == 36
		&& bytes.iter().enumerate().all(|(i, b)| {
			if matches!(i, 8 | 13 | 18 | 23) {
				*b == b'-'
			} else {
				b.is_ascii_digit() || (b'a'..=b'f').contains(b)
			}
		})
}

impl TryFrom<String> for SubjectRef {
	type Error = DomainError;

	fn try_from(raw: String) -> Result<Self, DomainError> {
		Self::parse(&raw)
	}
}

impl From<SubjectRef> for String {
	fn from(subject: SubjectRef) -> Self {
		subject.0
	}
}

/// Longest memo, in characters (`ledger.proto`).
pub const MAX_MEMO_CHARS: usize = 140;

/// A memo shown to people in wallet history and on approvals: plain text, at most
/// [`MAX_MEMO_CHARS`] characters, no control characters and no link — the same needles
/// concierge's relay refuses (`no_url`), so a memo that reaches a mail is one it will send.
pub fn validate_memo(memo: &str) -> Result<(), DomainError> {
	if memo.chars().count() > MAX_MEMO_CHARS {
		return Err(DomainError::Validation(format!("memo is longer than {MAX_MEMO_CHARS} characters")));
	}
	if memo.chars().any(char::is_control) {
		return Err(DomainError::Validation("memo must be plain text without control characters".into()));
	}
	if contains_url(memo) {
		return Err(DomainError::Validation("memo must not carry a link".into()));
	}
	Ok(())
}

/// Why money moves between scope accounts without an approval (`TransferReason`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferReason {
	/// Fees collected from the caller's account into the scope's `fee` collector.
	FeeSweep,
}

impl TransferReason {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::FeeSweep => "fee_sweep",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"fee_sweep" => Ok(Self::FeeSweep),
			other => Err(DomainError::Validation(format!("unknown transfer reason: {other}"))),
		}
	}
}

/// What a client's automatic transfer is checked against, every figure read under the
/// ceiling's lock.
#[derive(Clone, Copy, Debug)]
pub struct AutoTransferFacts {
	pub amount: Usdt,
	/// What the client moved by `Transfer` in this scope over the last 24 h, `pending` and
	/// `posted`, this one excluded.
	pub transferred_24h: Usdt,
}

impl ScopePolicy {
	/// Whether a client's `Transfer` stays inside the scope's automatic ceilings — per
	/// operation and per rolling 24 h, per `(client, scope)`. Both inclusive.
	pub fn admit_auto_transfer(&self, facts: AutoTransferFacts) -> Result<(), LedgerRefusal> {
		if facts.amount > self.auto_transfer_per_op {
			return Err(LedgerRefusal::ApprovalRequired);
		}
		// An overflowing sum is past any ceiling a u128 can express.
		let total = facts.transferred_24h.checked_add(facts.amount).ok_or(LedgerRefusal::ApprovalRequired)?;
		if total > self.auto_transfer_24h {
			return Err(LedgerRefusal::ApprovalRequired);
		}
		Ok(())
	}
}

/// A unique scope transfer id (UUID). Minted by the application layer.
pub type ScopeTransferId = Id<ScopeTransferTag>;
/// Phantom tag making [`ScopeTransferId`] a distinct, incompatible identity type.
pub struct ScopeTransferTag;

/// A client moving money from a scope account it owns into the scope's `fee` collector. One
/// posted ledger transfer, applied by the relay: `Dr <from> / Cr <to>`, both scope accounts
/// of one scope.
#[derive(Clone, Debug)]
pub struct ScopeTransfer {
	id: ScopeTransferId,
	client: ClientId,
	scope: ScopeId,
	from: ScopeAccountLabel,
	to: ScopeAccountLabel,
	amount: Usdt,
	reason: TransferReason,
	subject_ref: SubjectRef,
	memo: String,
	pending: Vec<ScopeLedgerEvent>,
}

/// Everything a transfer request names — what an idempotent repeat must match.
#[derive(Clone, Debug)]
pub struct TransferTerms {
	pub client: ClientId,
	pub scope: ScopeId,
	pub from: ScopeAccountLabel,
	pub to: ScopeAccountLabel,
	pub amount: Usdt,
	pub reason: TransferReason,
	pub subject_ref: SubjectRef,
	pub memo: String,
}

impl TransferTerms {
	/// The canonical rendering stored beside the idempotency key. Every field is
	/// length-prefixed, so no two different requests render alike.
	pub fn canonical(&self) -> String {
		canonical(&[
			self.from.as_str(),
			self.to.as_str(),
			&self.amount.base_units().to_string(),
			self.reason.as_str(),
			self.subject_ref.as_str(),
			&self.memo,
		])
	}
}

/// `len:value` per field, joined by `|`.
pub fn canonical(fields: &[&str]) -> String {
	fields.iter().map(|field| format!("{}:{field}", field.len())).collect::<Vec<_>>().join("|")
}

impl ScopeTransfer {
	/// A client's automatic transfer: into the scope's `fee` collector only, out of a label
	/// that is not one of banking's own, a positive amount, a valid memo. Who owns the source
	/// is the store's to check, under its lock.
	pub fn request(id: ScopeTransferId, terms: TransferTerms) -> Result<Self, DomainError> {
		let mut requested = Self::restore(id, terms)?;
		requested.pending.push(ScopeLedgerEvent::Transferred {
			transfer_id: id,
			scope: requested.scope.clone(),
			from_label: requested.from.clone(),
			to_scope: requested.scope.clone(),
			to_label: requested.to.clone(),
			amount: requested.amount,
		});
		Ok(requested)
	}

	/// Rebuild from storage (no events).
	pub fn restore(id: ScopeTransferId, terms: TransferTerms) -> Result<Self, DomainError> {
		if terms.amount.is_zero() {
			return Err(DomainError::Validation("amount must be positive".into()));
		}
		if terms.scope.is_main() {
			return Err(DomainError::Validation("a client's transfer never names main".into()));
		}
		if terms.from == terms.to {
			return Err(DomainError::Validation("a transfer needs two different accounts".into()));
		}
		// The domain's own floor under the use case's checks: no client transfer debits one
		// of banking's labels, and a fee sweep only ever lands in the fee collector.
		if terms.from.is_reserved() {
			return Err(DomainError::Forbidden(format!("{} is banking's own account", terms.from)));
		}
		match terms.reason {
			TransferReason::FeeSweep if terms.to != ScopeAccountLabel::fee() => return Err(DomainError::Validation("a fee sweep goes into the fee collector".into())),
			TransferReason::FeeSweep => {}
		}
		validate_memo(&terms.memo)?;
		Ok(Self {
			id,
			client: terms.client,
			scope: terms.scope,
			from: terms.from,
			to: terms.to,
			amount: terms.amount,
			reason: terms.reason,
			subject_ref: terms.subject_ref,
			memo: terms.memo,
			pending: Vec::new(),
		})
	}

	pub fn id(&self) -> ScopeTransferId {
		self.id
	}

	pub fn client(&self) -> &ClientId {
		&self.client
	}

	pub fn scope(&self) -> &ScopeId {
		&self.scope
	}

	pub fn from(&self) -> &ScopeAccountLabel {
		&self.from
	}

	pub fn to(&self) -> &ScopeAccountLabel {
		&self.to
	}

	pub fn amount(&self) -> Usdt {
		self.amount
	}

	pub fn reason(&self) -> TransferReason {
		self.reason
	}

	pub fn subject_ref(&self) -> &SubjectRef {
		&self.subject_ref
	}

	pub fn memo(&self) -> &str {
		&self.memo
	}
}

impl Entity for ScopeTransfer {
	type Id = ScopeTransferId;

	fn id(&self) -> ScopeTransferId {
		self.id
	}
}

impl AggregateRoot for ScopeTransfer {
	const NAME: &'static str = "scope_transfer";
}

/// Facts raised by the scope ledger; internally tagged so the stored JSON describes itself.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScopeLedgerEvent {
	/// Relay: post `Dr scope_account:<scope>:<from_label> / Cr
	/// scope_account:<to_scope>:<to_label>` for `amount`.
	Transferred {
		transfer_id: ScopeTransferId,
		scope: ScopeId,
		from_label: ScopeAccountLabel,
		to_scope: ScopeId,
		to_label: ScopeAccountLabel,
		amount: Usdt,
	},
}

impl DomainEvent for ScopeLedgerEvent {
	const KIND: &'static str = "scope_ledger";
}

impl EmitsEvents for ScopeTransfer {
	type Event = ScopeLedgerEvent;

	fn drain_events(&mut self) -> Vec<ScopeLedgerEvent> {
		core::mem::take(&mut self.pending)
	}
}

/// A unique hold id (UUID). Minted by the application layer.
pub type HoldId = Id<HoldTag>;
/// Phantom tag making [`HoldId`] a distinct, incompatible identity type.
pub struct HoldTag;

/// Where a hold stands (`HoldState` on the wire).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldState {
	/// Holding what is left; may be captured or voided.
	Active,
	/// Everything held was captured.
	Captured,
	/// Released by the client (or, later, by a revoked mandate).
	Voided,
	/// Released by its TTL.
	Expired,
}

impl HoldState {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Active => "active",
			Self::Captured => "captured",
			Self::Voided => "voided",
			Self::Expired => "expired",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"active" => Ok(Self::Active),
			"captured" => Ok(Self::Captured),
			"voided" => Ok(Self::Voided),
			"expired" => Ok(Self::Expired),
			other => Err(DomainError::Validation(format!("unknown hold state: {other}"))),
		}
	}
}

/// Where a capture sends held money: a scope account of the client that placed the hold, or
/// another user's compartment in the same scope (the cash leg of a book fill).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum HoldDestination {
	Account(ScopeAccountLabel),
	User(UserId),
}

/// Everything a `Reserve` names once banking has resolved the user and the mandate.
#[derive(Clone, Debug)]
pub struct HoldTerms {
	pub client: ClientId,
	pub scope: ScopeId,
	pub user: UserId,
	pub amount: Usdt,
	/// Unix seconds; `None` holds until captured or voided.
	pub expires_at: Option<i64>,
	pub subject_ref: SubjectRef,
	/// The mandate the hold was placed under, for its whole life.
	pub mandate_id: uuid::Uuid,
}

/// Where a hold stands, as stored — what [`Hold::restore`] needs beyond its terms.
#[derive(Clone, Copy, Debug)]
pub struct HoldProgress {
	pub captured: Usdt,
	pub state: HoldState,
	/// The version of the hold's last event; each event is one more.
	pub version: u64,
	/// The version whose event created the ledger reservation standing now. Its ledger id
	/// derives from it, so the next capture or release knows which pending to void.
	pub pending_version: u64,
}

/// Why a capture did not happen: a refusal the wire names, or a request that cannot be one.
#[derive(Debug)]
pub enum HoldError {
	Refused(LedgerRefusal),
	Invalid(DomainError),
}

/// A client's hold on a user's compartment in a scope: money that stops being spendable for
/// the user but stays theirs until the client captures it, or until it is voided or expires.
///
/// In the ledger a hold is ONE pending transfer at a time, `Dr <compartment> / Cr
/// hold_clearing`, for exactly what is left. A capture voids it, posts the captured amount out
/// of the compartment, and reserves the rest anew — one linked chain, so a partial capture
/// never leaves the user's money free in between. TigerBeetle can post a pending only once,
/// which is why a capture is not a partial post.
#[derive(Clone, Debug)]
pub struct Hold {
	id: HoldId,
	terms: HoldTerms,
	progress: HoldProgress,
	pending: Vec<HoldEvent>,
}

impl Hold {
	/// A new hold for `terms.amount`, raising the reservation's event.
	pub fn place(id: HoldId, terms: HoldTerms) -> Result<Self, DomainError> {
		let progress = HoldProgress {
			captured: Usdt::ZERO,
			state: HoldState::Active,
			version: 1,
			pending_version: 1,
		};
		let mut hold = Self::restore(id, terms, progress)?;
		hold.pending.push(HoldEvent::Placed {
			hold_id: id,
			scope: hold.terms.scope.clone(),
			user: hold.terms.user,
			amount: hold.terms.amount,
			version: 1,
		});
		Ok(hold)
	}

	/// Rebuild from storage (no events).
	pub fn restore(id: HoldId, terms: HoldTerms, progress: HoldProgress) -> Result<Self, DomainError> {
		if terms.amount.is_zero() {
			return Err(DomainError::Validation("amount must be positive".into()));
		}
		if terms.scope.is_main() {
			return Err(DomainError::Validation("a hold is never placed in main".into()));
		}
		if progress.captured > terms.amount {
			return Err(DomainError::Validation(format!("hold {id} captured more than it held")));
		}
		Ok(Self {
			id,
			terms,
			progress,
			pending: Vec::new(),
		})
	}

	pub fn id(&self) -> HoldId {
		self.id
	}

	pub fn terms(&self) -> &HoldTerms {
		&self.terms
	}

	pub fn progress(&self) -> HoldProgress {
		self.progress
	}

	pub fn state(&self) -> HoldState {
		self.progress.state
	}

	/// What the hold still holds: nothing once it is no longer active.
	pub fn remaining(&self) -> Usdt {
		match self.progress.state {
			HoldState::Active => self.terms.amount.checked_sub(self.progress.captured).unwrap_or(Usdt::ZERO),
			HoldState::Captured | HoldState::Voided | HoldState::Expired => Usdt::ZERO,
		}
	}

	/// Active, but past its expiry: the reaper's to release, and no longer capturable.
	pub fn is_due(&self, now: i64) -> bool {
		self.progress.state == HoldState::Active && self.terms.expires_at.is_some_and(|at| at <= now)
	}

	/// Move `amount` (everything left when `None`) to `to`. Refused unless the hold is active
	/// and not yet due, and for more than it holds; capturing into the held user's own
	/// compartment is not a capture at all — that is a void.
	pub fn capture(&mut self, amount: Option<Usdt>, to: HoldDestination, subject_ref: SubjectRef, now: i64) -> Result<Usdt, HoldError> {
		if to == HoldDestination::User(self.terms.user) {
			return Err(HoldError::Invalid(DomainError::Validation("a capture into the held user's own compartment is a void".into())));
		}
		if self.progress.state != HoldState::Active || self.is_due(now) {
			return Err(HoldError::Refused(LedgerRefusal::HoldNotActive));
		}
		let held = self.remaining();
		let amount = amount.unwrap_or(held);
		if amount.is_zero() {
			return Err(HoldError::Invalid(DomainError::Validation("amount must be positive".into())));
		}
		if amount > held {
			return Err(HoldError::Refused(LedgerRefusal::InsufficientFunds));
		}
		let version = self.progress.version + 1;
		let released = self.progress.pending_version;
		// `amount <= held <= terms.amount - captured`, so this cannot overflow.
		self.progress.captured = self
			.progress
			.captured
			.checked_add(amount)
			.ok_or_else(|| HoldError::Invalid(DomainError::Validation("capture overflows".into())))?;
		self.progress.version = version;
		if amount == held {
			self.progress.state = HoldState::Captured;
		} else {
			self.progress.pending_version = version;
		}
		self.pending.push(HoldEvent::Captured {
			hold_id: self.id,
			scope: self.terms.scope.clone(),
			user: self.terms.user,
			version,
			released,
			held,
			amount,
			to,
			subject_ref,
		});
		Ok(amount)
	}

	/// Release what is left back to the user. A hold that is no longer active is left as it
	/// is (`false`); one past its expiry expires instead of being voided — its TTL decided it
	/// first.
	pub fn void(&mut self, subject_ref: SubjectRef, now: i64) -> bool {
		if self.is_due(now) {
			return self.release(HoldState::Expired, Some(subject_ref));
		}
		self.progress.state == HoldState::Active && self.release(HoldState::Voided, Some(subject_ref))
	}

	/// The reaper's release of a hold past its expiry; `false` when it is not due.
	pub fn expire(&mut self, now: i64) -> bool {
		self.is_due(now) && self.release(HoldState::Expired, None)
	}

	fn release(&mut self, state: HoldState, subject_ref: Option<SubjectRef>) -> bool {
		let held = self.remaining();
		let version = self.progress.version + 1;
		self.progress.version = version;
		self.progress.state = state;
		self.pending.push(HoldEvent::Released {
			hold_id: self.id,
			scope: self.terms.scope.clone(),
			user: self.terms.user,
			version,
			released: self.progress.pending_version,
			held,
			expired: state == HoldState::Expired,
			subject_ref,
		});
		true
	}
}

impl Entity for Hold {
	type Id = HoldId;

	fn id(&self) -> HoldId {
		self.id
	}
}

impl AggregateRoot for Hold {
	const NAME: &'static str = "scope_hold";
}

/// A hold's ledger steps, relayed in order. Every one names the user, so a scan of unapplied
/// debits for that user's compartment finds it. `version` is the event's own; a reservation's
/// ledger id derives from the version that created it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HoldEvent {
	/// Relay: reserve `Dr scope_claim:<scope>:<user> / Cr hold_clearing` for `amount`,
	/// pending, as the reservation of `version`.
	Placed {
		hold_id: HoldId,
		scope: ScopeId,
		user: UserId,
		amount: Usdt,
		version: u64,
	},
	/// Relay, linked: void the reservation of `released` (`held`), post `amount` out of the
	/// compartment to `to`, and reserve `held - amount` as the reservation of `version`
	/// when anything is left.
	Captured {
		hold_id: HoldId,
		scope: ScopeId,
		user: UserId,
		version: u64,
		released: u64,
		held: Usdt,
		amount: Usdt,
		to: HoldDestination,
		/// The capturing call's own ref.
		subject_ref: SubjectRef,
	},
	/// Relay: void the reservation of `released`, giving `held` back to the user.
	Released {
		hold_id: HoldId,
		scope: ScopeId,
		user: UserId,
		version: u64,
		released: u64,
		held: Usdt,
		expired: bool,
		/// The voiding call's ref; `None` when the TTL released it.
		subject_ref: Option<SubjectRef>,
	},
}

impl DomainEvent for HoldEvent {
	const KIND: &'static str = "scope_holds";
}

impl EmitsEvents for Hold {
	type Event = HoldEvent;

	fn drain_events(&mut self) -> Vec<HoldEvent> {
		core::mem::take(&mut self.pending)
	}
}

/// What a client's money-event stream says happened (`MoneyEvent.Kind`). Stored by name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoneyEventKind {
	AccountOpened,
	HoldPlaced,
	HoldCaptured,
	HoldVoided,
	HoldExpired,
	Transferred,
	InvoiceCreated,
	InvoicePaid,
	InvoiceCancelled,
	InvoiceExpired,
}

impl MoneyEventKind {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::AccountOpened => "account_opened",
			Self::HoldPlaced => "hold_placed",
			Self::HoldCaptured => "hold_captured",
			Self::HoldVoided => "hold_voided",
			Self::HoldExpired => "hold_expired",
			Self::Transferred => "transferred",
			Self::InvoiceCreated => "invoice_created",
			Self::InvoicePaid => "invoice_paid",
			Self::InvoiceCancelled => "invoice_cancelled",
			Self::InvoiceExpired => "invoice_expired",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"account_opened" => Ok(Self::AccountOpened),
			"hold_placed" => Ok(Self::HoldPlaced),
			"hold_captured" => Ok(Self::HoldCaptured),
			"hold_voided" => Ok(Self::HoldVoided),
			"hold_expired" => Ok(Self::HoldExpired),
			"transferred" => Ok(Self::Transferred),
			"invoice_created" => Ok(Self::InvoiceCreated),
			"invoice_paid" => Ok(Self::InvoicePaid),
			"invoice_cancelled" => Ok(Self::InvoiceCancelled),
			"invoice_expired" => Ok(Self::InvoiceExpired),
			other => Err(DomainError::Validation(format!("unknown money event kind: {other}"))),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn usdt(raw: &str) -> Usdt {
		Usdt::parse_decimal(raw).unwrap()
	}

	fn client(raw: &str) -> ClientId {
		ClientId::parse(raw).unwrap()
	}

	fn scope(raw: &str) -> ScopeId {
		ScopeId::parse(raw).unwrap()
	}

	fn label(raw: &str) -> ScopeAccountLabel {
		ScopeAccountLabel::parse(raw).unwrap()
	}

	fn grant(c: &str, s: &str, prefix: &str) -> Grant {
		Grant {
			client: client(c),
			scope: scope(s),
			label_prefix: prefix.to_owned(),
			compartments: false,
		}
	}

	#[test]
	fn client_ids_are_slugs() {
		for ok in ["invest", "sa-panel", "a", "a_1"] {
			assert!(ClientId::parse(ok).is_ok(), "{ok:?} refused");
		}
		for bad in ["", "1a", "Invest", "a b", "a:b", &"a".repeat(65)] {
			assert!(ClientId::parse(bad).is_err(), "{bad:?} accepted");
		}
		assert!(ClientId::banking().is_banking());
	}

	#[test]
	fn a_grant_set_refuses_banking_main_an_empty_prefix_and_overlaps() {
		assert!(Grants::new(vec![grant("invest", "invest", "alloc:"), grant("panel", "sa", "panel:"), grant("invest", "sa", "alloc:")]).is_ok());
		let refused = |grants: Vec<Grant>, needle: &str| {
			let err = Grants::new(grants).expect_err(needle).to_string();
			assert!(err.contains(needle), "expected {needle:?} in {err}");
		};
		refused(vec![grant("banking", "invest", "alloc:")], "banking");
		refused(vec![grant("invest", "main", "alloc:")], "scope main");
		refused(vec![grant("invest", "invest", "")], "label_prefix");
		refused(vec![grant("invest", "invest", "Alloc:")], "label_prefix");
		refused(vec![grant("invest", "sa", "alloc:"), grant("panel", "sa", "alloc:x")], "overlap");
		refused(vec![grant("invest", "sa", "alloc:x"), grant("panel", "sa", "alloc:")], "overlap");
		refused(vec![grant("invest", "sa", "a"), grant("invest", "sa", "a")], "overlap");
	}

	#[test]
	fn a_client_directory_maps_each_fingerprint_to_one_client() {
		let invest = "a".repeat(64);
		let panel = "b".repeat(64);
		let directory = ClientDirectory::new(vec![(client("invest"), vec![invest.clone(), "c".repeat(64)]), (client("panel"), vec![panel.clone()])]).unwrap();
		assert_eq!(directory.client_for(&invest), Some(&client("invest")));
		assert_eq!(directory.client_for(&"c".repeat(64)), Some(&client("invest")), "a second certificate for rotation");
		assert_eq!(directory.client_for(&panel), Some(&client("panel")));
		assert_eq!(directory.client_for(&"d".repeat(64)), None);
		assert!(directory.knows(&client("panel")) && !directory.knows(&client("sa")));
		assert!(ClientDirectory::new(vec![(ClientId::banking(), vec![invest.clone()])]).is_err());
		assert!(ClientDirectory::new(vec![(client("invest"), vec![])]).is_err());
		assert!(ClientDirectory::new(vec![(client("invest"), vec![invest.to_uppercase()])]).is_err());
		assert!(
			ClientDirectory::new(vec![(client("invest"), vec![invest.clone()]), (client("panel"), vec![invest])]).is_err(),
			"one certificate, two clients"
		);
	}

	#[test]
	fn a_label_is_openable_only_under_the_callers_prefix_and_never_a_reserved_one() {
		let grants = Grants::new(vec![grant("invest", "invest", "alloc:"), grant("panel", "sa", "f")]).unwrap();
		assert!(grants.may_open(&client("invest"), &scope("invest"), &label("alloc:quy_nhon")));
		assert!(!grants.may_open(&client("invest"), &scope("sa"), &label("alloc:quy_nhon")), "no grant in sa");
		assert!(!grants.may_open(&client("panel"), &scope("invest"), &label("alloc:x")), "another client's prefix");
		assert!(grants.may_open(&client("panel"), &scope("sa"), &label("fund")));
		assert!(!grants.may_open(&client("panel"), &scope("sa"), &label("fee")), "a prefix that covers fee still never opens it");
	}

	#[test]
	fn subject_refs_take_concierges_form() {
		assert!(SubjectRef::parse("subscription:4f0c2d1e-0000-4000-8000-0123456789ab").is_ok());
		for bad in [
			"",
			"subscription",
			"Subscription:4f0c2d1e-0000-4000-8000-0123456789ab",
			"subscription:4F0C2D1E-0000-4000-8000-0123456789AB",
			"1x:4f0c2d1e-0000-4000-8000-0123456789ab",
			"x:4f0c2d1e00004000800001234567890ab",
			"a:b:4f0c2d1e-0000-4000-8000-0123456789ab",
		] {
			assert!(SubjectRef::parse(bad).is_err(), "{bad:?} accepted");
		}
	}

	#[test]
	fn a_memo_is_short_plain_text_without_links() {
		assert!(validate_memo("").is_ok());
		assert!(validate_memo(&"é".repeat(140)).is_ok(), "the limit counts characters, not bytes");
		assert!(validate_memo(&"a".repeat(141)).is_err());
		assert!(validate_memo("see https://x").is_err());
		assert!(validate_memo("www.example").is_err());
		assert!(validate_memo("line\nbreak").is_err());
	}

	#[test]
	fn the_automatic_ceilings_are_inclusive_per_op_then_per_day() {
		let policy = ScopePolicy {
			min_kyc_level: 0,
			max_per_move: None,
			max_moved_in_24h: None,
			max_balance: None,
			auto_payout_per_op: usdt("1000"),
			auto_payout_24h: usdt("5000"),
			auto_transfer_per_op: usdt("1000"),
			auto_transfer_24h: usdt("5000"),
		};
		let facts = |amount: &str, before: &str| AutoTransferFacts {
			amount: usdt(amount),
			transferred_24h: usdt(before),
		};
		assert_eq!(policy.admit_auto_transfer(facts("1000", "4000")), Ok(()));
		assert_eq!(policy.admit_auto_transfer(facts("1000.000000000000000001", "0")), Err(LedgerRefusal::ApprovalRequired));
		assert_eq!(policy.admit_auto_transfer(facts("1", "5000")), Err(LedgerRefusal::ApprovalRequired));
		assert_eq!(
			policy.admit_auto_transfer(AutoTransferFacts {
				amount: usdt("1"),
				transferred_24h: Usdt::from_base_units(u128::MAX)
			}),
			Err(LedgerRefusal::ApprovalRequired),
			"an overflowing sum is over the ceiling"
		);
	}

	fn terms() -> TransferTerms {
		TransferTerms {
			client: client("invest"),
			scope: scope("invest"),
			from: label("alloc:quy_nhon"),
			to: ScopeAccountLabel::fee(),
			amount: usdt("12.5"),
			reason: TransferReason::FeeSweep,
			subject_ref: SubjectRef::parse("fee_settlement:4f0c2d1e-0000-4000-8000-0123456789ab").unwrap(),
			memo: String::new(),
		}
	}

	#[test]
	fn a_transfer_raises_one_event_naming_both_accounts() {
		let mut transfer = ScopeTransfer::request(ScopeTransferId::new(), terms()).unwrap();
		let events = transfer.drain_events();
		assert_eq!(events.len(), 1);
		let json = serde_json::to_value(&events[0]).unwrap();
		assert_eq!(json["type"], "transferred");
		assert_eq!(json["from_label"], "alloc:quy_nhon");
		assert_eq!(json["to_label"], "fee");
		assert_eq!(json["to_scope"], "invest");
		assert!(transfer.drain_events().is_empty());
		let same = TransferTerms { from: label("fee"), ..terms() };
		assert!(ScopeTransfer::request(ScopeTransferId::new(), same).is_err(), "same account twice");
		assert!(ScopeTransfer::request(ScopeTransferId::new(), TransferTerms { amount: Usdt::ZERO, ..terms() }).is_err());
		let from_treasury = TransferTerms { from: label("treasury"), ..terms() };
		assert!(
			matches!(ScopeTransfer::request(ScopeTransferId::new(), from_treasury), Err(DomainError::Forbidden(_))),
			"banking's own label is never a source"
		);
		let sideways = TransferTerms {
			to: label("alloc:other"),
			..terms()
		};
		assert!(ScopeTransfer::request(ScopeTransferId::new(), sideways).is_err(), "a fee sweep lands only in fee");
	}

	#[test]
	fn canonical_terms_cannot_collide_by_concatenation() {
		assert_ne!(canonical(&["a|1:b", ""]), canonical(&["a", "b|"]));
		let base = terms().canonical();
		assert_ne!(base, TransferTerms { memo: "x".into(), ..terms() }.canonical());
		assert_ne!(base, TransferTerms { amount: usdt("12.6"), ..terms() }.canonical());
	}

	fn sref() -> SubjectRef {
		SubjectRef::parse("subscription:4f0c2d1e-0000-4000-8000-0123456789ab").unwrap()
	}

	fn hold(amount: &str, expires_at: Option<i64>) -> Hold {
		let terms = HoldTerms {
			client: client("invest"),
			scope: scope("invest"),
			user: UserId::new(),
			amount: usdt(amount),
			expires_at,
			subject_ref: SubjectRef::parse("subscription:4f0c2d1e-0000-4000-8000-0123456789ab").unwrap(),
			mandate_id: uuid::Uuid::new_v4(),
		};
		Hold::place(HoldId::new(), terms).unwrap()
	}

	#[test]
	fn partial_captures_chain_reservations_until_the_hold_is_exhausted() {
		let mut h = hold("10", None);
		assert!(matches!(h.drain_events().as_slice(), [HoldEvent::Placed { version: 1, .. }]));
		let to = HoldDestination::Account(label("alloc:x"));
		assert_eq!(h.capture(Some(usdt("4")), to.clone(), sref(), 0).unwrap(), usdt("4"));
		assert_eq!(h.remaining(), usdt("6"));
		assert_eq!(h.state(), HoldState::Active);
		let events = h.drain_events();
		let [
			HoldEvent::Captured {
				version: 2,
				released: 1,
				held,
				amount,
				..
			},
		] = events.as_slice()
		else {
			panic!("{events:?}")
		};
		assert_eq!((*held, *amount), (usdt("10"), usdt("4")));
		assert_eq!(h.progress().pending_version, 2, "the rest is reserved anew as version 2");
		assert!(matches!(
			h.capture(Some(usdt("6.000000000000000001")), to.clone(), sref(), 0),
			Err(HoldError::Refused(LedgerRefusal::InsufficientFunds))
		));
		assert_eq!(h.capture(None, to.clone(), sref(), 0).unwrap(), usdt("6"), "empty = everything left");
		assert_eq!(h.state(), HoldState::Captured);
		assert_eq!(h.remaining(), Usdt::ZERO);
		let events = h.drain_events();
		assert!(matches!(events.as_slice(), [HoldEvent::Captured { version: 3, released: 2, .. }]), "{events:?}");
		assert!(matches!(h.capture(None, to, sref(), 0), Err(HoldError::Refused(LedgerRefusal::HoldNotActive))));
		assert!(!h.void(sref(), 0), "a captured hold is returned unchanged");
		assert!(h.drain_events().is_empty());
	}

	#[test]
	fn a_void_releases_what_is_left_and_a_due_hold_expires_instead() {
		let mut h = hold("10", Some(100));
		h.drain_events();
		h.capture(Some(usdt("3")), HoldDestination::User(UserId::new()), sref(), 99).unwrap();
		assert!(h.void(sref(), 99));
		assert_eq!(h.state(), HoldState::Voided);
		let events = h.drain_events();
		let [
			_,
			HoldEvent::Released {
				held, expired: false, released: 2, ..
			},
		] = events.as_slice()
		else {
			panic!("{events:?}")
		};
		assert_eq!(*held, usdt("7"));
		assert_eq!(h.progress().captured, usdt("3"), "captured keeps what was taken before");

		let mut due = hold("10", Some(100));
		due.drain_events();
		assert!(matches!(
			due.capture(None, HoldDestination::Account(label("a")), sref(), 100),
			Err(HoldError::Refused(LedgerRefusal::HoldNotActive))
		));
		assert!(!due.expire(99), "not due yet");
		assert!(due.void(sref(), 100));
		assert_eq!(due.state(), HoldState::Expired);
		assert!(matches!(due.drain_events().as_slice(), [HoldEvent::Released { expired: true, .. }]));
	}

	#[test]
	fn a_capture_into_the_held_users_own_compartment_is_not_a_capture() {
		let mut h = hold("1", None);
		let own = HoldDestination::User(h.terms().user);
		assert!(matches!(h.capture(None, own, sref(), 0), Err(HoldError::Invalid(_))));
		assert!(matches!(h.capture(Some(Usdt::ZERO), HoldDestination::Account(label("a")), sref(), 0), Err(HoldError::Invalid(_))));
	}

	#[test]
	fn hold_states_and_money_event_kinds_round_trip_by_name() {
		for state in [HoldState::Active, HoldState::Captured, HoldState::Voided, HoldState::Expired] {
			assert_eq!(HoldState::parse(state.as_str()).unwrap(), state);
		}
		for kind in [
			MoneyEventKind::AccountOpened,
			MoneyEventKind::HoldPlaced,
			MoneyEventKind::HoldCaptured,
			MoneyEventKind::HoldVoided,
			MoneyEventKind::HoldExpired,
			MoneyEventKind::Transferred,
		] {
			assert_eq!(MoneyEventKind::parse(kind.as_str()).unwrap(), kind);
		}
	}
}
