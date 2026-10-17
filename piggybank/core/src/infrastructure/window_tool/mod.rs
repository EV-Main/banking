//! The migration window's operator tool — the banking half of org restructure plan §6.5.
//!
//! `window-export` (steps 2) only reads. This is everything the window changes in banking,
//! one subcommand per step, run by hand against the primary while the hub is in maintenance:
//!
//! | Subcommand | Step | What it does |
//! | --- | --- | --- |
//! | [`maintenance`] | 1 | `operations_mode.read_only` on, and a one-screen status of the window |
//! | [`drain`] | 1 | cancel open book orders and open consilia, wait the outbox out, say what is left |
//! | [`burn`] | before 2 | retire the `fee` allocation's fee-class units in the `sa` products |
//! | [`remap`] | 4 | allocation cash off `service:<svc>` onto `scope_account:<scope>:alloc:<svc>` |
//! | [`close`] | 5 | unit holdings off the share ledger against the supply, recorded in `window_closures` |
//! | [`reconcile`] | 6 | the hub's own reconciliation scan, the window's leftovers, and the anchor |
//!
//! **Money safety is the point of the shape, not an afterthought.**
//!
//! * Every subcommand reads the *anchor* first and last — the `service_arb` holders, who must
//!   read 13 000 / 3 250 units (Σ 16 250) before and after ([`anchor`]). An op that changes it,
//!   or finds it already wrong, fails with a non-zero exit; nothing is "fixed" past it.
//! * Every mutating subcommand is a **dry run unless `--yes`**: the plan is printed and nothing
//!   is written. `maintenance on` is the one exception, it only flips the kill-switch.
//! * Every ledger id is deterministic ([`tool_id`]), so a second run finds its own work. Each
//!   subcommand is idempotent by *state* as well: it plans from what the ledger says now.
//! * Cash-moving steps check `Σ custody == Σ claims` before and after and that custody did not
//!   move. The only thing they change is whose claim a balance is on.
//! * One tool at a time: a session advisory lock ([`SingleRun`]) refuses a second mutating run.
//!
//! Writes that bypass the outbox are admissible for the reason the one-off ownership migration
//! (`docs/RUNBOOK-ownership-migration.md`) was: an operator runs them once, after a dry run,
//! with the outbox drained and the reconciliation printed at the end. The ledger is written
//! first and the control-plane rows second, so a crash between them is repaired by a rerun,
//! which reads the amounts back from the ledger.

use std::sync::Arc;

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use domain::{balance::LedgerAccountKey, money::Shares};
use sqlx::{PgPool, Postgres, pool::PoolConnection};
use uuid::Uuid;

use crate::ports::ledger::{CashInvariant, Ledger};

pub mod anchor;
pub mod burn;
pub mod close;
pub mod drain;
pub mod maintenance;
pub mod reconcile;
pub mod remap;

pub use anchor::Anchor;

/// What every subcommand runs against.
pub struct Window {
	pub pool: PgPool,
	pub ledger: Arc<dyn Ledger>,
	pub anchor: Anchor,
}

impl Window {
	/// The anchor right now, refused unless it has the shape the owner gave. Every
	/// subcommand reads it first and last, so a step never runs on a wrong anchor and never
	/// ends on one.
	pub async fn read_anchor(&self) -> Result<anchor::Reading> {
		let reading = anchor::read(&self.pool, self.ledger.as_ref(), &self.anchor).await?;
		reading.matches(&self.anchor)?;
		Ok(reading)
	}
}

/// Whether a mutating subcommand writes or only says what it would write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
	DryRun,
	Apply,
}

impl Mode {
	pub fn from_yes(yes: bool) -> Self {
		if yes { Self::Apply } else { Self::DryRun }
	}

	pub fn applies(self) -> bool {
		self == Self::Apply
	}
}

/// A subcommand's printed report and verdict. `ok == false` is the non-zero exit.
#[derive(Debug)]
pub struct Outcome {
	pub text: String,
	pub ok: bool,
}

/// Append a formatted line to a report. Writing to a `String` cannot fail.
macro_rules! say {
	($out:expr) => { $out.push('\n') };
	($out:expr, $($arg:tt)*) => {{
		use std::fmt::Write as _;
		let _ = writeln!($out, $($arg)*);
	}};
}
pub(crate) use say;

/// The v5 namespace of every id the tool derives, so no other writer can collide with them.
const NAMESPACE: Uuid = Uuid::from_u128(0x7e3c_91d0_5a41_4f6b_9b52_77a0_1c0d_e001);

/// The deterministic ledger id of a thing the tool does, named by a spelled-out key. The same
/// state asks for the same key, so a retry recomputes the id and finds the transfer instead of
/// posting a second one.
pub fn tool_id(key: &str) -> u128 {
	Uuid::new_v5(&NAMESPACE, key.as_bytes()).as_u128()
}

