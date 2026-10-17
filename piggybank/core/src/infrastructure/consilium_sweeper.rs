//! Consilium sweeper — the clock the governance aggregate has no other way to hear.
//!
//! Two jobs, both of which exist because a consilium's lifecycle depends on time passing and
//! on work that may not have finished:
//!
//!   - **Expiry.** A consilium runs out after 72h. Nothing else notices a deadline passing,
//!     so this closes it and tells the owners. An expired consilium can never execute — the
//!     aggregate only reaches `executed` from `approved`, and only reaches `expired` from
//!     `open`, so no ordering of the two produces a payout from a dead request.
//!   - **Execution retry.** A consilium is approved in one transaction and the payout is
//!     opened after it. A crash in between leaves an `approved` consilium with no payout,
//!     and nobody waiting to notice. This picks those up. An `execution_failed` one is
//!     deliberately NOT retried: that state means a human must look.
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
		consilium as consilium_app,
		payment_approvals::{self, ApprovalRound, ApprovalRoundPorts},
		payments as payments_app,
		scope_operations::CatalogGate,
	},
	config::KycGate,
	ports::{
		AllocationRegistry, Custody, Deposits, FeePolicyChanges, NavMarks, OutflowPolicy, PaymentRepository, SubscriptionRepository, UnitIssuanceRepository, UserRepository,
		WithdrawalRepository, consilium::ConsiliumRepository, deposit_addresses::DepositAddresses, ledger::Ledger, payments::PaymentApprovalStore, scope_operations::Approvals,
	},
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
	pub expired: usize,
	pub executed: usize,
	pub execution_failures: usize,
	pub voided: usize,
	/// The payment orders' own half of the sweep.
	pub payments: payments_app::SweepReport,
	/// The concierge-approved payments' half: proposals opened, decisions carried out.
	pub approvals: ApprovalRound,
}

/// What the sweep needs to drive payments decided over concierge's approvals. Absent, those
/// orders wait where they are and the old consilium path is untouched.
pub struct PaymentApprovalDriver {
	pub store: Arc<dyn PaymentApprovalStore>,
	pub approvals: Arc<dyn Approvals>,
	pub catalog: CatalogGate,
}

/// Constructed with a struct literal rather than a positional `new`: eight `Arc`s in a row
/// is a call site where a transposed pair compiles cleanly and misbehaves at runtime.
pub struct ConsiliumSweeper {
	pub pool: PgPool,
	pub consilia: Arc<dyn ConsiliumRepository>,
	pub withdrawals: Arc<dyn WithdrawalRepository>,
	pub payments: Arc<dyn PaymentRepository>,
	pub users: Arc<dyn UserRepository>,
	pub ledger: Arc<dyn Ledger>,
	pub custody: Arc<dyn Custody>,
	pub policy: Arc<dyn OutflowPolicy>,
	pub allocations: Arc<dyn AllocationRegistry>,
	pub fee_changes: Arc<dyn FeePolicyChanges>,
	pub issuances: Arc<dyn UnitIssuanceRepository>,
	pub deposits: Arc<dyn Deposits>,
	pub addresses: Arc<dyn DepositAddresses>,
	pub subscriptions: Arc<dyn SubscriptionRepository>,
	pub nav: Arc<dyn NavMarks>,
	pub notify: Arc<Notify>,
	pub configured: Arc<[Network]>,
	pub kyc: KycGate,
	pub approval_url_base: String,
	pub consent_url_base: String,
	pub payment_approvals: Option<PaymentApprovalDriver>,
}

