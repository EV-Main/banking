//! Observability adapter — the seam that hands captured errors to the monitoring
//! vendor via the shared `ev::error_monitoring` library (Sentry).
//!
//! The gRPC driving adapter calls [`report`] so the vendor can be swapped or
//! disabled without touching call sites (initialisation itself lives in `main`).
//! The integration is a no-op when Sentry has not been initialised (i.e.
//! `SENTRY_DSN` is unset).

use std::sync::atomic::{AtomicU64, Ordering};

/// The signer's sign-time unseal-failure message (the KEK-epoch dead-key class).
/// Must stay in sync with the signer's `Signer::unseal` wire message.
const UNSEAL_FAILURE_SIGNATURE: &str = "could not unseal the signing key";
/// Captures an unexpected error and forwards it to the error monitoring service.
///
/// Only call this for truly unexpected failures (5xx territory). Expected
/// domain errors — not found, validation, conflict — are client mistakes and
/// must not be reported here.
pub fn report(err: &dyn std::error::Error) {
	ev::error_monitoring::report(err);
}

pub fn unseal_failures() -> u64 {
	UNSEAL_FAILURES.load(Ordering::Relaxed)
}
/// Classify a signer-seam error: a dead-key unseal failure is counted and logged at
/// ERROR — it means real funds already cannot move, so it must never scroll by as a
/// WARN retry loop. Returns whether the message matched (callers keep their own
/// error handling either way).
pub fn note_signer_error(op: &'static str, wallet: &str, message: &str) -> bool {
	if !message.contains(UNSEAL_FAILURE_SIGNATURE) {
		return false;
	}
	let total = UNSEAL_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
	tracing::error!(
		op,
		%wallet,
		total,
		"signer could not unseal the signing key — PROVABLY DEAD KEY (KEK epoch): funds on this wallet's address cannot move. Check signer GetKeyHealth; supersede via RotateDepositAddress"
	);
	true
}
/// Process-lifetime count of signer unseal failures observed on money-moving paths
/// (sweep gas/consolidation, withdrawal signing). Surfaced on `Readiness` → the admin
/// Overview, because each hit means funds are stranded on a dead-key address.
static UNSEAL_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Process-lifetime count of reconciliation cash-invariant reads that timed out.
/// A transient blip is harmless (the next 60s scan usually succeeds), but a rising
/// count means the TigerBeetle bulk-`lookup_accounts` deadline is systematically
/// too tight or the ledger is overloaded — surfacing a drift early avoids days of
/// silent conservation-skip.
static CASH_INVARIANT_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// Record a reconciliation cash-invariant read failure. Callers (the reconciliation
/// job) already log at ERROR; this counter lets a dashboard/alert distinguish a one-off
/// from a persistent failure without grep'ing logs.
pub fn note_cash_invariant_read_failure() {
	let total = CASH_INVARIANT_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
	tracing::warn!(
		total,
		"reconciliation: cash-invariant read failed — TigerBeetle lookup_accounts unavailable. If this count climbs, the ledger may be overloaded or TB_CASH_INVARIANT_TIMEOUT may need raising."
	);
}

/// Total cash-invariant timeouts since process start. Surfaced on `Readiness` so the
/// admin Overview can flag a degraded reconciliation path.
pub fn cash_invariant_timeouts() -> u64 {
	CASH_INVARIANT_TIMEOUTS.load(Ordering::Relaxed)
}

/// Process-lifetime count of USDT arrivals on a treasury hot wallet that no deposit
/// watcher credited, because the chain names the wallet and not the person (#245): a
/// claim needs a holder, and the treasury is nobody's. Each one is money the ledger does
/// not describe until an operator attributes it with `SeedCapital`.
static UNATTRIBUTED_TREASURY_INFLOWS: AtomicU64 = AtomicU64::new(0);

/// Record a treasury arrival the watcher saw and deliberately did not credit. `error!`,
/// not `warn!`: it reaches Sentry, and it is a money incident with one remedy — the
/// operator who sent it seeds it under their own name — so the line carries what that
/// operator needs to find the transfer (rail, hash, sender, amount). The treasury drift
/// watch reports the same dollar as a surplus for as long as it stays unattributed, so
/// silence here would still not be silence there; this counter is the number to alert on.
pub fn note_unattributed_treasury_inflow(network: &str, tx: &str, from: &str, amount: &str) {
	let total = UNATTRIBUTED_TREASURY_INFLOWS.fetch_add(1, Ordering::Relaxed) + 1;
	tracing::error!(
		network,
		tx,
		from,
		amount,
		total,
		"treasury received USDT from outside and NOBODY was credited: a treasury arrival is not a deposit — attribute it to its sender with SeedCapital (the deposit is theirs; the units of `fund` follow)"
	);
}

/// Total unattributed treasury inflows since process start — a number for a dashboard,
/// where each increment is a pending `SeedCapital`.
pub fn unattributed_treasury_inflows() -> u64 {
	UNATTRIBUTED_TREASURY_INFLOWS.load(Ordering::Relaxed)
}

/// Process-lifetime count of reconciliation findings of **value without a holder**: an
/// allocation holding cash or product units while no unit of it is outstanding, so
/// nobody owns what it holds (#245). Not an alert: this is the expected state of the
/// `fee` allocation between the first ownership release and the data migration that
/// seats its holders, and it must read as a number on a dashboard, not as an incident.
/// Once the migration has run it should stop climbing; a rise after that is an
/// allocation everyone redeemed out of with cash still on its claim.
static UNHELD_ALLOCATION_VALUE: AtomicU64 = AtomicU64::new(0);

/// Record one allocation the reconciliation found holding value nobody owns. `warn!`,
/// deliberately below the cash-invariant and clearing alerts: nothing is lost and
/// nothing is inconsistent, the value is simply not yet anyone's. The line names the
/// allocation and what it holds so the operator can tell the expected window from a
/// stranded remainder.
pub fn note_unheld_allocation_value(service: &str, claim: &str, product_units: usize) {
	let total = UNHELD_ALLOCATION_VALUE.fetch_add(1, Ordering::Relaxed) + 1;
	tracing::warn!(
		service,
		claim,
		product_units,
		total,
		"reconciliation: allocation holds value with no units outstanding — nobody holds it yet (expected for `fee` until the ownership data migration seats its holders)"
	);
}

/// Total unheld-value findings since process start — a number for a dashboard.
pub fn unheld_allocation_value() -> u64 {
	UNHELD_ALLOCATION_VALUE.load(Ordering::Relaxed)
}

/// Scope transfers and payouts recorded and still not on the ledger past reconciliation's
/// stall threshold, as of its last pass — a gauge, not a counter: it falls back to zero once
/// the relay catches up.
static STALE_SCOPE_POSTINGS: AtomicU64 = AtomicU64::new(0);

/// Record reconciliation's latest count of stuck scope postings. `warn!` when nonzero: the
/// money is still promised away from its source (it counts against the spendable), so a client
/// sees its account short of what it paid out until the posting lands or fails.
pub fn note_stale_scope_postings(count: i64) {
	let count = u64::try_from(count).unwrap_or(0);
	STALE_SCOPE_POSTINGS.store(count, Ordering::Relaxed);
	if count > 0 {
		tracing::warn!(
			count,
			"reconciliation: scope transfers or payouts recorded and not on the ledger for over 15 minutes — is the relay draining?"
		);
	}
}

/// The latest count of stuck scope postings — a number for a dashboard.
pub fn stale_scope_postings() -> u64 {
	STALE_SCOPE_POSTINGS.load(Ordering::Relaxed)
}
