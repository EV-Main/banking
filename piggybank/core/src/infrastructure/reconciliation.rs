//! Reconciliation — the PG-vs-TB discrepancy scan (TB always wins).
//!
//! The outbox relay can **park** a money-moving event into a distinct terminal state
//! (`outbox.parked_at`, see [`super::outbox`]) when it hits a non-retryable failure. A
//! parked event is no longer dropped, but it still needs a watcher: this periodic job is
//! the design's reconciliation seam (PATTERNS.md § Reconciliation). It asserts these
//! things and **alerts** (a Sentry-shipped `error!`) on any drift, without mutating money
//! — TB is authoritative, so recovery is an operator/treasury action, not an auto-write:
//!
//!   1. **Global cash invariant** — `sum(custody) == sum(claims)` on the USDT ledger,
//!      read straight from TigerBeetle ([`Ledger::cash_invariant`]), the claims broken
//!      down by whose they are, every kind counted by its code. Value on a retired code's
//!      account (they still exist in TigerBeetle, emptied) and value on an account of no
//!      known kind are each their own finding.
//!   2. **Clearing vs control-plane** — the `clearing` account's reserved (pending +
//!      posted) balance vs the gross of every `queued`/`processing` withdrawal in
//!      Postgres; a mismatch means a withdrawal whose reserve parked (nothing locked) or a
//!      stranded reservation.
//!   3. **Parked-row scan** — every `outbox.parked_at` row, surfaced with its
//!      `last_error` and whether it has been flagged for compensation.
//!   4. **Treasury debits vs approvals** (D17) — everything debited from banking's
//!      treasury must be an approved payment out of it: the internal treasury orders the
//!      owners approved (`approved` / `executed`) plus the treasury withdrawals still
//!      standing (`queued` / `processing` / `completed`), each tied to its order by
//!      `withdrawals.payment_id`. A debit nobody approved is an alert.
//!   5. **Scope stream freshness** — how long since the scope registry last got a
//!      well-formed page of concierge's scope stream ([`scope_events::freshness`]). Past
//!      [`SCOPE_STREAM_STALL`] a freeze or close on concierge may not have reached the money
//!      plane: an alert — unless concierge answers UNIMPLEMENTED (it predates ScopeEvents),
//!      which is a `warn!`. Not part of [`ReconReport::clean`]: it is the registry's
//!      freshness, not a money invariant, and any database no consumer follows (every
//!      test's) would read dirty for a reason that says nothing about the money.
//!
//!   6. **Every scope account has an owner** — value on a `scope_account:<scope>:<label>`
//!      the ledger holds with no `scope_accounts` row saying whose it is
//!      ([`scope_ledger::unowned_scope_accounts`]). Scope accounts are already in (1) — the
//!      cash invariant counts them by code — so this is not about the sum but about the
//!      attribution: a scope account no client and not banking owns is money nobody may
//!      debit and nobody answers for. An alert, and part of [`ReconReport::clean`].
//!   7. **Scope money stuck in flight** — `pending` scope transfers and payouts older than
//!      [`SCOPE_POSTING_STALL`] ([`scope_payouts::stale_postings`]): recorded, counted
//!      against their source, and not on the ledger although the relay posts one within
//!      seconds. A gauge ([`telemetry::note_stale_scope_postings`]) and a `warn!`, not part of
//!      [`ReconReport::clean`]: the reaper replays a lost one after five minutes, so a
//!      nonzero reading is a backlog or a relay that is down — the parked-row scan (3) and
//!      the relay's own alerts say which.
//!   8. **Holds vs `hold_clearing`** — a hold is one pending transfer onto `hold_clearing`
//!      for exactly what it still holds, so the account's pending must equal Σ remaining of
//!      the ACTIVE holds in Postgres, and nothing is ever posted to it
//!      ([`scope_holds::hold_clearing_snapshot`]). Compared only on a pass with no hold event
//!      in flight and none written while the ledger was read, when the two sides are meant to
//!      agree; a run of passes that cannot compare is its own alert. A hold answering captures
//!      for money the ledger never placed (banking#514), or a reservation nobody will
//!      release, shows here: an alert, and part of [`ReconReport::clean`].
//!
//! It is **read-only** and idempotent, so running it on every standby is harmless; it is
//! wired as one `select!` branch of the composition root next to the relay.

