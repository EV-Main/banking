//! Fee sweeper — the worker that actually charges "2 and 20".
//!
//! Management fees accrue with the clock, so somebody has to wake up and collect them.
//! This is that somebody: every [`SWEEP_INTERVAL`] it takes the positions whose
//! management fee has not been accrued for at least [`MIN_ACCRUAL_AGE`] and assesses
//! each one. The performance leg rides along and crystallizes only when its own period
//! has elapsed, so a fund on annual crystallization is charged its 20% once a year even
//! though the sweeper visited the position daily.
//!
//! ## Why a minimum age, and why a batch cap
//!
//! Charging on every tick would be correct but wasteful — each charge is a Postgres
//! transaction plus a TigerBeetle transfer, and a fee accrued over a minute is
//! sub-base-unit dust that floors to nothing anyway. [`MIN_ACCRUAL_AGE`] makes the
//! cadence daily; the accrual itself is exact regardless, because it is a function of
//! elapsed seconds and not of how often this job runs. Miss a week and the next sweep
//! bills that week — no drift, no double-charge.
//!
//! The per-cycle [`BATCH`] cap bounds the work one pass can do so a fund with a hundred
//! thousand holders cannot monopolise the pool; the ordering (`oldest accrual first`)
//! makes the backlog drain fairly rather than starving whoever sorts last.
//!
//! ## What it will not do
//!
//! A per-position failure warns and moves on: one investor whose fund has a stale NAV
//! must not stop every other investor's fee from being collected. And a fee is never
//! forced — if the holder's units are locked or escrowed, the charge is recorded and
//! carried as debt rather than drawn from units the holder cannot spare; only a charge
//! that is not owed at all (it floors to nothing) persists nothing, and then the accrual
//! simply continues (see [`domain::fees::FeeCharge::is_empty`]).
//!
//! ## The second clock: promoting a change of terms
//!
//! A scheduled [`FeePolicyChange`](crate::ports::fees::FeePolicyChange) becomes the live
//! policy at its `effective_from`, and that moment needs a worker to notice it. It rides
//! this task on a tighter cadence ([`PROMOTION_INTERVAL`]) than the hourly charge: a minute
//! late is invisible against a 24h notice, an hour late is a holder charged the old rate
//! for an hour longer than they were told.

use std::{
	collections::HashMap,
	sync::Arc,
	time::{Duration, Instant},
};

use domain::fees::Trigger;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
	application::fees as fee_app,
	infrastructure::rails::now_unix_i64,
	ports::{
		fees::{FeeAssessments, FeePolicies, FeePolicyChanges, PositionAccruals},
		ledger::Ledger,
		nav::NavMarks,
	},
};

/// How often the sweeper wakes. Frequent enough that a missed cycle is invisible, rare
/// enough that it is not a load source.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// How often due policy changes are promoted. A change scheduled to bind at a moment is
/// promoted within a minute of it.
const PROMOTION_INTERVAL: Duration = Duration::from_secs(60);

/// A position is only assessed once its last accrual is this old — a daily cadence.
/// Purely an efficiency knob: the fee owed is decided by elapsed seconds, so lengthening
/// or shortening this changes how often value moves, never how much.
const MIN_ACCRUAL_AGE: i64 = 24 * 60 * 60;

/// Positions assessed per cycle.
const BATCH: i64 = 500;

pub struct FeeSweeper {
	policies: Arc<dyn FeePolicies>,
	changes: Arc<dyn FeePolicyChanges>,
	accruals: Arc<dyn PositionAccruals>,
	assessments: Arc<dyn FeeAssessments>,
	ledger: Arc<dyn Ledger>,
	nav: Arc<dyn NavMarks>,
	notify: Arc<Notify>,
	/// The maintenance switch: a charge moves units, and the migration window's cut must not.
	outflow: Arc<dyn OutflowPolicy>,
}

