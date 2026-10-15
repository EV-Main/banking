//! Saga reaper — recovery for abandoned/stuck sagas (BANK-FAULT-04).
//!
//! TB pendings are created with `timeout = 0` ("the saga owns the lifecycle, not TB's
//! clock"), so an in-flight reservation never auto-voids. That is correct, but it means an
//! abandoned saga — one whose external completion signal is lost or never arrives — wedges
//! the user's value forever unless *something* owns the timeout. This periodic job is that
//! owner. It is deliberately split by safety, per the cardinal withdrawal rule (never void
//! once the broadcast may have reached the chain):
//!
//!   - **`processing` withdrawals past the max age → ALERT ONLY.** The broadcast may have
//!     landed; auto-voiding would double-pay. Recovery needs a positive not-broadcast /
//!     dropped chain signal (an operator/watcher action), so the reaper only raises a
//!     Sentry-shipped `error!` for a human.
//!   - **`queued` redemptions past the max age → AUTO-FAIL.** A redemption is internal
//!     (claim→claim, nothing leaves the chain), so voiding the pending burn and returning
//!     the units is always safe.
//!   - **`queued` withdrawals past the max age → AUTO-CANCEL.** A queued withdrawal was
//!     never broadcast, so voiding the clearing reservation (full refund) is always safe;
//!     this is the backstop for a treasury worker that never dispatches.
//!
//!   - **`pending` compartment moves untouched past a few minutes, with no event left in
//!     the outbox → RE-ENQUEUE.** A relay that did not know the event (an older build in a
//!     rolling deploy or after a rollback) dispatched it as a no-op; replaying is safe
//!     because the move's ledger transfer id derives from the move id. A client's `pending`
//!     scope transfer is replayed the same way, for the same reason.
//!
//! The auto-fail/cancel paths go through the same row-locked repository commands a user
//! would (idempotent, drain the void event), so the relay reverses the money exactly once.

use std::{sync::Arc, time::Duration};

use domain::{redemptions::RedemptionId, withdrawals::WithdrawalId};
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
	infrastructure::{compartments, scope_holds, scope_invoices, scope_ledger},
	ports::{RedemptionRepository, WithdrawalRepository},
};

/// How often the reaper sweeps for stuck sagas.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// A saga older than this (since its last transition) is abandoned. Conservative — well
/// past any legitimate operator/watcher settle window — because the safe paths void real
/// reservations and the `processing` path pages a human. 24h for v1, matching the NAV
/// staleness horizon.
const MAX_SAGA_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// A `pending` compartment move (or scope transfer) untouched this long, with nothing left in
/// the outbox, is re-enqueued. The relay posts one within seconds; minutes leave room for a
/// backlog.
const MOVE_REDRIVE_AFTER: Duration = Duration::from_secs(5 * 60);

/// One reaper sweep's outcome — counts so the run loop and a test can assert without
/// scraping logs.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReaperReport {
	/// `processing` withdrawals past the max age — alerted, never auto-failed.
	pub stuck_processing_withdrawals: usize,
	/// `queued` withdrawals auto-cancelled (refunded).
	pub reaped_queued_withdrawals: usize,
	/// `queued` redemptions auto-failed (units returned).
	pub reaped_queued_redemptions: usize,
	/// `pending` compartment moves whose lost event was put back on the outbox.
	pub redriven_compartment_moves: usize,
	/// `pending` scope transfers whose lost event was put back on the outbox.
	pub redriven_scope_transfers: usize,
	/// Active scope holds past their TTL, released back to their users.
	pub expired_scope_holds: usize,
	/// Open scope invoices past their expiry, recorded expired.
	pub expired_scope_invoices: usize,
}

/// The reaper job: sweep on an interval until the process exits.
pub struct Reaper {
	pool: PgPool,
	withdrawals: Arc<dyn WithdrawalRepository>,
	redemptions: Arc<dyn RedemptionRepository>,
	notify: Arc<Notify>,
	max_age: Duration,
	move_redrive_after: Duration,
}

impl Reaper {
	pub fn new(pool: PgPool, withdrawals: Arc<dyn WithdrawalRepository>, redemptions: Arc<dyn RedemptionRepository>, notify: Arc<Notify>) -> Self {
		Self {
			pool,
			withdrawals,
			redemptions,
			notify,
			max_age: MAX_SAGA_AGE,
			move_redrive_after: MOVE_REDRIVE_AFTER,
		}
	}

	/// Test seam: replay a lost compartment move or scope transfer sooner than
	/// [`MOVE_REDRIVE_AFTER`].
	pub fn with_move_redrive_after(mut self, after: Duration) -> Self {
		self.move_redrive_after = after;
		self
	}

	/// Test seam: a shorter abandonment window so an integration test need not wait 24h.
	pub fn with_max_age(mut self, max_age: Duration) -> Self {
		self.max_age = max_age;
		self
	}

	pub async fn run(self, shutdown: CancellationToken) {
		info!("reaper: sweeping abandoned sagas every {SWEEP_INTERVAL:?} (max age {:?})", self.max_age);
		loop {
			if let Err(err) = self.sweep().await {
				warn!("reaper: sweep failed (will retry): {err}");
			}
			tokio::select! {
				() = shutdown.cancelled() => return,
				() = tokio::time::sleep(SWEEP_INTERVAL) => {},
			}
		}
	}