use std::{
	sync::{
		Arc,
		atomic::{AtomicU32, Ordering},
	},
	time::Duration,
};

use domain::balance::{LedgerAccountKey, Party};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{
	infrastructure::{
		outbox,
		scope_events::{self, StreamFreshness},
		scope_holds, scope_ledger, scope_payouts, telemetry,
	},
	ports::ledger::Ledger,
};

/// How often the reconciliation scan runs. Discrepancies are rare and operator-resolved,
/// so a slow cadence is fine; the relay (sub-second) owns the hot path.
const SCAN_INTERVAL: Duration = Duration::from_secs(60);

/// How far the scope registry may fall behind concierge's scope stream before it is alerted.
/// Several of the consumer's backed-off retries (at most a minute apart) fit inside it, so a
/// blip does not page anyone.
pub const SCOPE_STREAM_STALL: Duration = Duration::from_secs(5 * 60);

/// How long a scope transfer or payout may stay recorded and unposted before reconciliation
/// counts it as stuck: past the reaper's five-minute replay, so a reading means the replay did
/// not help either.
pub const SCOPE_POSTING_STALL: Duration = Duration::from_secs(15 * 60);

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
	/// Value on accounts of a retired code — emptied when their kind was retired.
	pub retired_claims: u128,
	/// Σ users' scope compartments and Σ scopes' own accounts — named parts of `claims`,
	/// reported so a reader sees how much of the custody sits in scopes.
	pub scope_claims: u128,
	pub scope_accounts: u128,
	pub clearing_reserved: u128,
	pub clearing_expected: u128,
	pub parked_rows: usize,
	pub uncompensated_parked: usize,
	/// Σ debits standing on `scope_account:main:treasury` (posted + pending).
	pub treasury_debits: u128,
	/// Σ what the owners approved paying out of the treasury (see the module header, 4).
	pub treasury_debits_approved: u128,
	/// The scope stream's standing this pass; `None` when it could not be read. Outside
	/// [`Self::clean`] (see the module header, 5).
	pub scope_stream: Option<StreamFreshness>,
	/// Scope accounts holding value with no owner on record, by logical key (module header, 6).
	pub unowned_scope_accounts: Vec<String>,
	/// Scope transfers and payouts stuck unposted (module header, 7); `None` when unread.
	/// Outside [`Self::clean`].
	pub stale_scope_postings: Option<i64>,
	/// `hold_clearing` against the active holds (module header, 8); `None` when not compared
	/// this pass.
	pub hold_clearing: Option<HoldClearingCheck>,
}

/// One comparison of `hold_clearing` with the holds Postgres says are active, base units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HoldClearingCheck {
	/// Σ remaining of the active holds.
	pub expected: u128,
	/// In flight on `hold_clearing`: its pending, whichever side the ledger reports it on.
	pub held: u128,
	pub posted: u128,
}

impl HoldClearingCheck {
	pub fn holds(&self) -> bool {
		self.held == self.expected && self.posted == 0
	}
}

impl ReconReport {
	/// Whether every checked invariant held (no operator action needed).
	pub fn clean(&self) -> bool {
		self.custody == self.claims
			&& self.unclassified_claims == 0
			&& self.retired_claims == 0
			&& self.clearing_reserved == self.clearing_expected
			&& self.parked_rows == 0
			&& self.treasury_debits == self.treasury_debits_approved
			&& self.unowned_scope_accounts.is_empty()
			&& self.hold_clearing.is_none_or(|check| check.holds())
	}
}

