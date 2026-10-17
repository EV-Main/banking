//! `window-tool maintenance on|status` — step 1 of the window, and its dashboard.
//!
//! `on` flips the existing money-plane kill-switch (`operations_mode`, migration 0017,
//! [`operations::set_read_only`]) — the same flag the admin console's "Read-only mode" toggle
//! writes — and reads it back. There is deliberately no `off`: leaving maintenance is the
//! release's decision once invest is live, made in the admin console, not a step of the tool.
//!
//! `status` writes nothing. It lists what the window still has to deal with — the same counts
//! `drain` ends on — next to the cash invariant, the anchor and the cash still sitting on
//! allocation claims, so one screen says how far the window is.

use color_eyre::eyre::{Result, WrapErr};
use domain::balance::{LedgerAccountKey, ServiceId};

use super::{Mode, Outcome, Window, balanced_cash, drain, say, units};
use crate::infrastructure::operations;

pub async fn on(w: &Window, mode: Mode) -> Result<Outcome> {
	let mut out = String::new();
	if !mode.applies() {
		let was = operations::is_read_only(&w.pool).await.wrap_err("read operations_mode")?;
		say!(out, "maintenance: read_only is {was}; dry run, nothing written (pass --yes to set it)");
		return Ok(Outcome { text: out, ok: true });
	}
	// The kill-switch goes first and does not wait on the anchor: a hub with a wrong anchor
	// is the one that most needs its money mutations paused. The anchor is checked after, and
	// a broken one fails the command with maintenance already on.
	let was = operations::is_read_only(&w.pool).await.wrap_err("read operations_mode")?;
	let now = operations::set_read_only(&w.pool, true).await.wrap_err("set operations_mode")?;
	say!(out, "maintenance: read_only {was} -> {now}{}", if was { " (already on; nothing changed)" } else { "" });
	match w.read_anchor().await {
		Ok(reading) => {
			say!(out, "{}", reading.render(&w.anchor));
			Ok(Outcome { text: out, ok: now })
		}
		// Returned, not raised: the report must still say that maintenance is on.
		Err(e) => {
			say!(out, "{e:#}");
			Ok(Outcome { text: out, ok: false })
		}
	}
}

pub async fn status(w: &Window) -> Result<Outcome> {
	let mut out = String::new();
	let read_only = super::read_only(&w.pool).await?;
	say!(out, "maintenance: read_only = {read_only}");
	let left = drain::remaining(&w.pool).await?;
	out.push_str(&left.render());

	let cash = balanced_cash(w.ledger.as_ref(), "now").await?;
	say!(
		out,
		"cash: custody {}  claims {}  (users {}, scope claims {}, service claims {}, scope accounts {}, retired {})",
		units(cash.custody),
		units(cash.claims),
		units(cash.user_claims),
		units(cash.scope_claims),
		units(cash.service_claims),
		units(cash.scope_accounts),
		units(cash.retired_claims)
	);

	let services: Vec<String> = sqlx::query_scalar("SELECT service FROM allocations ORDER BY service")
		.fetch_all(&w.pool)
		.await
		.wrap_err("list allocations")?;
	let mut on_claims = Vec::new();
	for slug in &services {
		let service = ServiceId::parse(slug).map_err(|e| color_eyre::eyre::eyre!("allocation slug `{slug}`: {e}"))?;
		let claim = w.ledger.balance(&LedgerAccountKey::ServiceClaim(service)).await.wrap_err("read an allocation claim")?;
		if claim.posted != 0 || claim.pending != 0 || claim.locked != 0 {
			on_claims.push(format!("{slug} {}", units(claim.posted)));
		}
	}
	say!(
		out,
		"cash still on allocation claims: {}",
		if on_claims.is_empty() { "none".to_owned() } else { on_claims.join(", ") }
	);

	let reading = w.read_anchor().await?;
	say!(out, "{}", reading.render(&w.anchor));
	Ok(Outcome { text: out, ok: true })
}
