//! The reading side of `window-export`: one consistent, strictly read-only cut of the
//! unit book for the migration window (`docs/WINDOW-EXPORT.md`, plan §6.5 step 2).
//!
//! Read-only is enforced three times over, because the allocation this runs against holds
//! live clients' money: the caller's pool is opened with `default_transaction_read_only`,
//! the snapshot transaction is declared `READ ONLY` (Postgres itself refuses a write),
//! and the TigerBeetle side only goes through [`Ledger`] reads (`balance`,
//! `share_holdings`) — never `ensure_account`, never a transfer.
//!
//! The control-plane tables are read in one `REPEATABLE READ` transaction so they agree
//! with each other. TigerBeetle is read after it, outside that snapshot, which is only a
//! consistent cut because the window drained the outbox first; the trailer reports the
//! undispatched count so a file taken without that is visibly not clean.

use std::{
	collections::{BTreeMap, BTreeSet},
	io::Write,
};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use domain::{
	balance::{LedgerAccountKey, ServiceId},
	window_export::{
		Allocation, AllocationTotal, BookPolicy, FeeAssessment, FeePolicy, FeePolicyChange, FeeSettlement, Header, HeldProductUnits, HoldingKind, OwnedUnits, Ownership, Position, Record,
		SCHEMA_VERSION, ShareHolding, Supply, Trailer, UserRef, Valuation,
	},
};
use serde::de::DeserializeOwned;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	application::ownership::allocation_ownership,
	ports::ledger::{HoldingScope, Ledger},
};

/// Run a query whose rows are shaped exactly like `T`'s JSON. Postgres renders the row, so
/// the typed struct in `domain` is the only place a column is named twice, and a column
/// that drifts away from it fails the export instead of exporting a different file.
macro_rules! json_rows {
	($conn:expr, $ty:ty, $sql:literal) => {
		fetch_json::<$ty>(
			$conn,
			sqlx::query_scalar::<_, String>(concat!("SELECT row_to_json(t)::text FROM (", $sql, ") t")),
			stringify!($ty),
		)
		.await
	};
}

async fn fetch_json<T: DeserializeOwned>(conn: &mut PgConnection, query: sqlx::query::QueryScalar<'_, sqlx::Postgres, String, sqlx::postgres::PgArguments>, what: &str) -> Result<Vec<T>> {
	let rows = query.fetch_all(conn).await.wrap_err_with(|| format!("read {what}"))?;
	rows.iter()
		.map(|row| serde_json::from_str(row).wrap_err_with(|| format!("{what} row does not match the export schema: {row}")))
		.collect()
}

/// Writes records as JSON Lines and counts them by kind for the trailer.
struct Lines<W: Write> {
	out: W,
	counts: BTreeMap<String, u64>,
}

impl<W: Write> Lines<W> {
	fn put(&mut self, record: &Record) -> Result<()> {
		serde_json::to_writer(&mut self.out, record).wrap_err("write export record")?;
		self.out.write_all(b"\n").wrap_err("write export record")?;
		*self.counts.entry(record.kind().to_owned()).or_default() += 1;
		Ok(())
	}

	fn put_all<T>(&mut self, rows: Vec<T>, wrap: fn(T) -> Record) -> Result<()> {
		rows.into_iter().try_for_each(|row| self.put(&wrap(row)))
	}
}

/// What the TigerBeetle half of one allocation read.
struct Tb {
	holdings: Vec<ShareHolding>,
	supply: Supply,
	total: AllocationTotal,
	ownership: Option<Ownership>,
	/// Pending + locked on the supply, the cash claim and every holding.
	in_flight: u128,
}

/// What an export holds, for the operator's summary.
pub struct Summary {
	pub trailer: Trailer,
}

impl Summary {
	/// One line per allocation plus the verdict, units in whole-unit decimal.
	pub fn render(&self) -> String {
		let mut out = String::new();
		for a in &self.trailer.allocations {
			out.push_str(&format!(
				"{:<24} users {:>26}  book {:>26}  fee {:>26}  held {:>26}  outstanding {:>26}  {}\n",
				a.service,
				units(&a.user_shares),
				units(&a.book_shares),
				units(&a.fee_shares),
				units(&a.held),
				units(&a.shares_outstanding),
				if a.balanced { "ok" } else { "DRIFT" },
			));
		}
		out.push_str(&format!(
			"Σ held {}  Σ outstanding {}  outbox undispatched {}  {}\n",
			units(&self.trailer.total_held),
			units(&self.trailer.total_outstanding),
			self.trailer.outbox_undispatched,
			if self.trailer.clean { "CLEAN" } else { "NOT CLEAN" },
		));
		out
	}
}