/// The reconciliation job: scan on an interval until the process exits.
pub struct Reconciliation {
	pool: PgPool,
	ledger: Arc<dyn Ledger>,
	/// Passes in a row that could not compare `hold_clearing` (module header, 8).
	hold_clearing_skips: AtomicU32,
}

/// Passes in a row (a minute each) that may skip the `hold_clearing` comparison before it is an
/// alert: a busy book keeps a hold event in flight on many passes, never on an hour of them.
pub const HOLD_CLEARING_MAX_SKIPS: u32 = 60;

impl Reconciliation {
	pub fn new(pool: PgPool, ledger: Arc<dyn Ledger>) -> Self {
		Self {
			pool,
			ledger,
			hold_clearing_skips: AtomicU32::new(0),
		}
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
				report.retired_claims = inv.retired_claims;
				report.scope_claims = inv.scope_claims;
				report.scope_accounts = inv.scope_accounts;
				if !inv.balanced() {
					error!(
						custody = %inv.custody,
						claims = %inv.claims,
						user_claims = %inv.user_claims,
						clearing = %inv.clearing,
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
				if report.retired_claims != 0 {
					error!(
						retired = %report.retired_claims,
						"reconciliation: VALUE ON A RETIRED CLAIM — an account whose kind was retired (AccountCode::RETIRED) holds money no live path can move"
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

		// Treasury debits against what the owners approved. The two are read at different
		// instants and the relay applies an approval's reserve after it commits, so a
		// mismatch is re-read once before it is alerted.
		self.check_treasury(&mut report).await?;
		if report.treasury_debits != report.treasury_debits_approved {
			self.check_treasury(&mut report).await?;
		}
		// The two directions call for different responses: an excess debit is money gone
		// without say-so, a shortfall is an approved payment whose debit never landed.
		if report.treasury_debits > report.treasury_debits_approved {
			error!(
				debited = %report.treasury_debits,
				approved = %report.treasury_debits_approved,
				"reconciliation: TREASURY DEBIT WITHOUT APPROVAL — money left banking's treasury that no approved payment accounts for"
			);
		} else if report.treasury_debits < report.treasury_debits_approved {
			error!(
				debited = %report.treasury_debits,
				approved = %report.treasury_debits_approved,
				"reconciliation: TREASURY APPROVAL WITHOUT DEBIT — an approved treasury payment or withdrawal has not been debited on the ledger"
			);
		}

		// (3) Parked-row scan — every event the relay moved to the parked terminal state.
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

		// (5) Scope stream freshness. A read failure skips it this pass, like the ledger reads.
		match scope_events::freshness(&self.pool, SCOPE_STREAM_STALL).await {
			Ok(freshness) => {
				report.scope_stream = Some(freshness);
				match freshness {
					StreamFreshness::Fresh { .. } => {}
					StreamFreshness::Unserved { lag_secs } => warn!(
						lag_secs,
						"reconciliation: scope registry not following concierge — concierge answers UNIMPLEMENTED (predates ScopeEvents); scopes keep their last state until one that serves the stream is deployed"
					),
					StreamFreshness::Stalled { lag_secs } => error!(
						lag_secs,
						"reconciliation: SCOPE STREAM STALLED — no well-formed page from concierge's scope stream in over {SCOPE_STREAM_STALL:?}; a scope frozen or closed on concierge may still take money in here"
					),
				}
			}
			Err(err) => warn!("reconciliation: scope stream cursor read failed, freshness not verified this pass: {err}"),
		}

		// (6) Every scope account holding value has an owner. A read failure skips it this
		// pass, like the other ledger reads.
		match scope_ledger::unowned_scope_accounts(&self.pool).await {
			Ok(keys) =>
				for logical_key in keys {
					self.check_unowned(logical_key, &mut report).await;
				},
			Err(err) => warn!("reconciliation: scope account owners read failed, attribution not verified this pass: {err}"),
		}

		// (7) Scope money stuck in flight. A read failure skips it this pass.
		match scope_payouts::stale_postings(&self.pool, SCOPE_POSTING_STALL).await {
			Ok(stale) => {
				report.stale_scope_postings = Some(stale);
				telemetry::note_stale_scope_postings(stale);
			}
			Err(err) => warn!("reconciliation: scope postings in flight not read this pass: {err}"),
		}

		// (8) Holds against hold_clearing.
		self.check_hold_clearing(&mut report).await;

		if report.clean() {
			info!(custody = %report.custody, "reconciliation: clean — invariants hold, no parked rows");
		}
		Ok(report)
	}

	/// One scope account the ledger knows and no owner row names: alerted when it holds
	/// anything, posted or in flight either way. An empty one is a harmless leftover (a ledger account
	/// opened by a read), not a finding.
	async fn check_unowned(&self, logical_key: String, report: &mut ReconReport) {
		let key = match LedgerAccountKey::parse_logical_key(&logical_key) {
			Ok(key) => key,
			Err(err) => {
				warn!(%logical_key, "reconciliation: a code-31 ledger account does not parse as a scope account: {err}");
				return;
			}
		};
		match self.ledger.balance(&key).await {
			Ok(balance) if balance.posted == 0 && balance.pending == 0 && balance.locked == 0 => {}
			Ok(balance) => {
				error!(
					%logical_key,
					posted = %balance.posted,
					pending = %balance.pending,
					locked = %balance.locked,
					"reconciliation: SCOPE ACCOUNT WITHOUT OWNER — value on a scope account no client and not banking owns"
				);
				report.unowned_scope_accounts.push(logical_key);
			}
			Err(err) => warn!(%logical_key, "reconciliation: unowned scope account balance read failed, not verified this pass: {err}"),
		}
	}

	/// (8) Holds vs `hold_clearing`: Postgres, then the ledger, then Postgres again, compared
	/// only when nothing was in flight and no hold step landed between the two Postgres reads —
	/// otherwise the ledger read may sit on either side of it. A pass that cannot compare is
	/// skipped; [`HOLD_CLEARING_MAX_SKIPS`] of them in a row is an alert of its own, since a
	/// check that never runs proves nothing.
	async fn check_hold_clearing(&self, report: &mut ReconReport) {
		match self.compare_hold_clearing().await {
			Ok(check) => {
				self.hold_clearing_skips.store(0, Ordering::Relaxed);
				if !check.holds() {
					error!(
						expected = %check.expected,
						held = %check.held,
						posted = %check.posted,
						"reconciliation: HOLD CLEARING DRIFT — the ledger's hold reservations are not what the active holds say they hold"
					);
				}
				report.hold_clearing = Some(check);
			}
			Err(why) => {
				let skipped = self.hold_clearing_skips.fetch_add(1, Ordering::Relaxed) + 1;
				if skipped >= HOLD_CLEARING_MAX_SKIPS {
					error!(skipped, "reconciliation: hold_clearing not compared for {skipped} passes in a row: {why}");
				} else {
					warn!(skipped, "reconciliation: hold_clearing not compared this pass: {why}");
				}
			}
		}
	}

	async fn compare_hold_clearing(&self) -> Result<HoldClearingCheck, String> {
		let before = scope_holds::hold_clearing_snapshot(&self.pool).await.map_err(|err| format!("active holds read failed: {err}"))?;
		if before.in_flight {
			return Err("a hold event is in flight".into());
		}
		let balance = self
			.ledger
			.balance(&LedgerAccountKey::HoldClearing)
			.await
			.map_err(|err| format!("hold_clearing balance read failed: {err}"))?;
		let after = scope_holds::hold_clearing_snapshot(&self.pool)
			.await
			.map_err(|err| format!("active holds re-read failed: {err}"))?;
		if after != before {
			return Err("a hold changed during the ledger read".into());
		}
		Ok(HoldClearingCheck {
			expected: before.expected.base_units(),
			held: balance.pending.saturating_add(balance.locked),
			posted: balance.posted,
		})
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
}
