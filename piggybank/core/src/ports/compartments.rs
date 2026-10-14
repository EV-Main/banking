//! The scope registry and users' compartment moves — the control-plane half of
//! `CompartmentService`. The balances are TigerBeetle's ([`Ledger`](super::ledger::Ledger));
//! this port knows which scopes exist, what their policies say, and which moves were
//! recorded, under which idempotency key, and where each one stands.

use async_trait::async_trait;
use domain::{
	error::DomainError,
	money::Usdt,
	scopes::{CompartmentMove, MoveRefusal, MoveState, ScopeId, ScopePolicy, ScopeState},
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

/// A move as it was recorded, with where it stands now.
#[derive(Clone, Debug)]
pub struct RecordedMove {
	pub movement: CompartmentMove,
	pub state: MoveState,
	/// Why the ledger refused it; `Some` exactly when `state` is `Failed`.
	pub failure_reason: Option<String>,
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

/// Why a move did not happen: a refusal the wire names by its reason, or anything else.
#[derive(Debug)]
pub enum MoveError {
	Refused(MoveRefusal),
	Domain(DomainError),
}

impl From<DomainError> for MoveError {
	fn from(err: DomainError) -> Self {
		Self::Domain(err)
	}
}

/// What the control plane knows about a user's moves in flight, read under the locks the
/// move is about to be recorded under — so two moves cannot both count the same money.
#[derive(Clone, Copy, Debug)]
pub struct InFlight {
	/// Σ the user's `pending` moves OUT of the source compartment: already promised, not yet
	/// off the ledger.
	pub pending_out_of_source: Usdt,
	/// Σ the user's `pending` moves INTO the target compartment.
	pub pending_into_target: Usdt,
	/// Σ the user's moves into the move's scope over the last 24 h, `pending` and `posted`.
	pub moved_in_24h: Usdt,
}

impl Default for InFlight {
	fn default() -> Self {
		Self {
			pending_out_of_source: Usdt::ZERO,
			pending_into_target: Usdt::ZERO,
			moved_in_24h: Usdt::ZERO,
		}
	}
}

/// The rules a move must pass, asked by the store while it holds the locks. Everything the
/// ledger says is read inside [`Self::admit`], after the locks are taken.
#[async_trait]
pub trait MoveAdmission: Send + Sync {
	async fn admit(&self, in_flight: &InFlight) -> Result<(), MoveError>;
}

#[async_trait]
pub trait CompartmentStore: Send + Sync {
	/// Every scope banking knows, `main` excluded, ordered by id.
	async fn scopes(&self) -> Result<Vec<ScopeRecord>, DomainError>;

	/// One scope, or `None` when banking does not know it.
	async fn scope(&self, id: &ScopeId) -> Result<Option<ScopeRecord>, DomainError>;

	/// The move the user recorded under `idempotency_key`, if any.
	async fn find_move(&self, user: UserId, idempotency_key: &str) -> Result<Option<RecordedMove>, DomainError>;

	/// Record the move and drain its event to the outbox in one transaction. Under the
	/// source compartment's write lock — and, for a move in, the target's — it answers a
	/// key the user already used with the stored move ([`MoveOutcome::Existing`]),
	/// otherwise asks `admission` with the in-flight sums and records only on its yes.
	async fn record_move(&self, movement: &mut CompartmentMove, idempotency_key: &str, admission: &dyn MoveAdmission) -> Result<MoveOutcome, MoveError>;
}
