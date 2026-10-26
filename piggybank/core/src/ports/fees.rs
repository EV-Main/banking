//! Persistence ports for the fee plane — policy, per-investor accrual state, the
//! charge, and the periodic conversion of accumulated fee units into cash.
//!
//! Three seams, deliberately separate:
//!
//! - [`FeePolicies`] is plain control-plane configuration keyed by `service` — terms,
//!   not money. A service with no row charges nothing.
//! - [`PositionAccruals`] is the read side of the accrual clocks living on the
//!   `fund_positions` projection. It deliberately does **not** return a unit balance:
//!   units are authoritative in TigerBeetle, and a fee capped by a projection that lags
//!   the ledger would be a fee capped by the wrong number. The application joins this
//!   with a live TB read.
//! - [`FeeAssessments`] writes a charge. Its one method is atomic across three things
//!   that must never drift apart — the audit row, the position's new debt/mark/clocks,
//!   and the outbox event the relay turns into the unit clawback.

use std::sync::Arc;

use async_trait::async_trait;
use domain::{
	architecture::Repository,
	balance::ServiceId,
	error::DomainError,
	fees::{FeeAssessment, FeePolicy, FeeSettlement, Trigger},
	money::{Nav, Shares, Usdt},
	users::UserId,
};

/// The live row of `fee_policies`: the terms in force right now, which version of the
/// product's history they are, and since when.
#[derive(Clone, Copy, Debug)]
pub struct PolicyRecord {
	pub policy: FeePolicy,
	pub version: u32,
	pub effective_from_unix: i64,
	pub updated_at_unix: i64,
}

/// The policy port is READ-ONLY.
#[async_trait]
pub trait FeePolicies: Send + Sync {
	/// One fund's terms. `None` means the product charges no fee at all — the safe
	/// default, so a fee can never appear on a product nobody configured.
	async fn find(&self, service: &ServiceId) -> Result<Option<FeePolicy>, DomainError>;

	/// The same row with its version and timestamps — what the wire shows.
	async fn current(&self, service: &ServiceId) -> Result<Option<PolicyRecord>, DomainError>;

	/// Every configured policy, ordered by `service` — the operator console's view.
	async fn list(&self) -> Result<Vec<(ServiceId, PolicyRecord)>, DomainError>;
}

/// The control-plane half of a holding's fee state. `units` is absent on purpose — see
/// the module note.
#[derive(Clone, Debug)]
pub struct PositionAccrual {
	pub user: UserId,
	pub service: ServiceId,
	/// Net cash invested (average cost) — the invested-capital base.
	pub cost_basis: Usdt,
	/// This investor's own high-water mark.
	pub high_water_mark: Nav,
	/// Fee assessed but not yet collected.
	pub debt: Usdt,
	/// Unix seconds of the last management accrual.
	pub accrued_at_unix: i64,
	/// Unix seconds of the last performance crystallization.
	pub crystallized_at_unix: i64,
}

#[async_trait]
pub trait PositionAccruals: Send + Sync {
	/// One holding's accrual state, or `None` if the investor has no position.
	async fn find(&self, user: UserId, service: &ServiceId) -> Result<Option<PositionAccrual>, DomainError>;

	/// Unit-holding positions whose management fee has not been accrued since
	/// `accrued_before_unix`, oldest first, capped at `limit`. The sweeper's work queue:
	/// bounded per cycle so one pass can never monopolise the pool, and ordered so the
	/// longest-unaccrued holding is always served first.
	async fn due(&self, accrued_before_unix: i64, limit: i64) -> Result<Vec<PositionAccrual>, DomainError>;
}

#[async_trait]
pub trait FeeAssessments: Repository<Aggregate = FeeAssessment> {
	/// Persist one charge in a single transaction: the immutable `fee_assessments` audit
	/// row, the position's new `fee_debt` / `high_water_mark` / clocks, and the drained
	/// `Charged` event into `event_log` + `outbox`.
	///
	/// It takes the position row lock, which is the same target a redemption settle
	/// takes — so a fee assessment and a settle can never interleave and each divide a
	/// cost basis by a figure the other is about to change.
	///
	/// `expected_accrued_at_unix` is the accrual clock the caller assessed against, and
	/// the write is conditional on it still being there. `Ok(false)` means it was not:
	/// another assessor charged this holding for the same elapsed window first, so this
	/// charge is abandoned whole rather than billing the investor twice. The lock cannot
	/// substitute for this — it orders the two writes but cannot tell that the second
	/// one's inputs went stale while it waited.
	///
	/// `now_unix` is stamped onto the clocks, so the window the charge was computed for
	/// and the window recorded as charged are the same one.
	async fn charge(&self, assessment: &mut FeeAssessment, expected_accrued_at_unix: i64, now_unix: i64) -> Result<bool, DomainError>;

	/// One investor's charges, newest first — the fee statement.
	async fn list_by_user(&self, user: UserId) -> Result<Vec<AssessmentRecord>, DomainError>;

	/// Every charge against one fund, newest first — the operator's view.
	async fn list_by_service(&self, service: &ServiceId) -> Result<Vec<AssessmentRecord>, DomainError>;
}

/// An audit row: what was owed, what was actually taken, and what deferred. `due` and
/// `charged_cash` differ exactly when a holding could not cover the charge.
#[derive(Clone, Debug)]
pub struct AssessmentRecord {
	pub user: UserId,
	pub service: ServiceId,
	pub trigger: Trigger,
	pub nav: Nav,
	pub management: Usdt,
	pub performance: Usdt,
	pub debt_opening: Usdt,
	pub charged_units: Shares,
	pub charged_cash: Usdt,
	pub debt_carried: Usdt,
	pub high_water_mark: Nav,
	pub assessed_at_unix: i64,
}

#[async_trait]
pub trait FeeSettlements: Repository<Aggregate = FeeSettlement> {
	/// Record a bulk conversion of fee units into cash + its `SharesSettled` event.
	async fn settle(&self, settlement: &mut FeeSettlement, settled_by: &str) -> Result<(), DomainError>;

	/// Settlement history for one fund, newest first.
	async fn list_by_service(&self, service: &ServiceId) -> Result<Vec<SettlementRecord>, DomainError>;
}

/// One bulk conversion of accumulated fee units into fee revenue.
#[derive(Clone, Debug)]
pub struct SettlementRecord {
	pub service: ServiceId,
	pub units: Shares,
	pub nav: Nav,
	pub cash: Usdt,
	pub settled_by: String,
	pub settled_at_unix: i64,
}

/// The fee plane's four seams, handed around together because they always travel
/// together: an assessment needs the policy, the accrual clocks, and the charge, and
/// nothing outside the fee plane needs any of them. One field on the app state instead
/// of four, and one parameter instead of four on every handler.
#[derive(Clone)]
pub struct FeePorts {
	pub policies: Arc<dyn FeePolicies>,
	pub accruals: Arc<dyn PositionAccruals>,
	pub assessments: Arc<dyn FeeAssessments>,
	pub settlements: Arc<dyn FeeSettlements>,
}
