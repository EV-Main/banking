//! `window-tool reconcile [--final]` — step 6: the end-of-window check, and the anchor.
//!
//! Read-only. It runs the hub's own reconciliation pass once ([`Reconciliation::scan`]: the
//! cash invariant, clearing, units-at-a-holder per allocation, parked rows, treasury debits,
//! scope-account owners, holds), then adds what only the window knows about: cash still on
//! allocation claims, units still on share accounts, anything left on the retired `fund` /
//! `fee` claims, and the anchor.
//!
//! Without `--final` those leftovers are progress and only reported — the command is useful
//! after every step. With `--final` they are failures: the window is over only when
//! reconciliation is clean, no allocation claim holds cash, no share account holds units and
//! the retired claims are at zero.

use std::sync::Arc;

use color_eyre::eyre::{Result, WrapErr, eyre};
use domain::balance::{LedgerAccountKey, ServiceId};

use super::{Outcome, Window, balanced_cash, say, units};
use crate::{
	infrastructure::{allocations::PgAllocations, reconciliation::Reconciliation},
	ports::ledger::HoldingScope,
};

pub async fn run(w: &Window, finalize: bool) -> Result<Outcome> {
	let mut out = String::new();
	let mut ok = true;
	let before = w.read_anchor().await?;
	say!(out, "{}", before.render(&w.anchor));

	let report = Reconciliation::new(w.pool.clone(), Arc::clone(&w.ledger), Arc::new(PgAllocations::new(w.pool.clone())))
		.scan()
		.await
		.wrap_err("run the reconciliation pass")?;
	say!(
		out,
		"reconciliation: {}  custody {}  claims {}  unclassified {}  scope claims {}  scope accounts {}  parked rows {}  units drift {:?}  unowned scope accounts {:?}",
		if report.clean() { "CLEAN" } else { "NOT CLEAN" },
		units(report.custody),
		units(report.claims),
		units(report.unclassified_claims),
		units(report.scope_claims),
		units(report.scope_accounts),
		report.parked_rows,
		report.units_drift.iter().map(ToString::to_string).collect::<Vec<_>>(),
		report.unowned_scope_accounts
	);
	ok &= report.clean();

	// The scan counts what is wrong with the books; this says how far the window got.
	let cash = balanced_cash(w.ledger.as_ref(), "now").await?;
	if cash.retired_claims != 0 {
		say!(out, "FINDING: {} on the retired fund/fee claims", units(cash.retired_claims));
		ok = false;
	}
	let services: Vec<String> = sqlx::query_scalar("SELECT service FROM allocations ORDER BY service")
		.fetch_all(&w.pool)
		.await
		.wrap_err("list allocations")?;
	let (mut on_claims, mut on_shares) = (Vec::new(), Vec::new());
	for slug in &services {
		let service = ServiceId::parse(slug).map_err(|e| eyre!("allocation slug `{slug}`: {e}"))?;
		let claim = w.ledger.balance(&LedgerAccountKey::ServiceClaim(service.clone())).await.wrap_err("read a claim")?;
		if claim.posted != 0 || claim.pending != 0 || claim.locked != 0 {
			on_claims.push(format!("{slug} {}", units(claim.posted)));
		}
		let held: u128 = w
			.ledger
			.share_holdings(&HoldingScope::Product(service.clone()))
			.await
			.wrap_err("read the holdings")?
			.iter()
			.map(|(_, posted)| *posted)
			.sum();
		let supply = w.ledger.balance(&LedgerAccountKey::SharesOutstanding(service)).await.wrap_err("read a supply")?.posted;
		if held != 0 || supply != 0 {
			on_shares.push(format!("{slug} holdings {} supply {}", units(held), units(supply)));
		}
	}
	say!(out, "cash on allocation claims: {}", if on_claims.is_empty() { "none".to_owned() } else { on_claims.join(", ") });
	say!(out, "units on share accounts: {}", if on_shares.is_empty() { "none".to_owned() } else { on_shares.join(", ") });
	let closed: i64 = sqlx::query_scalar("SELECT count(*) FROM window_closures")
		.fetch_one(&w.pool)
		.await
		.wrap_err("count window_closures")?;
	say!(out, "closed share accounts on record: {closed}");
	if finalize && (!on_claims.is_empty() || !on_shares.is_empty()) {
		say!(out, "FINDING: the window is not finished (--final)");
		ok = false;
	}

	let after = w.read_anchor().await?;
	after.same_holders_as(&before)?;
	say!(out, "{}", if ok { "OK" } else { "NOT OK" });
	Ok(Outcome { text: out, ok })
}
