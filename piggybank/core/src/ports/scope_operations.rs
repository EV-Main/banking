//! The control plane of payouts out of scope accounts and of the operations that wait on an
//! approval (`scope_payouts`, `scope_operations`, migration 0057), and the approvals seam to
//! concierge those operations are decided over. The balances are TigerBeetle's.

use async_trait::async_trait;
use domain::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, ScopeTransfer},
	scope_operations::{ApprovalField, KindSpec, PendingOperation, ScopePayout},
	scopes::{MoveState, ScopeAccountLabel, ScopeId},
	users::UserId,
};
use uuid::Uuid;

use crate::ports::{
	compartments::ScopeRecord,
	ledger::LockedReads,
	scope_ledger::{HoldUser, Recorded, RequestKey, TransferError},
};

pub const RPC_PAYOUT: &str = "payout";
pub const RPC_REQUEST_PAYOUT: &str = "request_payout";
pub const RPC_REQUEST_TRANSFER: &str = "request_transfer";

/// A payout as recorded, with where it stands now.
#[derive(Clone, Debug)]
pub struct RecordedPayout {
	pub payout: ScopePayout,
	pub state: MoveState,
	/// Why the ledger refused it; `Some` exactly when `state` is `Failed`.
	pub failure_reason: Option<String>,
	/// Unix seconds.
	pub created_at: i64,
}

/// A user a payout line names, as banking resolved them.
#[derive(Clone, Debug)]
pub struct ResolvedUser {
	pub auth_subject: String,
	pub user: HoldUser,
}

/// What the control plane says about a payout or a requested operation, read under the key's
/// lock, the scope's budget lock and the source account's claim lock.
#[derive(Clone, Debug)]
pub struct OutflowFacts {
	/// The scope as it stands now; `None` when banking does not know it.
	pub scope: Option<ScopeRecord>,
	/// Who owns the source account; `None` when it does not exist.
	pub source_owner: Option<ClientId>,
	/// Every user the request names that banking knows, by `auth_subject`. A name missing
	/// here is a user banking has no wallet for.
	pub users: Vec<ResolvedUser>,
	/// Σ the client's automatic payouts in the scope over the last 24 h, `pending` and
	/// `posted`.
	pub paid_24h: Usdt,
	/// Σ what left the scope's accounts over the last 24 h without the platform owners: every
	/// client's automatic payouts, plus the scope-level payout decisions still live or
	/// executed in that window (one still unopened counts — its level is not known yet).
	pub scope_out_24h: Usdt,
	/// A requested transfer: who owns the destination (`None` when it does not exist).
	pub destination_owner: Option<ClientId>,
	/// A requested transfer into another scope: that scope as it stands now, held `FOR
	/// SHARE`; `None` within one scope or when banking does not know it.
	pub destination_scope: Option<ScopeRecord>,
	/// The caller's operations in the scope still awaiting their approval.
	pub live_operations: u32,
}

/// Decides a payout under the store's locks and returns it, or refuses.
#[async_trait]
pub trait PayoutAdmission: Send + Sync {
	async fn admit(&self, facts: &OutflowFacts, reads: &mut dyn LockedReads) -> Result<ScopePayout, TransferError>;
}

/// What a requested operation is stored with beyond the aggregate: the digests the approval
/// carries, taken over the stored record when it was built.
#[derive(Clone, Debug)]
pub struct AdmittedOperation {
	pub operation: PendingOperation,
	pub lines_digest: Option<String>,
	pub payload_hash: String,
}

/// Decides a requested operation under the store's locks and returns it, or refuses.
#[async_trait]
pub trait OperationAdmission: Send + Sync {
	/// The source's spendable is read inside, through `reads`, under its claim lock.
	async fn admit(&self, facts: &OutflowFacts, reads: &mut dyn LockedReads) -> Result<AdmittedOperation, TransferError>;
}

/// The level concierge resolved an approval to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalLevel {
	Platform,
	Scope,
}

impl ApprovalLevel {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Platform => "platform",
			Self::Scope => "scope",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"platform" => Ok(Self::Platform),
			"scope" => Ok(Self::Scope),
			other => Err(DomainError::Repository(format!("unknown approval level: {other}"))),
		}
	}
}

