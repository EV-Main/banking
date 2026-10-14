//! The scope registry and users' compartment moves — the control-plane half of
//! `CompartmentService`. The balances are TigerBeetle's ([`Ledger`](super::ledger::Ledger));
//! this port knows which scopes exist, what their policies say, and which moves were
//! recorded under which idempotency key.

use async_trait::async_trait;
use domain::{
	error::DomainError,
	money::Usdt,
	scopes::{CompartmentMove, ScopeId, ScopePolicy, ScopeState},
	users::UserId,
};

/// One scope banking knows, with its policy.
#[derive(Clone, Debug)]
pub struct ScopeRecord {
	pub id: ScopeId,
	pub state: ScopeState,
	pub display_name: String,
	pub policy: ScopePolicy,
	/// When the policy last changed, unix seconds.
	pub policy_updated_at: i64,
}

/// A move as it was recorded, with the moment it was.
#[derive(Clone, Debug)]
pub struct RecordedMove {
	pub movement: CompartmentMove,
	/// Unix seconds.
	pub created_at: i64,
}

/// What [`CompartmentStore::record_move`] did with a move.
#[derive(Debug)]
pub enum MoveOutcome {
	/// Recorded now; its event is on the outbox.
	Recorded(RecordedMove),
	/// The key was already taken by the user — the original, whatever its terms; the
	/// caller decides whether it is a repeat or a conflict.
	Existing(RecordedMove),
}

#[async_trait]
pub trait CompartmentStore: Send + Sync {
	/// Every scope banking knows, `main` excluded, ordered by id.
	async fn scopes(&self) -> Result<Vec<ScopeRecord>, DomainError>;

	/// One scope, or `None` when banking does not know it.
	async fn scope(&self, id: &ScopeId) -> Result<Option<ScopeRecord>, DomainError>;

	/// The move the user recorded under `idempotency_key`, if any.
	async fn find_move(&self, user: UserId, idempotency_key: &str) -> Result<Option<RecordedMove>, DomainError>;

	/// What the user moved into `scope` over the last 24 hours.
	async fn moved_in_24h(&self, user: UserId, scope: &ScopeId) -> Result<Usdt, DomainError>;

	/// Record the move and drain its event to the outbox in one transaction, serialized on
	/// the source claim's write lock with every other flow that spends it. A key the user
	/// already used is not recorded twice: the stored move comes back as
	/// [`MoveOutcome::Existing`].
	async fn record_move(&self, movement: &mut CompartmentMove, idempotency_key: &str) -> Result<MoveOutcome, DomainError>;
}
