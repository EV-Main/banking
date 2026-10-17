//! `window-tool remap` — step 4 of the window: the cash that sat on allocation claims moves
//! onto the scope accounts the new architecture keeps it on (plan §3.3, §6.5).
//!
//! | From | To |
//! | --- | --- |
//! | `service:service_arb` | `scope_account:sa:alloc:service_arb` |
//! | `service:<any other product>` | `scope_account:invest:alloc:<product>` |
//! | `service:fee` | `scope_account:invest:alloc:fee` (less the treasury carve-out below) |
//! | `service:fund` | `scope_account:invest:alloc:fund` |
//! | the part of `service:fee` that is banking's own fee income | `scope_account:main:treasury` |
//!
//! **Label, not the plan's shorthand.** §6.5 writes `ScopeAccount(invest, fee|fund)`; the
//! account the rest of the code keeps `fee`'s cash on is `alloc:fee` (§3.3; the fee route of
//! migration 0059, [`ScopeAccountLabel::fee_allocation`]), because the bare label `fee` is
//! banking's own collector in every scope and no client may own it. `fund` follows the same
//! `alloc:<slug>` rule. Say so if the owner meant the bare labels.
//!
//! **The treasury carve-out is the owner's number, not the tool's.** Banking's own fee income
//! that landed on `service:fee` before D17 cannot be told from the allocation's fees by any
//! record the ledger keeps; the plan says to settle it against the owner's snapshot. The tool
//! therefore moves nothing to the treasury unless `--treasury-from-fee <usdt>` names the
//! amount, and refuses an amount above what `fee` holds.
//!
//! Each allocation is ONE linked TigerBeetle chain (`Dr service:<svc>` against the destination
//! and, for `fee`, the treasury), so it moves whole or not at all. Ids derive from the claim's
//! lifetime debits, so a retry of an interrupted run recomputes the same ids and finds them,
//! and a later run (cash that arrived after) gets fresh ones. A claim with anything pending or
//! locked is refused: that is a saga the drain was supposed to finish.
//!
//! The destination accounts are opened with the `invest` client as owner — the engine that
//! opens an allocation's account (§3.3) — in the `scope_accounts` registry, in the same
//! transaction as the claim's advisory lock (the one every spender of a claim takes).
//! Reconciliation attributes value on a scope account to its registry row; a row-less account
//! would be an alert.

use std::collections::BTreeMap;

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use domain::{
	balance::{LedgerAccountKey, ServiceId, TransferCode},
	money::Usdt,
	scope_ledger::ClientId,
	scopes::{ScopeAccountLabel, ScopeId},
};
use sqlx::PgConnection;

use super::{Mode, Outcome, Window, balanced_cash, now_unix, require_drained, require_maintenance, say, tb_id, tool_id, units};
use crate::{
	infrastructure::{money_events, outbox},
	ports::{ledger::LedgerTransfer, scope_ledger::OwnedAccount},
};

/// The one allocation whose cash lives in the `sa` scope; every other product is `invest`'s.
const SA_SERVICE: &str = "service_arb";
/// The engine that opens allocation accounts (§3.3).
const ENGINE_CLIENT: &str = "invest";

/// The scope an allocation's cash belongs to.
pub fn scope_of(service: &ServiceId) -> Result<ScopeId> {
	let scope = if service.as_str() == SA_SERVICE { "sa" } else { "invest" };
	ScopeId::parse(scope).map_err(|e| eyre!("{e}"))
}

/// The account an allocation's cash is kept on once banking no longer knows allocations.
pub fn destination(service: &ServiceId) -> Result<(ScopeId, ScopeAccountLabel)> {
	let label = ScopeAccountLabel::parse(&format!("alloc:{service}"))
		.map_err(|e| eyre!("allocation `{service}` has no valid scope-account label (the registry wants lowercase [a-z0-9_:.-]): {e}"))?;
	Ok((scope_of(service)?, label))
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
	/// Base units of `service:fee` that are banking's own and go to the treasury.
	pub treasury_from_fee: u128,
}

impl Options {
	pub fn parse_treasury(raw: &str) -> Result<u128> {
		Ok(Usdt::parse_decimal(raw).map_err(|e| eyre!("--treasury-from-fee `{raw}`: {e}"))?.base_units())
	}
}

