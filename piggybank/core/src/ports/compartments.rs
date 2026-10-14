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

use crate::ports::{ledger::LockedReads, outflow::PayoutStanding};

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

/// What the control plane says about a move, read by the store under the locks the move is
/// about to be recorded under and on the connection that holds them — so two moves cannot
/// both count the same room, and the check never waits on the pool while holding a lock.
/// What the source may still spend is the ledger's answer
/// ([`Ledger::spendable_locked`]), asked through the [`LockedReads`] handed in beside this.
///
/// [`Ledger::spendable_locked`]: crate::ports::ledger::Ledger::spendable_locked
#[derive(Clone, Debug, Default)]
pub struct InFlight {
	/// Σ the user's `pending` moves INTO the target compartment.
	pub pending_into_target: Usdt,
	/// Σ the user's moves into the move's scope over the last 24 h, `pending` and `posted`.
	pub moved_in_24h: Usdt,
	/// The move's scope as it stands now; read for a move in only, `None` otherwise or when
	/// banking does not know it.
	pub scope: Option<ScopeRecord>,
	/// The user's freeze and KYC level now; read for a move in only.
	pub standing: Option<PayoutStanding>,
}

/// The rules a move must pass, asked by the store while it holds the locks. Everything the
/// ledger says is read inside [`Self::admit`], after the locks are taken, through `reads`.
#[async_trait]
pub trait MoveAdmission: Send + Sync {
	async fn admit(&self, in_flight: &InFlight, reads: &mut dyn LockedReads) -> Result<(), MoveError>;
}

#[async_trait]
pub trait CompartmentStore: Send + Sync {
	/// Every scope banking knows, `main` excluded, ordered by id. May be up to half a minute
	/// stale — for listings and totals; a decision reads [`Self::scope`].
	async fn scopes(&self) -> Result<Vec<ScopeRecord>, DomainError>;

	/// One scope, or `None` when banking does not know it.
	async fn scope(&self, id: &ScopeId) -> Result<Option<ScopeRecord>, DomainError>;

	/// The move the user recorded under `idempotency_key`, if any.
	async fn find_move(&self, user: UserId, idempotency_key: &str) -> Result<Option<RecordedMove>, DomainError>;

	/// Record the move and drain its event to the outbox in one transaction. Under a lock on
	/// the user's key it answers a key already used with the stored move
	/// ([`MoveOutcome::Existing`]); otherwise, under the source compartment's write lock —
	/// and, for a move in, the target's — it asks `admission` with the in-flight sums and
	/// records only on its yes. A key taken by a racer past the lock is `Conflict`.
	async fn record_move(&self, movement: &mut CompartmentMove, idempotency_key: &str, admission: &dyn MoveAdmission) -> Result<MoveOutcome, MoveError>;
}
