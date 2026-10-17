//! `window-tool close-share-accounts --export <file>` — step 5 of the window: after invest has
//! imported the units, banking's unit holdings are closed.
//!
//! TigerBeetle accounts cannot be deleted or re-coded, so "closing" is: every holding's balance
//! moves off against the supply (`Dr SharesOutstanding / Cr <holding>`, one linked chain per
//! product under [`TransferCode::WindowMigration`]), and the account is written down in
//! `window_closures` with the units that left it and the transfer that took them. The share
//! accounts end at zero, so do the supplies, and the Share ledger still adds up per product
//! (`Σ holdings == SharesOutstanding`) at every instant — the hub's own reconciliation has
//! nothing to alert on, which a units-parked-on-a-side-account design would not have given.
//! The units are not lost: they are in the export file, in invest, and per holder in
//! `window_closures`, where the anchor reads them from.
//!
//! **The export is the contract.** The tool closes what the file lists and nothing else:
//!
//! * the file must be a clean cut — header not `live`, taken in maintenance, trailer `clean`;
//! * every holding in it must be on the ledger *now* with exactly the exported units (or be
//!   already closed with exactly that transfer). Units that changed since the export were
//!   never imported, so they are not closed; a live balance the file does not list is not
//!   closed either. Either is a refusal that names the account;
//! * resting-sell units (`book_shares`) in the file mean the drain was skipped: refused.
//!
//! The tool cannot see invest. That the import is done and its Σ reconciled is the operator's
//! confirmation, which is what `--yes` stands for; the dry run says so.
//!
//! Marking the *codes* retired (`AccountCode::is_retired`) is deliberately not done here: it
//! changes how the running hub reads the Share ledger and belongs to the release that removes
//! the share ledger from banking. `window_closures` is the data-side mark.
//!
//! Idempotent by state, and crash-safe the way the ownership migration was: the ledger is
//! written first, the rows second; a rerun finds the transfers, reads their amounts back and
//! writes the missing rows from the ledger.

use std::collections::{BTreeMap, BTreeSet};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use domain::{
	balance::{LedgerAccountKey, ServiceId, TransferCode},
	users::UserId,
	window_export::{HoldingKind, Record, SCHEMA_VERSION, ShareHolding},
};
use uuid::Uuid;

use super::{Mode, Outcome, Window, balanced_cash, require_maintenance, require_quiet_outbox, say, tb_id, tool_id, units};
use crate::ports::ledger::{HoldingScope, LedgerTransfer};

/// A chain is one TigerBeetle batch; the client caps a request well above this.
const MAX_CHAIN: usize = 4000;

/// What the tool takes from a window-export file.
#[derive(Debug)]
pub struct Export {
	pub holdings: Vec<ShareHolding>,
	/// Every allocation the cut covers, holdings or not.
	pub services: BTreeSet<String>,
}

/// Parse and vet an export file. A file that is not a clean, non-live cut of a hub in
/// maintenance is refused whole: closing accounts against it would close the wrong thing.
pub fn parse_export(text: &str) -> Result<Export> {
	let mut records = Vec::new();
	for (n, line) in text.lines().enumerate() {
		if line.trim().is_empty() {
			continue;
		}
		records.push(serde_json::from_str::<Record>(line).wrap_err_with(|| format!("export line {} is not a record", n + 1))?);
	}
	let Some(Record::Header(header)) = records.first() else {
		bail!("the export does not start with a header");
	};
	if header.schema_version != SCHEMA_VERSION || header.source != "banking" {
		bail!(
			"the export is schema {} from `{}`, this tool reads schema {SCHEMA_VERSION} from `banking`",
			header.schema_version,
			header.source
		);
	}
	if header.live {
		bail!("the export was taken with --allow-live (a rehearsal, not a consistent cut): export again in maintenance");
	}
	if !header.operations_read_only {
		bail!("the export was taken while the hub was not in maintenance");
	}
	let Some(Record::Trailer(trailer)) = records.last() else {
		bail!("the export has no trailer: it is truncated");
	};
	if !trailer.clean {
		bail!("the export's trailer says it is not clean (an allocation does not add up, or the outbox was not drained)");
	}
	if trailer.counts.values().sum::<u64>() as usize != records.len() {
		bail!("the export's trailer counts {} lines, the file has {}", trailer.counts.values().sum::<u64>(), records.len());
	}
	let holdings = records
		.iter()
		.filter_map(|r| match r {
			Record::ShareHolding(h) => Some(h.clone()),
			_ => None,
		})
		.collect();
	let services = trailer.allocations.iter().map(|a| a.service.clone()).collect();
	Ok(Export { holdings, services })
}

