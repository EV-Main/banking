//! Users' mandates, as the user grants, revokes and lists them. What a draw on a mandate is
//! allowed is decided under the hold store's locks ([`HoldFacts::mandate`]); this port is the
//! user's side only.
//!
//! [`HoldFacts::mandate`]: super::scope_ledger::HoldFacts::mandate

use async_trait::async_trait;
use domain::{
	error::DomainError,
	scope_mandates::{Mandate, MandateTerms, MandateUsage},
	users::UserId,
};
use uuid::Uuid;

/// A mandate with what its client has drawn on the user's compartment now.
#[derive(Clone, Debug)]
pub struct RecordedMandate {
	pub mandate: Mandate,
	pub usage: MandateUsage,
}

/// What a grant did.
#[derive(Debug)]
pub enum Granted {
	/// Recorded now; any mandate it replaced was stamped in the same transaction.
	New(RecordedMandate),
	/// The user's key was already used: the mandate it recorded and the canonical terms it was
	/// recorded under. The application decides whether this is a repeat or a conflict.
	Existing { terms: String, mandate: RecordedMandate },
}

#[async_trait]
pub trait MandateStore: Send + Sync {
	/// Record `terms` as the user's current mandate for the client in the scope, replacing any
	/// current one, and deliver MANDATE_GRANTED to the client — in one transaction, under a
	/// lock on the user's key. Refuses (Validation) an `expires_at` not after the database's
	/// clock.
	async fn grant(&self, terms: &MandateTerms, idempotency_key: &str) -> Result<Granted, DomainError>;

	/// End the user's mandate `id` if it is current: stamp it revoked, void every active hold
	/// its client has on the user's compartment in the scope and deliver MANDATE_REVOKED, in
	/// one transaction. One already revoked or replaced is returned as it is; `None` when the
	/// user has no mandate `id`.
	async fn revoke(&self, user: UserId, id: Uuid) -> Result<Option<RecordedMandate>, DomainError>;

	/// The user's live mandates (neither revoked, replaced nor expired), newest first.
	async fn live(&self, user: UserId) -> Result<Vec<RecordedMandate>, DomainError>;
}