/// An operation as recorded, with the facts the wire and the executor need beside it.
#[derive(Clone, Debug)]
pub struct RecordedOperation {
	pub operation: PendingOperation,
	pub lines_digest: Option<String>,
	pub payload_hash: String,
	pub approval_level: Option<ApprovalLevel>,
	/// Unix seconds.
	pub created_at: i64,
	/// Unix seconds; `None` while awaiting.
	pub decided_at: Option<i64>,
	/// How many times the executor put it off: the backoff after concierge did not answer.
	pub attempts: u32,
}

/// How an approved operation was carried out, decided under the locks of its execution.
#[derive(Debug)]
pub enum Execution {
	Payout(ScopePayout),
	Transfer(ScopeTransfer),
	/// It cannot be carried out now (`insufficient_funds`, `scope_frozen`, …): FAILED, for
	/// good.
	Fail(String),
}

/// What the control plane says about an approved operation at the moment it executes, read
/// with the operation's row locked, the scopes held `FOR SHARE` and the source's claim lock.
#[derive(Clone, Debug)]
pub struct ExecutionFacts {
	pub scope: Option<ScopeRecord>,
	pub source_owner: Option<ClientId>,
	/// A transfer's destination owner, in its own scope.
	pub destination_owner: Option<ClientId>,
	/// A transfer into another scope: that scope as it stands now, held `FOR SHARE`.
	pub destination_scope: Option<ScopeRecord>,
	/// A payout's users who are frozen or disabled now, their rows held `FOR SHARE`.
	pub blocked_users: Vec<UserId>,
}

/// Turns a locked, still-awaiting operation into its execution.
#[async_trait]
pub trait OperationExecution: Send + Sync {
	async fn execute(&self, operation: &PendingOperation, facts: &ExecutionFacts, reads: &mut dyn LockedReads) -> Result<Execution, TransferError>;
}

/// How an operation leaves `awaiting_approval` without being executed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationEnd {
	Rejected,
	Failed,
}

#[async_trait]
pub trait PayoutStore: Send + Sync {
	/// The payout `id` if `client` made it in `scope`; `None` for anyone else's, as for one
	/// that does not exist.
	async fn payout(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<Option<RecordedPayout>, DomainError>;

	/// Record a client's automatic payout and its chain's event in one transaction. Under the
	/// key's lock it answers a key already used; otherwise, under the scope's budget lock, the
	/// scope row `FOR SHARE` and the source's claim lock, it asks `admission` with the facts
	/// and records only on its yes.
	async fn record_payout(
		&self,
		key: RequestKey<'_>,
		terms: &str,
		from: &ScopeAccountLabel,
		subjects: &[String],
		admission: &dyn PayoutAdmission,
	) -> Result<Recorded<RecordedPayout>, TransferError>;
}

#[async_trait]
pub trait OperationStore: Send + Sync {
	/// The operation `id` if `client` requested it in `scope`; anyone else's is `None`.
	async fn operation(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<Option<RecordedOperation>, DomainError>;

	/// The operation `id`, whoever requested it — the executor's read.
	async fn operation_by_id(&self, id: Uuid) -> Result<Option<RecordedOperation>, DomainError>;

	/// Record a requested operation. Under the key's lock it answers a key already used;
	/// otherwise, under the scope's budget lock and the scope row `FOR SHARE`, it asks
	/// `admission` and stores what it returns. `to` names a transfer's destination, `None`
	/// for a payout.
	async fn record_operation(
		&self,
		key: RequestKey<'_>,
		terms: &str,
		from: &ScopeAccountLabel,
		to: Option<(&ScopeId, &ScopeAccountLabel)>,
		subjects: &[String],
		admission: &dyn OperationAdmission,
	) -> Result<Recorded<RecordedOperation>, TransferError>;

	/// Awaiting operations whose approval is not opened yet and whose turn has come, oldest
	/// first.
	async fn due_to_propose(&self, limit: u32) -> Result<Vec<RecordedOperation>, DomainError>;

	/// Awaiting operations with an approval whose turn to be asked about has come.
	async fn due_to_check(&self, limit: u32) -> Result<Vec<RecordedOperation>, DomainError>;

	/// The approval `approval_id` (resolved to `level`; `None` when concierge did not say) was
	/// opened for the operation: record it and announce the request in the client's money
	/// events. A repeat with the same id is a no-op; a different id than one already recorded
	/// is a `Conflict`.
	async fn opened(&self, id: Uuid, approval_id: &str, level: Option<ApprovalLevel>) -> Result<(), DomainError>;

	/// Try again no sooner than `delay_secs` from now; counts one more attempt.
	async fn postpone(&self, id: Uuid, delay_secs: u32) -> Result<(), DomainError>;

	/// End, REJECTED with `reason`, every operation whose approval was never opened and that
	/// was asked for more than `max_age_secs` ago, telling each client. Returns how many.
	async fn expire_unopened(&self, max_age_secs: u32, reason: &str) -> Result<usize, DomainError>;

	/// End an awaiting operation unexecuted, with `reason`, and tell its client. `false` when
	/// it had already left `awaiting_approval`.
	async fn close(&self, id: Uuid, end: OperationEnd, reason: &str) -> Result<bool, DomainError>;

	/// Carry out an awaiting operation, once: under its row lock `FOR UPDATE` it asks
	/// `execution`, records the payout or transfer with its event, and marks it `executed` —
	/// or `failed` with the reason. An operation no longer awaiting is returned as it stands,
	/// with nothing done.
	async fn execute(&self, id: Uuid, execution: &dyn OperationExecution) -> Result<RecordedOperation, TransferError>;
}

/// Where a concierge approval stands (`ApprovalState`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalState {
	Open,
	Passed,
	Executed,
	ExecutionFailed,
	Rejected,
	Vetoed,
	Expired,
	Cancelled,
	Void,
}

impl ApprovalState {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Open => "open",
			Self::Passed => "passed",
			Self::Executed => "executed",
			Self::ExecutionFailed => "execution_failed",
			Self::Rejected => "rejected",
			Self::Vetoed => "vetoed",
			Self::Expired => "expired",
			Self::Cancelled => "cancelled",
			Self::Void => "void",
		}
	}
}

