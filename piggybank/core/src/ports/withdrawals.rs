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
	money::{Network, TxRef, Usdt, WalletAddress},
	users::UserId,
	withdrawals::{Withdrawal, WithdrawalId, WithdrawalSource},
};

use crate::ports::{audit::RequestAudit, ledger::SpendGuard};

#[async_trait]
pub trait WithdrawalRepository: Repository<Aggregate = Withdrawal> + Reader<Aggregate = Withdrawal> {
	/// Persist a brand-new withdrawal and drain its events (`Requested`, plus
	/// `Dispatched` when the rail was liquid at request time) atomically — the relay
	/// reserves the gross against the user's claim into clearing, then broadcasts. `guard`
	/// runs under the source claim's lock, before anything is written.
	async fn open(&self, withdrawal: &mut Withdrawal, guard: &dyn SpendGuard) -> Result<(), DomainError>;

	/// [`open`](Self::open) for a request that carries an idempotency key (per user). Under the
	/// claim's lock, before the guard runs, a withdrawal already recorded for `(user, key)` is
	/// returned as `Some` and nothing is written — the caller compares its terms; `None` means
	/// this one was recorded.
	async fn open_keyed(&self, withdrawal: &mut Withdrawal, guard: &dyn SpendGuard, user: UserId, key: &str) -> Result<Option<Withdrawal>, DomainError>;

	/// [`open`](Self::open) / [`open_keyed`](Self::open_keyed) for the self-service wallet: under
	/// the same claim lock and in the same transaction, the key replay, the spend guard, the
	/// profile's velocity limits over the user's recent withdrawals, and the audit columns.
	/// `Replayed` carries the first withdrawal under `key` and records nothing.
	async fn open_screened(&self, withdrawal: &mut Withdrawal, guard: &dyn SpendGuard, key: Option<&str>, screening: &Screening<'_>) -> Result<Opened, DomainError>;

	/// Where `address` stands in the user's address book, read before the claim lock to decide
	/// whether the withdrawal may dispatch at once. [`open_screened`](Self::open_screened) asks
	/// again under the lock and refuses to record a dispatched withdrawal the answer has since
	/// turned into a held one.
	async fn address_hold(&self, user: UserId, network: Network, address: &WalletAddress) -> Result<AddressHold, DomainError>;

	/// The user's withdrawal recorded under `key`, if any.
	async fn find_by_key(&self, user: UserId, key: &str) -> Result<Option<Withdrawal>, DomainError>;

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
/// What [`WithdrawalRepository::open_screened`] did.
#[derive(Debug)]
pub enum Opened {
	/// Recorded. `new_address` is true when this request put the destination into the book.
	Recorded {
		new_address: bool,
	},
	Replayed(Withdrawal),
}

/// A destination's standing in the user's address book.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressHold {
	/// Never withdrawn to: seen for the first time by this request.
	New,
	/// In the book, but its release time is still ahead.
	Held,
	/// In the book and released: money may go there at once.
	Released,
}

/// The velocity limits one request is judged against: the per-operation ceiling, the ceiling
/// over the user's last 24 hours, and an optional cap on how many in the last hour.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LimitProfile {
	pub per_op: Usdt,
	pub per_24h: Usdt,
	pub max_per_hour: Option<u32>,
}

/// What a user has already requested: the gross of every non-failed, non-cancelled withdrawal
/// of the last 24 hours, and how many of them were made in the last hour.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Usage {
	pub gross_24h: Usdt,
	pub count_1h: u32,
}

impl LimitProfile {
	/// The per-operation ceiling alone, cheap enough to run before any lock.
	pub fn check_op(&self, amount: Usdt) -> Result<(), DomainError> {
		if amount > self.per_op {
			return Err(DomainError::Precondition(format!(
				"a withdrawal is limited to {} USDT per operation",
				self.per_op.to_decimal_string()
			)));
		}
		Ok(())
	}

	/// All three limits for a withdrawal of `amount` on top of `usage`. A limit the state can
	/// lift by waiting is a `Precondition` (FAILED_PRECONDITION), like the pause and the freeze.
	pub fn check(&self, amount: Usdt, usage: Usage) -> Result<(), DomainError> {
		self.check_op(amount)?;
		// An overflowing sum is over any limit.
		let within_day = usage.gross_24h.checked_add(amount).is_some_and(|total| total <= self.per_24h);
		if !within_day {
			return Err(DomainError::Precondition(format!(
				"withdrawals are limited to {} USDT per 24 hours",
				self.per_24h.to_decimal_string()
			)));
		}
		if let Some(max) = self.max_per_hour
			&& usage.count_1h >= max
		{
			return Err(DomainError::Precondition(format!("at most {max} withdrawals per hour are allowed")));
		}
		Ok(())
	}
}

/// How a self-service request is screened and what is recorded with it.
pub struct Screening<'a> {
	pub profile: &'a LimitProfile,
	pub audit: &'a RequestAudit,
	/// Seconds a first-seen destination is held; 0 = no hold.
	pub hold_secs: u64,
	/// Seconds after a sign-in change during which a first-seen destination is refused; 0 = off.
	pub cooloff_secs: u64,
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