impl FeeSweeper {
	pub fn new(
		policies: Arc<dyn FeePolicies>,
		changes: Arc<dyn FeePolicyChanges>,
		accruals: Arc<dyn PositionAccruals>,
		assessments: Arc<dyn FeeAssessments>,
		ledger: Arc<dyn Ledger>,
		nav: Arc<dyn NavMarks>,
		notify: Arc<Notify>,
		outflow: Arc<dyn OutflowPolicy>,
	) -> Self {
		Self {
			policies,
			changes,
			accruals,
			assessments,
			ledger,
			nav,
			notify,
			outflow,
		}
	}

	pub async fn run(self, shutdown: CancellationToken) {
		info!("fee sweeper: assessing due positions every {SWEEP_INTERVAL:?}, promoting due policy changes every {PROMOTION_INTERVAL:?}");
		// One loop on the tighter cadence; the hourly sweep runs on the ticks where it is due.
		let mut last_sweep: Option<Instant> = None;
		let mut promotion_failures = HashMap::new();
		loop {
			let now = now_unix_i64();
			if last_sweep.is_none_or(|at| at.elapsed() >= SWEEP_INTERVAL) {
				match self.sweep(now).await {
					Ok(charged) if charged > 0 => info!(charged, "fee sweeper: charged management/performance fees"),
					Ok(_) => {}
					Err(err) => warn!("fee sweeper: sweep failed (will retry): {err}"),
				}
				last_sweep = Some(Instant::now());
			}
			match self.promote(now, &mut promotion_failures).await {
				Ok(promoted) if promoted > 0 => info!(promoted, "fee sweeper: promoted scheduled fee-policy changes"),
				Ok(_) => {}
				Err(err) => warn!("fee sweeper: could not list due fee-policy changes (will retry): {err}"),
			}
			tokio::select! {
				() = shutdown.cancelled() => return,
				() = tokio::time::sleep(PROMOTION_INTERVAL) => {},
			}
		}
	}

	/// Whether the hub is in maintenance. Unreadable counts as paused: a skipped cycle is
	/// retried, a charge made while the switch could not be read is not undone.
	async fn paused(&self) -> bool {
		match self.outflow.outflows_paused().await {
			Ok(paused) => paused,
			Err(err) => {
				warn!("fee sweeper: cannot read the maintenance switch, skipping this cycle: {err}");
				true
			}
		}
	}

	/// Promote due fee-policy changes; a no-op in maintenance.
	pub async fn promote(&self, now_unix: i64, failures: &mut HashMap<domain::fees::FeePolicyChangeId, u32>) -> Result<usize, domain::error::DomainError> {
		if self.paused().await {
			info!("fee sweeper: maintenance is on, not promoting fee-policy changes");
			return Ok(0);
		}
		fee_app::promote_due(self.changes.as_ref(), now_unix, failures).await
	}

	/// One sweep; returns how many positions it actually charged. Public and
	/// `now_unix`-parameterised so an integration test can drive a year of accrual
	/// deterministically instead of waiting for one.
	///
	/// Only the backlog read can fail the sweep. Everything per-position — a fund with no
	/// policy, a stale NAV, a locked holding, a charge that floors to nothing — is a
	/// non-event that leaves the position exactly as it was for the next cycle.
	pub async fn sweep(&self, now_unix: i64) -> Result<usize, domain::error::DomainError> {
		if self.paused().await {
			info!("fee sweeper: maintenance is on, not charging fees");
			return Ok(0);
		}
		let due = self.accruals.due(now_unix.saturating_sub(MIN_ACCRUAL_AGE), BATCH).await?;
		let mut charged = 0usize;
		for accrual in due {
			let assessed = fee_app::assess_position(
				self.policies.as_ref(),
				self.accruals.as_ref(),
				self.assessments.as_ref(),
				self.ledger.as_ref(),
				self.nav.as_ref(),
				&self.notify,
				accrual.user,
				accrual.service.clone(),
				Trigger::Period,
				now_unix,
			)
			.await;
			match assessed {
				Ok(Some(_)) => charged += 1,
				Ok(None) => {}
				Err(err) => warn!(user = %accrual.user, service = %accrual.service, "fee sweeper: could not assess this position: {err}"),
			}
		}
		Ok(charged)
	}
}