/// One allocation's claim and where it goes.
struct Step {
	service: ServiceId,
	source: LedgerAccountKey,
	posted: u128,
	destination: (ScopeId, ScopeAccountLabel),
	/// The part of `posted` for the treasury (only ever non-zero for `fee`).
	treasury: u128,
	/// The claim's lifetime debits, the discriminator of this move's ids.
	debits: u128,
	/// The claim's TigerBeetle id, `0` while the claim has nothing to move and no account.
	source_id: u128,
}

impl Step {
	fn dest_key(&self) -> LedgerAccountKey {
		LedgerAccountKey::ScopeAccount(self.destination.0.clone(), self.destination.1.clone())
	}

	/// Where this step's cash lands and how much on each account.
	fn credits(&self) -> Vec<(LedgerAccountKey, u128)> {
		self.legs().into_iter().map(|leg| (leg.credit, leg.amount)).collect()
	}

	/// The chain: the treasury leg first (if any), then the rest to the destination.
	fn legs(&self) -> Vec<LedgerTransfer> {
		let reference = tool_id(&format!("window:remap:{}", self.service));
		let leg = |role: &str, credit: LedgerAccountKey, amount: u128| LedgerTransfer {
			id: tool_id(&format!("window:remap:{}:{role}:{:032x}:{}", self.service, self.source_id, self.debits)),
			debit: self.source.clone(),
			credit,
			amount,
			code: TransferCode::WindowMigration,
			reference,
		};
		let mut legs = Vec::new();
		if self.treasury > 0 {
			legs.push(leg("treasury", LedgerAccountKey::ScopeAccount(ScopeId::main(), ScopeAccountLabel::treasury()), self.treasury));
		}
		if self.posted > self.treasury {
			legs.push(leg("alloc", self.dest_key(), self.posted - self.treasury));
		}
		legs
	}
}

pub async fn run(w: &Window, mode: Mode, options: Options) -> Result<Outcome> {
	let mut out = String::new();
	let before = w.read_anchor().await?;
	say!(out, "{}", before.render(&w.anchor));
	require_maintenance(&w.pool).await?;
	require_drained(&w.pool).await?;
	let cash_before = balanced_cash(w.ledger.as_ref(), "before the remap").await?;
	if cash_before.retired_claims != 0 {
		say!(
			out,
			"note: {} sits on the retired fund/fee claims (codes 1/40); the remap does not touch it",
			units(cash_before.retired_claims)
		);
	}

	let steps = plan(w, options).await?;
	let total: u128 = steps.iter().map(|s| s.posted).sum();
	for step in &steps {
		let to = format!("{}/{}", step.destination.0, step.destination.1);
		if step.treasury > 0 {
			say!(
				out,
				"{:<24} {:>26} -> {to} {} and main/treasury {}",
				step.service,
				units(step.posted),
				units(step.posted - step.treasury),
				units(step.treasury)
			);
		} else {
			say!(out, "{:<24} {:>26} -> {to}", step.service, units(step.posted));
		}
	}
	say!(out, "Σ to move {}", units(total));
	if !mode.applies() {
		say!(out, "dry run: nothing written (pass --yes to remap)");
		return Ok(Outcome { text: out, ok: true });
	}

	// What every account that receives cash must read afterwards: what it read now plus what
	// this run moves onto it. Per account, because the hub keeps taking deposits in
	// maintenance and a global total would flag those as the remap's doing.
	let mut expected: BTreeMap<String, (LedgerAccountKey, u128)> = BTreeMap::new();
	for step in &steps {
		for (key, add) in step.credits() {
			let entry = match expected.entry(key.logical_key()) {
				std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
				std::collections::btree_map::Entry::Vacant(e) => {
					let now = w.ledger.balance(&key).await.wrap_err("read a destination before the remap")?.posted;
					e.insert((key, now))
				}
			};
			entry.1 += add;
		}
	}

	for step in &steps {
		apply(w, step).await.wrap_err_with(|| format!("remap {}", step.service))?;
	}

	// Verify against the ledger, not against what the steps believe they did.
	for step in &steps {
		let claim = w.ledger.balance(&step.source).await.wrap_err("re-read a claim")?;
		if claim.pending != 0 || claim.locked != 0 || claim.posted != 0 {
			bail!("service:{} still holds {} after the remap", step.service, units(claim.posted));
		}
	}
	for (key, want) in expected.values() {
		let got = w.ledger.balance(key).await.wrap_err("re-read a destination")?.posted;
		if got != *want {
			bail!("{} reads {} after the remap, expected {}", key.logical_key(), units(got), units(*want));
		}
	}
	// The invariant is the only global post-condition; the totals themselves move with deposits.
	let cash_after = balanced_cash(w.ledger.as_ref(), "after the remap").await?;
	say!(out, "moved {}; custody {} == claims {}", units(total), units(cash_after.custody), units(cash_after.claims));
	let after = w.read_anchor().await?;
	after.same_holders_as(&before)?;
	say!(out, "{}", after.render(&w.anchor));
	Ok(Outcome { text: out, ok: true })
}

