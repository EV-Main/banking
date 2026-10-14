//! Reconciliation — the PG-vs-TB discrepancy scan (TB always wins).
//!
//! The outbox relay can **park** a money-moving event into a distinct terminal state
//! (`outbox.parked_at`, see [`super::outbox`]) when it hits a non-retryable failure. A
//! parked event is no longer dropped, but it still needs a watcher: this periodic job is
//! the design's reconciliation seam (PATTERNS.md § Reconciliation). It asserts four
//! things and **alerts** (a Sentry-shipped `error!`) on any drift, without mutating money
//! — TB is authoritative, so recovery is an operator/treasury action, not an auto-write:
//!
//!   1. **Global cash invariant** — `sum(custody) == sum(claims)` on the USDT ledger,
//!      read straight from TigerBeetle ([`Ledger::cash_invariant`]), the claims broken
//!      down by whose they are: `Σ user + Σ scope compartment + Σ service + Σ scope
//!      account + clearing + book cash + retired == Σ custody`. The retired `fund`/`fee` accounts (codes 1/40) still exist in
//!      TigerBeetle and count as claims by code — zero since the ownership data migration
//!      of 2026-09-27 — and value on an account of no known kind is its own finding.
//!   2. **Clearing vs control-plane** — the `clearing` account's reserved (pending +
//!      posted) balance vs the gross of every `queued`/`processing` withdrawal in
//!      Postgres; a mismatch means a withdrawal whose reserve parked (nothing locked) or a
//!      stranded reservation.
//!   3. **Every unit at a holder, per allocation** (#245) — for each allocation the
//!      registry knows, `Σ holder units == SharesOutstanding` read off the Share ledger
//!      ([`ownership_app::allocation_ownership`]). Double entry makes this hold by
//!      construction, so a mismatch is an inconsistency: an alert. Alongside it, **value
//!      without a holder** — cash or product units on an allocation no unit of which is
//!      outstanding — is a `warn!` and a counter
//!      ([`telemetry::note_unheld_allocation_value`]), NOT an alert: fees can settle into
//!      an allocation before a holder grant seats anyone, which loses nothing — the owners
//!      decide whose it is. After the 2026-09-27 data migration it reads zero everywhere.
//!   4. **Parked-row scan** — every `outbox.parked_at` row, surfaced with its
//!      `last_error` and whether it has been flagged for compensation.
//!   5. **Treasury debits vs approvals** (D17) — everything debited from banking's
//!      treasury must be an approved payment out of it: the internal treasury orders the
//!      owners approved (`approved` / `executed`) plus the treasury withdrawals still
//!      standing (`queued` / `processing` / `completed`), each tied to its order by
//!      `withdrawals.payment_id`. A debit nobody approved is an alert.
//!
//! It is **read-only** and idempotent, so running it on every standby is harmless; it is
//! wired as one `select!` branch of the composition root next to the relay.

use std::{sync::Arc, time::Duration};

use domain::balance::{LedgerAccountKey, Party, ServiceId};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{
	application::ownership::{self as ownership_app, AllocationOwnership},
	infrastructure::{outbox, telemetry},
	ports::{AllocationRegistry, ledger::Ledger},
};

/// How often the reconciliation scan runs. Discrepancies are rare and operator-resolved,
/// so a slow cadence is fine; the relay (sub-second) owns the hot path.
const SCAN_INTERVAL: Duration = Duration::from_secs(60);

/// One reconciliation pass over the cash plane. The fields are raw 18-dp USDT base units
/// and the parked-row count, so a caller (a test, the run loop) can assert on the outcome
/// without scraping logs.
#[derive(Clone, Debug, Default)]
pub struct ReconReport {
	pub custody: u128,
	pub claims: u128,
	/// Claims on accounts of a kind the breakdown does not name — value the chart of
	/// accounts cannot attribute to anyone.
	pub unclassified_claims: u128,
	/// Σ users' scope compartments and Σ scopes' own accounts — named parts of `claims`,
	/// reported so a reader sees how much of the custody sits in scopes.
	pub scope_claims: u128,
	pub scope_accounts: u128,
	pub clearing_reserved: u128,
	pub clearing_expected: u128,
	/// Allocations whose supply is not what their holders add up to, even on a second
	/// look — an inconsistency, alerted.
	pub units_drift: Vec<ServiceId>,
	/// Allocations holding value while no unit of them is outstanding — reported, counted,
	/// not alerted (see the module header).
	pub unheld: Vec<ServiceId>,
	pub parked_rows: usize,
	pub uncompensated_parked: usize,
	/// Σ debits standing on `scope_account:main:treasury` (posted + pending).
	pub treasury_debits: u128,
	/// Σ what the owners approved paying out of the treasury (see the module header, 5).
	pub treasury_debits_approved: u128,
}