/// 18-dp base units as a whole-unit decimal, for people.
pub fn units(base: u128) -> String {
	Shares::from_base_units(base).to_decimal_string()
}

pub fn now_unix() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		// A clock before 1970 is a broken host; zero makes every freshness check fail loudly
		// rather than letting the tool deal at a made-up instant.
		.map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The TigerBeetle id of an account the ledger map already knows. Part of the ids the tool
/// derives for an account's moves: the logical key alone names an account only as long as its
/// map row lives, and an id that outlives the row (a rebuilt control plane over a kept ledger,
/// which is what a rehearsal is) would be taken for the transfer of an account that is gone.
pub async fn tb_id(pool: &PgPool, key: &LedgerAccountKey) -> Result<u128> {
	let mut conn = pool.acquire().await.wrap_err("acquire a connection")?;
	crate::infrastructure::ledger::account_id(&mut conn, key)
		.await
		.map_err(|e| eyre!("{e}"))?
		.ok_or_else(|| eyre!("the ledger map has no account for {}", key.logical_key()))
}

/// `operations_mode.read_only`.
pub async fn read_only(pool: &PgPool) -> Result<bool> {
	let value: Option<bool> = sqlx::query_scalar("SELECT read_only FROM operations_mode WHERE id = TRUE")
		.fetch_optional(pool)
		.await
		.wrap_err("read operations_mode")?;
	value.ok_or_else(|| eyre!("operations_mode has no row"))
}

/// Refuse unless the hub is in maintenance: the window's steps assume nobody is spending.
pub async fn require_maintenance(pool: &PgPool) -> Result<()> {
	if !read_only(pool).await? {
		bail!("the hub is not in maintenance (operations_mode.read_only = false): run `window-tool maintenance on` first");
	}
	Ok(())
}

/// Outbox events neither dispatched nor parked: money the relay has yet to move.
pub async fn outbox_undispatched(pool: &PgPool) -> Result<i64> {
	sqlx::query_scalar("SELECT count(*) FROM outbox WHERE dispatched_at IS NULL AND parked_at IS NULL")
		.fetch_one(pool)
		.await
		.wrap_err("count the undispatched outbox")
}

/// Refuse while the relay still has work: a step that moves money on top of an unfinished
/// saga would compute from balances that are about to change.
pub async fn require_quiet_outbox(pool: &PgPool) -> Result<()> {
	let n = outbox_undispatched(pool).await?;
	if n != 0 {
		bail!("{n} outbox event(s) are not dispatched yet: run `window-tool drain` and let the relay finish first");
	}
	Ok(())
}

/// The cash invariant, refused if it does not hold: nothing moves on a ledger that is
/// already out of balance, and nothing is declared done on one that ended up so.
pub async fn balanced_cash(ledger: &dyn Ledger, when: &str) -> Result<CashInvariant> {
	let inv = ledger.cash_invariant().await.wrap_err_with(|| format!("read the cash invariant ({when})"))?;
	if !inv.balanced() || inv.unclassified() != 0 {
		bail!(
			"cash invariant broken {when}: custody {} vs claims {} (unclassified {})",
			units(inv.custody),
			units(inv.claims),
			units(inv.unclassified())
		);
	}
	Ok(inv)
}

/// The advisory lock that keeps two window tools from running at once. Held by the session
/// for as long as the value lives; Postgres drops it when the connection closes, so a crashed
/// tool cannot leave it behind.
pub struct SingleRun {
	_conn: PoolConnection<Postgres>,
}

/// The lock's key: spelled-out and derived once, not a magic number.
fn single_run_key() -> i64 {
	let id = tool_id("window-tool:single-run");
	// Truncation is the point: any 64 bits of a v5 id name the lock as well as all 128.
	(id >> 64) as i64
}

impl SingleRun {
	pub async fn acquire(pool: &PgPool) -> Result<Self> {
		let mut conn = pool.acquire().await.wrap_err("acquire a connection for the single-run lock")?;
		let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
			.bind(single_run_key())
			.fetch_one(&mut *conn)
			.await
			.wrap_err("take the single-run lock")?;
		if !got {
			bail!("another window-tool run holds the single-run lock");
		}
		Ok(Self { _conn: conn })
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// The id names the thing, not the run: it must be identical on every machine and every
	// run, or a retry would post a second transfer instead of finding the first.
	#[test]
	fn tool_ids_are_stable_and_distinct() {
		assert_eq!(tool_id("window:close:a"), tool_id("window:close:a"));
		assert_ne!(tool_id("window:close:a"), tool_id("window:close:b"));
	}

	#[test]
	fn units_render_as_whole_unit_decimals() {
		assert_eq!(units(13_000_000_000_000_000_000_000), "13000");
		assert_eq!(units(0), "0");
	}
}