/// One approval as the executing service is shown it.
#[derive(Clone, Debug)]
pub struct ApprovalView {
	pub id: String,
	pub kind: String,
	pub scope: String,
	pub subject_ref: String,
	pub state: ApprovalState,
	pub level: Option<ApprovalLevel>,
	pub payload_hash: String,
}

/// A proposal banking opens for an operation.
#[derive(Clone, Debug)]
pub struct Proposal {
	pub kind: String,
	pub scope: String,
	pub subject_ref: String,
	/// The requester's words, shown attributed to banking.
	pub reason: String,
	pub fields: Vec<ApprovalField>,
	/// Idempotency key on concierge's side.
	pub dedupe_key: String,
}

/// How banking reports an approval it was pulled for (`ApprovalOutcome`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalOutcome {
	Executed,
	Failed,
	/// Not carried out because it no longer applies; nothing was attempted.
	Void,
}

/// Why the approvals seam did not answer.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalsError {
	/// Concierge refused the call for good (INVALID_ARGUMENT, FAILED_PRECONDITION, …): a
	/// retry with the same request fails the same way.
	#[error("refused: {0}")]
	Refused(String),
	/// Concierge could not be reached or answered with a transient failure; retry later.
	#[error("unavailable: {0}")]
	Unavailable(String),
	/// Concierge is reachable but holds too many of banking's live proposals in that scope
	/// (RESOURCE_EXHAUSTED): only this proposal waits, the others go on.
	#[error("throttled: {0}")]
	Throttled(String),
}

/// concierge's `ApprovalExecutionService`, as banking — the only proposer and executor of
/// `bank:*` kinds — uses it.
#[async_trait]
pub trait Approvals: Send + Sync {
	async fn propose(&self, proposal: &Proposal) -> Result<ApprovalView, ApprovalsError>;

	async fn get(&self, approval_id: &str) -> Result<ApprovalView, ApprovalsError>;

	/// Passed decisions of banking's namespace due for execution, oldest first.
	async fn pull_approved(&self, limit: u32) -> Result<Vec<ApprovalView>, ApprovalsError>;

	async fn report(&self, approval_id: &str, outcome: ApprovalOutcome, detail: &str) -> Result<(), ApprovalsError>;

	/// Withdraw a proposal banking opened (`WithdrawProposal`), so owners stop voting on an
	/// operation that is dead and its slot in the scope's live-proposal limit is freed.
	/// Concierge allows it only while the proposal is OPEN, or PASSED before `executable_at`.
	async fn withdraw(&self, approval_id: &str, reason: &str) -> Result<(), ApprovalsError>;

	/// Publish banking's namespace catalog (`ScopeCatalogService.PublishScopeCatalog`), whole:
	/// the same version with the same kinds is a no-op on concierge's side.
	async fn publish_catalog(&self, version: u64, kinds: &[KindSpec]) -> Result<(), ApprovalsError>;
}