async fn plan(w: &Window, options: Options) -> Result<Vec<Step>> {
	let slugs: Vec<String> = sqlx::query_scalar("SELECT service FROM allocations ORDER BY service")
		.fetch_all(&w.pool)
		.await
		.wrap_err("list allocations")?;
	let mut steps = Vec::with_capacity(slugs.len());
	for slug in slugs {
		let service = ServiceId::parse(&slug).map_err(|e| eyre!("allocation slug `{slug}`: {e}"))?;
		let source = LedgerAccountKey::ServiceClaim(service.clone());
		let claim = w.ledger.balance(&source).await.wrap_err_with(|| format!("read service:{slug}"))?;
		if claim.pending != 0 || claim.locked != 0 {
			bail!(
				"service:{slug} has {} pending and {} locked: a saga is in flight, drain first",
				units(claim.pending),
				units(claim.locked)
			);
		}
		let treasury = if service == ServiceId::fee() { options.treasury_from_fee } else { 0 };
		if treasury > claim.posted {
			bail!("--treasury-from-fee {} is more than service:fee holds ({})", units(treasury), units(claim.posted));
		}
		steps.push(Step {
			debits: w.ledger.total_debits(&source).await.wrap_err("read a claim's debits")?,
			source_id: if claim.posted > 0 { tb_id(&w.pool, &source).await? } else { 0 },
			destination: destination(&service)?,
			posted: claim.posted,
			service,
			source,
			treasury,
		});
	}
	if options.treasury_from_fee > 0 && !steps.iter().any(|s| s.service == ServiceId::fee()) {
		bail!("--treasury-from-fee given but there is no `fee` allocation");
	}
	Ok(steps)
}

async fn apply(w: &Window, step: &Step) -> Result<()> {
	let (scope, label) = &step.destination;
	let state: Option<String> = sqlx::query_scalar("SELECT state FROM scopes WHERE id = $1")
		.bind(scope.as_str())
		.fetch_optional(&w.pool)
		.await
		.wrap_err("read the scope")?;
	match state.as_deref() {
		None => bail!("scope `{scope}` is not registered in banking"),
		Some("closed") => bail!("scope `{scope}` is closed"),
		Some(_) => {}
	}
	if step.treasury > 0 {
		let owner: Option<String> = sqlx::query_scalar("SELECT owner_client FROM scope_accounts WHERE scope = 'main' AND label = 'treasury'")
			.fetch_optional(&w.pool)
			.await
			.wrap_err("read the treasury account")?;
		if owner.as_deref() != Some("banking") {
			bail!("main/treasury is not registered as banking's account (owner {owner:?})");
		}
	}

	let owner = ClientId::parse(ENGINE_CLIENT).map_err(|e| eyre!("{e}"))?;
	let mut tx = w.pool.begin().await.wrap_err("begin the remap transaction")?;
	// The lock every spender of this claim takes, held to commit: nothing else spends it
	// between the read below and the chain.
	outbox::lock_claim(&mut tx, &step.source).await.map_err(|e| eyre!("{e}"))?;
	open_row(&mut tx, scope, label, &owner).await?;
	let claim = w.ledger.balance(&step.source).await.wrap_err("re-read the claim under its lock")?;
	if claim.posted != step.posted || claim.pending != 0 || claim.locked != 0 {
		bail!(
			"service:{} changed since it was planned (now {} posted, {} pending, {} locked)",
			step.service,
			units(claim.posted),
			units(claim.pending),
			units(claim.locked)
		);
	}
	let legs = step.legs();
	if legs.is_empty() {
		// Nothing to move, but the account must exist for the new owner's reads.
		w.ledger.ensure_account(&step.dest_key()).await.wrap_err("create the destination account")?;
	} else {
		w.ledger.post_linked(&legs).await.wrap_err("post the chain")?;
	}
	tx.commit().await.wrap_err("commit the remap transaction")?;
	Ok(())
}

