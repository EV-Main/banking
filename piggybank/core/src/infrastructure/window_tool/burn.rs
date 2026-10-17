//! `window-tool burn-fee-shares` — before the export (owner's decision): the fee-class units
//! the `fee` allocation holds in the `sa` products are retired, so no `service_arb` unit that
//! is not a client's goes into the file.
//!
//! The fee class (`shares_fee:<svc>`) is the `fee` allocation's holding in a product, not a
//! client's: the anchor does not count it. Retiring it is the existing operator burn
//! ([`retire_units`]) with `fee` as the holder — a recorded `unit_issuances` row the relay
//! posts as `Dr SharesOutstanding / Cr FeeShares` — not a new kind of write. Supply shrinks by
//! exactly the fee class, no cash moves, and the clients' units are untouched.
//!
//! The burn is priced at the product's dealing NAV (the row records it), and the use case
//! refuses a stale mark: if it does, post a fresh valuation and rerun. `force` is set because
//! the product is live; the window is the operator saying so explicitly.
//!
//! Idempotent: the key names the product, the use case returns the existing row for the same
//! request, and once the relay has applied it the fee class reads zero and there is nothing to
//! plan. The tool does not run a relay — the hub's does — so it waits for the outbox up to
//! `--wait-secs` and reports "queued" if the relay had not got there; rerun to confirm.

use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr, eyre};
use domain::{
	balance::{LedgerAccountKey, ServiceId},
	issuance::{IdempotencyKey, UnitHolder},
	money::Shares,
};
use tokio::sync::Notify;

use super::{Mode, Outcome, Window, drain::wait_for_outbox, now_unix, remap::scope_of, require_maintenance, say, units};
use crate::{
	application::{
		funds::FundPorts,
		issuance::{RetireUnitsRequest, retire_units},
	},
	infrastructure::{allocations::PgAllocations, issuance::PgUnitIssuances, nav::PgNav, users::PgUsers},
	ports::ledger::HoldingScope,
};

/// The products whose fee class the window burns: those whose cash and units go to the `sa`
/// scope, i.e. `service_arb`.
async fn sa_products(w: &Window) -> Result<Vec<ServiceId>> {
	let slugs: Vec<String> = sqlx::query_scalar("SELECT service FROM allocations ORDER BY service")
		.fetch_all(&w.pool)
		.await
		.wrap_err("list allocations")?;
	let mut out = Vec::new();
	for slug in slugs {
		let service = ServiceId::parse(&slug).map_err(|e| eyre!("allocation slug `{slug}`: {e}"))?;
		if !service.is_reserved() && scope_of(&service)?.as_str() == "sa" {
			out.push(service);
		}
	}
	Ok(out)
}

pub async fn run(w: &Window, mode: Mode, wait: Duration) -> Result<Outcome> {
	let mut out = String::new();
	let before = w.read_anchor().await?;
	say!(out, "{}", before.render(&w.anchor));
	require_maintenance(&w.pool).await?;

	let products = sa_products(w).await?;
	let mut plan = Vec::new();
	for service in &products {
		let key = LedgerAccountKey::FeeShares(service.clone());
		let balance = w.ledger.balance(&key).await.wrap_err("read a fee class")?;
		if balance.locked != 0 {
			return Err(eyre!("{} fee-class units of {service} are locked in a pending transfer: drain first", units(balance.locked)));
		}
		say!(out, "{service}: fee class {} units to burn", units(balance.available()));
		if balance.available() > 0 {
			plan.push((service.clone(), balance.available()));
		}
	}
	if products.is_empty() {
		say!(out, "no allocation maps to the sa scope: nothing to burn");
	}
	if !mode.applies() {
		say!(out, "dry run: nothing written (pass --yes to burn)");
		return Ok(Outcome { text: out, ok: true });
	}

	let allocations = PgAllocations::new(w.pool.clone());
	let nav = PgNav::new(w.pool.clone());
	let issuances = PgUnitIssuances::new(w.pool.clone());
	let users = PgUsers::new(w.pool.clone());
	// Nobody is listening: the hub's relay polls the outbox, this process runs none.
	let relay = Notify::new();
	let ports = FundPorts {
		allocations: &allocations,
		ledger: w.ledger.as_ref(),
		nav: &nav,
		relay: &relay,
	};
	for (service, burn) in &plan {
		let request = RetireUnitsRequest {
			service: service.clone(),
			holder: UnitHolder::Allocation(ServiceId::fee()),
			units: Shares::from_base_units(*burn),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse(&format!("window-burn-fee-shares:{service}:{burn}")).map_err(|e| eyre!("{e}"))?,
			force: true,
		};
		let record = retire_units(&ports, &issuances, &users, request, now_unix())
			.await
			.map_err(|e| eyre!("retire the fee class of {service}: {e} (a stale mark needs a fresh valuation posted first)"))?;
		say!(out, "{service}: retirement recorded ({} units, state {})", units(*burn), record.issuance.state().as_str());
	}

	let undispatched = wait_for_outbox(&w.pool, wait).await?;
	let mut ok = true;
	for service in &products {
		let fee_class = w.ledger.balance(&LedgerAccountKey::FeeShares(service.clone())).await.wrap_err("re-read a fee class")?.posted;
		let outstanding = w.ledger.balance(&LedgerAccountKey::SharesOutstanding(service.clone())).await.wrap_err("read the supply")?.posted;
		let holders: u128 = w
			.ledger
			.share_holdings(&HoldingScope::Product(service.clone()))
			.await
			.wrap_err("read the holdings")?
			.iter()
			.map(|(_, posted)| *posted)
			.sum();
		if fee_class == 0 && outstanding == holders {
			say!(out, "{service}: fee class 0, supply {} == holdings {}", units(outstanding), units(holders));
		} else {
			ok = false;
			say!(
				out,
				"{service}: fee class {} supply {} holdings {} - not applied yet ({undispatched} outbox event(s) undispatched); rerun once the relay has",
				units(fee_class),
				units(outstanding),
				units(holders)
			);
		}
	}
	let after = w.read_anchor().await?;
	after.same_holders_as(&before)?;
	say!(out, "{}", after.render(&w.anchor));
	Ok(Outcome { text: out, ok })
}
