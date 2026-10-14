//! The scope ledger's control plane — who owns which scope account, which calls went through
//! under which idempotency key, and where each client transfer stands. The balances are
//! TigerBeetle's ([`Ledger`](super::ledger::Ledger)).

use async_trait::async_trait;
use domain::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, LedgerRefusal, ScopeTransfer},
	scopes::{MoveState, ScopeAccountLabel, ScopeId},
};

use crate::ports::{compartments::ScopeRecord, ledger::LockedReads};

/// The RPC names idempotency keys are unique under, as stored.
pub const RPC_OPEN_ACCOUNT: &str = "open_account";
pub const RPC_TRANSFER: &str = "transfer";

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

/// Why a transfer did not happen: a refusal the wire names by its reason, or anything else.
#[derive(Debug)]
pub enum TransferError {
	Refused(LedgerRefusal),
	Domain(DomainError),
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
