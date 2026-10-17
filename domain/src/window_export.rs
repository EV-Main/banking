//! The wire format of the migration-window export (`window-export`, schema version 1).
//!
//! One JSON object per line (JSON Lines). The first line is always [`Record::Header`], the
//! last [`Record::Trailer`]; a file without a trailer was cut short and must be refused.
//! Every line carries `"record": "<kind>"` (snake_case) next to the kind's own fields.
//! The format is documented for humans in `docs/WINDOW-EXPORT.md`.
//!
//! These types live in `domain` — plain serde, no I/O, wasm-safe — so the importer (invest)
//! pins this crate by git and parses the file with the very structs that wrote it, instead
//! of re-declaring a second reading of the same format.
//!
//! Conventions that make the file safe to read in any language:
//! - Every unit and money amount is a **decimal string** of 18-dp base units (`u128` does
//!   not survive a JSON number), exactly as the control plane stores it.
//! - Every id is a canonical UUID string; allocations are named by their `service` slug.
//! - Every timestamp is RFC 3339 in UTC, as Postgres renders it.
//! - Rows are exported as they stand, history included; nothing is derived or "fixed up"
//!   on the way out. Zero-balance TigerBeetle holdings are left out ([`ShareHolding`]).
//!
//! The format is versioned by [`SCHEMA_VERSION`]. A field is only ever added together with
//! a bump; a reader must refuse a version it does not know rather than guess.

use serde::{Deserialize, Serialize};

/// The version this crate writes and understands.
pub const SCHEMA_VERSION: u32 = 1;

/// One line of the export.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
	Header(Header),
	Allocation(Allocation),
	FeePolicy(FeePolicy),
	FeePolicyChange(FeePolicyChange),
	BookPolicy(BookPolicy),
	Valuation(Valuation),
	Position(Position),
	FeeAssessment(FeeAssessment),
	FeeSettlement(FeeSettlement),
	User(UserRef),
	ShareHolding(ShareHolding),
	Supply(Supply),
	Ownership(Ownership),
	Trailer(Trailer),
}

impl Record {
	/// The `record` tag this line is written with; the trailer's `counts` are keyed by it.
	/// Spelled out rather than read back from serde so a new variant fails to compile here.
	pub const fn kind(&self) -> &'static str {
		match self {
			Self::Header(_) => "header",
			Self::Allocation(_) => "allocation",
			Self::FeePolicy(_) => "fee_policy",
			Self::FeePolicyChange(_) => "fee_policy_change",
			Self::BookPolicy(_) => "book_policy",
			Self::Valuation(_) => "valuation",
			Self::Position(_) => "position",
			Self::FeeAssessment(_) => "fee_assessment",
			Self::FeeSettlement(_) => "fee_settlement",
			Self::User(_) => "user",
			Self::ShareHolding(_) => "share_holding",
			Self::Supply(_) => "supply",
			Self::Ownership(_) => "ownership",
			Self::Trailer(_) => "trailer",
		}
	}
}

/// First line. Says what produced the file and under which conditions it was read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Header {
	pub schema_version: u32,
	/// Always `banking`; the importer refuses a file from anywhere else.
	pub source: String,
	/// The snapshot instant of the Postgres reads (the transaction's start), RFC 3339 UTC.
	pub exported_at: String,
	/// `operations_mode.read_only` at the snapshot. The window requires `true`; an export
	/// taken with it `false` is a rehearsal and says so in [`Self::live`].
	pub operations_read_only: bool,
	/// The operator passed `--allow-live`: the hub was still taking writes, so the file is
	/// not a consistent cut and must not be imported.
	pub live: bool,
	pub tigerbeetle_cluster_id: String,
}

/// An allocation (investable product or reserved `fee` / `fund`) as the registry holds it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Allocation {
	pub id: String,
	pub service: String,
	pub title: String,
	pub summary: String,
	/// `draft | open | closed`.
	pub state: String,
	/// `hidden | view | invest`.
	pub access: String,
	/// `cash | in_kind`.
	pub backing: String,
	pub icon: String,
	/// Authorised unit supply, base units.
	pub unit_cap: String,
	pub created_at: String,
	pub updated_at: String,
}

/// The fee terms in force now.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FeePolicy {
	pub service: String,
	pub management_bps: u32,
	pub performance_bps: u32,
	pub hurdle_bps: u32,
	/// `invested_capital | market_value`.
	pub basis: String,
	/// `monthly | quarterly | semi_annual | annual`.
	pub crystallization: String,
	pub version: u32,
	pub effective_from: String,
	pub updated_by: String,
	pub updated_at: String,
}

/// One entry of a product's fee-policy history; every row carries the full terms.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FeePolicyChange {
	pub id: String,
	pub service: String,
	pub version: u32,
	pub management_bps: u32,
	pub performance_bps: u32,
	pub hurdle_bps: u32,
	pub basis: String,
	pub crystallization: String,
	/// `awaiting_consilium | scheduled | active | superseded | rejected | cancelled`.
	pub state: String,
	/// `admin | owner_consilium`.
	pub requirement: String,
	pub effective_from: String,
	pub requested_by: String,
	pub requested_at: String,
	pub reason: String,
	pub consilium_id: Option<String>,
	pub scheduled_at: Option<String>,
	pub applied_at: Option<String>,
	pub closed_reason: Option<String>,
	pub notices_waived_by: Option<String>,
	pub notices_waived_at: Option<String>,
	pub notices_waived_users: Option<Vec<String>>,
}

/// A product's trading terms. No row for a product means a closed book.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BookPolicy {
	pub service: String,
	pub book_open: bool,
	pub taker_fee_bps: u32,
	pub price_tick: String,
	pub lot_size: String,
	pub market_slippage_bps: u32,
	pub updated_at: String,
}

