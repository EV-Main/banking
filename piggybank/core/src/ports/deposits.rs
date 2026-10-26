//! The deposit-log port — the control-plane record behind every on-chain arrival, always
//! a person's (no fund-owned party since #245).
//!
//! These are the aggregate-less standalone [`LedgerEvent`](domain::balance::LedgerEvent)
//! facts (see [`domain::balance`]) — there is no aggregate to hang a `Repository`
//! marker on, so this is a plain driven port. Each
//! method is its own atomic unit: the fact row (where one exists) and its outbox
//! event commit in one Postgres transaction; the relay moves the money afterwards
//! (Write-Last).

use async_trait::async_trait;
use domain::{
	error::DomainError,
	money::{Network, TxRef, Usdt},
	users::UserId,
};

#[async_trait]
pub trait Deposits: Send + Sync {
	/// Record an on-chain deposit, **idempotent by `tx_ref`**: the unique gate makes
	/// a second record of the same chain tx impossible, so the credit happens at most
	/// once even under concurrent recorders. Returns `true` if newly recorded,
	/// `false` for a duplicate.
	async fn record(&self, tx_ref: TxRef, user: UserId, network: Network, amount: Usdt) -> Result<bool, DomainError>;

	/// The caller's credited on-chain deposits, newest first — a projection read of
	/// the idempotency-gate rows where `party_kind = 'user'`.
	async fn list_by_user(&self, user: UserId) -> Result<Vec<DepositRecord>, DomainError>;

	/// Whether `tx_ref` has already passed the gate — under any party. The gate itself, not
	/// this read, is what keeps a double credit impossible.
	async fn is_recorded(&self, tx_ref: &TxRef) -> Result<bool, DomainError>;

	/// Does `user` still have a credited deposit on `network` that no sweep cycle has
	/// observed drained?
	///
	/// This is the hub's answer to "can that deposit address still be holding money", and it
	/// is deliberately the SAME predicate the sweeper itself scans on (`swept_at IS NULL`)
	/// rather than a second opinion: if the sweeper thinks an address may hold funds, so must
	/// anything about to retire it, and the two can never disagree because there is only one
	/// rule. `true` is "not safe to retire".
	///
	/// It is bookkeeping, not a chain read, and the difference matters in one direction only.
	/// `swept_at` is stamped when a cycle sees the address drained **below the sweep
	/// minimum**, so `false` means "nothing the fund is owed is left there", not "the balance
	/// is exactly zero" — dust under the sweep floor can remain, and is knowingly abandoned
	/// (moving it costs more gas than it is worth). What `false` does NOT cover is an arrival
	/// the watcher has not credited yet; that residual is why the signer archives the old key
	/// instead of destroying it.
	async fn has_unswept(&self, user: UserId, network: Network) -> Result<bool, DomainError>;
}
/// A credited on-chain deposit, read back from the idempotency-gate rows — a
/// projection read model, not an aggregate.
pub struct DepositRecord {
	pub tx_ref: TxRef,
	pub network: Network,
	pub amount: Usdt,
	/// Unix seconds the hub recorded the credit.
	pub created_at: i64,
}
