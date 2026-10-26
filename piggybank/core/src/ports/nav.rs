//! The fund-valuation (NAV) port — the operator-posted AUM marks a fund's NAV is
//! derived from.
//!
//! NAV per share = `AUM / units_outstanding`, derived at post time and frozen until the
//! next mark. This port stores the append-only marks and returns the latest (the
//! "current" price subscribe/redeem deal on). Plain control-plane reads/writes — a
//! valuation is not an aggregate, so no `Repository` marker (and no "Repository" in
//! the name, which the kernel reserves for aggregate persistence).

use async_trait::async_trait;
use domain::{
	balance::{ServiceId, ValuationId},
	error::DomainError,
	money::{Nav, Shares, Usdt},
};

#[async_trait]
pub trait NavMarks: Send + Sync {
	/// The latest mark for `service`, or `None` if the fund has never been valued.
	async fn current(&self, service: &ServiceId) -> Result<Option<Valuation>, DomainError>;

	/// The mark the rolling move window is measured against: the LATEST mark with
	/// `posted_at ≤ at_unix`, or — when no mark is that old yet — the fund's EARLIEST mark.
	/// `None` only for a fund that has never been valued. See
	/// [`NAV_MOVE_WINDOW_SECS`](crate::application::funds::NAV_MOVE_WINDOW_SECS).
	async fn anchor(&self, service: &ServiceId, at_unix: i64) -> Result<Option<Valuation>, DomainError>;

	/// Whether `subject` posted a mark for `service` strictly after `since_unix` — the
	/// redeem cooldown's question. `subject` is the `posted_by` string as recorded.
	async fn posted_by_since(&self, service: &ServiceId, subject: &str, since_unix: i64) -> Result<bool, DomainError>;

	/// The marks with `from_unix ≤ posted_at ≤ to_unix`, OLDEST first, at most `limit` of
	/// them — and when more fell in the window, the NEWEST `limit` (a chart that has to
	/// drop something drops the deep past, never the current price).
	async fn history(&self, service: &ServiceId, from_unix: i64, to_unix: i64, limit: usize) -> Result<Vec<Valuation>, DomainError>;

	/// Append a new mark — `id` is caller-minted, `posted_at` is DB-stamped. Returns the
	/// stamped `posted_at` (unix seconds) so the caller can report the recorded mark.
	async fn record(&self, id: ValuationId, service: &ServiceId, aum: Usdt, units_outstanding: Shares, nav: Nav, posted_by: &str) -> Result<i64, DomainError>;
}
/// One operator valuation mark for a fund. NAV is derived (`aum / units_outstanding`)
/// and frozen until the next mark; `posted_at_unix` is the age seam for the staleness
/// guard, `posted_by` the operator subject (the trust seam).
#[derive(Clone, Debug)]
pub struct Valuation {
	pub service: ServiceId,
	pub aum: Usdt,
	pub units_outstanding: Shares,
	pub nav: Nav,
	pub posted_by: String,
	pub posted_at_unix: i64,
}
