//! Persistence + read port for the [`Subscription`] aggregate.
//!
//! A subscription is an immutable mint record, so the write side is a single `open`
//! (no state transitions). `open` takes the shared per-user claim lock, writes the
//! subscription row, and drains its `Subscribed` event — all in one transaction. The
//! relay then posts the cash move + unit mint (Write-Last) and, only after the cash leg
//! lands, the `fund_positions` cost-basis projection — so a parked cash leg can never
//! strand a phantom basis.

use async_trait::async_trait;
use domain::{
	architecture::Repository,
	error::DomainError,
	subscriptions::{Subscription, SubscriptionId},
};

use crate::ports::ledger::SpendGuard;

#[async_trait]
pub trait SubscriptionRepository: Repository<Aggregate = Subscription> {
	/// Persist a brand-new subscription, drain its `Subscribed` event, and upsert the
	/// user's `fund_positions` cost basis / high-water mark — atomically. `guard` runs under
	/// the user's claim lock, before anything is written.
	async fn open(&self, subscription: &mut Subscription, guard: &dyn SpendGuard) -> Result<(), DomainError>;

	/// Load a subscription by id (no lock; for queries) — a caller that derives the id
	/// from its input asks this whether its mint already exists.
	async fn find_by_id(&self, id: SubscriptionId) -> Result<Option<Subscription>, DomainError>;
}
