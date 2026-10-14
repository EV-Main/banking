//! Persistence + read port for the [`Withdrawal`] aggregate.
//!
//! Mirrors [`RedemptionRepository`](super::RedemptionRepository): each command is
//! internally atomic and row-locked — load `FOR UPDATE`, apply the aggregate command
//! inside the lock (the aggregate is the single authority on a transition's
//! validity), then persist the transition together with the drained events
//! (`event_log` + `outbox`) in one transaction. The relay then reserves/settles/voids
//! the money in TigerBeetle (Write-Last).

use async_trait::async_trait;
use domain::{
	architecture::{Reader, Repository},
	error::DomainError,
	money::{Network, TxRef, Usdt},
	users::UserId,
	withdrawals::{Withdrawal, WithdrawalId, WithdrawalSource},
};

use crate::ports::ledger::SpendGuard;

#[async_trait]
pub trait WithdrawalRepository: Repository<Aggregate = Withdrawal> + Reader<Aggregate = Withdrawal> {
	/// Persist a brand-new withdrawal and drain its events (`Requested`, plus
	/// `Dispatched` when the rail was liquid at request time) atomically — the relay
	/// reserves the gross against the user's claim into clearing, then broadcasts. `guard`
	/// runs under the source claim's lock, before anything is written.
	async fn open(&self, withdrawal: &mut Withdrawal, guard: &dyn SpendGuard) -> Result<(), DomainError>;

	/// Dispatch a queued withdrawal whose rail now has liquidity (the treasury worker):
	/// apply [`Withdrawal::dispatch`] under the row lock, persist + drain the
	/// `Dispatched` event (the relay broadcasts). Idempotent; returns the aggregate.
	async fn dispatch(&self, id: WithdrawalId) -> Result<Withdrawal, DomainError>;

	/// Settle a processing withdrawal (it has the required confirmations): apply
	/// [`Withdrawal::settle`] under the row lock, persist + drain the `Settled` event
	/// (the relay posts the reservation and moves the net out of custody). Idempotent;
	/// returns the current aggregate.
	async fn settle(&self, id: WithdrawalId, tx_ref: TxRef) -> Result<Withdrawal, DomainError>;

	/// Fail a processing withdrawal (it never reached the chain): apply
	/// [`Withdrawal::fail`] under the row lock, persist + drain the `Failed` event (the
	/// relay voids the reservation, refunding the user). Idempotent.
	async fn fail(&self, id: WithdrawalId) -> Result<Withdrawal, DomainError>;

	/// Cancel a still-queued withdrawal (user or admin): apply [`Withdrawal::cancel`]
	/// under the row lock, persist + drain the `Cancelled` event (the relay voids the
	/// reservation, refunding the user). Always safe — nothing was broadcast. Idempotent.
	async fn cancel(&self, id: WithdrawalId) -> Result<Withdrawal, DomainError>;

	/// Load a withdrawal by id (no lock; for queries).
	async fn find_by_id(&self, id: WithdrawalId) -> Result<Option<Withdrawal>, DomainError>;

	/// A user's withdrawals (projection), newest first.
	async fn list_by_user(&self, user: UserId) -> Result<Vec<Withdrawal>, DomainError>;

	/// The cross-user queue of withdrawals awaiting operator action, oldest first —
	/// the admin Withdrawals screen's clear-the-queue surface.
	async fn list_actionable(&self) -> Result<Vec<QueuedWithdrawal>, DomainError>;
}
/// A lightweight cross-user row for the operator withdrawal queue: a withdrawal
/// awaiting action — `queued` (awaiting liquidity/dispatch) or `processing`
/// (broadcast in flight: settle with the mined tx, fail only if nothing landed).
pub struct QueuedWithdrawal {
	pub id: WithdrawalId,
	/// Which claim funds it.
	pub source: WithdrawalSource,
	/// Mirrored identity email; empty for a user the bridge hasn't populated yet.
	pub email: String,
	pub network: Network,
	/// Destination on-chain address, as stored.
	pub address: String,
	/// Gross debited from the user.
	pub amount: Usdt,
	/// What ships on-chain (gross − fee).
	pub net_amount: Usdt,
	pub state: String,
	/// Unix seconds the withdrawal was requested.
	pub created_at: i64,
}
