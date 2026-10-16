//! The control plane of one-off charges (`scope_charges`, migration 0060): the charge a client
//! made under an idempotency key, where it stands on the ledger, and the facts a charge is
//! decided on under the compartment's lock. The balances are TigerBeetle's
//! ([`Ledger`](super::ledger::Ledger)).

use async_trait::async_trait;
use domain::{
	error::DomainError,
	scope_charges::ScopeCharge,
	scope_ledger::ClientId,
	scopes::{MoveState, ScopeAccountLabel, ScopeId},
};
use uuid::Uuid;

use crate::ports::{
	compartments::ScopeRecord,
	ledger::LockedReads,
	scope_ledger::{HoldUser, MandateDraw, Recorded, RequestKey, TransferError},
};

/// The RPC name `Charge` idempotency keys are unique under, as stored.
pub const RPC_CHARGE: &str = "charge";

/// A charge as recorded, with the facts the wire echoes beside the aggregate.
#[derive(Clone, Debug)]
pub struct RecordedCharge {
	pub charge: ScopeCharge,
	/// The user as the client named them.
	pub auth_subject: String,
	/// `Pending` until the relay posts it; `Failed` when the ledger refused it.
	pub state: MoveState,
	/// Unix seconds.
	pub created_at: i64,
}

/// What the control plane says about a new charge, read under the key's lock and the
/// compartment's write lock.
#[derive(Clone, Debug)]
pub struct ChargeFacts {
	/// The scope as it stands now; `None` when banking does not know it.
	pub scope: Option<ScopeRecord>,
	/// The user the client named; `None` when banking has no such user.
	pub user: Option<HoldUser>,
	/// Who owns the account to credit; `None` when it does not exist.
	pub destination_owner: Option<ClientId>,
	/// The user's current mandate for the client in the scope, its row locked, with what the
	/// client has drawn under the compartment's lock; `None` when there is none.
	pub mandate: Option<MandateDraw>,
	/// The database's clock, unix seconds.
	pub now: i64,
}

/// Decides a new charge under the store's locks and returns it — or refuses. The
/// compartment's spendable is read inside, through `reads`.
#[async_trait]
pub trait ChargeAdmission: Send + Sync {
	async fn admit(&self, facts: &ChargeFacts, reads: &mut dyn LockedReads) -> Result<ScopeCharge, TransferError>;
}

#[async_trait]
pub trait ChargeStore: Send + Sync {
	/// The charge `id` if `client` made it in `scope`; `None` for anyone else's, as for one that
	/// does not exist.
	async fn charge(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<Option<RecordedCharge>, DomainError>;

	/// Record a charge of `auth_subject`'s compartment crediting `to`. Under a lock on the key
	/// it answers a key already used with what that call recorded; otherwise, under the
	/// compartment's write lock and the mandate row's, it asks `admission` and records the
	/// charge it returns, with its event, in one transaction.
	async fn record_charge(
		&self,
		key: RequestKey<'_>,
		terms: &str,
		auth_subject: &str,
		to: &ScopeAccountLabel,
		admission: &dyn ChargeAdmission,
	) -> Result<Recorded<RecordedCharge>, TransferError>;
}