impl ReconReport {
	/// Whether every checked invariant held (no operator action needed). Unheld value is
	/// not an invariant: a clean report can still list allocations nobody holds yet.
	pub fn clean(&self) -> bool {
		self.custody == self.claims
			&& self.unclassified_claims == 0
			&& self.clearing_reserved == self.clearing_expected
			&& self.units_drift.is_empty()
			&& self.parked_rows == 0
			&& self.treasury_debits == self.treasury_debits_approved
	}
}

/// The reconciliation job: scan on an interval until the process exits.
pub struct Reconciliation {
	pool: PgPool,
	ledger: Arc<dyn Ledger>,
	allocations: Arc<dyn AllocationRegistry>,
}

impl Reconciliation {
	pub fn new(pool: PgPool, ledger: Arc<dyn Ledger>, allocations: Arc<dyn AllocationRegistry>) -> Self {
		Self { pool, ledger, allocations }
	}

	pub async fn run(self, shutdown: CancellationToken) {
		info!("reconciliation: starting PG-vs-TB scan every {SCAN_INTERVAL:?}");
		loop {
			if let Err(err) = self.scan().await {
				warn!("reconciliation: scan failed (will retry): {err}");
			}
			tokio::select! {
				() = shutdown.cancelled() => return,
				() = tokio::time::sleep(SCAN_INTERVAL) => {},
			}
		}
	}

	/// One scan pass. Public so an integration test can drive it deterministically and
	/// assert a parked event is surfaced. Returns the report; alerts are a side effect.
	pub async fn scan(&self) -> Result<ReconReport, sqlx::Error> {
		let mut report = ReconReport::default();

		// (1) Global cash invariant — sum(custody) == sum(claims), straight from TB, with
		// the claims named by whose they are so the alert says where the drift is.
		match self.ledger.cash_invariant().await {
			Ok(inv) => {
				report.custody = inv.custody;
				report.claims = inv.claims;
				report.unclassified_claims = inv.unclassified();
				report.scope_claims = inv.scope_claims;
				report.scope_accounts = inv.scope_accounts;
				if !inv.balanced() {
					error!(
						custody = %inv.custody,
						claims = %inv.claims,
						user_claims = %inv.user_claims,
						service_claims = %inv.service_claims,
						clearing = %inv.clearing,
						book_cash = %inv.book_cash,
						scope_claims = %inv.scope_claims,
						scope_accounts = %inv.scope_accounts,
						retired_claims = %inv.retired_claims,
						"reconciliation: CASH INVARIANT BROKEN — sum(custody) != sum(claims)"
					);
				}
				if report.unclassified_claims != 0 {
					error!(
						unclassified = %report.unclassified_claims,
						"reconciliation: CLAIMS OF UNKNOWN KIND — value on a USDT-ledger account the chart of accounts cannot attribute to anyone"
					);
				}
			}
			// A read failure leaves custody/claims at 0, so `clean()` would read the cash leg
			// as a trivially-satisfied `0 == 0`. Alert (not warn): a persistent failure — e.g.
			// an oversized `lookup_accounts` at scale — silently disables the conservation
			// check, and must not be mistaken for a transient TB blip that self-heals.
			Err(err) => {
				crate::infrastructure::telemetry::note_cash_invariant_read_failure();
				error!("reconciliation: CASH INVARIANT UNCHECKED — cash-invariant read failed, conservation not verified this pass: {err}")
			}
		}

		// (2) Clearing reservation vs the gross of in-flight withdrawals. `queued`/
		// `processing` are the in-flight states; their gross should equal what is reserved
		// (pending + posted-but-not-yet-disbursed) on the clearing claim.
		let expected: Option<String> = sqlx::query_scalar("SELECT COALESCE(SUM(amount::numeric), 0)::text FROM withdrawals WHERE state IN ('queued', 'processing')")
			.fetch_one(&self.pool)
			.await?;
		report.clearing_expected = expected.and_then(|s| s.parse().ok()).unwrap_or(0);
		match self.ledger.balance(&LedgerAccountKey::WithdrawalClearing).await {
			Ok(bal) => {
				report.clearing_reserved = bal.posted.saturating_add(bal.pending);
				if report.clearing_reserved != report.clearing_expected {
					error!(
						reserved = %report.clearing_reserved,
						expected = %report.clearing_expected,
						"reconciliation: CLEARING MISMATCH — reserved on the ledger != gross of in-flight withdrawals (a parked reserve, or a stranded reservation)"
					);
				}
			}
			Err(err) => warn!("reconciliation: clearing balance read failed: {err}"),
		}

		// (3) Every unit at a holder, per allocation. The registry is the list — the
		// platform's own picture of what it runs, hidden reserved rows included — and the
		// ledger is the witness. A registry read failure skips the check this pass and says
		// so, like a clearing read failure: the next pass retries.
		match self.allocations.list_all().await {
			Ok(allocations) =>
				for allocation in allocations {
					self.check_allocation(allocation.service().clone(), &mut report).await;
				},
			Err(err) => warn!("reconciliation: allocation registry read failed, per-allocation supply not verified this pass: {err}"),
		}

		// Treasury debits against what the owners approved. The two are read at different
		// instants and the relay applies an approval's reserve after it commits, so a
		// mismatch is re-read once before it is alerted.
		self.check_treasury(&mut report).await?;
		if report.treasury_debits != report.treasury_debits_approved {
			self.check_treasury(&mut report).await?;
		}
		if report.treasury_debits != report.treasury_debits_approved {
			error!(
				debited = %report.treasury_debits,
				approved = %report.treasury_debits_approved,
				"reconciliation: TREASURY DEBIT WITHOUT APPROVAL — money left banking's treasury that no approved payment accounts for"
			);
		}

		// (4) Parked-row scan — every event the relay moved to the parked terminal state.
		let parked = outbox::parked_rows(&self.pool).await?;
		report.parked_rows = parked.len();
		report.uncompensated_parked = parked.iter().filter(|r| !r.compensated).count();
		for row in &parked {
			error!(
				seq = row.seq,
				event_id = %row.event_id,
				aggregate = %row.aggregate,
				aggregate_id = %row.aggregate_id,
				kind = %row.kind,
				compensated = row.compensated,
				"reconciliation: PARKED outbox event needs intervention: {}",
				row.last_error.as_deref().unwrap_or("(no last_error)")
			);
		}

		if report.clean() {
			info!(custody = %report.custody, unheld = report.unheld.len(), "reconciliation: clean — invariants hold, no parked rows");
		}
		Ok(report)
	}