/// One account to close, as the export names it.
#[derive(Debug)]
struct Item {
	service: ServiceId,
	user: Option<UserId>,
	key: LedgerAccountKey,
	units: u128,
	transfer_id: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
	/// On the ledger with the exported units, no closing transfer: to be closed.
	Open,
	/// Emptied by exactly the closing transfer: nothing to post, the row may be missing.
	Closed,
}

fn items(export: &Export) -> Result<Vec<Item>> {
	let mut out = Vec::with_capacity(export.holdings.len());
	for h in &export.holdings {
		let service = ServiceId::parse(&h.service).map_err(|e| eyre!("export names allocation `{}`: {e}", h.service))?;
		let user = h
			.user_id
			.as_deref()
			.map(|raw| Uuid::parse_str(raw).map(UserId::from_raw).wrap_err_with(|| format!("export names holder `{raw}`")))
			.transpose()?;
		let key = match (h.kind, user) {
			(HoldingKind::UserShares, Some(user)) => LedgerAccountKey::UserShares(service.clone(), user),
			(HoldingKind::FeeShares, None) => LedgerAccountKey::FeeShares(service.clone()),
			(HoldingKind::BookShares, _) => bail!(
				"the export holds {} units resting in a sell order of {}: the drain was skipped, export again after it",
				h.units,
				h.service
			),
			(kind, user) => bail!("the export has a {kind:?} holding of {} with holder {user:?}, which no account has", h.service),
		};
		let units: u128 = h.units.parse().wrap_err_with(|| format!("export units `{}` of {}", h.units, key.logical_key()))?;
		if units == 0 {
			bail!("the export lists a zero holding of {}", key.logical_key());
		}
		out.push(Item {
			// Filled in by `run` once the account's ledger id is known.
			transfer_id: 0,
			service,
			user,
			key,
			units,
		});
	}
	out.sort_by_key(|i| i.key.logical_key());
	if out.windows(2).any(|pair| pair[0].key == pair[1].key) {
		bail!("the export lists one account twice");
	}
	Ok(out)
}

async fn classify(w: &Window, item: &Item) -> Result<State> {
	let live = w.ledger.balance(&item.key).await.wrap_err("read a holding")?;
	if live.pending != 0 || live.locked != 0 {
		bail!("{} has units pending or locked: a saga is in flight, drain first", item.key.logical_key());
	}
	let closed_with = w.ledger.transfer_amount(item.transfer_id).await.wrap_err("read a closing transfer")?;
	match (live.posted, closed_with) {
		(posted, None) if posted == item.units => Ok(State::Open),
		(0, Some(amount)) if amount == item.units => Ok(State::Closed),
		(posted, closed) => bail!(
			"{} is not as the export has it: the file says {} units, the ledger holds {} and its closing transfer {}. Units that changed after the export were not imported; export again",
			item.key.logical_key(),
			units(item.units),
			units(posted),
			closed.map_or("does not exist".to_owned(), |c| format!("moved {}", units(c)))
		),
	}
}