/// 18-dp base units as a decimal, for people; the file itself keeps the exact string.
fn units(base: &str) -> String {
	const SCALE: usize = 18;
	let digits = format!("{:0>width$}", base, width = SCALE + 1);
	let (whole, frac) = digits.split_at(digits.len() - SCALE);
	let frac = frac.trim_end_matches('0');
	if frac.is_empty() { whole.to_owned() } else { format!("{whole}.{frac}") }
}

/// Export the unit book to `out`. `allow_live` lets an export run while the hub still takes
/// writes — a rehearsal, flagged in the header, never importable.
pub async fn export<W: Write>(pool: &PgPool, ledger: &dyn Ledger, tigerbeetle_cluster_id: &str, allow_live: bool, out: W) -> Result<Summary> {
	let mut conn = pool.acquire().await.wrap_err("acquire a control-plane connection")?;
	let conn: &mut PgConnection = &mut conn;
	sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
		.execute(&mut *conn)
		.await
		.wrap_err("open the read-only snapshot")?;
	let result = export_in_snapshot(conn, ledger, tigerbeetle_cluster_id, allow_live, out).await;
	// Nothing was written, so there is nothing to keep: ROLLBACK just ends the snapshot.
	// A failure here cannot change the file, and the connection is dropped right after.
	if let Err(e) = sqlx::query("ROLLBACK").execute(&mut *conn).await {
		tracing::warn!("ending the export snapshot failed: {e}");
	}
	result
}