	async fn check_treasury(&self, report: &mut ReconReport) -> Result<(), sqlx::Error> {
		let approved: Option<String> = sqlx::query_scalar(
			"SELECT (COALESCE((SELECT SUM(amount::numeric) FROM payments WHERE from_kind = 'treasury' AND to_kind IS NOT NULL AND state IN ('approved', 'executed')), 0) \
			 + COALESCE((SELECT SUM(amount::numeric) FROM withdrawals WHERE source = 'treasury' AND state IN ('queued', 'processing', 'completed')), 0))::text",
		)
		.fetch_one(&self.pool)
		.await?;
		report.treasury_debits_approved = approved.and_then(|sum| sum.parse().ok()).unwrap_or(0);
		match self.ledger.total_debits(&Party::Treasury.claim_key()).await {
			Ok(debits) => report.treasury_debits = debits,
			// Unread is not a finding; match the approvals so `clean()` does not invent one.
			Err(err) => {
				warn!("reconciliation: treasury debits read failed, not verified this pass: {err}");
				report.treasury_debits = report.treasury_debits_approved;
			}
		}
		Ok(())
	}

	/// One allocation's supply against its holders, and whether anyone holds what it
	/// holds. Each account is read at its own instant, so a mint landing between the
	/// holders' scan and the supply read tears the picture by exactly that mint; a
	/// mismatch is therefore re-read once before it is alerted, which a real drift
	/// survives and a race between two reads milliseconds apart does not. A ledger read
	/// failure is a skip with a warning, never a finding.
	async fn check_allocation(&self, service: ServiceId, report: &mut ReconReport) {
		let mut ownership = match ownership_app::allocation_ownership(self.ledger.as_ref(), service.clone()).await {
			Ok(ownership) => ownership,
			Err(err) => {
				warn!(%service, "reconciliation: allocation ownership read failed, not verified this pass: {err}");
				return;
			}
		};
		if !ownership.units_reconcile() {
			ownership = match ownership_app::allocation_ownership(self.ledger.as_ref(), service.clone()).await {
				Ok(ownership) => ownership,
				Err(err) => {
					warn!(%service, "reconciliation: allocation ownership re-read failed, not verified this pass: {err}");
					return;
				}
			};
		}
		if !ownership.units_reconcile() {
			report.units_drift.push(service.clone());
			error!(
				%service,
				outstanding = %ownership.units_outstanding.to_decimal_string(),
				held = %ownership.held_units().to_decimal_string(),
				holders = ownership.holders.len(),
				"reconciliation: SUPPLY DRIFT — units outstanding != sum of holders' units (a unit nobody holds, or held beyond the supply)"
			);
		}
		if ownership.is_unheld() {
			report.unheld.push(service.clone());
			note_unheld(&ownership);
		}
	}
}

fn note_unheld(ownership: &AllocationOwnership) {
	telemetry::note_unheld_allocation_value(
		ownership.service.as_str(),
		&ownership.claim.posted.to_decimal_string(),
		ownership.product_units.iter().filter(|(_, units)| !units.is_zero()).count(),
	);
}
