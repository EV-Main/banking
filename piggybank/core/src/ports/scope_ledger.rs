//! The scope ledger's control plane — who owns which scope account, which calls went through
//! under which idempotency key, where each client transfer and hold stands, and the money
//! events each client is shown. The balances are TigerBeetle's
//! ([`Ledger`](super::ledger::Ledger)).

use async_trait::async_trait;
use domain::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, Hold, LedgerRefusal, MandateOperation, MoneyEventKind, ScopeTransfer},
	scopes::{MoveState, ScopeAccountLabel, ScopeId},
	users::UserId,
};
use uuid::Uuid;

use crate::ports::{compartments::ScopeRecord, ledger::LockedReads};

/// The RPC names idempotency keys are unique under, as stored.
pub const RPC_OPEN_ACCOUNT: &str = "open_account";
pub const RPC_TRANSFER: &str = "transfer";
pub const RPC_RESERVE: &str = "reserve";
pub const RPC_CAPTURE: &str = "capture";
pub const RPC_VOID: &str = "void";

/// A scope account and the client that opened it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedAccount {
	pub scope: ScopeId,
	pub label: ScopeAccountLabel,
	pub owner: ClientId,
	/// Unix seconds.
	pub opened_at: i64,
}

/// What an earlier call under the same idempotency key asked and produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredRequest {
	/// The canonical rendering of everything the earlier call asked.
	pub terms: String,
	/// The label it opened, or the transfer it recorded.
	pub result_id: String,
}

/// One idempotency key, unique per (client, scope, RPC).
#[derive(Clone, Copy, Debug)]
pub struct RequestKey<'a> {
	pub client: &'a ClientId,
	pub scope: &'a ScopeId,
	pub rpc: &'static str,
	pub key: &'a str,
}

/// What a store did with a mutating call.
#[derive(Debug)]
pub enum Recorded<T> {
	/// Done now, and the key recorded with it.
	New(T),
	/// The key was already used: what that call asked and produced. The application decides
	/// whether this is a repeat (same terms) or a conflict.
	Existing(StoredRequest),
}

/// A transfer as it was recorded, with where it stands now.
#[derive(Clone, Debug)]
pub struct RecordedTransfer {
	pub transfer: ScopeTransfer,
	pub to_scope: ScopeId,
	pub state: MoveState,
	/// Why the ledger refused it; `Some` exactly when `state` is `Failed`.
	pub failure_reason: Option<String>,
	/// Unix seconds.
	pub created_at: i64,
}

/// Why a call did not happen: a refusal the wire names by its reason, a broken invariant of
/// banking's own (INTERNAL — no retry fixes it, and it is not the caller's to know why), or
/// anything else.
#[derive(Debug)]
pub enum TransferError {
	Refused(LedgerRefusal),
	Internal(String),
	Domain(DomainError),
	/// The caller holds as much as it may of something at once (RESOURCE_EXHAUSTED).
	Exhausted(String),
}

impl From<DomainError> for TransferError {
	fn from(err: DomainError) -> Self {
		Self::Domain(err)
	}
}

/// What the control plane says about a transfer, read under the locks it is about to be
/// recorded under and on the connection that holds them.
#[derive(Clone, Debug)]
pub struct TransferFacts {
	/// The scope as it stands now; `None` when banking does not know it.
	pub scope: Option<ScopeRecord>,
	/// Who owns the source account; `None` when it does not exist.
	pub source_owner: Option<ClientId>,
	/// Who owns the destination account; `None` when it does not exist.
	pub destination_owner: Option<ClientId>,
	/// Σ the client's transfers in the scope over the last 24 h, `pending` and `posted`.
	pub transferred_24h: Usdt,
}

/// The rules a transfer must pass, asked by the store while it holds the locks. What the
/// ledger says is read inside [`Self::admit`], through `reads`.
#[async_trait]
pub trait TransferAdmission: Send + Sync {
	async fn admit(&self, facts: &TransferFacts, reads: &mut dyn LockedReads) -> Result<(), TransferError>;
}

#[async_trait]
pub trait ScopeLedgerStore: Send + Sync {
	/// Every account `client` owns in `scope`, ordered by label.
	async fn accounts_of(&self, client: &ClientId, scope: &ScopeId) -> Result<Vec<OwnedAccount>, DomainError>;