impl ConsiliumSweeper {
	pub async fn run(self, shutdown: CancellationToken) {
		const MAX_BACKOFF: Duration = Duration::from_secs(30);
		let mut backoff = Duration::from_millis(500);
		info!("consilium sweeper: expiring stale requests and finishing approved ones every {SWEEP_INTERVAL:?}");
		'acquire: loop {
			let mut lock = tokio::select! {
				biased;
				() = shutdown.cancelled() => return,
				lock = super::singleton::acquire(&self.pool, super::singleton::keys::CONSILIUM_SWEEPER) => match lock {
					Ok(lock) => lock,
					Err(err) => {
						error!("consilium sweeper: could not acquire the sweeper lock (retrying in {backoff:?}): {err}");
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
			info!("consilium sweeper: acquired the sweeper lock — sweeping as the singleton worker");
			loop {
				if let Err(err) = self.sweep().await {
					warn!("consilium sweeper: sweep failed (will retry): {err}");
				}
				tokio::select! {
					() = shutdown.cancelled() => return,
					() = tokio::time::sleep(SWEEP_INTERVAL) => {},
				}
				// A reaped backend releases the lock silently. Re-acquiring (not exiting) is
				// the recovery; closing rather than dropping keeps a half-dead pooled session
				// from holding the lock against our own re-acquisition.
				if !super::singleton::still_held(&mut lock).await {
					error!("consilium sweeper: lost the sweeper lock connection — re-acquiring");
					let _ = lock.close().await;
					continue 'acquire;
				}
			}
		}
	}

	/// One sweep. Public so an integration test can drive it deterministically.
	pub async fn sweep(&self) -> Result<SweepReport, domain::error::DomainError> {
		let now = unix_now();
		let mut report = SweepReport {
			expired: self.consilia.expire_due(now).await?,
			..SweepReport::default()
		};
		// A change to the owner roster voids any payout request that was already open — the
		// other half of the cooling-off period, which on its own would only stop NEW
		// proposals and could be straddled by opening one first.
		if let Some(changed_at) = self.consilia.last_roster_change_at().await? {
			report.voided = self.consilia.void_open_for_roster_change(changed_at, now).await?;
			if report.voided > 0 {
				warn!(voided = report.voided, "consilium sweeper: voided open payout requests after an owner roster change");
			}
		}
		let ports = consilium_app::ConsiliumPorts {
			consilia: self.consilia.as_ref(),
			withdrawals: self.withdrawals.as_ref(),
			payments: self.payments.as_ref(),
			users: self.users.as_ref(),
			ledger: self.ledger.as_ref(),
			custody: self.custody.as_ref(),
			policy: self.policy.as_ref(),
			allocations: self.allocations.as_ref(),
			fee_changes: self.fee_changes.as_ref(),
			issuances: self.issuances.as_ref(),
			deposits: self.deposits.as_ref(),
			addresses: self.addresses.as_ref(),
			subscriptions: self.subscriptions.as_ref(),
			nav: self.nav.as_ref(),
			relay: &self.notify,
			configured: &self.configured,
			kyc: self.kyc,
			approval_url_base: &self.approval_url_base,
			consent_url_base: &self.consent_url_base,
			governance_mail_wired: super::governance_mail::is_wired(),
		};
		for id in self.consilia.awaiting_execution().await? {
			// Per-consilium failures warn and continue: one stuck request must not stop the
			// rest of the governance queue from resolving.
			match consilium_app::execute(&ports, id, now).await {
				Ok(view) if view.consilium.state() == domain::consilium::ConsiliumState::Executed => report.executed += 1,
				Ok(view) => {
					report.execution_failures += 1;
					error!(consilium_id = %id, state = view.consilium.state().as_str(), reason = view.consilium.failure_reason().unwrap_or_default(), "consilium: an approved request could not be carried");
				}
				Err(err) => warn!(consilium_id = %id, "consilium: execution attempt failed (will retry): {err}"),
			}
		}
		// The payment orders' clock rides the same sweep: their windows run out on the same
		// 72h, and an approved order whose effect never got created (a crash after the
		// consent, a reservation the relay had not yet applied) is picked up here.
		report.payments = payments_app::sweep(&ports.payment_ports(), now).await?;
		// Runs whichever way NEW orders are seated: an order opened under concierge's approvals
		// before a rollback to the consilium still has to be proposed, carried out and reported.
		if let Some(driver) = &self.payment_approvals {
			let payment_ports = ports.payment_ports();
			report.approvals = payment_approvals::round(
				&ApprovalRoundPorts {
					payments: &payment_ports,
					store: driver.store.as_ref(),
					approvals: driver.approvals.as_ref(),
					catalog: &driver.catalog,
				},
				now,
			)
			.await?;
		}
		Ok(report)
	}
}