pub async fn run(w: &Window, mode: Mode, export: &Export) -> Result<Outcome> {
	let mut out = String::new();
	require_maintenance(&w.pool).await?;
	require_quiet_outbox(&w.pool).await?;
	let cash_before = balanced_cash(w.ledger.as_ref(), "before closing").await?;

	let mut items = items(export)?;
	for item in &mut items {
		let account = tb_id(&w.pool, &item.key).await.wrap_err("the export lists an account the ledger does not know")?;
		item.transfer_id = tool_id(&format!("window:close:{}:{account:032x}", item.key.logical_key()));
	}
	let mut state = Vec::with_capacity(items.len());
	for item in &items {
		state.push(classify(w, item).await.wrap_err_with(|| format!("{} of {}", item.key.logical_key(), item.service))?);
	}

	// What the export does not list must not be on the ledger either.
	let listed: BTreeSet<String> = items.iter().map(|i| i.key.logical_key()).collect();
	for slug in &export.services {
		let service = ServiceId::parse(slug).map_err(|e| eyre!("export names allocation `{slug}`: {e}"))?;
		for (key, posted) in w.ledger.share_holdings(&HoldingScope::Product(service.clone())).await.wrap_err("scan the share ledger")? {
			if posted != 0 && !listed.contains(&key.logical_key()) {
				bail!(
					"{} holds {} units the export does not list: they were never exported, so they are not closed",
					key.logical_key(),
					units(posted)
				);
			}
		}
		let open: u128 = items.iter().zip(&state).filter(|(i, s)| i.service == service && **s == State::Open).map(|(i, _)| i.units).sum();
		let supply = w.ledger.balance(&LedgerAccountKey::SharesOutstanding(service.clone())).await.wrap_err("read a supply")?.posted;
		if supply != open {
			bail!(
				"the supply of {service} is {} but the accounts still to close hold {}: the Share ledger does not add up, closing would not zero it",
				units(supply),
				units(open)
			);
		}
	}

	// The anchor counts closed units through their rows, so a crash that left the ledger
	// ahead of the rows would make the anchor unreadable and the run unable to repair it.
	// The rows are facts of the ledger, so they are written first and the anchor read after.
	let mut missing = 0usize;
	for (item, s) in items.iter().zip(&state) {
		if *s == State::Closed && !has_row(w, item).await? {
			missing += 1;
			if mode.applies() {
				record_closed(w, item).await?;
			}
		}
	}
	if missing > 0 {
		say!(
			out,
			"{missing} closed account(s) have no row in window_closures{}",
			if mode.applies() { ": written from the ledger" } else { " (--yes writes them)" }
		);
	}
	let before = if missing > 0 && !mode.applies() {
		None
	} else {
		let reading = w.read_anchor().await?;
		say!(out, "{}", reading.render(&w.anchor));
		Some(reading)
	};

	let to_close = state.iter().filter(|s| **s == State::Open).count();
	let to_record = items.len() - to_close;
	let total: u128 = items.iter().zip(&state).filter(|(_, s)| **s == State::Open).map(|(i, _)| i.units).sum();
	let mut per_service: BTreeMap<&str, (usize, u128)> = BTreeMap::new();
	for (item, s) in items.iter().zip(&state) {
		if *s == State::Open {
			let e = per_service.entry(item.service.as_str()).or_default();
			e.0 += 1;
			e.1 += item.units;
		}
	}
	for (service, (accounts, sum)) in &per_service {
		say!(out, "{service:<24} {accounts:>5} account(s)  {:>26} units to close", units(*sum));
	}
	say!(out, "close {to_close} account(s), {} units; {to_record} already closed", units(total));
	if !mode.applies() {
		say!(
			out,
			"dry run: nothing written. --yes means: invest has imported this file and its Σ per allocation reconciled with the export's trailer"
		);
		return Ok(Outcome { text: out, ok: true });
	}

	for service in per_service.keys() {
		let service = ServiceId::parse(service).map_err(|e| eyre!("{e}"))?;
		let reference = tool_id(&format!("window:close:{service}"));
		let legs: Vec<LedgerTransfer> = items
			.iter()
			.zip(&state)
			.filter(|(i, s)| i.service == service && **s == State::Open)
			.map(|(i, _)| LedgerTransfer {
				id: i.transfer_id,
				debit: LedgerAccountKey::SharesOutstanding(service.clone()),
				credit: i.key.clone(),
				amount: i.units,
				code: TransferCode::WindowMigration,
				reference,
			})
			.collect();
		if legs.len() > MAX_CHAIN {
			bail!("{service} has {} accounts to close, more than one chain takes ({MAX_CHAIN})", legs.len());
		}
		w.ledger.post_linked(&legs).await.wrap_err_with(|| format!("close the accounts of {service}"))?;
	}

	// The rows come from the ledger, not from the file: what the transfer moved is the fact.
	for item in &items {
		record_closed(w, item).await?;
	}

	// Verify against the ledger.
	for slug in &export.services {
		let service = ServiceId::parse(slug).map_err(|e| eyre!("{e}"))?;
		let left: Vec<String> = w
			.ledger
			.share_holdings(&HoldingScope::Product(service.clone()))
			.await
			.wrap_err("re-scan the share ledger")?
			.into_iter()
			.filter(|(_, posted)| *posted != 0)
			.map(|(key, _)| key.logical_key())
			.collect();
		let supply = w.ledger.balance(&LedgerAccountKey::SharesOutstanding(service.clone())).await.wrap_err("re-read a supply")?.posted;
		if !left.is_empty() || supply != 0 {
			bail!("after closing, {service} still has holdings {left:?} and supply {}", units(supply));
		}
	}
	let cash_after = balanced_cash(w.ledger.as_ref(), "after closing").await?;
	if cash_after.custody != cash_before.custody || cash_after.claims != cash_before.claims {
		bail!(
			"closing share accounts moved cash: custody {} -> {}, claims {} -> {}",
			units(cash_before.custody),
			units(cash_after.custody),
			units(cash_before.claims),
			units(cash_after.claims)
		);
	}
	say!(
		out,
		"closed: every share account of {} allocation(s) at zero, supplies at zero, cash untouched",
		export.services.len()
	);
	let after = w.read_anchor().await?;
	// `before` is only absent in a dry run, which returned above.
	if let Some(before) = &before {
		after.same_holders_as(before)?;
	}
	say!(out, "{}", after.render(&w.anchor));
	Ok(Outcome { text: out, ok: true })
}