async fn export_in_snapshot<W: Write>(conn: &mut PgConnection, ledger: &dyn Ledger, cluster_id: &str, allow_live: bool, out: W) -> Result<Summary> {
	sqlx::query("SET LOCAL TIME ZONE 'UTC'").execute(&mut *conn).await.wrap_err("pin the snapshot's time zone")?;
	let read_only: bool = sqlx::query_scalar("SELECT read_only FROM operations_mode WHERE id = TRUE")
		.fetch_optional(&mut *conn)
		.await
		.wrap_err("read operations_mode")?
		.ok_or_else(|| eyre!("operations_mode has no row"))?;
	if !read_only && !allow_live {
		bail!("the hub is not in maintenance (operations_mode.read_only = false): the window sets it first, or pass --allow-live for a rehearsal that is not importable");
	}
	let exported_at: String = sqlx::query_scalar(r#"SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"')"#)
		.fetch_one(&mut *conn)
		.await
		.wrap_err("read the snapshot instant")?;
	let outbox_undispatched: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE dispatched_at IS NULL AND parked_at IS NULL")
		.fetch_one(&mut *conn)
		.await
		.wrap_err("count the undispatched outbox")?;

	let allocations = json_rows!(
		conn,
		Allocation,
		"SELECT id::text AS id, service, title, summary, state, access, backing, icon, unit_cap, created_at, updated_at FROM allocations ORDER BY service"
	)?;
	let fee_policies = json_rows!(
		conn,
		FeePolicy,
		"SELECT service, management_bps, performance_bps, hurdle_bps, basis, crystallization, version, effective_from, updated_by, updated_at FROM fee_policies ORDER BY service"
	)?;
	let fee_policy_changes = json_rows!(
		conn,
		FeePolicyChange,
		"SELECT id::text AS id, service, version, management_bps, performance_bps, hurdle_bps, basis, crystallization, state, requirement, effective_from, requested_by, \
		 requested_at, reason, consilium_id::text AS consilium_id, scheduled_at, applied_at, closed_reason, notices_waived_by, notices_waived_at, \
		 notices_waived_users::text[] AS notices_waived_users FROM fee_policy_changes ORDER BY service, version"
	)?;
	let book_policies = json_rows!(
		conn,
		BookPolicy,
		"SELECT service, book_open, taker_fee_bps, price_tick, lot_size, market_slippage_bps, updated_at FROM book_policies ORDER BY service"
	)?;
	let valuations = json_rows!(
		conn,
		Valuation,
		"SELECT id::text AS id, service, aum, units_outstanding, nav, posted_by, posted_at FROM fund_valuations ORDER BY service, posted_at, id"
	)?;
	let positions = json_rows!(
		conn,
		Position,
		"SELECT user_id::text AS user_id, service, cost_basis, high_water_mark, units, fee_debt, fees_accrued_at, crystallized_at, created_at, updated_at \
		 FROM fund_positions ORDER BY service, user_id"
	)?;
	let fee_assessments = json_rows!(
		conn,
		FeeAssessment,
		"SELECT id::text AS id, user_id::text AS user_id, service, trigger_kind, nav, management, performance, debt_opening, charged_units, charged_cash, debt_carried, \
		 high_water_mark, assessed_at FROM fee_assessments ORDER BY service, assessed_at, id"
	)?;
	let fee_settlements = json_rows!(
		conn,
		FeeSettlement,
		"SELECT id::text AS id, service, units, nav, cash, settled_by, settled_at FROM fee_settlements ORDER BY service, settled_at, id"
	)?;

	let mut tb = Vec::with_capacity(allocations.len());
	for allocation in &allocations {
		tb.push(
			read_allocation(ledger, &allocation.service)
				.await
				.wrap_err_with(|| format!("read {} from TigerBeetle", allocation.service))?,
		);
	}

	let mut holders: BTreeSet<Uuid> = positions
		.iter()
		.map(|p| p.user_id.parse())
		.collect::<Result<_, _>>()
		.wrap_err("a position names a malformed user id")?;
	for holding in tb.iter().flat_map(|t| &t.holdings) {
		if let Some(user) = &holding.user_id {
			holders.insert(user.parse().wrap_err("a holding names a malformed user id")?);
		}
	}
	let ids: Vec<Uuid> = holders.into_iter().collect();
	let users = fetch_json::<UserRef>(
		conn,
		sqlx::query_scalar::<_, String>(
			"SELECT row_to_json(t)::text FROM (SELECT id::text AS user_id, concierge_user_id::text AS concierge_user_id FROM users WHERE id = ANY($1) ORDER BY id) t",
		)
		.bind(ids),
		"UserRef",
	)
	.await?;

	let mut lines = Lines { out, counts: BTreeMap::new() };
	lines.put(&Record::Header(Header {
		schema_version: SCHEMA_VERSION,
		source: "banking".to_owned(),
		exported_at,
		operations_read_only: read_only,
		live: allow_live,
		tigerbeetle_cluster_id: cluster_id.to_owned(),
	}))?;
	lines.put_all(allocations, Record::Allocation)?;
	lines.put_all(fee_policies, Record::FeePolicy)?;
	lines.put_all(fee_policy_changes, Record::FeePolicyChange)?;
	lines.put_all(book_policies, Record::BookPolicy)?;
	lines.put_all(valuations, Record::Valuation)?;
	lines.put_all(positions, Record::Position)?;
	lines.put_all(fee_assessments, Record::FeeAssessment)?;
	lines.put_all(fee_settlements, Record::FeeSettlement)?;
	lines.put_all(users, Record::User)?;
	let mut totals = Vec::with_capacity(tb.len());
	let (mut total_held, mut total_outstanding, mut in_flight) = (0u128, 0u128, 0u128);
	for t in tb {
		in_flight = in_flight.checked_add(t.in_flight).ok_or_else(|| eyre!("Σ pending units overflow u128"))?;
		lines.put_all(t.holdings, Record::ShareHolding)?;
		lines.put(&Record::Supply(t.supply))?;
		if let Some(ownership) = t.ownership {
			lines.put(&Record::Ownership(ownership))?;
		}
		total_held = total_held.checked_add(parse_units(&t.total.held)?).ok_or_else(|| eyre!("Σ held overflows u128"))?;
		total_outstanding = total_outstanding
			.checked_add(parse_units(&t.total.shares_outstanding)?)
			.ok_or_else(|| eyre!("Σ outstanding overflows u128"))?;
		totals.push(t.total);
	}

	let outbox_undispatched = u64::try_from(outbox_undispatched).wrap_err("outbox count is negative")?;
	let clean = outbox_undispatched == 0 && in_flight == 0 && totals.iter().all(|t| t.balanced);
	// The trailer counts itself, so a reader summing `counts` gets the line count of the file.
	lines.counts.insert("trailer".to_owned(), 1);
	let trailer = Trailer {
		schema_version: SCHEMA_VERSION,
		counts: lines.counts.clone(),
		allocations: totals,
		total_held: total_held.to_string(),
		total_outstanding: total_outstanding.to_string(),
		outbox_undispatched,
		pending_or_locked: in_flight.to_string(),
		clean,
	};
	lines.put(&Record::Trailer(trailer.clone()))?;
	lines.out.flush().wrap_err("flush the export")?;
	Ok(Summary { trailer })
}

fn parse_units(raw: &str) -> Result<u128> {
	raw.parse().wrap_err_with(|| format!("`{raw}` is not a u128 base-unit amount"))
}

/// One allocation's supply, cash and holders from TigerBeetle. Zero balances are not
/// holdings, so an emptied holder is absent rather than a line of zeros.
async fn read_allocation(ledger: &dyn Ledger, slug: &str) -> Result<Tb> {
	let service = ServiceId::parse(slug).map_err(|e| eyre!("allocation slug `{slug}`: {e}"))?;
	let outstanding = ledger.balance(&LedgerAccountKey::SharesOutstanding(service.clone())).await.map_err(|e| eyre!("{e}"))?;
	let claim = ledger.balance(&LedgerAccountKey::ServiceClaim(service.clone())).await.map_err(|e| eyre!("{e}"))?;

	let mut holdings = Vec::new();
	let (mut users, mut book, mut fee) = (0u128, 0u128, 0u128);
	let scanned = ledger.share_holdings(&HoldingScope::Product(service.clone())).await.map_err(|e| eyre!("{e}"))?;
	// `share_holdings` is posted-only; a pending leg (a redemption's reserved burn) is read
	// from the account itself, zero-balance holdings included.
	let keys: Vec<LedgerAccountKey> = scanned.iter().map(|(key, _)| key.clone()).collect();
	let mut in_flight = [&outstanding, &claim]
		.iter()
		.try_fold(0u128, |acc, b| acc.checked_add(b.pending)?.checked_add(b.locked))
		.ok_or_else(|| eyre!("pending units of {slug} overflow u128"))?;
	for balance in ledger.balances(&keys).await.map_err(|e| eyre!("{e}"))? {
		in_flight = in_flight
			.checked_add(balance.pending)
			.and_then(|s| s.checked_add(balance.locked))
			.ok_or_else(|| eyre!("pending units of {slug} overflow u128"))?;
	}
	for (key, posted) in scanned {
		if posted == 0 {
			continue;
		}
		let (kind, user_id, sum) = match &key {
			LedgerAccountKey::UserShares(_, user) => (HoldingKind::UserShares, Some(user.raw().to_string()), &mut users),
			LedgerAccountKey::BookShares(_, user) => (HoldingKind::BookShares, Some(user.raw().to_string()), &mut book),
			LedgerAccountKey::FeeShares(_) => (HoldingKind::FeeShares, None, &mut fee),
			other => bail!("share_holdings returned a key that is not a holding: {}", other.logical_key()),
		};
		*sum = sum.checked_add(posted).ok_or_else(|| eyre!("Σ units of {slug} overflow u128"))?;
		holdings.push(ShareHolding {
			service: slug.to_owned(),
			kind,
			user_id,
			units: posted.to_string(),
		});
	}
	// The ledger scan is unordered; a stable order makes two exports of one state byte-equal.
	holdings.sort_by(|a, b| (kind_rank(a.kind), &a.user_id).cmp(&(kind_rank(b.kind), &b.user_id)));

	let held = users.checked_add(book).and_then(|s| s.checked_add(fee)).ok_or_else(|| eyre!("Σ units of {slug} overflow u128"))?;
	let ownership = if service.is_reserved() {
		let view = allocation_ownership(ledger, service.clone()).await.map_err(|e| eyre!("{e}"))?;
		Some(Ownership {
			service: slug.to_owned(),
			units_outstanding: view.units_outstanding.base_units().to_string(),
			claim_posted: view.claim.posted.base_units().to_string(),
			holders: view
				.holders
				.iter()
				.filter_map(|h| {
					h.holder.user_id().map(|user| OwnedUnits {
						user_id: user.raw().to_string(),
						units: h.units.base_units().to_string(),
					})
				})
				.collect(),
			product_units: view
				.product_units
				.iter()
				.map(|(product, held)| HeldProductUnits {
					service: product.as_str().to_owned(),
					units: held.base_units().to_string(),
				})
				.collect(),
		})
	} else {
		None
	};
	Ok(Tb {
		holdings,
		supply: Supply {
			service: slug.to_owned(),
			shares_outstanding: outstanding.posted.to_string(),
			claim_posted: claim.posted.to_string(),
			claim_pending: claim.pending.to_string(),
			claim_locked: claim.locked.to_string(),
		},
		total: AllocationTotal {
			service: slug.to_owned(),
			user_shares: users.to_string(),
			book_shares: book.to_string(),
			fee_shares: fee.to_string(),
			held: held.to_string(),
			shares_outstanding: outstanding.posted.to_string(),
			balanced: held == outstanding.posted,
		},
		ownership,
		in_flight,
	})
}

const fn kind_rank(kind: HoldingKind) -> u8 {
	match kind {
		HoldingKind::UserShares => 0,
		HoldingKind::BookShares => 1,
		HoldingKind::FeeShares => 2,
	}
}

#[cfg(test)]
mod tests {
	use super::units;

	#[test]
	fn units_render_as_whole_unit_decimals() {
		assert_eq!(units("0"), "0");
		assert_eq!(units("13000000000000000000000"), "13000");
		assert_eq!(units("3250500000000000000000"), "3250.5");
		assert_eq!(units("1"), "0.000000000000000001");
	}
}
