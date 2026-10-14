//! The outflow-policy port — the control-plane facts that decide whether money may
//! leave the platform *right now*.
//!
//! A **query-side** port: it reports facts and decides nothing. The policy that reads
//! them lives in [`application::withdrawals`](crate::application::withdrawals), so the
//! dispatcher sweep, the admin `DispatchWithdrawal` RPC and any future payout path share
//! one rule instead of one copy each — which is exactly how the KYC floor came to be
//! enforced at withdrawal *admission* and nowhere near the point where the money actually
//! leaves.
//!
//! Both facts are read-side only; nothing here writes. The freeze flag and the mirrored
//! KYC tier come back together ([`PayoutStanding`]) because they are columns of the same
//! row and every caller wants both — two round trips would buy nothing but a window in
//! which they disagree.

use async_trait::async_trait;
use domain::{error::DomainError, money::Network, users::UserId};

/// The control-plane facts a payout must clear.
#[async_trait]
pub trait OutflowPolicy: Send + Sync {
	/// Whether the global read-only kill-switch is engaged — "pause deposits &
	/// withdrawals". An `Err` is a control-plane failure, never a `false`.
	async fn outflows_paused(&self) -> Result<bool, DomainError>;

	/// The owner's money-out standing, or `None` when the user has no local row.
	///
	/// `None` is deliberately NOT folded into a "permitted" default here: on the dispatch
	/// path a missing row means the gate cannot be evaluated for a withdrawal that has
	/// already reserved money, and the caller is the one that turns that into a refusal.
	async fn standing(&self, user: UserId) -> Result<Option<PayoutStanding>, DomainError>;

	/// Rails an operator froze: nothing new is admitted on them and what is queued stays queued.
	async fn frozen_rails(&self) -> Result<Vec<Network>, DomainError>;
}

/// One owner's money-out standing, as the cross-plane bridge mirrors it.
#[derive(Clone, Copy, Debug)]
pub struct PayoutStanding {
	/// Blocked from moving money by EITHER a concierge `SUSPENDED` (mirrored onto
	/// `frozen`) OR a banking-side `DisableUser` (`status = 'disabled'`) — the same fold
	/// token issuance and the sync RPC boundary apply.
	pub blocked: bool,
	/// The mirrored concierge KYC tier. Compare against
	/// [`KYC_LEVEL_VERIFIED`](domain::users::KYC_LEVEL_VERIFIED) rather than a literal.
	pub kyc_level: u32,
}
