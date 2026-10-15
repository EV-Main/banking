//! Browser-facing JSON DTOs. These reproduce the exact wire shape the old TS BFF
//! emitted (proto-loader with `keepCase` + `longs: String`): snake_case fields, with
//! 64-bit integers rendered as strings so the committed `shared/contracts/gen` types
//! stay valid and the React fetch code is unchanged.

use evbanking_contracts::banking::v1 as bk;
use evconcierge_contracts::concierge::v1 as cc;
use serde::Serialize;

/// Declares one of the list DTOs: a `Vec` of an already-converted element type, the scalar
/// fields the wire shape carries alongside it, and the `From` that builds it from the proto
/// message. Every name is spelled out at the call site rather than derived from the DTO's,
/// because none of them line up reliably — the field is not always the plural of the element
/// type (`ParkedEventList.events`), and the source message is not always `<Dto>`
/// (`AdminUserList` comes from `cc::ListUsersResponse`).
///
/// The second form names the source binding so extra fields can be given as expressions over
/// it, which is what a field needing more than a move requires (`total: r.total.to_string()`).
macro_rules! list_dto {
	($(#[$meta:meta])* $name:ident from $src:path { $field:ident: Vec<$elem:ty> $(,)? }) => {
		list_dto! { $(#[$meta])* $name from $src as l { $field: Vec<$elem> } }
	};
	(
		$(#[$meta:meta])*
		$name:ident from $src:path as $msg:ident {
			$field:ident: Vec<$elem:ty>
			$(, $(#[$xmeta:meta])* $extra:ident: $xty:ty = $value:expr)* $(,)?
		}
	) => {
		$(#[$meta])*
		#[derive(Serialize)]
		pub struct $name {
			pub $field: Vec<$elem>,
			$($(#[$xmeta])* pub $extra: $xty,)*
		}

		impl From<$src> for $name {
			fn from($msg: $src) -> Self {
				Self {
					$field: $msg.$field.into_iter().map(<$elem>::from).collect(),
					$($extra: $value,)*
				}
			}
		}
	};
}

// ── /api/auth/session ────────────────────────────────────────────────────────
// Note the camelCase principal: the old BFF mapped the proto `user_id` → `userId`
// for this endpoint only (the session principal), unlike the snake_case passthroughs.

// ── concierge: user profile + sessions ───────────────────────────────────────

#[derive(Serialize)]
pub struct UserProfile {
	pub user_id: String,
	pub email: String,
	pub email_verified: bool,
	pub status: String,
	pub token_version: String,
	pub legal_name: String,
	pub preferred_name: String,
	pub phone: String,
	pub date_of_birth: String,
	pub nationality: String,
	pub tax_residence: String,
	pub residential_address: String,
	pub language: String,
	pub base_currency: String,
	pub timezone: String,
	pub kyc_level: u32,
	pub role: String,
	/// True when `role` came from the OWNER_SUBJECTS emergency allowlist rather than
	/// the persisted `users.role`. The console must say so: an "owner" the consilium
	/// has never heard of is exactly the divergence this whole change removes.
	pub role_is_break_glass: bool,
	/// WHY the account is disabled — and therefore which control the console may offer.
	/// `"admin_hold"` lapses and one admin lifts it; `"governance"` never lapses and only
	/// the owners lift it; EMPTY on an active user and also on one suspended before the
	/// field existed, which carries the old one-act, never-lapsing semantics. Three cases,
	/// not two: a boolean here would silently file the third under whichever of the other
	/// two it was compared against.
	pub suspended_by: String,
	/// Unix seconds an `admin_hold` lapses; `"0"` when nothing lapses. A string like every
	/// other stamp the BFF emits — see [`AdminUserSummary::created_at`].
	pub hold_expires_at: String,
	/// What the caller may do, concrete (no alias, no wildcard). Only `/api/users` (GetMe)
	/// fills it; every other profile the BFF relays carries it empty, which means "not
	/// reported", not "holds none".
	pub permissions: Vec<String>,
}

impl From<cc::UserProfile> for UserProfile {
	fn from(p: cc::UserProfile) -> Self {
		Self {
			user_id: p.user_id,
			email: p.email,
			email_verified: p.email_verified,
			status: p.status,
			token_version: p.token_version.to_string(),
			legal_name: p.legal_name,
			preferred_name: p.preferred_name,
			phone: p.phone,
			date_of_birth: p.date_of_birth,
			nationality: p.nationality,
			tax_residence: p.tax_residence,
			residential_address: p.residential_address,
			language: p.language,
			base_currency: p.base_currency,
			timezone: p.timezone,
			kyc_level: p.kyc_level,
			role: p.role,
			role_is_break_glass: p.role_is_break_glass,
			suspended_by: p.suspended_by,
			hold_expires_at: p.hold_expires_at.to_string(),
			permissions: p.permissions,
		}
	}
}

/// One grant of a target inside a tenant's namespace. `orphaned`: the tenant's current
/// catalog defines nothing the target names, so it grants nothing. Stamps cross as strings
/// like every other int64.
#[derive(Serialize)]
pub struct PermissionGrant {
	pub id: String,
	pub user_id: String,
	pub target: String,
	pub granted_by: String,
	pub granted_at: String,
	pub reason: String,
	pub orphaned: bool,
}

impl From<cc::PermissionGrant> for PermissionGrant {
	fn from(g: cc::PermissionGrant) -> Self {
		Self {
			id: g.id.to_string(),
			user_id: g.user_id,
			target: g.target,
			granted_by: g.granted_by,
			granted_at: g.granted_at.to_string(),
			reason: g.reason,
			orphaned: g.orphaned,
		}
	}
}

/// One holder of a grant for the allocation's "Access" tab. Both names are empty for a
/// caller who is only a delegate: the team sees each other's email and grant, nothing more.
#[derive(Serialize)]
pub struct GrantHolder {
	pub grant: PermissionGrant,
	pub email: String,
	pub legal_name: String,
	pub preferred_name: String,
}

/// A namespace's active grants, oldest first. A row concierge sends without its grant
/// names nobody the tab could act on, so it is dropped rather than rendered half-empty.
#[derive(Serialize)]
pub struct GrantHolderList {
	pub holders: Vec<GrantHolder>,
}

impl From<cc::ListGrantsResponse> for GrantHolderList {
	fn from(r: cc::ListGrantsResponse) -> Self {
		Self {
			holders: r
				.holders
				.into_iter()
				.filter_map(|h| {
					Some(GrantHolder {
						grant: h.grant?.into(),
						email: h.email,
						legal_name: h.legal_name,
						preferred_name: h.preferred_name,
					})
				})
				.collect(),
		}
	}
}

// ── piggybank: wallet ────────────────────────────────────────────────────────

/// The unified balance by lifecycle: `total = available + in_orders + invested +
/// pending_withdrawal + in_compartments`. `in_orders` is the cash the book holds for the
/// caller's resting buy orders, `in_compartments` the caller's money in scope compartments —
/// both still theirs, so inside `total` and outside `available`.
#[derive(Default, Serialize)]
pub struct Balance {
	pub available: String,
	pub in_orders: String,
	pub invested: String,
	pub pending_withdrawal: String,
	pub in_compartments: String,
	pub total: String,
}

impl From<bk::Balance> for Balance {
	fn from(b: bk::Balance) -> Self {
		Self {
			available: b.available,
			in_orders: b.in_orders,
			invested: b.invested,
			pending_withdrawal: b.pending_withdrawal,
			in_compartments: b.in_compartments,
			total: b.total,
		}
	}
}

#[derive(Serialize)]
pub struct DepositAddress {
	pub network: String,
	pub address: String,
	pub min_confirmations: u32,
	pub is_testnet: bool,
}

impl From<bk::DepositAddress> for DepositAddress {
	fn from(d: bk::DepositAddress) -> Self {
		Self {
			network: d.network,
			address: d.address,
			min_confirmations: d.min_confirmations,
			is_testnet: d.is_testnet,
		}
	}
}

#[derive(Serialize)]
pub struct NetworkWithdrawable {
	pub network: String,
	pub withdrawable: String,
	pub instant: String,
	pub min_withdrawal: String,
	pub withdrawal_fee: String,
}

impl From<bk::NetworkWithdrawable> for NetworkWithdrawable {
	fn from(n: bk::NetworkWithdrawable) -> Self {
		Self {
			network: n.network,
			withdrawable: n.withdrawable,
			instant: n.instant,
			min_withdrawal: n.min_withdrawal,
			withdrawal_fee: n.withdrawal_fee,
		}
	}
}

#[derive(Serialize)]
pub struct Wallet {
	pub balance: Balance,
	pub deposit_addresses: Vec<DepositAddress>,
	pub withdrawable: Vec<NetworkWithdrawable>,
}

impl From<bk::Wallet> for Wallet {
	fn from(w: bk::Wallet) -> Self {
		Self {
			balance: w.balance.map(Balance::from).unwrap_or_default(),
			deposit_addresses: w.deposit_addresses.into_iter().map(DepositAddress::from).collect(),
			withdrawable: w.withdrawable.into_iter().map(NetworkWithdrawable::from).collect(),
		}
	}
}

#[derive(Serialize)]
pub struct Withdrawal {
	pub id: String,
	pub network: String,
	pub address: String,
	pub amount: String,
	pub fee: String,
	pub net_amount: String,
	pub state: String,
	pub tx_ref: String,
}

impl From<bk::Withdrawal> for Withdrawal {
	fn from(w: bk::Withdrawal) -> Self {
		Self {
			id: w.id,
			network: w.network,
			address: w.address,
			amount: w.amount,
			fee: w.fee,
			net_amount: w.net_amount,
			state: w.state,
			tx_ref: w.tx_ref,
		}
	}
}

list_dto! { WithdrawalList from bk::WithdrawalList { withdrawals: Vec<Withdrawal> } }

#[derive(Serialize)]
pub struct Deposit {
	pub tx_ref: String,
	pub network: String,
	pub amount: String,
	pub created_at: String,
}

impl From<bk::Deposit> for Deposit {
	fn from(d: bk::Deposit) -> Self {
		Self {
			tx_ref: d.tx_ref,
			network: d.network,
			amount: d.amount,
			created_at: d.created_at.to_string(),
		}
	}
}

list_dto! { DepositList from bk::DepositList { deposits: Vec<Deposit> } }

/// A scope's invoice as its would-be payer sees it. The amount and the payee are the
/// server's, never the page's: the browser only names the invoice.
#[derive(Serialize)]
pub struct PayableInvoice {
	pub invoice_id: String,
	pub scope: String,
	pub display_name: String,
	pub amount: String,
	pub memo: String,
	pub state: String,
	pub expires_at: String,
	pub paid_at: String,
	pub paid_from: String,
	pub available_main: String,
	pub available_scope: String,
}

impl From<bk::PayableInvoice> for PayableInvoice {
	fn from(i: bk::PayableInvoice) -> Self {
		Self {
			invoice_id: i.invoice_id,
			scope: i.scope,
			display_name: i.display_name,
			amount: i.amount,
			memo: i.memo,
			state: i.state,
			expires_at: i.expires_at.to_string(),
			paid_at: i.paid_at.to_string(),
			paid_from: i.paid_from,
			available_main: i.available_main,
			available_scope: i.available_scope,
		}
	}
}

/// One of the caller's mandates: what a scope's service may draw on their compartment there.
/// Caps and usage are decimal USDT strings; `operations` are `reserve`, `charge`, `book_fill`.
#[derive(Serialize)]
pub struct Mandate {
	pub mandate_id: String,
	pub scope: String,
	pub client_id: String,
	pub client_display_name: String,
	pub cap_per_op: String,
	pub cap_24h: String,
	pub cap_outstanding: String,
	pub cap_book_fill_24h: String,
	pub operations: Vec<&'static str>,
	/// Unix seconds as strings; `"0"` = none.
	pub expires_at: String,
	pub created_at: String,
	pub revoked_at: String,
	pub replaced_at: String,
	pub used_24h: String,
	pub outstanding: String,
	pub book_filled_24h: String,
}

/// The wire name of a mandate operation, as the page sends it back on a grant.
pub fn mandate_operation_name(raw: i32) -> &'static str {
	match bk::MandateOperation::try_from(raw) {
		Ok(bk::MandateOperation::Reserve) => "reserve",
		Ok(bk::MandateOperation::Charge) => "charge",
		Ok(bk::MandateOperation::BookFill) => "book_fill",
		Ok(bk::MandateOperation::Unspecified) | Err(_) => "unspecified",
	}
}

impl From<bk::Mandate> for Mandate {
	fn from(m: bk::Mandate) -> Self {
		Self {
			mandate_id: m.mandate_id,
			scope: m.scope,
			client_id: m.client_id,
			client_display_name: m.client_display_name,
			cap_per_op: m.cap_per_op,
			cap_24h: m.cap_24h,
			cap_outstanding: m.cap_outstanding,
			cap_book_fill_24h: m.cap_book_fill_24h,
			operations: m.operations.into_iter().map(mandate_operation_name).collect(),
			expires_at: m.expires_at.to_string(),
			created_at: m.created_at.to_string(),
			revoked_at: m.revoked_at.to_string(),
			replaced_at: m.replaced_at.to_string(),
			used_24h: m.used_24h,
			outstanding: m.outstanding,
			book_filled_24h: m.book_filled_24h,
		}
	}
}

list_dto! { MandateList from bk::MandateList { mandates: Vec<Mandate> } }

// ── piggybank: operations (the unified activity timeline) ────────────────────

/// One row of the caller's activity timeline. `kind` is the discriminator: it decides
/// which of the remaining fields carry a value, and the rest arrive as empty strings
/// (proto3 defaults). Passed through verbatim — the BFF adds no interpretation to a
/// read model that already has exactly one shape.
#[derive(Serialize)]
pub struct Operation {
	pub id: String,
	pub kind: String,
	pub state: String,
	/// Unix seconds, rendered as a string like every other timestamp on this surface —
	/// an i64 does not survive JSON's 2^53 in every client.
	pub created_at: String,
	pub amount: String,
	pub fee: String,
	pub net_amount: String,
	pub units: String,
	pub nav: String,
	pub service: String,
	pub network: String,
	pub address: String,
	pub tx_ref: String,
	/// fee only — the two legs of a charge, so a timeline row can say whether it was rent
	/// on the capital or a share of the gain rather than only how much was taken.
	pub management: String,
	pub performance: String,
}

impl From<bk::Operation> for Operation {
	fn from(o: bk::Operation) -> Self {
		Self {
			id: o.id,
			kind: o.kind,
			state: o.state,
			created_at: o.created_at.to_string(),
			amount: o.amount,
			fee: o.fee,
			net_amount: o.net_amount,
			units: o.units,
			nav: o.nav,
			service: o.service,
			network: o.network,
			address: o.address,
			tx_ref: o.tx_ref,
			management: o.management,
			performance: o.performance,
		}
	}
}

list_dto! {
	OperationList from bk::OperationList as l {
		operations: Vec<Operation>,
		/// True when the history is longer than the page returned.
		truncated: bool = l.truncated,
	}
}

// ── piggybank: fees (operator) ───────────────────────────────────────────────

list_dto! {
	/// Every configured policy, for the operator's fees table. The element type is the same
	/// `FeePolicy` the investor read returns — the operator sees no more of a fund's terms
	/// than the people paying them do.
	FeePolicyList from bk::FeePolicyList { policies: Vec<FeePolicy> }
}

/// The manager's uncollected fee units in one fund. `value` is `units × current NAV` —
/// what a settlement would convert them to if it ran now.
#[derive(Serialize)]
pub struct FeeShares {
	pub service: String,
	pub units: String,
	pub value: String,
}

impl From<bk::FeeShares> for FeeShares {
	fn from(f: bk::FeeShares) -> Self {
		Self {
			service: f.service,
			units: f.units,
			value: f.value,
		}
	}
}

#[derive(Serialize)]
pub struct FeeSettlement {
	pub service: String,
	pub units: String,
	pub nav: String,
	pub cash: String,
}

impl From<bk::FeeSettlement> for FeeSettlement {
	fn from(s: bk::FeeSettlement) -> Self {
		Self {
			service: s.service,
			units: s.units,
			nav: s.nav,
			cash: s.cash,
		}
	}
}

/// One charge against one holding. `charged_cash` is below `management + performance`
/// exactly when the holding could not cover the whole charge and the rest went to
/// `debt_carried`.
#[derive(Serialize)]
pub struct FeeAssessment {
	pub service: String,
	pub trigger: String,
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

impl From<bk::FeeAssessment> for FeeAssessment {
	fn from(a: bk::FeeAssessment) -> Self {
		Self {
			service: a.service,
			trigger: a.trigger,
			nav: a.nav,
			management: a.management,
			performance: a.performance,
			debt_opening: a.debt_opening,
			charged_units: a.charged_units,
			charged_cash: a.charged_cash,
			debt_carried: a.debt_carried,
			high_water_mark: a.high_water_mark,
			assessed_at: a.assessed_at.to_string(),
		}
	}
}

list_dto! { FeeAssessmentList from bk::FeeAssessmentList { assessments: Vec<FeeAssessment> } }

// ── piggybank: allocations (the registry of investable products) ─────────────

/// One catalog entry. Carries no money — units/NAV/P&L come from the funds surface
/// keyed by the same `service`.
#[derive(Serialize)]
pub struct Allocation {
	pub service: String,
	pub title: String,
	pub summary: String,
	/// `draft` | `open` | `closed`.
	pub state: String,
	pub created_at: String,
	pub updated_at: String,
	/// Authorised unit supply, decimal. Subscribe refuses a mint that would pass it.
	pub unit_cap: String,
	/// Catalog glyph the client maps onto one of its SVGs — presentation only. Always
	/// present and always one of the contract's values (`fund` when the operator picked
	/// none), so a client never has to render a missing field. A client reading a
	/// *cached* allocation from before this field existed still has to tolerate its
	/// absence.
	pub icon: String,
	/// The product's default access level: `hidden` | `view` | `invest`. What the
	/// admin console edits; an investor screen renders the card off the field below.
	pub access: String,
	/// The level the CALLER effectively holds — `max(access, their grant)`. The
	/// subscribe control keys off this, never off `state` alone: an `open` product at
	/// `view` is one the investor can see and cannot buy into. Honest for an admin too
	/// (their permission does not make them an investor), so the console must not read
	/// it as "what investors get".
	pub caller_access: String,
	/// What stands behind the units: `cash` | `in_kind`. Gates money on the way OUT: a
	/// redemption pays out of the fund's cash claim, and an `in_kind` product has none
	/// there, so the hub refuses `Redeem` on it (412) and holders exit through the book.
	/// The client draws the redeem control off this, never off `state` alone.
	pub backing: String,
}

impl From<bk::Allocation> for Allocation {
	fn from(a: bk::Allocation) -> Self {
		Self {
			service: a.service,
			title: a.title,
			summary: a.summary,
			state: a.state,
			unit_cap: a.unit_cap,
			icon: a.icon,
			access: a.access,
			caller_access: a.caller_access,
			backing: a.backing,
			created_at: a.created_at.to_string(),
			updated_at: a.updated_at.to_string(),
		}
	}
}

list_dto! { AllocationList from bk::AllocationList { allocations: Vec<Allocation> } }

/// One investor raised above a product's default level. `user_id` and `granted_by` are
/// the ids the console carries for a person — the concierge id when the bridge has
/// mirrored them, the banking id as a fallback — the same order the hub accepts on a
/// grant or revoke, so either can be echoed straight back. `email` is filled in by the
/// grants route from the identity plane and is `null` when the directory cannot name
/// the id (a banking-only mirror, or a directory outage): the grant still lists.
/// `granted_at` crosses as a string like every other int64.
#[derive(Serialize)]
pub struct AllocationAccessGrant {
	pub service: String,
	pub user_id: String,
	pub email: Option<String>,
	/// `view` | `invest`.
	pub level: String,
	pub granted_by: String,
	pub granted_at: String,
}

impl From<bk::AllocationAccessGrant> for AllocationAccessGrant {
	fn from(g: bk::AllocationAccessGrant) -> Self {
		Self {
			service: g.service,
			user_id: g.user_id,
			email: None,
			level: g.level,
			granted_by: g.granted_by,
			granted_at: g.granted_at.to_string(),
		}
	}
}

list_dto! { AllocationAccessGrantList from bk::AllocationAccessGrantList { grants: Vec<AllocationAccessGrant> } }

/// One in-kind issuance — units moved with no cash behind them: minted (`source: mint`,
/// by an operator or an executed holder grant) or burnt out of the holder's account
/// (`source: retire`, supply shrank). `units` is always the magnitude; `source` is the
/// direction. `holder_id` is a BANKING user id for a `user` holder and the holding
/// allocation's slug for an `allocation` holder (the `fee` allocation holding a product's
/// fee class) — never resolve the latter as a user. `state` is `queued` until the relay posts the
/// leg, then `applied`; the console polls for the latter before it shows the holder
/// their units.
#[derive(Serialize)]
pub struct UnitIssuance {
	pub id: String,
	pub service: String,
	/// `user` | `allocation`.
	pub holder_kind: String,
	pub holder_id: String,
	pub units: String,
	pub nav: String,
	pub cost_basis: String,
	/// `queued` | `applied`.
	pub state: String,
	pub created_at: String,
	/// `mint` | `retire`.
	pub source: String,
}

impl From<bk::UnitIssuance> for UnitIssuance {
	fn from(i: bk::UnitIssuance) -> Self {
		Self {
			id: i.id,
			service: i.service,
			holder_kind: i.holder_kind,
			holder_id: i.holder_id,
			units: i.units,
			nav: i.nav,
			cost_basis: i.cost_basis,
			state: i.state,
			created_at: i.created_at.to_string(),
			source: i.source,
		}
	}
}

/// One holder of an allocation's units, as the cap table and the treasury name them:
/// `kind` is `user` (`id` is the banking user id) or `allocation` (`id` is the holding
/// allocation's slug — the reserved `fee` allocation holding a product's fee class). A
/// screen resolves a `user` through the directory and renders an `allocation` by its
/// slug; it never looks the latter up as a person.
#[derive(Serialize)]
pub struct UnitHolderRef {
	/// `user` | `allocation`.
	pub kind: String,
	pub id: String,
}

impl From<bk::UnitHolderRef> for UnitHolderRef {
	fn from(h: bk::UnitHolderRef) -> Self {
		Self { kind: h.kind, id: h.id }
	}
}

/// One line of a cap table: who, and how many units (decimal). Zero holdings are not
/// listed. `holder` is never `null`: the plane always names one, and a line without a
/// holder would be the very "unit that is nobody's" #245 rules out — it is dropped
/// rather than rendered as a blank row.
#[derive(Serialize)]
pub struct UnitHolding {
	pub holder: UnitHolderRef,
	pub units: String,
}

impl UnitHolding {
	fn from_wire(h: bk::UnitHolding) -> Option<Self> {
		Some(Self {
			holder: h.holder.map(UnitHolderRef::from)?,
			units: h.units,
		})
	}
}

/// A product's cap table (#245): the settled supply and every holder of it, largest
/// first — people and the `fee` allocation alike, each a line. `queued_units` are mints
/// recorded but not yet on the ledger — the console must not pin the cap to
/// `units_outstanding` while it is non-zero.
#[derive(Serialize)]
pub struct UnitHolders {
	pub service: String,
	pub units_outstanding: String,
	pub queued_units: String,
	pub holders: Vec<UnitHolding>,
}

impl From<bk::UnitHolders> for UnitHolders {
	fn from(h: bk::UnitHolders) -> Self {
		Self {
			service: h.service,
			units_outstanding: h.units_outstanding,
			queued_units: h.queued_units,
			holders: h.holders.into_iter().filter_map(UnitHolding::from_wire).collect(),
		}
	}
}

// ── piggybank: funds (the service currency) ──────────────────────────────────

#[derive(Serialize)]
pub struct Position {
	pub service: String,
	pub units: String,
	pub nav: String,
	pub value: String,
	pub cost_basis: String,
	pub pnl: String,
	pub nav_as_of: String,
}

impl From<bk::Position> for Position {
	fn from(p: bk::Position) -> Self {
		Self {
			service: p.service,
			units: p.units,
			nav: p.nav,
			value: p.value,
			cost_basis: p.cost_basis,
			pnl: p.pnl,
			nav_as_of: p.nav_as_of.to_string(),
		}
	}
}

list_dto! { PositionList from bk::PositionList { positions: Vec<Position> } }

#[derive(Serialize)]
pub struct Subscription {
	pub id: String,
	pub service: String,
	pub cash: String,
	pub nav: String,
	pub units: String,
}

impl From<bk::Subscription> for Subscription {
	fn from(s: bk::Subscription) -> Self {
		Self {
			id: s.id,
			service: s.service,
			cash: s.cash,
			nav: s.nav,
			units: s.units,
		}
	}
}

#[derive(Serialize)]
pub struct Redemption {
	pub id: String,
	pub service: String,
	pub units: String,
	pub nav: String,
	pub cash: String,
	pub state: String,
}

impl From<bk::Redemption> for Redemption {
	fn from(r: bk::Redemption) -> Self {
		Self {
			id: r.id,
			service: r.service,
			units: r.units,
			nav: r.nav,
			cash: r.cash,
			state: r.state,
		}
	}
}

list_dto! { RedemptionList from bk::RedemptionList { redemptions: Vec<Redemption> } }

#[derive(Serialize)]
pub struct FundNav {
	pub service: String,
	pub nav: String,
	pub aum: String,
	pub units_outstanding: String,
	pub posted_at: String,
	pub stale: bool,
	pub unit_cap: String,
	/// Units still issuable — already nets off in-flight mints, so a screen offering it
	/// can never offer more than Subscribe accepts.
	pub remaining_capacity: String,
}

impl From<bk::FundNav> for FundNav {
	fn from(f: bk::FundNav) -> Self {
		Self {
			service: f.service,
			nav: f.nav,
			aum: f.aum,
			units_outstanding: f.units_outstanding,
			posted_at: f.posted_at.to_string(),
			stale: f.stale,
			unit_cap: f.unit_cap,
			remaining_capacity: f.remaining_capacity,
		}
	}
}

/// One posted valuation mark. Stamps are strings like every timestamp here.
#[derive(Serialize)]
pub struct NavMark {
	pub nav: String,
	pub aum: String,
	pub posted_at: String,
}

impl From<bk::NavMark> for NavMark {
	fn from(m: bk::NavMark) -> Self {
		Self {
			nav: m.nav,
			aum: m.aum,
			posted_at: m.posted_at.to_string(),
		}
	}
}

/// The caller's holding valued at one instant (units held then × the NAV in force then).
#[derive(Serialize)]
pub struct ParticipationPoint {
	pub at: String,
	pub value: String,
}

impl From<bk::ParticipationPoint> for ParticipationPoint {
	fn from(p: bk::ParticipationPoint) -> Self {
		Self {
			at: p.at.to_string(),
			value: p.value,
		}
	}
}

/// The two series of the performance chart, oldest first. `participation` ends at the
/// window's end priced off the live holding, so its last point equals the position card;
/// `truncated` says the window held more marks than the cap and the OLDEST were dropped.
#[derive(Serialize)]
pub struct FundNavHistory {
	pub allocation: String,
	pub marks: Vec<NavMark>,
	pub participation: Vec<ParticipationPoint>,
	pub truncated: bool,
}

impl From<bk::FundNavHistory> for FundNavHistory {
	fn from(h: bk::FundNavHistory) -> Self {
		Self {
			allocation: h.allocation,
			marks: h.marks.into_iter().map(NavMark::from).collect(),
			participation: h.participation.into_iter().map(ParticipationPoint::from).collect(),
			truncated: h.truncated,
		}
	}
}

// ── fees ──────────────────────────────────────────────────────────────────────

/// A fund's fee terms as the investor should read them. `configured` distinguishes "this
/// product charges nothing" from "we could not tell you" — the client renders no fee
/// section at all in the first case rather than a row of zeros that looks like a policy.
#[derive(Serialize)]
pub struct FeePolicy {
	pub service: String,
	pub configured: bool,
	pub management_bps: u32,
	pub performance_bps: u32,
	pub hurdle_bps: u32,
	pub basis: String,
	pub crystallization: String,
	pub updated_at: String,
	/// Which version of the fund's history these terms are; `0` when unconfigured.
	pub version: u32,
	/// Unix seconds (as a string, like every timestamp here) since which they bind.
	pub effective_from: String,
	/// The change on its way — awaiting the owners or scheduled — or `null`. Shown to
	/// holders and non-holders alike: the terms coming are part of deciding to stay in.
	pub pending: Option<FeePolicyChange>,
}

impl From<bk::FeePolicy> for FeePolicy {
	fn from(p: bk::FeePolicy) -> Self {
		Self {
			service: p.service,
			configured: p.configured,
			management_bps: p.management_bps,
			performance_bps: p.performance_bps,
			hurdle_bps: p.hurdle_bps,
			basis: p.basis,
			crystallization: p.crystallization,
			updated_at: p.updated_at.to_string(),
			version: p.version,
			effective_from: p.effective_from.to_string(),
			pending: p.pending.map(FeePolicyChange::from),
		}
	}
}

/// One row of a fund's fee-policy history — the shape `ScheduleFeePolicy`,
/// `CancelFeePolicyChange` and `AcknowledgeUndeliveredNotices` answer with, and the rows of
/// `/api/admin/fees/changes`. `state` is awaiting_consilium | scheduled | active |
/// superseded | rejected | cancelled; `requirement` is admin | owner_consilium. Timestamps
/// are unix seconds as strings, `"0"` where the moment has not come.
#[derive(Serialize)]
pub struct FeePolicyChange {
	pub id: String,
	pub service: String,
	pub version: u32,
	pub state: String,
	pub management_bps: u32,
	pub performance_bps: u32,
	pub hurdle_bps: u32,
	pub basis: String,
	pub crystallization: String,
	pub effective_from: String,
	pub requirement: String,
	/// The consilium this change waits on; `null` for an administrator's change.
	pub consilium_id: Option<String>,
	pub requested_by: String,
	pub requested_at: String,
	pub scheduled_at: String,
	pub applied_at: String,
	pub reason: String,
	/// Who took responsibility for holders who could not be told — the latest to, when the
	/// record was extended; `null` until someone did.
	pub notices_waived_by: Option<String>,
	/// How the identity plane names `notices_waived_by`; `null` when nobody did or the
	/// directory cannot name the id (banking#328). Filled by the route, not the proto — the
	/// money plane stores no email.
	pub notices_waived_by_email: Option<String>,
	/// When; `"0"` until someone did.
	pub notices_waived_at: String,
	/// The holders (banking user ids) the acknowledgement covers; empty until someone did.
	pub notices_waived_users: Vec<String>,
	/// Notices of this change not yet delivered to a current holder, how many of those the
	/// mailer has given up on — what the pending card says "N holders could not be told"
	/// from — and how many of the given-up ones no acknowledgement covers yet: the ones a
	/// (further) acknowledgement is offered over. Counted only while `scheduled`.
	pub undelivered_notices: u32,
	pub notices_given_up: u32,
	pub notices_unacknowledged: u32,
}

impl From<bk::FeePolicyChange> for FeePolicyChange {
	fn from(c: bk::FeePolicyChange) -> Self {
		Self {
			id: c.id,
			service: c.service,
			version: c.version,
			state: c.state,
			management_bps: c.management_bps,
			performance_bps: c.performance_bps,
			hurdle_bps: c.hurdle_bps,
			basis: c.basis,
			crystallization: c.crystallization,
			effective_from: c.effective_from.to_string(),
			requirement: c.requirement,
			consilium_id: non_empty(c.consilium_id),
			requested_by: c.requested_by,
			requested_at: c.requested_at.to_string(),
			scheduled_at: c.scheduled_at.to_string(),
			applied_at: c.applied_at.to_string(),
			reason: c.reason,
			notices_waived_by: non_empty(c.notices_waived_by),
			notices_waived_by_email: None,
			notices_waived_at: c.notices_waived_at.to_string(),
			notices_waived_users: c.notices_waived_users,
			undelivered_notices: c.undelivered_notices,
			notices_given_up: c.notices_given_up,
			notices_unacknowledged: c.notices_unacknowledged,
		}
	}
}

list_dto! { FeePolicyChangeList from bk::FeePolicyChangeList { changes: Vec<FeePolicyChange> } }

/// What a holding owes right now without being charged. `high_water_mark` travels with it
/// because it is the single number that explains why two investors in the same fund at the
/// same price owe different performance fees.
#[derive(Serialize)]
pub struct AccruedFees {
	pub service: String,
	pub configured: bool,
	pub management: String,
	pub performance: String,
	pub debt: String,
	pub total: String,
	pub high_water_mark: String,
}

impl From<bk::AccruedFees> for AccruedFees {
	fn from(a: bk::AccruedFees) -> Self {
		Self {
			service: a.service,
			configured: a.configured,
			management: a.management,
			performance: a.performance,
			debt: a.debt,
			total: a.total,
			high_water_mark: a.high_water_mark,
		}
	}
}

// ── piggybank: book (the secondary market in an allocation's units) ──────────
// Every amount, size and price crosses as the decimal string the hub sent; every int64
// timestamp and uint64 revision as a string, like the rest of this wire. The string
// vocabularies (`side`, `kind`, `tif`, `state`, `resolution`) are pinned in
// `evbanking_contracts::book` and relayed verbatim.

/// One order. `user_id` is populated only on the caller's own orders. `price` is the
/// limit — for a market order, the one the hub derived from the quote at placement.
/// `avg_fill_price` is empty until the first fill; `reject_reason` is set only on
/// `rejected`, `cancel_reason` only on `cancelled`.
#[derive(Serialize)]
pub struct Order {
	pub id: String,
	pub service: String,
	pub user_id: String,
	/// `buy` | `sell`.
	pub side: String,
	/// `limit` | `market`.
	pub kind: String,
	/// `gtc` | `ioc` | `alo`.
	pub tif: String,
	pub price: String,
	pub size: String,
	pub filled: String,
	pub remaining: String,
	pub avg_fill_price: String,
	pub fee_paid: String,
	/// `open` | `partially_filled` | `filled` | `cancelled` | `rejected`.
	pub state: String,
	pub reject_reason: String,
	/// `user` | `ioc_remainder` | `market_remainder`; empty unless `cancelled`.
	pub cancel_reason: String,
	pub client_order_id: String,
	pub created_at: String,
	pub updated_at: String,
}

impl From<bk::Order> for Order {
	fn from(o: bk::Order) -> Self {
		Self {
			id: o.id,
			service: o.service,
			user_id: o.user_id,
			side: o.side,
			kind: o.kind,
			tif: o.tif,
			price: o.price,
			size: o.size,
			filled: o.filled,
			remaining: o.remaining,
			avg_fill_price: o.avg_fill_price,
			fee_paid: o.fee_paid,
			state: o.state,
			reject_reason: o.reject_reason,
			cancel_reason: o.cancel_reason,
			client_order_id: o.client_order_id,
			created_at: o.created_at.to_string(),
			updated_at: o.updated_at.to_string(),
		}
	}
}

list_dto! { OrderList from bk::OrderList { orders: Vec<Order> } }

/// One fill. On the public tape `user_side`, `order_id` and `fee` are empty; on the
/// caller's own tape they are the caller's side, their order and what they paid as taker
/// (empty when they were the maker). A counterparty is never disclosed on either.
#[derive(Serialize)]
pub struct Trade {
	pub id: String,
	pub service: String,
	pub price: String,
	pub size: String,
	/// `buy` | `sell` — the side that crossed the spread.
	pub taker_side: String,
	pub executed_at: String,
	pub user_side: String,
	pub order_id: String,
	pub fee: String,
}

impl From<bk::Trade> for Trade {
	fn from(t: bk::Trade) -> Self {
		Self {
			id: t.id,
			service: t.service,
			price: t.price,
			size: t.size,
			taker_side: t.taker_side,
			executed_at: t.executed_at.to_string(),
			user_side: t.user_side,
			order_id: t.order_id,
			fee: t.fee,
		}
	}
}

list_dto! { TradeList from bk::TradeList { trades: Vec<Trade> } }

/// One aggregated price level of one side.
#[derive(Serialize)]
pub struct BookLevel {
	pub price: String,
	pub size: String,
	pub orders: u32,
}

impl From<bk::BookLevel> for BookLevel {
	fn from(l: bk::BookLevel) -> Self {
		Self {
			price: l.price,
			size: l.size,
			orders: l.orders,
		}
	}
}

/// The book as of `revision`: `bids` best (highest) first, `asks` best (lowest) first.
/// `last_price`/`last_side` are empty before the first trade; `mid`/`spread` unless both
/// sides have a level; `change_24h` when there is no trade a day back. `nav` is the fund's
/// accounting mark, carried so a ticker can show both prices side by side.
#[derive(Serialize)]
pub struct BookSnapshot {
	pub service: String,
	pub revision: String,
	pub bids: Vec<BookLevel>,
	pub asks: Vec<BookLevel>,
	pub last_price: String,
	/// `buy` | `sell` — the taker's side of the last trade.
	pub last_side: String,
	pub mid: String,
	pub spread: String,
	pub nav: String,
	pub volume_24h: String,
	/// Signed decimal percent, e.g. `-2.5`.
	pub change_24h: String,
	pub as_of: String,
}

impl From<bk::BookSnapshot> for BookSnapshot {
	fn from(s: bk::BookSnapshot) -> Self {
		Self {
			service: s.service,
			revision: s.revision.to_string(),
			bids: s.bids.into_iter().map(BookLevel::from).collect(),
			asks: s.asks.into_iter().map(BookLevel::from).collect(),
			last_price: s.last_price,
			last_side: s.last_side,
			mid: s.mid,
			spread: s.spread,
			nav: s.nav,
			volume_24h: s.volume_24h,
			change_24h: s.change_24h,
			as_of: s.as_of.to_string(),
		}
	}
}

/// One OHLCV bucket; `time` is the bucket's start. Empty buckets are omitted by the
/// hub — a chart carries the last close forward itself.
#[derive(Serialize)]
pub struct Candle {
	pub time: String,
	pub open: String,
	pub high: String,
	pub low: String,
	pub close: String,
	pub volume: String,
}

impl From<bk::Candle> for Candle {
	fn from(c: bk::Candle) -> Self {
		Self {
			time: c.time.to_string(),
			open: c.open,
			high: c.high,
			low: c.low,
			close: c.close,
			volume: c.volume,
		}
	}
}

list_dto! {
	CandleList from bk::CandleList as l {
		candles: Vec<Candle>,
		service: String = l.service,
		/// `1m` | `5m` | `15m` | `1h` | `4h` | `1d`.
		resolution: String = l.resolution,
	}
}

/// One allocation's trading terms. `updated_at` is `"0"` when the product has no policy
/// row — the defaults, and a closed book.
#[derive(Serialize)]
pub struct BookPolicy {
	pub service: String,
	pub book_open: bool,
	pub taker_fee_bps: u32,
	pub price_tick: String,
	pub lot_size: String,
	pub market_slippage_bps: u32,
	/// The operator acknowledged that the units trade unbacked by fund cash; the terminal
	/// tells buyers the book is their only exit.
	pub allow_unbacked_trading: bool,
	pub updated_at: String,
}

impl From<bk::BookPolicy> for BookPolicy {
	fn from(p: bk::BookPolicy) -> Self {
		Self {
			service: p.service,
			book_open: p.book_open,
			taker_fee_bps: p.taker_fee_bps,
			price_tick: p.price_tick,
			lot_size: p.lot_size,
			market_slippage_bps: p.market_slippage_bps,
			allow_unbacked_trading: p.allow_unbacked_trading,
			updated_at: p.updated_at.to_string(),
		}
	}
}

/// One frame of the live book feed. `orders_revision` is the book revision at which the
/// CALLER's own orders on this allocation last changed (`"0"` = never): the client
/// refetches `/api/book/orders` only when it moves.
#[derive(Serialize)]
pub struct BookEvent {
	/// `None` only if the hub sent a frame without one, which the contract does not allow;
	/// kept optional rather than invented so a client sees the gap instead of an empty book.
	pub snapshot: Option<BookSnapshot>,
	/// The latest public trades, newest first.
	pub trades: Vec<Trade>,
	pub orders_revision: String,
}

impl From<bk::BookEvent> for BookEvent {
	fn from(e: bk::BookEvent) -> Self {
		Self {
			snapshot: e.snapshot.map(BookSnapshot::from),
			trades: e.trades.into_iter().map(Trade::from).collect(),
			orders_revision: e.orders_revision.to_string(),
		}
	}
}

// ── admin console ─────────────────────────────────────────────────────────────

/// A user row in the operator user list.
#[derive(Serialize)]
pub struct AdminUserSummary {
	pub user_id: String,
	pub email: String,
	pub status: String,
	pub kyc_level: u32,
	pub role: String,
	pub role_is_break_glass: bool,
	pub token_version: String,
	pub created_at: String,
	/// Why the account is disabled — on the LIST and not only the drawer, because a held
	/// account and a suspended one need different action buttons and a console that cannot
	/// tell them apart offers the wrong one. See [`UserProfile::suspended_by`] for why
	/// this is three cases rather than two.
	pub suspended_by: String,
	/// Unix seconds an `admin_hold` lapses; `"0"` when nothing lapses.
	pub hold_expires_at: String,
}

impl From<cc::AdminUserSummary> for AdminUserSummary {
	fn from(u: cc::AdminUserSummary) -> Self {
		Self {
			user_id: u.user_id,
			email: u.email,
			status: u.status,
			kyc_level: u.kyc_level,
			role: u.role,
			role_is_break_glass: u.role_is_break_glass,
			token_version: u.token_version.to_string(),
			created_at: u.created_at.to_string(),
			suspended_by: u.suspended_by,
			hold_expires_at: u.hold_expires_at.to_string(),
		}
	}
}

list_dto! {
	AdminUserList from cc::ListUsersResponse as r {
		users: Vec<AdminUserSummary>,
		total: String = r.total.to_string(),
	}
}

/// A live user balance (the user-detail drawer): `amount = main + in_compartments`.
#[derive(Serialize)]
pub struct UserBalance {
	pub amount: String,
	pub pending: String,
	pub authoritative: bool,
	pub as_of: String,
	pub main: String,
	pub in_compartments: String,
}

impl From<bk::UserBalanceResponse> for UserBalance {
	fn from(b: bk::UserBalanceResponse) -> Self {
		Self {
			amount: b.amount,
			pending: b.pending,
			authoritative: b.authoritative,
			as_of: b.as_of.to_string(),
			main: b.main,
			in_compartments: b.in_compartments,
		}
	}
}

/// Per-rail liquidity plus the operator funding view (`treasury_*`/`onchain_*` are
/// best-effort chain reads — empty means the rail is unconfigured or the read was
/// unavailable, never an error).
#[derive(Serialize)]
pub struct RailLiquidity {
	pub network: String,
	pub custody: String,
	pub treasury_address: String,
	pub onchain_usdt: String,
	pub onchain_gas: String,
	/// The rail's sweep gas-station wallet — fund native coin here (never USDT).
	pub gas_station_address: String,
	pub gas_station_gas: String,
	/// Whether the addresses above are testnet-tagged (TON renders a different friendly
	/// address per realm); false on the rails whose address form is network-agnostic.
	pub is_testnet: bool,
}

/// An allocation's cash claim, read off one ledger balance: `posted` is settled,
/// `reserved` is spoken for by approved payments out of it, `available` is the difference.
#[derive(Default, Serialize)]
pub struct AllocationClaim {
	pub posted: String,
	pub available: String,
	pub reserved: String,
}

impl From<bk::AllocationClaim> for AllocationClaim {
	fn from(c: bk::AllocationClaim) -> Self {
		Self {
			posted: c.posted,
			available: c.available,
			reserved: c.reserved,
		}
	}
}

/// One allocation as the treasury shows it (#245): name, cash, supply, price and who
/// holds it — the same shape for a product and for the hidden `fee` / `fund` allocations
/// (`access: hidden`), which is also what the Revenue screen reads for `fee`.
/// `nav_posted_at` is `"0"` while nothing under it has been marked (seed-priced). A
/// missing `claim` on the wire (a plane older than this contract) crosses as zeros
/// rather than `null`, so a figure is always a figure.
#[derive(Serialize)]
pub struct AllocationTreasury {
	pub service: String,
	pub title: String,
	/// `hidden` | `view` | `invest`.
	pub access: String,
	pub claim: AllocationClaim,
	pub units_outstanding: String,
	pub nav: String,
	pub nav_posted_at: String,
	pub holders: Vec<UnitHolding>,
	/// banking's own treasury beside the `fee` allocation (D17) — set on the Revenue
	/// screen's answer only, absent from every treasury line.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub banking_treasury: Option<AllocationClaim>,
}

impl From<bk::AllocationTreasury> for AllocationTreasury {
	fn from(a: bk::AllocationTreasury) -> Self {
		Self {
			service: a.service,
			title: a.title,
			access: a.access,
			claim: a.claim.map(AllocationClaim::from).unwrap_or_default(),
			units_outstanding: a.units_outstanding,
			nav: a.nav,
			nav_posted_at: a.nav_posted_at_unix.to_string(),
			holders: a.holders.into_iter().filter_map(UnitHolding::from_wire).collect(),
			banking_treasury: a.banking_treasury.map(AllocationClaim::from),
		}
	}
}

/// The two-layer treasury picture (Treasury screen): per-rail custody, and who the claims
/// on it belong to — people directly (`held_by_users`) or through the units of an
/// allocation (`allocations`, the hidden `fee` and `fund` included). No remainder: each
/// figure is read off its own accounts.
#[derive(Serialize)]
pub struct Treasury {
	pub rails: Vec<RailLiquidity>,
	pub bank: String,
	pub total_custody: String,
	pub reserved_for_withdrawals: String,
	pub held_by_users: String,
	pub allocations: Vec<AllocationTreasury>,
	/// banking's own treasury (D17), a line of its own; zeros from a plane older than it.
	pub banking_treasury: AllocationClaim,
	/// Σ users' scope compartments — still people's.
	pub held_in_compartments: String,
}

impl From<bk::Treasury> for Treasury {
	fn from(t: bk::Treasury) -> Self {
		Self {
			rails: t
				.rails
				.into_iter()
				.map(|r| RailLiquidity {
					network: r.network,
					custody: r.custody,
					treasury_address: r.treasury_address,
					onchain_usdt: r.onchain_usdt,
					onchain_gas: r.onchain_gas,
					gas_station_address: r.gas_station_address,
					gas_station_gas: r.gas_station_gas,
					is_testnet: r.is_testnet,
				})
				.collect(),
			bank: t.bank,
			total_custody: t.total_custody,
			reserved_for_withdrawals: t.reserved_for_withdrawals,
			held_by_users: t.held_by_users,
			allocations: t.allocations.into_iter().map(AllocationTreasury::from).collect(),
			banking_treasury: t.banking_treasury.map(AllocationClaim::from).unwrap_or_default(),
			held_in_compartments: t.held_in_compartments,
		}
	}
}

/// What `POST /api/admin/treasury/seed-capital` answers: a seed is a consilium, not a
/// write, so `recorded` is always false and `consilium_id` is where the proposal lives.
#[derive(Serialize)]
pub struct SeedCapitalProposal {
	pub recorded: bool,
	pub amount: String,
	pub consilium_id: String,
}

impl From<bk::SeedCapitalResponse> for SeedCapitalProposal {
	fn from(r: bk::SeedCapitalResponse) -> Self {
		Self {
			recorded: r.recorded,
			amount: r.amount,
			consilium_id: r.consilium_id,
		}
	}
}

/// One outbox row the relay parked — the "money didn't move" set (Outbox screen).
/// `reason` is the relay's last error; a `compensated` row already ran its recovery and
/// must never be unparked (the hub refuses).
#[derive(Serialize)]
pub struct ParkedEvent {
	pub seq: String,
	pub event_id: String,
	pub aggregate: String,
	pub aggregate_id: String,
	pub kind: String,
	pub reason: String,
	pub parked_at: String,
	pub compensated: bool,
}

impl From<bk::ParkedEvent> for ParkedEvent {
	fn from(e: bk::ParkedEvent) -> Self {
		Self {
			seq: e.seq.to_string(),
			event_id: e.event_id,
			aggregate: e.aggregate,
			aggregate_id: e.aggregate_id,
			kind: e.kind,
			reason: e.reason,
			parked_at: e.parked_at.to_string(),
			compensated: e.compensated,
		}
	}
}

list_dto! { ParkedEventList from bk::ParkedEventList { events: Vec<ParkedEvent> } }

/// One queued redemption in the Valuation screen's queue.
#[derive(Serialize)]
pub struct RedemptionQueueItem {
	pub redemption_id: String,
	pub user_id: String,
	pub email: String,
	pub service: String,
	pub units: String,
	pub created_at: String,
}

#[derive(Serialize)]
pub struct RedemptionQueue {
	pub items: Vec<RedemptionQueueItem>,
}

impl From<bk::RedemptionQueue> for RedemptionQueue {
	fn from(q: bk::RedemptionQueue) -> Self {
		Self {
			items: q
				.items
				.into_iter()
				.map(|i| RedemptionQueueItem {
					redemption_id: i.redemption_id,
					user_id: i.user_id,
					email: i.email,
					service: i.service,
					units: i.units,
					created_at: i.created_at.to_string(),
				})
				.collect(),
		}
	}
}

/// One withdrawal awaiting operator action (admin Withdrawals screen). `source` is `user`
/// (the row names its investor) or `treasury` — an approved payment out of banking's
/// treasury (D17), with empty `user_id` and `email`.
#[derive(Serialize)]
pub struct WithdrawalQueueItem {
	pub withdrawal_id: String,
	pub source: String,
	pub user_id: String,
	pub email: String,
	pub network: String,
	pub address: String,
	pub amount: String,
	pub net_amount: String,
	pub state: String,
	pub created_at: String,
}

#[derive(Serialize)]
pub struct WithdrawalQueue {
	pub items: Vec<WithdrawalQueueItem>,
}

impl From<bk::WithdrawalQueue> for WithdrawalQueue {
	fn from(q: bk::WithdrawalQueue) -> Self {
		Self {
			items: q
				.items
				.into_iter()
				.map(|i| WithdrawalQueueItem {
					withdrawal_id: i.withdrawal_id,
					source: i.source,
					user_id: i.user_id,
					email: i.email,
					network: i.network,
					address: i.address,
					amount: i.amount,
					net_amount: i.net_amount,
					state: i.state,
					created_at: i.created_at.to_string(),
				})
				.collect(),
		}
	}
}

/// A rail on the Coins screen.
#[derive(Serialize)]
pub struct Rail {
	pub network: String,
	pub configured: bool,
	pub frozen: bool,
	pub gas_coin: String,
}

impl From<bk::Rail> for Rail {
	fn from(r: bk::Rail) -> Self {
		Self {
			network: r.network,
			configured: r.configured,
			frozen: r.frozen,
			gas_coin: r.gas_coin,
		}
	}
}

/// The money-plane read-only kill-switch state (Cabinet screen).
#[derive(Serialize)]
pub struct OperationsMode {
	pub read_only: bool,
}

impl From<bk::OperationsMode> for OperationsMode {
	fn from(m: bk::OperationsMode) -> Self {
		Self { read_only: m.read_only }
	}
}

#[derive(Serialize)]
pub struct FeatureFlag {
	pub key: String,
	pub description: String,
	pub enabled: bool,
	pub rollout: u32,
}

/// The platform/cabinet config (Cabinet screen: maintenance, announcement, flags).
#[derive(Serialize)]
pub struct PlatformConfig {
	pub maintenance_mode: bool,
	pub announcement_title: String,
	pub announcement_body: String,
	pub announcement_active: bool,
	pub flags: Vec<FeatureFlag>,
}

impl From<cc::PlatformConfig> for PlatformConfig {
	fn from(c: cc::PlatformConfig) -> Self {
		Self {
			maintenance_mode: c.maintenance_mode,
			announcement_title: c.announcement_title,
			announcement_body: c.announcement_body,
			announcement_active: c.announcement_active,
			flags: c
				.flags
				.into_iter()
				.map(|f| FeatureFlag {
					key: f.key,
					description: f.description,
					enabled: f.enabled,
					rollout: f.rollout,
				})
				.collect(),
		}
	}
}

/// One inbox entry. Unix-second timestamps are rendered as strings for the same
/// reason every other i64 here is: the old proto-loader emitted `longs: String`, and
/// the frontend's generated types still expect that shape.
#[derive(serde::Serialize)]
pub struct Notification {
	pub id: String,
	pub topic: String,
	pub kind: String,
	pub title: String,
	pub body: String,
	pub link: String,
	pub occurred_at: String,
	pub created_at: String,
	/// "0" while unread.
	pub read_at: String,
}

list_dto! {
	NotificationList from cc::ListNotificationsResponse as r {
		notifications: Vec<Notification>,
		next_cursor: String = r.next_cursor,
		unread_count: u32 = r.unread_count,
	}
}

#[derive(serde::Serialize)]
pub struct UnreadCount {
	pub unread_count: u32,
}

#[derive(serde::Serialize)]
pub struct MarkReadResult {
	pub marked: u32,
	pub unread_count: u32,
}

#[derive(serde::Serialize)]
pub struct TopicSubscription {
	pub topic: String,
	pub label: String,
	pub description: String,
	pub subscribed: bool,
	pub email_enabled: bool,
}

/// The whole delivery surface. Both master switches may be false at once — that is a
/// supported state, so the frontend must not treat it as a broken record.
#[derive(serde::Serialize)]
pub struct NotificationSettings {
	pub in_app_enabled: bool,
	pub email_enabled: bool,
	pub email: String,
	pub email_verified: bool,
	pub topics: Vec<TopicSubscription>,
}

impl From<cc::Notification> for Notification {
	fn from(n: cc::Notification) -> Self {
		Self {
			id: n.id,
			topic: n.topic,
			kind: n.kind,
			title: n.title,
			body: n.body,
			link: n.link,
			occurred_at: n.occurred_at.to_string(),
			created_at: n.created_at.to_string(),
			read_at: n.read_at.to_string(),
		}
	}
}

impl From<cc::GetUnreadCountResponse> for UnreadCount {
	fn from(r: cc::GetUnreadCountResponse) -> Self {
		Self { unread_count: r.unread_count }
	}
}

impl From<cc::MarkReadResponse> for MarkReadResult {
	fn from(r: cc::MarkReadResponse) -> Self {
		Self {
			marked: r.marked,
			unread_count: r.unread_count,
		}
	}
}

impl From<cc::NotificationSettings> for NotificationSettings {
	fn from(s: cc::NotificationSettings) -> Self {
		Self {
			in_app_enabled: s.in_app_enabled,
			email_enabled: s.email_enabled,
			email: s.email,
			email_verified: s.email_verified,
			topics: s
				.topics
				.into_iter()
				.map(|t| TopicSubscription {
					topic: t.topic,
					label: t.label,
					description: t.description,
					subscribed: t.subscribed,
					email_enabled: t.email_enabled,
				})
				.collect(),
		}
	}
}

// ── consilium: multi-owner authorization (money plane) ───────────────────────

/// A proto enum on the browser wire: the generated `as_str_name()` minus its
/// `SCREAMING_PREFIX_`, lowercased. Derived rather than hand-matched so a variant added
/// to the proto reaches the browser as itself instead of falling through a stale arm.
fn enum_label(name: &str, prefix: &str) -> String {
	name.strip_prefix(prefix).unwrap_or(name).to_ascii_lowercase()
}

/// `ada@example.com` → `a***@example.com`. Applied on every redacted surface a non-owner
/// can reach; idempotent, so re-masking what the plane already masked is safe and this
/// stays a second line of defense rather than a substitute for the plane's own. A value
/// with no `@`, or with no local part, is replaced wholesale — something that is not an
/// address is not something to reveal a prefix of.
pub fn mask_email(email: &str) -> String {
	if email.is_empty() {
		return String::new();
	}
	match email.split_once('@') {
		// `chars().next()` and not `&local[..1]`: a non-ASCII local part would put that
		// slice inside a UTF-8 character and panic.
		Some((local, domain)) if !domain.is_empty() => match local.chars().next() {
			Some(first) => format!("{first}***@{domain}"),
			None => "***".to_string(),
		},
		_ => "***".to_string(),
	}
}

#[derive(Serialize)]
pub struct ConsiliumVoter {
	pub user_id: String,
	pub email: String,
	pub decision: String,
	pub decided_at: String,
	pub notified: bool,
}

impl From<bk::ConsiliumVoter> for ConsiliumVoter {
	fn from(v: bk::ConsiliumVoter) -> Self {
		let decision = enum_label(v.decision().as_str_name(), "VOTE_DECISION_");
		Self {
			user_id: v.user_id,
			email: v.email,
			decision,
			decided_at: v.decided_at.to_string(),
			notified: v.notified,
		}
	}
}

/// The immutable subject of a PAYMENT consilium — a projection of the order, not the
/// order. The plane renders both ends as LABELS (the wording the approval mail carries),
/// so each is delivered as a [`PaymentEnd`] with only `label` populated: the consilium
/// screen and the payments screen then read one shape, and the label is the only part of
/// it an approval is bound to.
#[derive(Serialize)]
pub struct ConsiliumPaymentTerms {
	pub payment_id: String,
	pub tier: String,
	pub source: PaymentEnd,
	pub destination: PaymentEnd,
	pub amount: String,
	pub reason: String,
}

impl From<bk::ConsiliumPaymentTerms> for ConsiliumPaymentTerms {
	fn from(t: bk::ConsiliumPaymentTerms) -> Self {
		Self {
			payment_id: t.payment_id,
			tier: t.tier,
			source: PaymentEnd::labelled(t.source),
			destination: PaymentEnd::labelled(t.destination),
			amount: t.amount,
			reason: t.reason,
		}
	}
}

/// The immutable subject of a VALUATION-OVERRIDE consilium (banking#232): the fund and the
/// AUM to mark it at, past the NAV-move guard. Both are covered by `payload_hash`.
#[derive(Serialize)]
pub struct ValuationOverrideTerms {
	pub service: String,
	pub aum: String,
}

impl From<bk::ValuationOverrideTerms> for ValuationOverrideTerms {
	fn from(t: bk::ValuationOverrideTerms) -> Self {
		Self { service: t.service, aum: t.aum }
	}
}

/// One set of fee terms as a consilium states them.
#[derive(Serialize)]
pub struct FeeTerms {
	pub management_bps: u32,
	pub performance_bps: u32,
	pub hurdle_bps: u32,
	pub basis: String,
	pub crystallization: String,
}

/// The immutable subject of a FEE-POLICY consilium: which change, over which fund, from
/// what terms (`null` when the fund charged nothing) to what terms, and why. `holder_count`
/// and `allocation_name` are read live beside the hashed subject.
#[derive(Serialize)]
pub struct ConsiliumFeePolicyTerms {
	pub change_id: String,
	pub service: String,
	pub allocation_name: String,
	pub from: Option<FeeTerms>,
	pub to: FeeTerms,
	/// Unix seconds the requester asked the change to bind from; `"0"` = as soon as allowed.
	pub effective_from: String,
	pub holder_count: u32,
	pub reason: String,
}

impl From<bk::ConsiliumFeePolicyTerms> for ConsiliumFeePolicyTerms {
	fn from(t: bk::ConsiliumFeePolicyTerms) -> Self {
		Self {
			change_id: t.change_id,
			service: t.service,
			allocation_name: t.allocation_name,
			from: t.from_configured.then_some(FeeTerms {
				management_bps: t.from_management_bps,
				performance_bps: t.from_performance_bps,
				hurdle_bps: t.from_hurdle_bps,
				basis: t.from_basis,
				crystallization: t.from_crystallization,
			}),
			to: FeeTerms {
				management_bps: t.to_management_bps,
				performance_bps: t.to_performance_bps,
				hurdle_bps: t.to_hurdle_bps,
				basis: t.to_basis,
				crystallization: t.to_crystallization,
			},
			effective_from: t.effective_from.to_string(),
			holder_count: t.holder_count,
			reason: t.reason,
		}
	}
}

/// The immutable subject of a holder-grant consilium (#245): `units` of the reserved
/// `allocation` (`fee` | `fund`) for `user_id` — the money-plane id the terms stored.
#[derive(Serialize)]
pub struct HolderGrantTerms {
	pub allocation: String,
	pub user_id: String,
	pub units: String,
}

impl From<bk::HolderGrantTerms> for HolderGrantTerms {
	fn from(t: bk::HolderGrantTerms) -> Self {
		Self {
			allocation: t.allocation,
			user_id: t.user_id,
			units: t.units,
		}
	}
}

/// The immutable subject of a seed-capital consilium (#245): the chain transfer
/// `tx_ref` on `network`, worth `amount`, is `depositor_user_id`'s — booked as their
/// deposit and subscription into `fund` once the quorum carries. The reference is
/// carried in FULL, as a payout's address is.
#[derive(Serialize)]
pub struct SeedCapitalTerms {
	pub tx_ref: String,
	pub network: String,
	pub amount: String,
	pub depositor_user_id: String,
}

impl From<bk::SeedCapitalTerms> for SeedCapitalTerms {
	fn from(t: bk::SeedCapitalTerms) -> Self {
		Self {
			tx_ref: t.tx_ref,
			network: t.network,
			amount: t.amount,
			depositor_user_id: t.depositor_user_id,
		}
	}
}

/// One consilium in full — the owner-only view, with the per-voter breakdown.
///
/// Exactly one of `payment`, `valuation_override`, `fee_policy`, `holder_grant` and
/// `seed_capital` describes the subject; the others are `null` — never an empty object,
/// which the invitation page treats as unrenderable — so a screen tells the kinds apart by
/// which sibling is set. (`revenue_payout` left with the kind, #245 / C-9.)
#[derive(Serialize)]
pub struct Consilium {
	pub id: String,
	pub state: String,
	pub payment: Option<ConsiliumPaymentTerms>,
	pub valuation_override: Option<ValuationOverrideTerms>,
	pub fee_policy: Option<ConsiliumFeePolicyTerms>,
	/// Set exactly when this is a HOLDER_GRANT consilium.
	pub holder_grant: Option<HolderGrantTerms>,
	/// Set exactly when this is a SEED_CAPITAL consilium.
	pub seed_capital: Option<SeedCapitalTerms>,
	pub payload_hash: String,
	pub initiator_user_id: String,
	pub initiator_email: String,
	pub owner_count: u32,
	pub threshold: u32,
	pub approvals: u32,
	pub rejections: u32,
	pub voters: Vec<ConsiliumVoter>,
	pub created_at: String,
	pub expires_at: String,
	pub decided_at: String,
	pub executed_withdrawal_id: String,
	/// The order an executed PAYMENT consilium carried; `null` otherwise.
	pub executed_payment_id: Option<String>,
	/// The mark an executed VALUATION-OVERRIDE consilium recorded; `null` otherwise.
	pub executed_valuation_id: Option<String>,
	/// The change an executed FEE_POLICY consilium scheduled; `null` otherwise.
	pub executed_fee_policy_change_id: Option<String>,
	/// The in-kind issuance an executed HOLDER_GRANT consilium minted; `null` otherwise.
	pub executed_issuance_id: Option<String>,
	/// The `fund` subscription an executed SEED_CAPITAL consilium booked; `null` otherwise.
	pub executed_subscription_id: Option<String>,
	pub failure_reason: String,
	/// Monotonic per consilium. The live page watches this and refetches when it moves.
	pub version: String,
}

impl From<bk::Consilium> for Consilium {
	fn from(c: bk::Consilium) -> Self {
		let state = enum_label(c.state().as_str_name(), "CONSILIUM_STATE_");
		Self {
			id: c.id,
			state,
			payment: c.payment.map(ConsiliumPaymentTerms::from),
			valuation_override: c.valuation_override.map(ValuationOverrideTerms::from),
			fee_policy: c.fee_policy.map(ConsiliumFeePolicyTerms::from),
			holder_grant: c.holder_grant.map(HolderGrantTerms::from),
			seed_capital: c.seed_capital.map(SeedCapitalTerms::from),
			payload_hash: c.payload_hash,
			initiator_user_id: c.initiator_user_id,
			initiator_email: c.initiator_email,
			owner_count: c.owner_count,
			threshold: c.threshold,
			approvals: c.approvals,
			rejections: c.rejections,
			voters: c.voters.into_iter().map(ConsiliumVoter::from).collect(),
			created_at: c.created_at.to_string(),
			expires_at: c.expires_at.to_string(),
			decided_at: c.decided_at.to_string(),
			executed_withdrawal_id: c.executed_withdrawal_id,
			executed_payment_id: non_empty(c.executed_payment_id),
			executed_valuation_id: non_empty(c.executed_valuation_id),
			executed_fee_policy_change_id: non_empty(c.executed_fee_policy_change_id),
			executed_issuance_id: non_empty(c.executed_issuance_id),
			executed_subscription_id: non_empty(c.executed_subscription_id),
			failure_reason: c.failure_reason,
			version: c.version.to_string(),
		}
	}
}

list_dto! { ConsiliumList from bk::ConsiliumList { items: Vec<Consilium> } }

/// What the emailed owner is shown before voting. Deliberately narrower than
/// [`Consilium`]: no other owner's identity and no vote-by-vote breakdown — and both
/// addresses are masked, because this is the one surface reachable with no session.
#[derive(Default, Serialize)]
pub struct ConsiliumInvitation {
	pub consilium_id: String,
	pub state: String,
	/// Set exactly when this is a PAYMENT consilium — see [`Consilium`].
	pub payment: Option<ConsiliumPaymentTerms>,
	/// Set exactly when this is a VALUATION-OVERRIDE consilium — see [`Consilium`].
	pub valuation_override: Option<ValuationOverrideTerms>,
	/// Set exactly when this is a FEE_POLICY consilium — see [`Consilium`].
	pub fee_policy: Option<ConsiliumFeePolicyTerms>,
	/// Set exactly when this is a HOLDER_GRANT consilium — see [`Consilium`].
	pub holder_grant: Option<HolderGrantTerms>,
	/// Set exactly when this is a SEED_CAPITAL consilium — see [`Consilium`].
	pub seed_capital: Option<SeedCapitalTerms>,
	pub payload_hash: String,
	pub initiator_email: String,
	pub voter_email: String,
	pub threshold: u32,
	pub approvals: u32,
	pub owner_count: u32,
	pub created_at: String,
	pub expires_at: String,
	pub decision: String,
	pub attempts_remaining: u32,
}

impl From<bk::ConsiliumInvitation> for ConsiliumInvitation {
	fn from(i: bk::ConsiliumInvitation) -> Self {
		let state = enum_label(i.state().as_str_name(), "CONSILIUM_STATE_");
		let decision = enum_label(i.decision().as_str_name(), "VOTE_DECISION_");
		Self {
			consilium_id: i.consilium_id,
			state,
			payment: i.payment.map(ConsiliumPaymentTerms::from),
			valuation_override: i.valuation_override.map(ValuationOverrideTerms::from),
			fee_policy: i.fee_policy.map(ConsiliumFeePolicyTerms::from),
			holder_grant: i.holder_grant.map(HolderGrantTerms::from),
			seed_capital: i.seed_capital.map(SeedCapitalTerms::from),
			payload_hash: i.payload_hash,
			initiator_email: mask_email(&i.initiator_email),
			voter_email: mask_email(&i.voter_email),
			threshold: i.threshold,
			approvals: i.approvals,
			owner_count: i.owner_count,
			created_at: i.created_at.to_string(),
			expires_at: i.expires_at.to_string(),
			decision,
			attempts_remaining: i.attempts_remaining,
		}
	}
}

/// The invitation as it stands after a vote, plus whether this vote carried the verdict.
#[derive(Serialize)]
pub struct ConsiliumDecision {
	pub invitation: ConsiliumInvitation,
	pub decided: bool,
}

impl From<bk::SubmitDecisionResponse> for ConsiliumDecision {
	fn from(r: bk::SubmitDecisionResponse) -> Self {
		Self {
			invitation: r.invitation.map(ConsiliumInvitation::from).unwrap_or_default(),
			decided: r.decided,
		}
	}
}

// ── payments: the order that moves money between two named ends ──────────────
//
// The one family on this wire whose browser contract was fixed BEFORE the DTOs were
// written — the payments and consent screens are built to it in parallel — and it differs
// from the older passthroughs in two ways a reader of the neighbours will notice: unix
// stamps cross as RFC 3339 strings rather than as unix-seconds strings, and the proto's
// "empty means unset" sentinels cross as `null`. Both are deliberate: the screen renders
// what it is given and never has to know which zero means "not yet".

/// Unix seconds → RFC 3339 in UTC (`2025-09-12T09:30:00Z`). Written by hand because this
/// crate declares `time` without its `formatting` feature: the feature happens to be
/// unified in through `cookie` and `sentry-types` today, but leaning on a transitive
/// feature would make one fixed layout hostage to a dependency's own flags.
fn rfc3339(secs: i64) -> String {
	let Ok(t) = time::OffsetDateTime::from_unix_timestamp(secs) else {
		// Unrepresentable stamps cannot come from the plane (it writes `now()`); an
		// empty string is the honest rendering of one rather than a made-up date.
		return String::new();
	};
	format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
}

/// A stamp the proto leaves at 0 while the event has not happened yet.
fn rfc3339_opt(secs: i64) -> Option<String> {
	(secs != 0).then(|| rfc3339(secs))
}

/// A string the proto leaves empty when the field does not apply.
fn non_empty(value: String) -> Option<String> {
	(!value.is_empty()).then_some(value)
}

/// One end of a payment, as every approval surface describes it. `label` is the wording
/// shared with the mails; the structured fields let a screen link to the account, and
/// stay EMPTY (not `null`) where they do not apply, exactly as the proto carries them.
#[derive(Default, Serialize)]
pub struct PaymentEnd {
	pub label: String,
	/// `service` | `user`, or `external` for an address (the retired `piggybank` /
	/// `revenue` kinds are refused at the hub and no order carries them).
	pub kind: String,
	pub id: String,
	pub network: String,
	pub address: String,
	/// What a person recognises this end BY: the receiving investor's masked mailbox, the
	/// product's title. Never part of the digest.
	pub detail: String,
}

impl PaymentEnd {
	/// An end the plane rendered as words only — the consilium projection.
	fn labelled(label: String) -> Self {
		Self { label, ..Default::default() }
	}
}

impl From<bk::PaymentEnd> for PaymentEnd {
	fn from(e: bk::PaymentEnd) -> Self {
		Self {
			label: e.label,
			kind: e.kind,
			id: e.id,
			network: e.network,
			address: e.address,
			detail: e.detail,
		}
	}
}

/// The consent seat of an investor-sourced order, as the operator surface sees it.
#[derive(Serialize)]
pub struct PaymentConsent {
	/// Masked by the plane (`a***@example.com`).
	pub subject_email: String,
	/// `pending` | `approve` | `reject`.
	pub decision: String,
	pub notified: bool,
	pub attempts_remaining: u32,
	/// True when a pin recorded at open (the subject's token version, their mailbox) has
	/// moved: the seat can no longer be answered or executed.
	pub invalidated: bool,
	pub invalidation_reason: String,
}

impl From<bk::PaymentConsent> for PaymentConsent {
	fn from(c: bk::PaymentConsent) -> Self {
		let decision = enum_label(c.decision().as_str_name(), "CONSENT_DECISION_");
		Self {
			subject_email: c.subject_email,
			decision,
			notified: c.notified,
			attempts_remaining: c.attempts_remaining,
			invalidated: c.invalidated,
			invalidation_reason: c.invalidation_reason,
		}
	}
}

/// One payment order in full — the Admin/Owner view.
#[derive(Serialize)]
pub struct Payment {
	pub id: String,
	/// `pending` | `approved` | `executed` | `execution_failed` | `rejected` | `expired` |
	/// `cancelled`.
	pub state: String,
	/// `internal` | `service` | `external` — derived from the destination.
	pub tier: String,
	pub source: PaymentEnd,
	pub destination: PaymentEnd,
	pub amount: String,
	pub reason: String,
	/// `owner_consilium` | `subject_consent` — the one requirement, read off the source.
	pub requirement: String,
	pub payload_hash: String,
	pub initiator_email: String,
	/// Set exactly when the requirement is the owner consilium.
	pub consilium_id: Option<String>,
	/// Set exactly when the requirement is the subject's consent.
	pub consent: Option<PaymentConsent>,
	pub created_at: String,
	pub expires_at: String,
	/// `null` while pending.
	pub decided_at: Option<String>,
	/// Set on EXECUTED for an external order only: an internal one settled as a ledger
	/// transfer and records no id of its own.
	pub executed_withdrawal_id: Option<String>,
	pub failure_reason: Option<String>,
	/// Monotonic per order.
	pub version: u64,
}

impl From<bk::Payment> for Payment {
	fn from(p: bk::Payment) -> Self {
		let state = enum_label(p.state().as_str_name(), "PAYMENT_STATE_");
		Self {
			id: p.id,
			state,
			tier: p.tier,
			source: p.source.map(PaymentEnd::from).unwrap_or_default(),
			destination: p.destination.map(PaymentEnd::from).unwrap_or_default(),
			amount: p.amount,
			reason: p.reason,
			requirement: p.requirement,
			payload_hash: p.payload_hash,
			initiator_email: p.initiator_email,
			consilium_id: non_empty(p.consilium_id),
			consent: p.consent.map(PaymentConsent::from),
			created_at: rfc3339(p.created_at),
			expires_at: rfc3339(p.expires_at),
			decided_at: rfc3339_opt(p.decided_at),
			executed_withdrawal_id: non_empty(p.executed_withdrawal_id),
			failure_reason: non_empty(p.failure_reason),
			version: p.version,
		}
	}
}

list_dto! { PaymentList from bk::PaymentList { items: Vec<Payment> } }

/// What the emailed investor is shown before answering. Deliberately narrower than
/// [`Payment`]: the terms they are consenting to and nothing about the operator beyond a
/// masked address — and both addresses are masked here as well as by the plane, because
/// this is the one payments surface reachable with no session.
#[derive(Default, Serialize)]
pub struct PaymentConsentInvitation {
	pub payment_id: String,
	pub state: String,
	pub tier: String,
	pub source: PaymentEnd,
	pub destination: PaymentEnd,
	pub amount: String,
	pub reason: String,
	pub payload_hash: String,
	pub initiator_email: String,
	pub subject_email: String,
	pub expires_at: String,
	/// This seat's own answer, so a reopened link shows what was already said.
	pub decision: String,
	pub attempts_remaining: u32,
}

impl From<bk::PaymentConsentInvitation> for PaymentConsentInvitation {
	fn from(i: bk::PaymentConsentInvitation) -> Self {
		let state = enum_label(i.state().as_str_name(), "PAYMENT_STATE_");
		let decision = enum_label(i.decision().as_str_name(), "CONSENT_DECISION_");
		Self {
			payment_id: i.payment_id,
			state,
			tier: i.tier,
			source: i.source.map(PaymentEnd::from).unwrap_or_default(),
			destination: i.destination.map(PaymentEnd::from).unwrap_or_default(),
			amount: i.amount,
			reason: i.reason,
			payload_hash: i.payload_hash,
			initiator_email: mask_email(&i.initiator_email),
			subject_email: mask_email(&i.subject_email),
			expires_at: rfc3339(i.expires_at),
			decision,
			attempts_remaining: i.attempts_remaining,
		}
	}
}

/// The invitation as it stands after an answer, plus whether this answer decided the
/// order.
#[derive(Serialize)]
pub struct PaymentConsentDecision {
	pub invitation: PaymentConsentInvitation,
	pub decided: bool,
}

impl From<bk::SubmitConsentResponse> for PaymentConsentDecision {
	fn from(r: bk::SubmitConsentResponse) -> Self {
		Self {
			invitation: r.invitation.map(PaymentConsentInvitation::from).unwrap_or_default(),
			decided: r.decided,
		}
	}
}

// ── governance: the roster, its removals and its admissions (ownership plane) ─
//
// A seat changes hands only through a consilium in this plane. Removal passes when the
// target accepts from their mailbox OR every eligible peer votes to remove; admission
// passes only on unanimity of every other owner, which is what stops a minority minting
// sock puppets into a majority before opening a payout.

#[derive(Serialize)]
pub struct Owner {
	pub user_id: String,
	pub email: String,
	pub display_name: String,
	pub owner_since: String,
}

impl From<cc::Owner> for Owner {
	fn from(o: cc::Owner) -> Self {
		Self {
			user_id: o.user_id,
			email: o.email,
			display_name: o.display_name,
			owner_since: o.owner_since.to_string(),
		}
	}
}

list_dto! {
	OwnerList from cc::OwnerList as l {
		items: Vec<Owner>,
		/// True below THREE owners, where the money plane's `floor(N/2)+1` threshold is
		/// unreachable and no payout can be authorized. Distinct from the removal floor,
		/// which is two: two owners is a recoverable pause, because two can still admit a
		/// third, whereas a floor of three would make a bad actor unremovable.
		below_payout_floor: bool = l.below_payout_floor,
	}
}

#[derive(Serialize)]
pub struct RemovalPeer {
	pub user_id: String,
	pub email: String,
	pub vote: String,
	pub voted_at: String,
}

impl From<cc::RemovalPeer> for RemovalPeer {
	fn from(p: cc::RemovalPeer) -> Self {
		let vote = enum_label(p.vote().as_str_name(), "REMOVAL_VOTE_");
		Self {
			user_id: p.user_id,
			email: p.email,
			vote,
			voted_at: p.voted_at.to_string(),
		}
	}
}

#[derive(Serialize)]
pub struct OwnerRemoval {
	pub id: String,
	pub state: String,
	pub target_user_id: String,
	pub target_email: String,
	pub initiator_user_id: String,
	pub initiator_email: String,
	pub reason: String,
	/// Every owner except the target and the initiator. Empty means peer unanimity is
	/// unavailable and only the target's own acceptance can carry this.
	pub peers: Vec<RemovalPeer>,
	pub target_decision: String,
	pub target_decided_at: String,
	pub target_notified: bool,
	pub owner_count: u32,
	pub created_at: String,
	pub expires_at: String,
	pub decided_at: String,
	pub void_reason: String,
	pub version: String,
}

impl From<cc::OwnerRemoval> for OwnerRemoval {
	fn from(r: cc::OwnerRemoval) -> Self {
		let state = enum_label(r.state().as_str_name(), "OWNER_REMOVAL_STATE_");
		let target_decision = enum_label(r.target_decision().as_str_name(), "REMOVAL_VOTE_");
		Self {
			id: r.id,
			state,
			target_user_id: r.target_user_id,
			target_email: r.target_email,
			initiator_user_id: r.initiator_user_id,
			initiator_email: r.initiator_email,
			reason: r.reason,
			peers: r.peers.into_iter().map(RemovalPeer::from).collect(),
			target_decision,
			target_decided_at: r.target_decided_at.to_string(),
			target_notified: r.target_notified,
			owner_count: r.owner_count,
			created_at: r.created_at.to_string(),
			expires_at: r.expires_at.to_string(),
			decided_at: r.decided_at.to_string(),
			void_reason: r.void_reason,
			version: r.version.to_string(),
		}
	}
}

list_dto! { OwnerRemovalList from cc::OwnerRemovalList { items: Vec<OwnerRemoval> } }

#[derive(Serialize)]
pub struct AdmissionPeer {
	pub user_id: String,
	pub email: String,
	pub vote: String,
	pub voted_at: String,
}

impl From<cc::AdmissionPeer> for AdmissionPeer {
	fn from(p: cc::AdmissionPeer) -> Self {
		let vote = enum_label(p.vote().as_str_name(), "ADMISSION_VOTE_");
		Self {
			user_id: p.user_id,
			email: p.email,
			vote,
			voted_at: p.voted_at.to_string(),
		}
	}
}

/// Granting a seat. The shape differs from [`OwnerRemoval`] by more than a rename: there
/// is no target-decision trio, because the candidate does not vote on their own admission
/// — they are not an owner yet.
#[derive(Serialize)]
pub struct OwnerAdmission {
	pub id: String,
	pub state: String,
	pub candidate_user_id: String,
	pub candidate_email: String,
	pub initiator_user_id: String,
	pub initiator_email: String,
	pub reason: String,
	/// Every owner except the initiator. Never empty: an admission with nobody to agree
	/// is refused at open rather than left open and unpassable.
	pub peers: Vec<AdmissionPeer>,
	pub owner_count: u32,
	pub created_at: String,
	pub expires_at: String,
	pub decided_at: String,
	pub void_reason: String,
	pub version: String,
}

impl From<cc::OwnerAdmission> for OwnerAdmission {
	fn from(a: cc::OwnerAdmission) -> Self {
		let state = enum_label(a.state().as_str_name(), "OWNER_ADMISSION_STATE_");
		Self {
			id: a.id,
			state,
			candidate_user_id: a.candidate_user_id,
			candidate_email: a.candidate_email,
			initiator_user_id: a.initiator_user_id,
			initiator_email: a.initiator_email,
			reason: a.reason,
			peers: a.peers.into_iter().map(AdmissionPeer::from).collect(),
			owner_count: a.owner_count,
			created_at: a.created_at.to_string(),
			expires_at: a.expires_at.to_string(),
			decided_at: a.decided_at.to_string(),
			void_reason: a.void_reason,
			version: a.version.to_string(),
		}
	}
}

list_dto! { OwnerAdmissionList from cc::OwnerAdmissionList { items: Vec<OwnerAdmission> } }

#[derive(Serialize)]
pub struct UserProposalPeer {
	pub user_id: String,
	pub email: String,
	pub vote: String,
	pub voted_at: String,
}

impl From<cc::UserProposalPeer> for UserProposalPeer {
	fn from(p: cc::UserProposalPeer) -> Self {
		let vote = enum_label(p.vote().as_str_name(), "PROPOSAL_VOTE_");
		Self {
			user_id: p.user_id,
			email: p.email,
			vote,
			voted_at: p.voted_at.to_string(),
		}
	}
}

/// The owners' verdict over one PERSON's standing — a suspension, a reinstatement or an
/// admin admission, told apart by `kind`.
///
/// Three kinds share one shape because all three ask the same question, and the vote is
/// NEUTRAL (`for`/`against`) where the owner consilia use remove/keep and admit/reject: a
/// kind-specific verb here would only mean something read against `kind`, and a vocabulary
/// that is correct only when cross-referenced eventually gets rendered wrong. The surface
/// knows the kind and picks the verb; this says which way the voter pushed.
///
/// `threshold` is carried rather than re-derived: it is frozen at open, so a console that
/// recomputed it from the current roster would show a bar this proposal is not measured
/// against.
#[derive(Serialize)]
pub struct UserProposal {
	pub id: String,
	pub kind: String,
	pub state: String,
	/// Any user — NOT necessarily an owner. That is the difference from [`OwnerAdmission`],
	/// and the reason none of its roster checks have an analogue here.
	pub subject_user_id: String,
	pub subject_email: String,
	pub initiator_user_id: String,
	pub initiator_email: String,
	pub reason: String,
	/// Every owner except the initiator. Never empty: a proposal with nobody to agree is
	/// refused at open rather than left open and unpassable.
	pub peers: Vec<UserProposalPeer>,
	pub owner_count: u32,
	/// How many of `peers` must vote FOR, frozen at open.
	pub threshold: u32,
	pub created_at: String,
	pub expires_at: String,
	pub decided_at: String,
	pub void_reason: String,
	pub version: String,
}

impl From<cc::UserProposal> for UserProposal {
	fn from(p: cc::UserProposal) -> Self {
		let kind = enum_label(p.kind().as_str_name(), "USER_PROPOSAL_KIND_");
		let state = enum_label(p.state().as_str_name(), "USER_PROPOSAL_STATE_");
		Self {
			id: p.id,
			kind,
			state,
			subject_user_id: p.subject_user_id,
			subject_email: p.subject_email,
			initiator_user_id: p.initiator_user_id,
			initiator_email: p.initiator_email,
			reason: p.reason,
			peers: p.peers.into_iter().map(UserProposalPeer::from).collect(),
			owner_count: p.owner_count,
			threshold: p.threshold,
			created_at: p.created_at.to_string(),
			expires_at: p.expires_at.to_string(),
			decided_at: p.decided_at.to_string(),
			void_reason: p.void_reason,
			version: p.version.to_string(),
		}
	}
}

list_dto! { UserProposalList from cc::UserProposalList { items: Vec<UserProposal> } }

/// What the TARGET is shown before answering: no peer identities, no vote breakdown.
/// Both addresses are masked — this surface, too, needs no session.
#[derive(Default, Serialize)]
pub struct OwnerRemovalInvitation {
	pub removal_id: String,
	pub state: String,
	pub initiator_email: String,
	pub target_email: String,
	pub reason: String,
	pub created_at: String,
	pub expires_at: String,
	pub decision: String,
	pub attempts_remaining: u32,
}

impl From<cc::OwnerRemovalInvitation> for OwnerRemovalInvitation {
	fn from(i: cc::OwnerRemovalInvitation) -> Self {
		let state = enum_label(i.state().as_str_name(), "OWNER_REMOVAL_STATE_");
		let decision = enum_label(i.decision().as_str_name(), "REMOVAL_VOTE_");
		Self {
			removal_id: i.removal_id,
			state,
			initiator_email: mask_email(&i.initiator_email),
			target_email: mask_email(&i.target_email),
			reason: i.reason,
			created_at: i.created_at.to_string(),
			expires_at: i.expires_at.to_string(),
			decision,
			attempts_remaining: i.attempts_remaining,
		}
	}
}

#[derive(Serialize)]
pub struct RemovalDecision {
	pub invitation: OwnerRemovalInvitation,
	pub decided: bool,
}

impl From<cc::SubmitSelfDecisionResponse> for RemovalDecision {
	fn from(r: cc::SubmitSelfDecisionResponse) -> Self {
		Self {
			invitation: r.invitation.map(OwnerRemovalInvitation::from).unwrap_or_default(),
			decided: r.decided,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn masking_keeps_one_character_and_is_idempotent() {
		assert_eq!(mask_email("ada@example.com"), "a***@example.com");
		assert_eq!(mask_email("a***@example.com"), "a***@example.com");
		assert_eq!(mask_email(""), "");
		assert_eq!(mask_email("not-an-address"), "***");
		assert_eq!(mask_email("@example.com"), "***");
		assert_eq!(mask_email("ada@"), "***");
	}

	/// The wallet's five lifecycle figures cross one-to-one — `in_orders` in particular,
	/// which the cabinet shows beside `available` so a resting buy order explains the dip.
	#[test]
	fn the_balance_carries_every_lifecycle_figure() {
		let balance = Balance::from(bk::Balance {
			available: "87.88".into(),
			in_orders: "12.12".into(),
			invested: "50".into(),
			pending_withdrawal: "0".into(),
			total: "160".into(),
			in_compartments: "10".into(),
		});
		assert_eq!(
			(
				balance.available.as_str(),
				balance.in_orders.as_str(),
				balance.invested.as_str(),
				balance.pending_withdrawal.as_str(),
				balance.in_compartments.as_str(),
				balance.total.as_str()
			),
			("87.88", "12.12", "50", "0", "10", "160")
		);
	}

	/// An issuance crosses with its `source` intact: the console tells a mint (supply
	/// grew) from a retirement (it shrank) by this field alone.
	#[test]
	fn an_issuance_carries_its_source_and_holder() {
		let issuance = UnitIssuance::from(bk::UnitIssuance {
			id: "7c1e".into(),
			service: "service_arb".into(),
			holder_kind: "user".into(),
			holder_id: "d4d6".into(),
			units: "13000".into(),
			nav: "1.25".into(),
			cost_basis: "16250".into(),
			state: "queued".into(),
			created_at: 1_700_000_000,
			source: "retire".into(),
		});
		assert_eq!(
			(issuance.holder_kind.as_str(), issuance.holder_id.as_str(), issuance.source.as_str(), issuance.state.as_str()),
			("user", "d4d6", "retire", "queued")
		);
		assert_eq!(
			(issuance.units.as_str(), issuance.cost_basis.as_str(), issuance.created_at.as_str()),
			("13000", "16250", "1700000000")
		);
	}

	/// The treasury lists every allocation with its holders (#245): a holder is named by
	/// kind and id — a person or the `fee` allocation — a line without a holder is
	/// dropped rather than rendered as nobody's units, a missing claim crosses as zeros,
	/// and the seed-priced `nav_posted_at` is the string `"0"` like every stamp here.
	#[test]
	fn the_treasury_names_every_allocation_and_its_holders() {
		let holding = |kind: &str, id: &str, units: &str| bk::UnitHolding {
			holder: Some(bk::UnitHolderRef { kind: kind.into(), id: id.into() }),
			units: units.into(),
		};
		let treasury = Treasury::from(bk::Treasury {
			rails: vec![],
			bank: "0".into(),
			total_custody: "1500".into(),
			reserved_for_withdrawals: "10".into(),
			held_by_users: "400".into(),
			allocations: vec![
				bk::AllocationTreasury {
					service: "fee".into(),
					title: "Fees".into(),
					access: "hidden".into(),
					claim: Some(bk::AllocationClaim {
						posted: "100".into(),
						available: "90".into(),
						reserved: "10".into(),
					}),
					units_outstanding: "80".into(),
					nav: "1.25".into(),
					nav_posted_at_unix: 0,
					holders: vec![holding("user", "d4d6", "64"), holding("user", "e5e7", "16")],
					banking_treasury: None,
				},
				bk::AllocationTreasury {
					service: "quy-nhon".into(),
					title: "Quy Nhon".into(),
					access: "invest".into(),
					claim: None,
					units_outstanding: "1000".into(),
					nav: "1.00".into(),
					nav_posted_at_unix: 1_750_000_000,
					holders: vec![holding("allocation", "fee", "20"), bk::UnitHolding { holder: None, units: "5".into() }],
					banking_treasury: None,
				},
			],
			banking_treasury: Some(bk::AllocationClaim {
				posted: "7".into(),
				available: "5".into(),
				reserved: "2".into(),
			}),
			held_in_compartments: "30".into(),
		});
		assert_eq!((treasury.held_by_users.as_str(), treasury.total_custody.as_str()), ("400", "1500"));
		assert_eq!(
			(
				treasury.banking_treasury.posted.as_str(),
				treasury.banking_treasury.reserved.as_str(),
				treasury.held_in_compartments.as_str()
			),
			("7", "2", "30"),
			"banking's treasury and the compartments cross as lines of their own"
		);
		assert!(treasury.allocations.iter().all(|line| line.banking_treasury.is_none()), "never per allocation line");
		let [fee, product] = treasury.allocations.as_slice() else { panic!("two allocations") };
		assert_eq!((fee.service.as_str(), fee.access.as_str(), fee.nav_posted_at.as_str()), ("fee", "hidden", "0"));
		assert_eq!((fee.claim.posted.as_str(), fee.claim.available.as_str(), fee.claim.reserved.as_str()), ("100", "90", "10"));
		assert_eq!(
			fee.holders.iter().map(|h| (h.holder.kind.as_str(), h.holder.id.as_str(), h.units.as_str())).collect::<Vec<_>>(),
			[("user", "d4d6", "64"), ("user", "e5e7", "16")]
		);
		assert_eq!(
			(product.claim.posted.as_str(), product.nav_posted_at.as_str()),
			("", "1750000000"),
			"a missing claim is zeros, never null"
		);
		assert_eq!(product.holders.len(), 1, "a line with no holder is not a line");
		assert_eq!((product.holders[0].holder.kind.as_str(), product.holders[0].holder.id.as_str()), ("allocation", "fee"));
	}

	/// The cap table crosses line by line, in the hub's order, with nothing summed.
	#[test]
	fn the_cap_table_keeps_the_hubs_order_and_sums_nothing() {
		let holders = UnitHolders::from(bk::UnitHolders {
			service: "quy-nhon".into(),
			units_outstanding: "13250".into(),
			queued_units: "100".into(),
			holders: vec![
				bk::UnitHolding {
					holder: Some(bk::UnitHolderRef {
						kind: "user".into(),
						id: "d4d6".into(),
					}),
					units: "13000".into(),
				},
				bk::UnitHolding {
					holder: Some(bk::UnitHolderRef {
						kind: "allocation".into(),
						id: "fee".into(),
					}),
					units: "250".into(),
				},
			],
		});
		assert_eq!((holders.units_outstanding.as_str(), holders.queued_units.as_str()), ("13250", "100"));
		assert_eq!(
			holders
				.holders
				.iter()
				.map(|h| (h.holder.kind.as_str(), h.holder.id.as_str(), h.units.as_str()))
				.collect::<Vec<_>>(),
			[("user", "d4d6", "13000"), ("allocation", "fee", "250")]
		);
		let json = serde_json::to_value(&holders).unwrap();
		for retired in ["company_units", "fee_units", "investor_units"] {
			assert!(json.get(retired).is_none(), "`{retired}` is not on the wire any more");
		}
	}

	/// The two #245 kinds cross as their own siblings, `null` on every other kind, and
	/// their effect ids follow the same empty-means-null rule as the older ones.
	#[test]
	fn a_holder_grant_and_a_seed_are_their_own_terms_siblings() {
		let grant = Consilium::from(bk::Consilium {
			holder_grant: Some(bk::HolderGrantTerms {
				allocation: "fund".into(),
				user_id: "d4d6".into(),
				units: "1000".into(),
			}),
			executed_issuance_id: "7c1e".into(),
			..Default::default()
		});
		let terms = grant.holder_grant.as_ref().expect("the grant terms");
		assert_eq!((terms.allocation.as_str(), terms.user_id.as_str(), terms.units.as_str()), ("fund", "d4d6", "1000"));
		assert!(grant.seed_capital.is_none() && grant.payment.is_none() && grant.valuation_override.is_none() && grant.fee_policy.is_none());
		assert_eq!(grant.executed_issuance_id.as_deref(), Some("7c1e"));
		assert_eq!(grant.executed_subscription_id, None);

		let seed = ConsiliumInvitation::from(bk::ConsiliumInvitation {
			seed_capital: Some(bk::SeedCapitalTerms {
				tx_ref: "0xabc:0".into(),
				network: "bep20".into(),
				amount: "250.5".into(),
				depositor_user_id: "d4d6".into(),
			}),
			..Default::default()
		});
		let terms = seed.seed_capital.as_ref().expect("the seed terms");
		assert_eq!(
			(terms.tx_ref.as_str(), terms.network.as_str(), terms.amount.as_str(), terms.depositor_user_id.as_str()),
			("0xabc:0", "bep20", "250.5", "d4d6")
		);
		assert!(seed.holder_grant.is_none());
	}

	/// A non-ASCII local part must not be sliced mid-character — a byte slice would panic
	/// on one, and a panic inside a handler is a 500 on a page a stranger can reach.
	#[test]
	fn masking_survives_a_non_ascii_local_part() {
		assert_eq!(mask_email("ада@example.com"), "а***@example.com");
	}

	/// The browser vocabulary is derived from the proto, so a state added upstream
	/// arrives as itself rather than through a stale hand-written arm.
	#[test]
	fn enum_labels_are_the_proto_name_without_its_prefix() {
		assert_eq!(enum_label(bk::ConsiliumState::Open.as_str_name(), "CONSILIUM_STATE_"), "open");
		assert_eq!(enum_label(bk::ConsiliumState::ExecutionFailed.as_str_name(), "CONSILIUM_STATE_"), "execution_failed");
		assert_eq!(enum_label(bk::VoteDecision::Approve.as_str_name(), "VOTE_DECISION_"), "approve");
	}

	/// The redacted invitation is the one surface a stranger holding a link can read: no
	/// address on it may be renderable back to a real mailbox.
	#[test]
	fn the_public_invitation_masks_both_addresses() {
		let invitation = ConsiliumInvitation::from(bk::ConsiliumInvitation {
			initiator_email: "ada@example.com".into(),
			voter_email: "grace@example.com".into(),
			..Default::default()
		});
		assert_eq!(invitation.initiator_email, "a***@example.com");
		assert_eq!(invitation.voter_email, "g***@example.com");
	}

	/// The same rule on the consent page: the initiator and the subject are the only two
	/// addresses on it, and a stranger with the link may read neither.
	#[test]
	fn the_consent_invitation_masks_both_addresses() {
		let invitation = PaymentConsentInvitation::from(bk::PaymentConsentInvitation {
			initiator_email: "ada@example.com".into(),
			subject_email: "grace@example.com".into(),
			..Default::default()
		});
		assert_eq!(invitation.initiator_email, "a***@example.com");
		assert_eq!(invitation.subject_email, "g***@example.com");
	}

	/// The hand-written layout against known dates, including a leap day and the end of a
	/// year — the two places a homegrown calendar goes wrong.
	#[test]
	fn unix_seconds_render_as_rfc3339_utc() {
		assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
		assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
		assert_eq!(rfc3339(1_757_669_400), "2025-09-12T09:30:00Z");
		assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
		assert_eq!(rfc3339_opt(0), None);
		assert_eq!(rfc3339_opt(1_757_669_400).as_deref(), Some("2025-09-12T09:30:00Z"));
	}

	/// The proto's "empty means unset" sentinels cross as `null` on this family, so the
	/// screen never has to know that `0` means "not decided" and `""` means "no consilium".
	#[test]
	fn a_pending_payment_carries_nulls_where_nothing_has_happened() {
		let payment = Payment::from(bk::Payment {
			state: bk::PaymentState::Pending as i32,
			created_at: 1_757_669_400,
			expires_at: 1_757_928_600,
			..Default::default()
		});
		assert_eq!(payment.state, "pending");
		assert_eq!(payment.created_at, "2025-09-12T09:30:00Z");
		assert!(payment.decided_at.is_none());
		assert!(payment.consilium_id.is_none() && payment.consent.is_none());
		assert!(payment.executed_withdrawal_id.is_none() && payment.failure_reason.is_none());
		assert_eq!(payment.version, 0);

		let executed = Payment::from(bk::Payment {
			state: bk::PaymentState::ExecutionFailed as i32,
			consilium_id: "c-1".into(),
			decided_at: 1_757_669_400,
			failure_reason: "rail unconfigured".into(),
			version: 3,
			..Default::default()
		});
		assert_eq!(executed.state, "execution_failed");
		assert_eq!(executed.consilium_id.as_deref(), Some("c-1"));
		assert_eq!(executed.decided_at.as_deref(), Some("2025-09-12T09:30:00Z"));
		assert_eq!(executed.failure_reason.as_deref(), Some("rail unconfigured"));
		assert_eq!(executed.version, 3);
	}

	/// A payment consilium's ends arrive from the plane as words; they land in `label`,
	/// and the structured fields stay empty rather than invented.
	#[test]
	fn a_payment_consilium_carries_its_terms_as_labelled_ends() {
		let consilium = Consilium::from(bk::Consilium {
			payment: Some(bk::ConsiliumPaymentTerms {
				payment_id: "p-1".into(),
				tier: "external".into(),
				source: "the fund's pooled capital".into(),
				destination: "0xabc on BEP20".into(),
				amount: "10.00".into(),
				reason: "rent".into(),
			}),
			executed_payment_id: "p-1".into(),
			..Default::default()
		});
		let terms = consilium.payment.expect("a payment consilium carries its terms");
		assert_eq!(terms.source.label, "the fund's pooled capital");
		assert_eq!(terms.destination.label, "0xabc on BEP20");
		assert_eq!(terms.destination.kind, "");
		assert_eq!(consilium.executed_payment_id.as_deref(), Some("p-1"));

		let revenue = Consilium::from(bk::Consilium::default());
		assert!(revenue.payment.is_none() && revenue.executed_payment_id.is_none());
	}

	/// The third sibling: set on a valuation override, `null` — never an empty object, which
	/// the invitation page treats as unrenderable — on every other kind, on the room's view
	/// and on the public invitation alike.
	#[test]
	fn a_valuation_override_consilium_carries_its_terms_and_nulls_elsewhere() {
		let consilium = Consilium::from(bk::Consilium {
			valuation_override: Some(bk::ValuationOverrideTerms {
				service: "svc-arb".into(),
				aum: "16250".into(),
			}),
			executed_valuation_id: "v-1".into(),
			..Default::default()
		});
		let terms = consilium.valuation_override.expect("a valuation override carries its terms");
		assert_eq!((terms.service.as_str(), terms.aum.as_str()), ("svc-arb", "16250"));
		assert_eq!(consilium.executed_valuation_id.as_deref(), Some("v-1"));
		assert!(consilium.payment.is_none());

		let other = Consilium::from(bk::Consilium::default());
		assert!(other.valuation_override.is_none() && other.executed_valuation_id.is_none());
		let invitation = ConsiliumInvitation::from(bk::ConsiliumInvitation {
			valuation_override: Some(bk::ValuationOverrideTerms {
				service: "svc-arb".into(),
				aum: "1".into(),
			}),
			..Default::default()
		});
		assert_eq!(invitation.valuation_override.map(|t| t.service).as_deref(), Some("svc-arb"));
		assert!(ConsiliumInvitation::from(bk::ConsiliumInvitation::default()).valuation_override.is_none());
	}
}
