//! Payments sweeper — the clock payment orders have no other way to hear.
//!
//! Two jobs, both of which exist because an order's lifecycle depends on time passing and on
//! work that may not have finished:
//!
//!   - **The orders' own sweep** ([`payments::sweep`]): an order whose window ran out is
//!     closed, and an approved one whose effect never got created (a crash after the answer,
//!     a reservation the relay had not yet applied) is carried out.
//!   - **The concierge approval round** ([`payment_approvals::round`]): decisions that passed
//!     are carried out, the others end their orders, outcomes are reported and dead
//!     proposals withdrawn.
//!
//! Both are idempotent, so the cadence is an efficiency knob rather than a correctness one.

use std::{
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use domain::money::Network;
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{
	application::{
		payment_approvals::{self, ApprovalRound, ApprovalRoundPorts, PaymentProposer},
		payments::{self, PaymentPorts},
	},
	config::KycGate,
	ports::{Custody, OutflowPolicy, PaymentRepository, UserRepository, WithdrawalRepository, ledger::Ledger},
};

fn unix_now() -> i64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default()
}

/// How often to look. A 72h window does not need a tight loop; a minute keeps the expiry
/// visible promptly and picks up an interrupted execution well inside a human's attention.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// One sweep's outcome — counts, so the run loop and a test can assert without scraping logs.
#[derive(Clone, Copy, Debug, Default)]
pub struct SweepReport {
	pub payments: payments::SweepReport,
	pub approvals: ApprovalRound,
}

/// Constructed with a struct literal rather than a positional `new`: a run of `Arc`s is a
/// call site where a transposed pair compiles cleanly and misbehaves at runtime.
pub struct PaymentsSweeper {
	pub pool: PgPool,
	pub withdrawals: Arc<dyn WithdrawalRepository>,
	pub payments: Arc<dyn PaymentRepository>,
	pub users: Arc<dyn UserRepository>,
	pub ledger: Arc<dyn Ledger>,
	pub custody: Arc<dyn Custody>,
	pub policy: Arc<dyn OutflowPolicy>,
	pub notify: Arc<Notify>,
	pub configured: Arc<[Network]>,
	pub kyc: KycGate,
	pub proposer: Arc<PaymentProposer>,
}

impl PaymentsSweeper {
	pub async fn run(self, shutdown: CancellationToken) {
		const MAX_BACKOFF: Duration = Duration::from_secs(30);
		let mut backoff = Duration::from_millis(500);
		info!("payments sweeper: expiring stale orders and finishing approved ones every {SWEEP_INTERVAL:?}");
		'acquire: loop {
			let mut lock = tokio::select! {
				biased;
				() = shutdown.cancelled() => return,
				lock = super::singleton::acquire(&self.pool, super::singleton::keys::PAYMENTS_SWEEPER) => match lock {
					Ok(lock) => lock,
					Err(err) => {
						error!("payments sweeper: could not acquire the sweeper lock (retrying in {backoff:?}): {err}");
						tokio::select! {
							() = shutdown.cancelled() => return,
							() = tokio::time::sleep(backoff) => {},
						}
						backoff = (backoff * 2).min(MAX_BACKOFF);
						continue 'acquire;
					}
				},
			};
			backoff = Duration::from_millis(500);
			info!("payments sweeper: acquired the sweeper lock — sweeping as the singleton worker");
			loop {
				if let Err(err) = self.sweep().await {
					warn!("payments sweeper: sweep failed (will retry): {err}");
				}
				tokio::select! {
					() = shutdown.cancelled() => return,
					() = tokio::time::sleep(SWEEP_INTERVAL) => {},
				}
				// A reaped backend releases the lock silently. Re-acquiring (not exiting) is
				// the recovery; closing rather than dropping keeps a half-dead pooled session
				// from holding the lock against our own re-acquisition.
				if !super::singleton::still_held(&mut lock).await {
					error!("payments sweeper: lost the sweeper lock connection — re-acquiring");
					let _ = lock.close().await;
					continue 'acquire;
				}
			}
		}
	}

	/// One sweep. Public so an integration test can drive it deterministically.
	pub async fn sweep(&self) -> Result<SweepReport, domain::error::DomainError> {
		let now = unix_now();
		let ports = PaymentPorts {
			payments: self.payments.as_ref(),
			users: self.users.as_ref(),
			withdrawals: self.withdrawals.as_ref(),
			ledger: self.ledger.as_ref(),
			custody: self.custody.as_ref(),
			policy: self.policy.as_ref(),
			relay: &self.notify,
			configured: &self.configured,
			kyc: self.kyc,
		};
		Ok(SweepReport {
			payments: payments::sweep(&ports, now).await?,
			approvals: payment_approvals::round(
				&ApprovalRoundPorts {
					payments: &ports,
					store: self.proposer.store.as_ref(),
					approvals: self.proposer.approvals.as_ref(),
					catalog: self.proposer.catalog.as_ref(),
				},
				now,
			)
			.await?,
		})
	}
}