	/// Open `(scope, label)` for `client`. Under a lock on the key it answers a key already
	/// used with what that call recorded; a label `client` already owns is its existing
	/// account (the key is recorded against it); a label another client owns is `Conflict`.
	/// `terms` is the canonical rendering stored with the key.
	async fn open_account(&self, key: RequestKey<'_>, label: &ScopeAccountLabel, terms: &str) -> Result<Recorded<OwnedAccount>, DomainError>;

	/// The transfer recorded under `id`, if any.
	async fn transfer(&self, id: uuid::Uuid) -> Result<Option<RecordedTransfer>, DomainError>;

	/// Record the transfer and drain its event to the outbox in one transaction. Under a lock
	/// on the key it answers a key already used with what that call recorded; otherwise,
	/// under the client's ceiling lock for the scope and the source account's write lock, it
	/// asks `admission` with the facts and records only on its yes.
	async fn record_transfer(&self, transfer: &mut ScopeTransfer, key: RequestKey<'_>, terms: &str, admission: &dyn TransferAdmission) -> Result<Recorded<RecordedTransfer>, TransferError>;
}

/// A hold as recorded, with the facts the wire echoes beside the aggregate.
#[derive(Clone, Debug)]
pub struct RecordedHold {
	pub hold: Hold,
	/// The user as the client named them.
	pub auth_subject: String,
	/// Unix seconds.
	pub created_at: i64,
}

/// A user a hold names, as banking knows them.
#[derive(Clone, Copy, Debug)]
pub struct HoldUser {
	pub id: UserId,
	/// Frozen or disabled: no new debit of their compartment.
	pub blocked: bool,
}

/// What the control plane says about a new hold, read under the key's lock and the
/// compartment's write lock.
#[derive(Clone, Debug)]
pub struct HoldFacts {
	/// The scope as it stands now; `None` when banking does not know it.
	pub scope: Option<ScopeRecord>,
	/// The user the client named; `None` when banking has no such user.
	pub user: Option<HoldUser>,
	/// The database's clock, unix seconds: what the TTL counts from.
	pub now: i64,
}

/// Decides a new hold under the store's locks and returns it placed — or refuses. The
/// compartment's spendable is read inside, through `reads`.
#[async_trait]
pub trait HoldAdmission: Send + Sync {
	async fn admit(&self, facts: &HoldFacts, reads: &mut dyn LockedReads) -> Result<Hold, TransferError>;
}

/// Where a capture asks the held money to go, as the client named it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CaptureTarget {
	Account(ScopeAccountLabel),
	/// Another user's compartment in the scope, by `auth_subject`.
	User(String),
}

/// What the control plane says about a capture or a void, read with the hold's row locked.
#[derive(Clone, Debug)]
pub struct HoldChangeFacts {
	pub scope: Option<ScopeRecord>,
	pub now: i64,
	/// The held user's standing now.
	pub held_user: Option<HoldUser>,
	/// A capture into an account: who owns it (`None` when it does not exist).
	pub destination_owner: Option<ClientId>,
	/// A capture into a user: who that is (`None` when banking has no such user).
	pub destination_user: Option<HoldUser>,
}

/// Applies a capture or a void to the locked hold, or refuses. A hold left unchanged (a void
/// of one no longer active) is recorded under the key all the same.
#[async_trait]
pub trait HoldChange: Send + Sync {
	async fn apply(&self, hold: &mut Hold, facts: &HoldChangeFacts) -> Result<(), TransferError>;
}

#[async_trait]
pub trait HoldStore: Send + Sync {
	/// The hold `id` if `client` placed it in `scope`; `None` for anyone else's, as for one
	/// that does not exist.
	async fn hold(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<Option<RecordedHold>, DomainError>;

	/// Place a hold on `auth_subject`'s compartment. Under a lock on the key it answers a key
	/// already used with what that call recorded; otherwise, under the compartment's write
	/// lock, it asks `admission` and records the hold it returns, with its event, in one
	/// transaction.
	async fn record_hold(&self, key: RequestKey<'_>, terms: &str, auth_subject: &str, admission: &dyn HoldAdmission) -> Result<Recorded<RecordedHold>, TransferError>;

	/// Capture (`target` is `Some`) or void a hold the caller placed. Under a lock on the key
	/// it answers a key already used; otherwise it locks the hold's row — NOT_FOUND when the
	/// caller did not place it in this scope — and lets `change` apply to it.
	async fn change_hold(&self, key: RequestKey<'_>, terms: &str, hold_id: Uuid, target: Option<&CaptureTarget>, change: &dyn HoldChange) -> Result<Recorded<RecordedHold>, TransferError>;
}

/// The user-consent side of a hold (`ledger.proto`, USER CONSENT): whether a live mandate
/// lets a client act on a user's compartment.
#[async_trait]
pub trait MandateBook: Send + Sync {
	/// The live mandate of `user` for `client` in `scope` that lists `operation` and allows
	/// `amount`; `None` when there is none.
	async fn authorize(&self, client: &ClientId, scope: &ScopeId, user: UserId, operation: MandateOperation, amount: Usdt) -> Result<Option<Uuid>, DomainError>;

	/// Whether the mandate a hold was placed under lists `operation`.
	async fn lists(&self, mandate: Uuid, operation: MandateOperation) -> Result<bool, DomainError>;
}

/// One end of a money movement in an event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventAccount {
	Account(ScopeAccountLabel),
	/// A user's compartment, by `auth_subject`.
	User(String),
}

/// A scope account's balance right after an event, with its owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventBalance {
	pub label: ScopeAccountLabel,
	pub owner: ClientId,
	pub balance: Usdt,
}

/// One money event as stored. Who may see it was decided when it was written.
#[derive(Clone, Debug)]
pub struct MoneyEventRecord {
	pub position: i64,
	pub event_id: Uuid,
	pub scope: ScopeId,
	pub kind: MoneyEventKind,
	pub ref_id: String,
	pub aggregate_version: u64,
	pub auth_subject: String,
	pub amount: Option<Usdt>,
	pub from: Option<EventAccount>,
	pub to: Option<EventAccount>,
	pub subject_ref: String,
	pub initiator_client: String,
	pub balances: Vec<EventBalance>,
	pub hold_remaining: Option<Usdt>,
	pub counterparty_scope: String,
	pub failure_reason: String,
	/// Unix seconds.
	pub occurred_at: i64,
}

#[async_trait]
pub trait MoneyEventStore: Send + Sync {
	/// The events delivered to `client` in `scopes` past `after`, oldest first, at most
	/// `limit`.
	async fn pull(&self, client: &ClientId, scopes: &[ScopeId], after: i64, limit: u32) -> Result<Vec<MoneyEventRecord>, DomainError>;
}