	/// One sweep. Public so an integration test can drive it deterministically. Alerts on
	/// stuck `processing` withdrawals and auto-resolves the safe `queued` sagas.
	pub async fn sweep(&self) -> Result<ReaperReport, sqlx::Error> {
		let mut report = ReaperReport::default();
		let cutoff_secs = self.max_age.as_secs() as i64;

		// (1) `processing` withdrawals past the cutoff — ALERT ONLY (may be broadcast).
		// `user_id` is NULL for a FUND REVENUE PAYOUT, and the schema says so out loud:
		// `CHECK ((source = 'user') = (user_id IS NOT NULL))`. Decoding it as a non-null
		// `String` made one stuck revenue payout abort this whole sweep with a column-decode
		// error — taking step (2) with it, so abandoned queued withdrawals stopped being
		// auto-cancelled for as long as that row sat there. The alert must be able to
		// describe the fund's own money, not just a user's.
		let stuck: Vec<(Uuid, Option<String>)> = sqlx::query_as("SELECT id, user_id::text FROM withdrawals WHERE state = 'processing' AND updated_at < now() - make_interval(secs => $1)")
			.bind(cutoff_secs)
			.fetch_all(&self.pool)
			.await?;
		report.stuck_processing_withdrawals = stuck.len();
		for (id, user) in &stuck {
			error!(
				withdrawal_id = %id,
				user_id = user.as_deref().unwrap_or("<fund revenue payout>"),
				"reaper: STUCK processing withdrawal past max age — needs a confirmed not-broadcast signal before fail/void (never auto-voided)"
			);
		}

		// (2) `queued` withdrawals past the cutoff — AUTO-CANCEL (never broadcast → safe).
		let stale_withdrawals: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM withdrawals WHERE state = 'queued' AND updated_at < now() - make_interval(secs => $1)")
			.bind(cutoff_secs)
			.fetch_all(&self.pool)
			.await?;
		for id in stale_withdrawals {
			match self.withdrawals.cancel(WithdrawalId::from_raw(id)).await {
				Ok(_) => {
					report.reaped_queued_withdrawals += 1;
					warn!(withdrawal_id = %id, "reaper: auto-cancelled abandoned queued withdrawal (refunded)");
				}
				Err(err) => warn!(withdrawal_id = %id, "reaper: could not auto-cancel queued withdrawal: {err}"),
			}
		}

		// (3) `queued` redemptions past the cutoff — AUTO-FAIL (internal claim→claim → safe).
		let stale_redemptions: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM redemptions WHERE state = 'queued' AND updated_at < now() - make_interval(secs => $1)")
			.bind(cutoff_secs)
			.fetch_all(&self.pool)
			.await?;
		for id in stale_redemptions {
			match self.redemptions.fail(RedemptionId::from_raw(id)).await {
				Ok(_) => {
					report.reaped_queued_redemptions += 1;
					warn!(redemption_id = %id, "reaper: auto-failed abandoned queued redemption (units returned)");
				}
				Err(err) => warn!(redemption_id = %id, "reaper: could not auto-fail queued redemption: {err}"),
			}
		}

		// `pending` compartment moves whose event is gone — RE-ENQUEUE: the ledger transfer
		// id is the move's, so a replay cannot post twice.
		report.redriven_compartment_moves = compartments::redrive_stale_moves(&self.pool, self.move_redrive_after).await?;
		if report.redriven_compartment_moves > 0 {
			warn!(
				count = report.redriven_compartment_moves,
				"reaper: re-enqueued pending compartment moves whose event was dispatched without posting them"
			);
		}

		report.redriven_scope_transfers = scope_ledger::redrive_stale_transfers(&self.pool, self.move_redrive_after).await?;
		if report.redriven_scope_transfers > 0 {
			warn!(
				count = report.redriven_scope_transfers,
				"reaper: re-enqueued pending scope transfers whose event was dispatched without posting them"
			);
		}

		// A TTL is the client's own word on how long a hold may stand, so releasing it is
		// routine: logged at INFO, not a finding.
		report.expired_scope_holds = scope_holds::expire_due_holds(&self.pool).await?;
		if report.expired_scope_holds > 0 {
			info!(count = report.expired_scope_holds, "reaper: released scope holds past their TTL");
		}

		// Like a hold's TTL, the expiry is the client's own word; nothing to relay either, an
		// expiry moves no money, so the relay is not nudged for it.
		report.expired_scope_invoices = scope_invoices::expire_due_invoices(&self.pool).await?;
		if report.expired_scope_invoices > 0 {
			info!(count = report.expired_scope_invoices, "reaper: expired scope invoices past their expiry");
		}

		if report.reaped_queued_withdrawals > 0
			|| report.reaped_queued_redemptions > 0
			|| report.redriven_compartment_moves > 0
			|| report.redriven_scope_transfers > 0
			|| report.expired_scope_holds > 0
		{
			self.notify.notify_one();
		}
		Ok(report)
	}
}