/// One append-only valuation mark.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Valuation {
	pub id: String,
	pub service: String,
	pub aum: String,
	pub units_outstanding: String,
	pub nav: String,
	pub posted_by: String,
	pub posted_at: String,
}

/// A holder's `fund_positions` projection: what fee maths needs and TigerBeetle cannot say.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Position {
	pub user_id: String,
	pub service: String,
	pub cost_basis: String,
	pub high_water_mark: String,
	/// The projection's own unit count; compare with the ledger's [`ShareHolding`], do not
	/// trust it over them.
	pub units: String,
	pub fee_debt: String,
	pub fees_accrued_at: String,
	pub crystallized_at: String,
	pub created_at: String,
	pub updated_at: String,
}

/// One fee charge (units clawed back from a holder, or deferred into `fee_debt`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FeeAssessment {
	pub id: String,
	pub user_id: String,
	pub service: String,
	/// `period | redemption`.
	pub trigger_kind: String,
	pub nav: String,
	pub management: String,
	pub performance: String,
	pub debt_opening: String,
	pub charged_units: String,
	pub charged_cash: String,
	pub debt_carried: String,
	pub high_water_mark: String,
	pub assessed_at: String,
}

/// One bulk conversion of a product's fee-class units into cash.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FeeSettlement {
	pub id: String,
	pub service: String,
	pub units: String,
	pub nav: String,
	pub cash: String,
	pub settled_by: String,
	pub settled_at: String,
}

/// A banking user that holds something in the export, with the id the identity plane knows
/// them by. The importer maps holders through `concierge_user_id`: banking ids do not
/// outlive banking.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UserRef {
	pub user_id: String,
	pub concierge_user_id: Option<String>,
}

/// Which TigerBeetle Share-ledger account a [`ShareHolding`] was read from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldingKind {
	/// `UserShares(service, user)` — a person's free units.
	UserShares,
	/// `BookShares(service, user)` — the same person's units locked in resting sell orders.
	/// Still theirs; counted in the supply.
	BookShares,
	/// `FeeShares(service)` — the `fee` allocation's fee class in that product. No `user_id`.
	FeeShares,
}

/// A non-zero units balance in TigerBeetle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ShareHolding {
	pub service: String,
	pub kind: HoldingKind,
	/// The holder for `user_shares` / `book_shares`; `None` for `fee_shares`.
	pub user_id: Option<String>,
	pub units: String,
}

/// An allocation's supply and cash, read from TigerBeetle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Supply {
	pub service: String,
	/// `SharesOutstanding(service)`, posted.
	pub shares_outstanding: String,
	/// `ServiceClaim(service)`: the allocation's cash.
	pub claim_posted: String,
	pub claim_pending: String,
	pub claim_locked: String,
}

/// One person's stake in an allocation, as the ownership view reports it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OwnedUnits {
	pub user_id: String,
	pub units: String,
}

/// One product's units held by a reserved allocation (`fee`'s fee classes).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HeldProductUnits {
	pub service: String,
	pub units: String,
}

/// The holders of a reserved allocation (`fee`, `fund`) as banking's ownership view reads
/// them. A derived view over [`ShareHolding`]; the importer cross-checks it and takes its
/// holder table from here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Ownership {
	pub service: String,
	pub units_outstanding: String,
	pub claim_posted: String,
	/// Largest first, people only; free and order-locked units of one person summed.
	pub holders: Vec<OwnedUnits>,
	pub product_units: Vec<HeldProductUnits>,
}

/// Σ of the units of one allocation, by where they sit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AllocationTotal {
	pub service: String,
	pub user_shares: String,
	pub book_shares: String,
	pub fee_shares: String,
	/// `user_shares + book_shares + fee_shares`.
	pub held: String,
	pub shares_outstanding: String,
	/// `held == shares_outstanding`.
	pub balanced: bool,
}

/// Last line: the checksums the importer must reproduce, and the verdict of the exporter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Trailer {
	pub schema_version: u32,
	/// Lines per record kind, header and trailer included, keyed by the `record` tag.
	pub counts: std::collections::BTreeMap<String, u64>,
	pub allocations: Vec<AllocationTotal>,
	/// Σ `held` over every allocation.
	pub total_held: String,
	/// Σ `shares_outstanding` over every allocation.
	pub total_outstanding: String,
	/// Outbox events neither dispatched nor parked at the snapshot. The window needs `0`:
	/// anything else is money that had not reached TigerBeetle when it was read.
	pub outbox_undispatched: u64,
	/// Σ units and cash the ledger showed as pending or locked at the read (in-flight
	/// two-phase transfers: a redemption's reserved burn, a withdrawal's reserve). The window
	/// needs `0`: a pending leg can still post or void, so the posted balances are not final.
	/// Added after schema 1 shipped, with a default, so a file without it reads as `0` and
	/// a reader of the earlier shape ignores it: the schema version did not change.
	#[serde(default = "zero_units")]
	pub pending_or_locked: String,
	/// Every allocation balanced, nothing in the outbox and nothing pending or locked.
	pub clean: bool,
}

fn zero_units() -> String {
	"0".to_owned()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_record_carries_its_kind_next_to_its_fields() {
		let line = serde_json::to_string(&Record::ShareHolding(ShareHolding {
			service: "service_arb".into(),
			kind: HoldingKind::UserShares,
			user_id: Some("u".into()),
			units: "13000000000000000000000".into(),
		}))
		.unwrap();
		assert_eq!(
			line,
			r#"{"record":"share_holding","service":"service_arb","kind":"user_shares","user_id":"u","units":"13000000000000000000000"}"#
		);
		let back = serde_json::from_str::<Record>(&line).unwrap();
		assert_eq!(back.kind(), "share_holding");
	}
}