async fn has_row(w: &Window, item: &Item) -> Result<bool> {
	sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM window_closures WHERE logical_key = $1)")
		.bind(item.key.logical_key())
		.fetch_one(&w.pool)
		.await
		.wrap_err("look up a closure row")
}

/// Write the record of a closed account, taking the units from the ledger's transfer and
/// insisting they are what the export said.
async fn record_closed(w: &Window, item: &Item) -> Result<()> {
	let moved = w.ledger.transfer_amount(item.transfer_id).await.wrap_err("read a closing transfer")?;
	if moved != Some(item.units) {
		bail!("the closing transfer of {} moved {moved:?}, expected {}", item.key.logical_key(), item.units);
	}
	let kind = match item.key {
		LedgerAccountKey::UserShares(..) => "user_shares",
		LedgerAccountKey::FeeShares(_) => "fee_shares",
		_ => bail!("{} is not a closable holding", item.key.logical_key()),
	};
	sqlx::query("INSERT INTO window_closures (logical_key, service, kind, user_id, code, units, transfer_id) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (logical_key) DO NOTHING")
		.bind(item.key.logical_key())
		.bind(item.service.as_str())
		.bind(kind)
		.bind(item.user.map(|u| u.raw()))
		.bind(i32::from(item.key.account_code().code()))
		.bind(item.units.to_string())
		.bind(item.transfer_id.to_be_bytes().as_slice())
		.execute(&w.pool)
		.await
		.wrap_err("record a closed account")?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn header(live: bool, read_only: bool) -> String {
		format!(
			r#"{{"record":"header","schema_version":{SCHEMA_VERSION},"source":"banking","exported_at":"2026-10-17T00:00:00Z","operations_read_only":{read_only},"live":{live},"tigerbeetle_cluster_id":"0"}}"#
		)
	}

	fn trailer(clean: bool, lines: u64) -> String {
		format!(
			r#"{{"record":"trailer","schema_version":{SCHEMA_VERSION},"counts":{{"header":1,"trailer":1,"share_holding":{}}},"allocations":[{{"service":"svc","user_shares":"0","book_shares":"0","fee_shares":"0","held":"0","shares_outstanding":"0","balanced":true}}],"total_held":"0","total_outstanding":"0","outbox_undispatched":0,"clean":{clean}}}"#,
			lines - 2
		)
	}

	fn file(live: bool, read_only: bool, clean: bool) -> String {
		format!("{}\n{}\n", header(live, read_only), trailer(clean, 2))
	}

	#[test]
	fn a_clean_non_live_cut_of_a_hub_in_maintenance_is_accepted() {
		let export = parse_export(&file(false, true, true)).unwrap();
		assert_eq!(export.services.iter().collect::<Vec<_>>(), ["svc"]);
		assert!(export.holdings.is_empty());
	}

	// Closing accounts against the wrong file closes the wrong accounts, so every doubt about
	// the file is a refusal.
	#[test]
	fn a_rehearsal_a_live_hub_a_dirty_cut_or_a_truncated_file_is_refused() {
		assert!(parse_export(&file(true, true, true)).unwrap_err().to_string().contains("allow-live"));
		assert!(parse_export(&file(false, false, true)).unwrap_err().to_string().contains("maintenance"));
		assert!(parse_export(&file(false, true, false)).unwrap_err().to_string().contains("not clean"));
		assert!(parse_export(&header(false, true)).unwrap_err().to_string().contains("trailer"));
		assert!(parse_export("").unwrap_err().to_string().contains("header"));
		let mut extra = file(false, true, true);
		extra.insert_str(
			extra.find('\n').unwrap() + 1,
			"{\"record\":\"share_holding\",\"service\":\"svc\",\"kind\":\"user_shares\",\"user_id\":\"00000000-0000-0000-0000-000000000001\",\"units\":\"5\"}\n",
		);
		let err = format!("{:#}", parse_export(&extra).unwrap_err());
		assert!(err.contains("counts"), "a line the trailer did not count, got: {err}");
	}

	#[test]
	fn resting_sell_units_in_the_file_mean_the_drain_was_skipped() {
		let export = Export {
			holdings: vec![ShareHolding {
				service: "svc".into(),
				kind: HoldingKind::BookShares,
				user_id: Some(Uuid::nil().to_string()),
				units: "5".into(),
			}],
			services: BTreeSet::new(),
		};
		assert!(items(&export).unwrap_err().to_string().contains("drain"));
	}

	#[test]
	fn a_holding_maps_to_its_account() {
		let user = Uuid::from_u128(7);
		let export = Export {
			holdings: vec![
				ShareHolding {
					service: "svc".into(),
					kind: HoldingKind::UserShares,
					user_id: Some(user.to_string()),
					units: "13".into(),
				},
				ShareHolding {
					service: "svc".into(),
					kind: HoldingKind::FeeShares,
					user_id: None,
					units: "2".into(),
				},
			],
			services: BTreeSet::new(),
		};
		let a = items(&export).unwrap();
		assert_eq!(
			a.iter().map(|i| i.key.logical_key()).collect::<Vec<_>>(),
			[format!("shares:svc:{user}"), "shares_fee:svc".to_owned()]
		);
		assert_eq!(a.iter().map(|i| i.units).collect::<Vec<_>>(), [13, 2]);
	}

	#[test]
	fn a_zero_holding_or_a_duplicate_account_is_refused() {
		let holding = |units: &str| ShareHolding {
			service: "svc".into(),
			kind: HoldingKind::FeeShares,
			user_id: None,
			units: units.into(),
		};
		let zero = Export {
			holdings: vec![holding("0")],
			services: BTreeSet::new(),
		};
		assert!(items(&zero).is_err());
		let twice = Export {
			holdings: vec![holding("1"), holding("2")],
			services: BTreeSet::new(),
		};
		assert!(items(&twice).unwrap_err().to_string().contains("twice"));
	}
}