/// Register `(scope, label)` as `owner`'s if it is not yet, and announce the opening like
/// any other. An existing row owned by someone else is a conflict, never overwritten.
async fn open_row(conn: &mut PgConnection, scope: &ScopeId, label: &ScopeAccountLabel, owner: &ClientId) -> Result<()> {
	let inserted = sqlx::query("INSERT INTO scope_accounts (scope, label, owner_client) VALUES ($1, $2, $3) ON CONFLICT (scope, label) DO NOTHING")
		.bind(scope.as_str())
		.bind(label.as_str())
		.bind(owner.as_str())
		.execute(&mut *conn)
		.await
		.wrap_err("register the scope account")?
		.rows_affected()
		== 1;
	let (registered_owner,): (String,) = sqlx::query_as("SELECT owner_client FROM scope_accounts WHERE scope = $1 AND label = $2")
		.bind(scope.as_str())
		.bind(label.as_str())
		.fetch_one(&mut *conn)
		.await
		.wrap_err("read the scope account")?;
	if registered_owner != owner.as_str() {
		bail!("scope account {scope}/{label} belongs to `{registered_owner}`, not to `{owner}`");
	}
	if inserted {
		let account = OwnedAccount {
			scope: scope.clone(),
			label: label.clone(),
			owner: owner.clone(),
			opened_at: now_unix(),
		};
		money_events::append(conn, &money_events::account_opened(&account)).await.map_err(|e| eyre!("{e}"))?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn service(slug: &str) -> ServiceId {
		ServiceId::parse(slug).unwrap()
	}

	// §6.5 step 4's table: service_arb is the one allocation in `sa`; every other product
	// and both reserved allocations are invest's.
	#[test]
	fn allocations_map_to_their_scope_account() {
		let key = |slug: &str| {
			let (scope, label) = destination(&service(slug)).unwrap();
			format!("{scope}/{label}")
		};
		assert_eq!(key("service_arb"), "sa/alloc:service_arb");
		assert_eq!(key("quy_nhon"), "invest/alloc:quy_nhon");
		assert_eq!(key("fee"), "invest/alloc:fee");
		assert_eq!(key("fund"), "invest/alloc:fund");
		assert_eq!(
			destination(&service("fee")).unwrap().1,
			ScopeAccountLabel::fee_allocation(),
			"the label the fee route already sweeps to"
		);
	}

	#[test]
	fn a_slug_the_registry_cannot_label_is_refused_by_name() {
		let err = destination(&service("Quy-Nhon")).unwrap_err().to_string();
		assert!(err.contains("Quy-Nhon"), "{err}");
	}

	#[test]
	fn the_treasury_carve_out_is_an_exact_decimal() {
		assert_eq!(Options::parse_treasury("50").unwrap(), 50_000_000_000_000_000_000);
		assert_eq!(Options::parse_treasury("0.000000000000000001").unwrap(), 1);
		assert!(Options::parse_treasury("-1").is_err());
		assert!(Options::parse_treasury("abc").is_err());
	}

	fn step(posted: u128, treasury: u128, debits: u128) -> Step {
		let service = ServiceId::fee();
		Step {
			source: LedgerAccountKey::ServiceClaim(service.clone()),
			destination: destination(&service).unwrap(),
			service,
			posted,
			treasury,
			debits,
			source_id: 7,
		}
	}

	// The chain moves exactly the claim: the treasury's part and the rest add up to it, and a
	// claim held entirely by the treasury has no allocation leg.
	#[test]
	fn the_legs_add_up_to_the_claim() {
		let legs = step(100, 30, 7).legs();
		assert_eq!(legs.iter().map(|l| l.amount).collect::<Vec<_>>(), [30, 70]);
		assert!(
			legs.iter()
				.all(|l| l.code == TransferCode::WindowMigration && l.debit == LedgerAccountKey::ServiceClaim(ServiceId::fee()))
		);
		assert_eq!(step(100, 100, 7).legs().len(), 1);
		assert_eq!(step(100, 0, 7).legs().len(), 1);
		assert!(step(0, 0, 7).legs().is_empty());
	}

	// A retry of the same attempt must find its own transfers; cash that arrives later must not
	// collide with them.
	#[test]
	fn ids_follow_the_claims_lifetime_debits() {
		let ids = |s: &Step| s.legs().iter().map(|l| l.id).collect::<Vec<_>>();
		assert_eq!(ids(&step(100, 30, 7)), ids(&step(100, 30, 7)));
		assert_ne!(ids(&step(100, 30, 7)), ids(&step(100, 30, 8)));
		let mut other_account = step(100, 30, 7);
		other_account.source_id = 8;
		assert_ne!(ids(&step(100, 30, 7)), ids(&other_account), "a different account never reuses a transfer id");
	}
}
