//! `allocations` bounded context — the registry of investable products.
//!
//! An [`Allocation`] is the control-plane record that makes a [`ServiceId`] investable.
//! Before it existed, a fund came into being the moment *any* user subscribed to an
//! arbitrary slug: `Subscribe` validated only the slug's shape, and a missing valuation
//! silently bootstrapped at the seed NAV. A typo therefore minted a real fund holding
//! real money and answering to no operator. The registry closes that: the subscribe
//! path resolves its service here first, so an investable product exists only because
//! someone holding `AllocationManage` registered and opened it.
//!
//! This aggregate carries **no money**. Units, NAV, cost basis and cash live in the
//! `subscriptions` / `redemptions` / `balance` contexts, keyed by the same [`ServiceId`]
//! — the registry owns identity, presentation, lifecycle and access only. Its events
//! are audit facts (`event_log`), never relayed to the ledger.
//!
//! **Access is a second axis, orthogonal to lifecycle.** [`AllocationState`] says whether
//! the product deals at all; [`AllocationAccess`] says who sees it and who may put money
//! in. The aggregate holds the product's *default* level; per-investor grants live beside
//! it in the registry (they are keyed by user, so loading them all onto the hot path
//! would be waste) and reach this aggregate only as the audit facts they raise. The
//! effective level for one investor — [`AllocationAccess::effective`] — is the higher
//! of the two, and it is what the subscribe gate and the catalog are decided on.
//!
//! Pure and wasm-safe (mirrors [`Withdrawal`](crate::withdrawals::Withdrawal)): ids are
//! minted by the application layer, no clock, no I/O. Timestamps are DB-stamped and
//! surfaced by the read port, so they never enter the aggregate.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{
	balance::ServiceId,
	error::DomainError,
	money::{Shares, contains_url},
	users::UserId,
};

/// The longest accepted display title.
const MAX_TITLE_LEN: usize = 120;
/// The longest accepted catalog one-liner.
const MAX_SUMMARY_LEN: usize = 280;
/// The authorised unit supply a product is registered with: 100,000,000 units.
///
/// High enough that it never surprises an operator who did not think about it, finite so
/// that "unlimited" is never the stored answer — the registry always has a number to show
/// and to refuse against. An operator narrows it (to a thousand, say) before opening.
pub const DEFAULT_UNIT_CAP: Shares = Shares::from_base_units(100_000_000 * 1_000_000_000_000_000_000);

/// A unique allocation id (UUID). Minted by the application layer.
///
/// A surrogate: the allocation's *natural* key is its [`ServiceId`] slug, which is what
/// every other context and the whole wire surface uses. The slug is a `String` newtype
/// and so cannot satisfy `Identifier: Copy`, hence the UUID — it exists to satisfy the
/// `Entity`/event-log plumbing (`event_log.aggregate_id`), never to be an alternative
/// public handle. Look allocations up by [`ServiceId`].
pub type AllocationId = Id<AllocationTag>;
/// Phantom tag making [`AllocationId`] a distinct, incompatible identity type.
pub struct AllocationTag;

/// The surrogate id of the reserved **fee** allocation ([`ServiceId::fee`]).
///
/// Fixed rather than minted, because the row is written by migration `0045`, not by an
/// operator: the literal is `uuid5(NAMESPACE_OID, "evbanking:allocation:fee")`, spelled
/// out here so the domain stays free of the v5 feature, and pinned by a hub test that
/// re-derives it and greps the migration for the same bytes.
pub const FEE_ALLOCATION_ID: AllocationId = AllocationId::from_raw(uuid::Uuid::from_u128(0x680240f5_1c41_58e2_987c_e52dbe2faa01));
/// The surrogate id of the reserved **fund** allocation ([`ServiceId::fund`]):
/// `uuid5(NAMESPACE_OID, "evbanking:allocation:fund")`. See [`FEE_ALLOCATION_ID`].
pub const FUND_ALLOCATION_ID: AllocationId = AllocationId::from_raw(uuid::Uuid::from_u128(0x7533b905_9200_5ffb_b053_844c9c8da99a));

/// The fixed id a reserved slug's registry row carries, `None` for an ordinary product
/// (whose id the application layer mints).
pub fn reserved_allocation_id(service: &ServiceId) -> Option<AllocationId> {
	if *service == ServiceId::fee() {
		Some(FEE_ALLOCATION_ID)
	} else if *service == ServiceId::fund() {
		Some(FUND_ALLOCATION_ID)
	} else {
		None
	}
}

/// Lifecycle of an investable product.
///
/// `Closed` deliberately still permits redemptions: an operator winding a product down
/// must not be able to strand an investor's money inside it. Only the *new money*
/// direction is gated.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationState {
	/// Registered but accepting no money — the product is being prepared. Hidden from
	/// the default catalog.
	Draft,
	/// Live: accepts subscriptions and redemptions.
	Open,
	/// Wound down: redemptions still settle, new subscriptions are refused.
	Closed,
}

impl AllocationState {
	/// The stored/wire discriminant. Keep byte-identical with
	/// `evbanking_contracts::allocation::state` (`allocation_state_strings_are_canonical`
	/// guards this side).
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Draft => "draft",
			Self::Open => "open",
			Self::Closed => "closed",
		}
	}

	/// Parse the stored/wire form. An unrecognized value is an error rather than a
	/// silent default, so a corrupt row never quietly opens a product for business.
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"draft" => Ok(Self::Draft),
			"open" => Ok(Self::Open),
			"closed" => Ok(Self::Closed),
			other => Err(DomainError::Validation(format!("unknown allocation state: {other}"))),
		}
	}
}

/// Who may see a product and who may put money in it.
///
/// Ranked, lowest first — the derived `Ord` is load-bearing: an investor's effective
/// level is the *higher* of the product's default and their own grant
/// ([`Self::effective`]), so the variant order here IS the access policy. Orthogonal
/// to [`AllocationState`]: a product can be `open` and `hidden` (invite-only, dealing
/// with the invited), or `draft` and `invest` (announced, not yet dealing).
///
/// Never consulted on the way *out*. Redemptions are gated by state alone
/// ([`Allocation::ensure_redeemable`]), so locking a product cannot trap the units an
/// investor already holds in it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationAccess {
	/// Not in the caller's catalog; a direct read answers as if unregistered.
	Hidden,
	/// Listed and readable; subscriptions refused. What a registration lands on.
	#[default]
	View,
	/// Listed, readable, and open to new money (subject to state and cap).
	Invest,
}

impl AllocationAccess {
	/// The level a product carries from registration until an operator changes it:
	/// visible, so the catalog shows what is coming, but locked — "closed by default".
	pub const DEFAULT: Self = Self::View;

	/// The stored/wire discriminant. Keep byte-identical with
	/// `evbanking_contracts::allocation::access` (`allocation_access_strings_are_canonical`
	/// guards this side).
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Hidden => "hidden",
			Self::View => "view",
			Self::Invest => "invest",
		}
	}

	/// Parse the stored/wire form. An unrecognized value is an error rather than a
	/// silent default, so a corrupt row never quietly opens a product to everyone.
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"hidden" => Ok(Self::Hidden),
			"view" => Ok(Self::View),
			"invest" => Ok(Self::Invest),
			other => Err(DomainError::Validation(format!("unknown allocation access level: {other}"))),
		}
	}

	/// The level one investor effectively holds: the product's `default`, raised by
	/// their `grant` if they have one. A grant can only add — see
	/// [`Allocation::grant_access`] for why `Hidden` is not grantable — so this is a
	/// plain `max` and the derived order decides it.
	pub fn effective(default: Self, grant: Option<Self>) -> Self {
		grant.map_or(default, |granted| default.max(granted))
	}

	/// Whether an allocation at this level belongs in the caller's catalog.
	pub fn permits_viewing(self) -> bool {
		self >= Self::View
	}

	/// Whether this level lets the caller put money in. Necessary alongside
	/// [`Allocation::ensure_subscribable`]'s state check, never sufficient on its own.
	pub fn permits_investing(self) -> bool {
		self >= Self::Invest
	}
}

/// The catalog glyph an operator picks for a product.
///
/// A closed vocabulary rather than a free-form asset reference: the client owns the
/// artwork and maps each variant onto one of its SVGs, so a registry row can never
/// point at a picture the shipped client has no way to draw. Adding a variant means
/// shipping the client that knows it first.
///
/// Presentation only — nothing here gates money or lifecycle.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationIcon {
	/// The neutral default every product lands on until an operator picks one.
	#[default]
	Fund,
	RealEstate,
	Trading,
	Yield,
	Venture,
	Treasury,
	Commodity,
	Credit,
	Index,
	Arbitrage,
}

impl AllocationIcon {
	/// Every icon, in the order an operator's picker shows them — [`Self::Fund`] first,
	/// because that is the one a product lands on when nobody chooses.
	///
	/// The set the DB CHECK in migration 0027 and `evbanking_contracts::allocation::
	/// icon::ALL` must agree with, member for member — `domain_icons_match_the_wire_
	/// contract` compares the two both ways, and
	/// `every_icon_the_domain_knows_is_accepted_by_the_column` writes each of these into
	/// the column and asserts the CHECK takes it.
	pub const ALL: [Self; Self::COUNT] = {
		let mut all = [Self::Fund; Self::COUNT];
		let (mut i, mut icon) = (1usize, Self::Fund);
		while let Some(next) = icon.next() {
			all[i] = next;
			icon = next;
			i += 1;
		}
		all
	};
	/// How many icons the vocabulary holds. Walked from the chain, never counted by hand.
	pub const COUNT: usize = {
		let (mut count, mut icon) = (1usize, Self::Fund);
		while let Some(next) = icon.next() {
			count += 1;
			icon = next;
		}
		count
	};

	/// The icon declared after `self`, or `None` at the end of the vocabulary.
	///
	/// Private, and existing only to derive [`Self::COUNT`] and [`Self::ALL`]: the match
	/// is exhaustive with no `_` arm, so a new variant fails to compile until it is
	/// linked into the chain — and once it is, both constants grow on their own. That is
	/// the whole point: a hand-maintained `[Self; 10]` literal keeps compiling, and
	/// keeps passing its test, while silently missing the eleventh icon that the DB
	/// CHECK, the wire contract and the client all still have to be told about.
	const fn next(self) -> Option<Self> {
		match self {
			Self::Fund => Some(Self::RealEstate),
			Self::RealEstate => Some(Self::Trading),
			Self::Trading => Some(Self::Yield),
			Self::Yield => Some(Self::Venture),
			Self::Venture => Some(Self::Treasury),
			Self::Treasury => Some(Self::Commodity),
			Self::Commodity => Some(Self::Credit),
			Self::Credit => Some(Self::Index),
			Self::Index => Some(Self::Arbitrage),
			Self::Arbitrage => None,
		}
	}

	/// The stored/wire discriminant. Keep byte-identical with
	/// `evbanking_contracts::allocation::icon` (`allocation_icon_strings_are_canonical`
	/// guards this side).
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Fund => "fund",
			Self::RealEstate => "real_estate",
			Self::Trading => "trading",
			Self::Yield => "yield",
			Self::Venture => "venture",
			Self::Treasury => "treasury",
			Self::Commodity => "commodity",
			Self::Credit => "credit",
			Self::Index => "index",
			Self::Arbitrage => "arbitrage",
		}
	}

	/// Parse the stored/wire form. An unrecognized value is an error rather than a
	/// silent fallback to [`Self::Fund`], because the one caller that must stay strict
	/// is the gRPC boundary: a *client* sending an icon this build cannot draw is a
	/// contract mismatch the operator has to see, and the request is still refusable.
	///
	/// The storage read path deliberately does not use it that way — see
	/// `piggybank_core::infrastructure::allocations`, which degrades an unknown stored
	/// value to [`Self::default`]. A row is already written; refusing it would take the
	/// catalog down over a presentation column.
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"fund" => Ok(Self::Fund),
			"real_estate" => Ok(Self::RealEstate),
			"trading" => Ok(Self::Trading),
			"yield" => Ok(Self::Yield),
			"venture" => Ok(Self::Venture),
			"treasury" => Ok(Self::Treasury),
			"commodity" => Ok(Self::Commodity),
			"credit" => Ok(Self::Credit),
			"index" => Ok(Self::Index),
			"arbitrage" => Ok(Self::Arbitrage),
			other => Err(DomainError::Validation(format!("unknown allocation icon: {other}"))),
		}
	}
}

/// What stands behind a product's units: the fund's cash, or an asset held in kind.
///
/// Every unit a subscription mints has cash behind it — the investor's claim moved into
/// the fund's, and a redemption pays that cash back out. Units minted in kind
/// (`AllocationsService.IssueUnits`) have none: the product is registered against an
/// asset the holders already own, and the fund's USDT claim never saw a cent for them.
/// A redemption on such a product would price the units at NAV and pay cash the fund
/// does not hold — it would queue forever, or worse, drain cash another product's
/// investors put in. So `Redeem` is refused on an `InKind` product
/// ([`Allocation::ensure_cash_backed`]) and the way out is the book, where a buyer's
/// cash pays for the units.
///
/// Set automatically by the first in-kind mint (`cash → in_kind`, never the reverse on
/// its own) and explicitly by an operator (`SetAllocationBacking`) once the fund does
/// hold cash for the units. Orthogonal to [`AllocationState`] and [`AllocationAccess`].
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationBacking {
	/// The units were paid for with cash into the fund's claim; a redemption pays out
	/// of it. What every registration lands on.
	#[default]
	Cash,
	/// The units stand for an asset held in kind; the fund's claim holds no cash for
	/// them. Redemptions are refused — holders exit through the book.
	InKind,
}

impl AllocationBacking {
	/// The stored/wire discriminant. Keep byte-identical with
	/// `evbanking_contracts::allocation::backing` (`allocation_backing_strings_are_canonical`
	/// guards this side).
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Cash => "cash",
			Self::InKind => "in_kind",
		}
	}

	/// Parse the stored/wire form. An unrecognized value is an error rather than a
	/// silent default, because the default is the permissive side: a corrupt row must
	/// never quietly let a redemption price units the fund holds no cash for.
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"cash" => Ok(Self::Cash),
			"in_kind" => Ok(Self::InKind),
			other => Err(DomainError::Validation(format!("unknown allocation backing: {other}"))),
		}
	}
}

/// The stored shape of an [`Allocation`], as the persistence adapter reads it back —
/// the input to [`Allocation::rehydrate`]. A struct rather than eight positional
/// arguments for the same reason as [`UserSnapshot`](crate::users::UserSnapshot): the
/// call site is a row-to-aggregate mapping, and named fields read against the `SELECT`
/// beside it where a positional list has to be counted out.
pub struct AllocationSnapshot {
	pub id: AllocationId,
	pub service: ServiceId,
	pub title: String,
	pub summary: String,
	pub state: AllocationState,
	pub unit_cap: Shares,
	pub icon: AllocationIcon,
	pub access: AllocationAccess,
	pub backing: AllocationBacking,
}

/// The allocation aggregate — one investable product's registry entry. Construct via
/// [`Allocation::register`] (raises [`AllocationEvent::Registered`]) or
/// [`Allocation::rehydrate`] (load from the store, no events).
#[derive(Clone, Debug)]
pub struct Allocation {
	id: AllocationId,
	service: ServiceId,
	title: String,
	summary: String,
	state: AllocationState,
	unit_cap: Shares,
	icon: AllocationIcon,
	access: AllocationAccess,
	backing: AllocationBacking,
	pending: Vec<AllocationEvent>,
}

impl Allocation {
	/// Register `service` as an investable product, in [`AllocationState::Draft`], at
	/// [`DEFAULT_UNIT_CAP`] and at [`AllocationAccess::DEFAULT`] — a registration never
	/// opens for business in the same step, so listing, sizing, funding and admitting
	/// investors stay separate operator decisions. Raises `Registered`.
	///
	/// A reserved slug ([`ServiceId::is_reserved`]) is refused: the `fee` and `fund`
	/// allocations are the platform's own, written once by migration `0045` with fixed
	/// ids and hidden access, and an operator "registering" one would either collide with
	/// that row or — on a database that predates it — create a public product wearing a
	/// name every fee and seed leg is about to credit.
	pub fn register(id: AllocationId, service: ServiceId, title: &str, summary: &str, icon: AllocationIcon) -> Result<Self, DomainError> {
		if service.is_reserved() {
			return Err(DomainError::Validation(format!("'{service}' is a reserved allocation and cannot be registered")));
		}
		let title = validate_title(title)?;
		let summary = validate_summary(summary)?;
		let mut allocation = Self {
			id,
			service: service.clone(),
			title: title.clone(),
			summary: summary.clone(),
			state: AllocationState::Draft,
			unit_cap: DEFAULT_UNIT_CAP,
			icon,
			access: AllocationAccess::DEFAULT,
			backing: AllocationBacking::default(),
			pending: Vec::new(),
		};
		allocation.pending.push(AllocationEvent::Registered {
			allocation_id: id,
			service,
			title,
			summary,
			unit_cap: DEFAULT_UNIT_CAP,
			icon,
			access: AllocationAccess::DEFAULT,
			backing: AllocationBacking::default(),
		});
		Ok(allocation)
	}

	/// Reconstitute from the store. Raises no events.
	pub fn rehydrate(snapshot: AllocationSnapshot) -> Self {
		Self {
			id: snapshot.id,
			service: snapshot.service,
			title: snapshot.title,
			summary: snapshot.summary,
			state: snapshot.state,
			unit_cap: snapshot.unit_cap,
			icon: snapshot.icon,
			access: snapshot.access,
			backing: snapshot.backing,
			pending: Vec::new(),
		}
	}

	/// Replace the presentation fields — title, summary and icon. Identity and state are
	/// untouched, so an operator may fix a title on a live product without disturbing
	/// money flow. The icon rides here rather than in a command of its own because it is
	/// presentation like the other two and gates nothing. Raises `DetailsUpdated`.
	///
	/// `icon` is `Option` where the other two are not: `None` means "the caller did not
	/// talk about the icon", and the stored pick is left alone. A full replace would
	/// make every caller that predates the field — an older gRPC consumer, a browser tab
	/// holding a stale bundle, a pod mid-rolling-deploy — erase the operator's choice
	/// while reporting success. `Some` is an explicit set, including `Some(default)` for
	/// a reset back to the neutral glyph. Title and summary have no such "unchanged"
	/// value on the wire, so they stay a replace and the boundary refuses a request that
	/// omits the title outright.
	pub fn update_details(&mut self, title: &str, summary: &str, icon: Option<AllocationIcon>) -> Result<(), DomainError> {
		self.title = validate_title(title)?;
		self.summary = validate_summary(summary)?;
		if let Some(icon) = icon {
			self.icon = icon;
		}
		self.pending.push(AllocationEvent::DetailsUpdated {
			allocation_id: self.id,
			service: self.service.clone(),
			title: self.title.clone(),
			summary: self.summary.clone(),
			// The icon the product now carries, not the one the request named — an audit
			// read of the log must answer "what does this card look like" without also
			// having to replay which edits happened to mention the field.
			icon: self.icon,
		});
		Ok(())
	}

	/// Open for business (from `Draft` or `Closed`). Idempotent: already `Open` is a
	/// no-op that raises nothing, so a retried operator command can't spam the log.
	pub fn open(&mut self) {
		if self.state == AllocationState::Open {
			return;
		}
		self.state = AllocationState::Open;
		self.pending.push(AllocationEvent::Opened {
			allocation_id: self.id,
			service: self.service.clone(),
		});
	}

	/// Wind the product down. Redemptions keep working ([`Self::ensure_redeemable`]);
	/// only new subscriptions stop. Idempotent, and legal from `Draft` too (retiring a
	/// product that never opened).
	pub fn close(&mut self) {
		if self.state == AllocationState::Closed {
			return;
		}
		self.state = AllocationState::Closed;
		self.pending.push(AllocationEvent::Closed {
			allocation_id: self.id,
			service: self.service.clone(),
		});
	}

	/// Resize the authorised unit supply. Idempotent: setting the cap it already has
	/// raises nothing. Raises `CapUpdated`.
	///
	/// A cap **below** the units already issued is deliberately legal — it is how an
	/// operator stops issuance on a product that has run further than intended. It means
	/// "no more units", not "some units are now invalid": nothing already minted is
	/// touched, and [`Self::ensure_redeemable`] is not consulted here, so holders still
	/// get out. Only zero is refused, because a zero cap is indistinguishable from an
	/// unset one and would silently close a live product to new money without saying so
	/// in its state.
	pub fn set_unit_cap(&mut self, unit_cap: Shares) -> Result<(), DomainError> {
		if unit_cap.is_zero() {
			return Err(DomainError::Validation(
				"allocation unit cap must be greater than zero — close the allocation to stop new money".into(),
			));
		}
		if unit_cap == self.unit_cap {
			return Ok(());
		}
		self.unit_cap = unit_cap;
		self.pending.push(AllocationEvent::CapUpdated {
			allocation_id: self.id,
			service: self.service.clone(),
			unit_cap,
		});
		Ok(())
	}

	/// The supply gate the subscribe path runs once it knows what it would mint:
	/// `issued + minting` must stay within the cap.
	///
	/// `issued` is what the caller reads from the ledger; this aggregate does no I/O, so
	/// it takes the figure rather than fetching it. Overflow is refused rather than
	/// wrapped — an addition that cannot be represented cannot be shown to be under a cap.
	pub fn ensure_capacity(&self, issued: Shares, minting: Shares) -> Result<(), DomainError> {
		let after = issued
			.checked_add(minting)
			.ok_or_else(|| DomainError::Validation(format!("allocation '{}' cannot mint that many units", self.service)))?;
		if after > self.unit_cap {
			let remaining = self.unit_cap.checked_sub(issued).unwrap_or(Shares::ZERO);
			return Err(DomainError::Validation(format!(
				"allocation '{}' has {} units left of its {} unit cap — this subscription would mint {}",
				self.service,
				remaining.to_decimal_string(),
				self.unit_cap.to_decimal_string(),
				minting.to_decimal_string(),
			)));
		}
		Ok(())
	}

	/// Units still issuable against the cap, given what the ledger says is already out.
	/// Saturating: a cap narrowed below the issued supply reports zero headroom, not a
	/// negative one.
	pub fn remaining_capacity(&self, issued: Shares) -> Shares {
		self.unit_cap.checked_sub(issued).unwrap_or(Shares::ZERO)
	}

	/// Set the default access level. Idempotent: the level it already holds raises
	/// nothing. Raises `AccessChanged`.
	///
	/// Lowering the default never touches a per-investor grant — a grant is the
	/// operator's explicit exception and stays until revoked — and never touches
	/// redemptions, which are gated by state alone.
	pub fn set_access(&mut self, access: AllocationAccess) {
		if access == self.access {
			return;
		}
		self.access = access;
		self.pending.push(AllocationEvent::AccessChanged {
			allocation_id: self.id,
			service: self.service.clone(),
			access,
		});
	}

	/// Mark the units as standing for an asset held in kind — what the first in-kind
	/// mint does on the product's behalf. Idempotent: an already `InKind` product raises
	/// nothing, so the second and every later mint leaves the log alone. Raises
	/// `BackingChanged`.
	///
	/// One-directional on purpose: nothing automatic ever flips a product back to `Cash`,
	/// because that is the direction that lets a redemption price units the fund holds
	/// no cash for. Only an operator does that, through [`Self::set_backing`].
	pub fn mark_in_kind(&mut self) {
		self.set_backing(AllocationBacking::InKind);
	}

	/// Set the backing explicitly — an operator declaring the fund now holds cash for
	/// the units (`Cash`), or correcting a product back to `InKind`. Idempotent: the
	/// backing it already holds raises nothing. Raises `BackingChanged`.
	pub fn set_backing(&mut self, backing: AllocationBacking) {
		if backing == self.backing {
			return;
		}
		self.backing = backing;
		self.pending.push(AllocationEvent::BackingChanged {
			allocation_id: self.id,
			service: self.service.clone(),
			backing,
		});
	}

	/// The second gate the redeem path runs, beside [`Self::ensure_redeemable`]: a
	/// redemption pays cash out of the fund's claim, so the units must be backed by that
	/// cash. Refused as a `Precondition` — the product is fine and the holder is
	/// entitled; the *fund's* state is what stands in the way, and an operator can
	/// change it (`set_backing`) once the cash is there. The message points at the exit
	/// that does work: selling the units on the book.
	pub fn ensure_cash_backed(&self) -> Result<(), DomainError> {
		match self.backing {
			AllocationBacking::Cash => Ok(()),
			AllocationBacking::InKind => Err(DomainError::Precondition(format!(
				"units of '{}' are not backed by fund cash — sell them on the book instead of redeeming",
				self.service
			))),
		}
	}

	/// Record that `user` was raised to `level` on this product by `granted_by`. The
	/// grant itself is stored beside the aggregate, keyed by user (see the module doc);
	/// this raises the audit fact and refuses the one level a grant must never carry.
	///
	/// `Hidden` is refused because a grant only ever *adds*: the effective level is
	/// `max(default, grant)`, so a `hidden` grant would be a no-op that reads, in the
	/// log and the operator's list, as if it locked someone out. Raises `AccessGranted`.
	pub fn grant_access(&mut self, user: UserId, level: AllocationAccess, granted_by: UserId) -> Result<(), DomainError> {
		if !level.permits_viewing() {
			return Err(DomainError::Validation(format!(
				"an access grant must be '{}' or '{}' — a grant only ever raises an investor above the default; revoke it to take one back",
				AllocationAccess::View.as_str(),
				AllocationAccess::Invest.as_str(),
			)));
		}
		self.pending.push(AllocationEvent::AccessGranted {
			allocation_id: self.id,
			service: self.service.clone(),
			user_id: user,
			level,
			granted_by,
		});
		Ok(())
	}

	/// Record that `user`'s grant on this product was taken back by `revoked_by`; they
	/// fall back to the default level. Raises `AccessRevoked`. The caller raises this
	/// only when a grant actually stood — revoking nothing is idempotent and unlogged.
	pub fn revoke_access(&mut self, user: UserId, revoked_by: UserId) {
		self.pending.push(AllocationEvent::AccessRevoked {
			allocation_id: self.id,
			service: self.service.clone(),
			user_id: user,
			revoked_by,
		});
	}

	/// The gate the subscribe path runs, on both axes: only an `Open` allocation takes
	/// new money, and only from a caller whose effective level ([`AllocationAccess::
	/// effective`]) is `Invest`.
	///
	/// State is checked first, so a locked-but-closed product answers "closed" — the
	/// answer that stays true after any grant. The two refusals are different kinds on
	/// purpose: state is `Validation` (the product is not dealing, nobody can change that
	/// for this caller), access is `Precondition` (the product deals, this caller has
	/// not been let in — an operator can). A client tells them apart by code alone.
	pub fn ensure_subscribable(&self, caller_access: AllocationAccess) -> Result<(), DomainError> {
		match self.state {
			AllocationState::Open => {}
			AllocationState::Draft => return Err(DomainError::Validation(format!("allocation '{}' is not open for subscriptions yet", self.service))),
			AllocationState::Closed => return Err(DomainError::Validation(format!("allocation '{}' is closed to new subscriptions", self.service))),
		}
		if !caller_access.permits_investing() {
			return Err(DomainError::Precondition(format!(
				"allocation '{}' is not open to you for investment — an operator must grant you '{}' access",
				self.service,
				AllocationAccess::Invest.as_str(),
			)));
		}
		Ok(())
	}

	/// The gate the redeem path runs. A `Closed` allocation still lets investors out —
	/// refusing here would lock real units inside a wound-down product. Only a `Draft`
	/// is refused, and only because it can hold no units to begin with.
	pub fn ensure_redeemable(&self) -> Result<(), DomainError> {
		match self.state {
			AllocationState::Open | AllocationState::Closed => Ok(()),
			AllocationState::Draft => Err(DomainError::Validation(format!("allocation '{}' has never been open", self.service))),
		}
	}

	/// Whether this allocation belongs in the catalog of a caller holding
	/// `caller_access`: dealing (`Open`) and at least visible to them.
	pub fn is_listed_for(&self, caller_access: AllocationAccess) -> bool {
		self.state == AllocationState::Open && caller_access.permits_viewing()
	}

	pub fn access(&self) -> AllocationAccess {
		self.access
	}

	pub fn backing(&self) -> AllocationBacking {
		self.backing
	}

	pub fn id(&self) -> AllocationId {
		self.id
	}

	pub fn service(&self) -> &ServiceId {
		&self.service
	}

	pub fn title(&self) -> &str {
		&self.title
	}

	pub fn summary(&self) -> &str {
		&self.summary
	}

	pub fn state(&self) -> AllocationState {
		self.state
	}

	pub fn unit_cap(&self) -> Shares {
		self.unit_cap
	}

	pub fn icon(&self) -> AllocationIcon {
		self.icon
	}
}

fn validate_title(raw: &str) -> Result<String, DomainError> {
	let title = raw.trim();
	if title.is_empty() {
		return Err(DomainError::Validation("allocation title must not be empty".into()));
	}
	if title.chars().count() > MAX_TITLE_LEN {
		return Err(DomainError::Validation(format!("allocation title must be at most {MAX_TITLE_LEN} characters")));
	}
	// The title is printed into approvals and notices. Concierge refuses a URL there; an
	// address it lets through, but a client still renders it as a link. A bare domain stays
	// allowed: products are legitimately named after their sites (`Aquafix.ae`).
	if contains_url(title) || title.contains('@') {
		return Err(DomainError::Validation("allocation title must not contain a link or an address".into()));
	}
	Ok(title.to_owned())
}

fn validate_summary(raw: &str) -> Result<String, DomainError> {
	let summary = raw.trim();
	if summary.chars().count() > MAX_SUMMARY_LEN {
		return Err(DomainError::Validation(format!("allocation summary must be at most {MAX_SUMMARY_LEN} characters")));
	}
	Ok(summary.to_owned())
}

impl Entity for Allocation {
	type Id = AllocationId;

	fn id(&self) -> AllocationId {
		self.id
	}
}

impl AggregateRoot for Allocation {
	const NAME: &'static str = "allocation";
}

/// Facts raised by the [`Allocation`] aggregate. Audit only — the relay never sees
/// them (they carry no money), so they are written to `event_log` with `relay = false`.
/// Internally tagged so the stored JSON is self-describing.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AllocationEvent {
	/// A service id became a registered product (in `Draft`, at the default unit cap).
	Registered {
		allocation_id: AllocationId,
		service: ServiceId,
		title: String,
		summary: String,
		unit_cap: Shares,
		/// `default` so a row written before icons existed still deserializes — those
		/// products were all rendered as the letter avatar this default replaces, and
		/// the column backfills to the same value.
		#[serde(default)]
		icon: AllocationIcon,
		/// `default` for rows written before access existed: a historical registration
		/// reads back as landing on [`AllocationAccess::DEFAULT`], which is what
		/// "registered" has always meant in effect — listed once opened, and the money
		/// gate decided elsewhere.
		#[serde(default)]
		access: AllocationAccess,
		/// `default` for rows written before backing existed: every registration has
		/// always landed on cash-backed, and the products that were not are flipped by the
		/// migration's backfill, not by rereading their registration.
		#[serde(default)]
		backing: AllocationBacking,
	},
	/// Presentation fields changed; state and identity did not.
	DetailsUpdated {
		allocation_id: AllocationId,
		service: ServiceId,
		title: String,
		summary: String,
		/// `default` for the same reason as on `Registered` — it keeps pre-icon rows
		/// readable.
		#[serde(default)]
		icon: AllocationIcon,
	},
	/// The authorised unit supply was resized. Its own fact rather than a field on
	/// `DetailsUpdated`: this one gates money, so "who changed the cap, and when" has to
	/// be answerable without diffing every title edit.
	CapUpdated { allocation_id: AllocationId, service: ServiceId, unit_cap: Shares },
	/// Now accepting subscriptions.
	Opened { allocation_id: AllocationId, service: ServiceId },
	/// No longer accepting subscriptions; redemptions continue.
	Closed { allocation_id: AllocationId, service: ServiceId },
	/// The default access level changed. Its own fact, like `CapUpdated`, because it
	/// gates money: "who opened this product to everyone, and when" must be answerable.
	AccessChanged {
		allocation_id: AllocationId,
		service: ServiceId,
		access: AllocationAccess,
	},
	/// What stands behind the units changed — raised by the first in-kind mint
	/// (`cash → in_kind`) or by an operator. Its own fact because it gates money the
	/// other way round from `AccessChanged`: it decides whether holders can redeem.
	BackingChanged {
		allocation_id: AllocationId,
		service: ServiceId,
		backing: AllocationBacking,
	},
	/// One investor was raised above the default. `granted_by` is the operator — the
	/// audit answer to "who let this investor in".
	AccessGranted {
		allocation_id: AllocationId,
		service: ServiceId,
		user_id: UserId,
		level: AllocationAccess,
		granted_by: UserId,
	},
	/// One investor's grant was taken back; they hold the default again.
	AccessRevoked {
		allocation_id: AllocationId,
		service: ServiceId,
		user_id: UserId,
		revoked_by: UserId,
	},
}

impl DomainEvent for AllocationEvent {
	const KIND: &'static str = "allocations";
}

impl EmitsEvents for Allocation {
	type Event = AllocationEvent;

	fn drain_events(&mut self) -> Vec<AllocationEvent> {
		core::mem::take(&mut self.pending)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn svc() -> ServiceId {
		ServiceId::parse("trading").unwrap()
	}

	fn registered() -> Allocation {
		Allocation::register(AllocationId::new(), svc(), "EV Trading", "Systematic crypto trading", AllocationIcon::Trading).unwrap()
	}

	#[test]
	fn a_reserved_slug_cannot_be_registered() {
		for reserved in [ServiceId::fee(), ServiceId::fund()] {
			let err = Allocation::register(AllocationId::new(), reserved.clone(), "Mine", "", AllocationIcon::Fund).unwrap_err();
			assert!(matches!(err, DomainError::Validation(ref m) if m.contains("reserved")), "{reserved}: {err:?}");
			// The row still rehydrates: the migration writes it, the registry reads it back.
			assert!(reserved_allocation_id(&reserved).is_some());
		}
		assert_eq!(reserved_allocation_id(&svc()), None);
		assert_ne!(FEE_ALLOCATION_ID, FUND_ALLOCATION_ID);
	}

	#[test]
	fn allocation_state_strings_are_canonical() {
		// Wire contract: these must match `evbanking_contracts::allocation::state`.
		assert_eq!(AllocationState::Draft.as_str(), "draft");
		assert_eq!(AllocationState::Open.as_str(), "open");
		assert_eq!(AllocationState::Closed.as_str(), "closed");
		for state in [AllocationState::Draft, AllocationState::Open, AllocationState::Closed] {
			assert_eq!(AllocationState::parse(state.as_str()).unwrap(), state);
		}
		assert!(AllocationState::parse("delisted").is_err());
	}

	#[test]
	fn allocation_icon_strings_are_canonical() {
		// Wire contract: these must match `evbanking_contracts::allocation::icon`.
		//
		// Driven off `ALL` rather than off a literal list of pairs: the old shape was ten
		// hand-written rows that would have gone on passing, unchanged and green, after an
		// eleventh variant was added. Here the array on the left is `[&str; COUNT]`, so a
		// new variant makes this line stop compiling until the expected list names it.
		let wire: [&str; AllocationIcon::COUNT] = AllocationIcon::ALL.map(AllocationIcon::as_str);
		assert_eq!(
			wire,
			["fund", "real_estate", "trading", "yield", "venture", "treasury", "commodity", "credit", "index", "arbitrage"]
		);
		for icon in AllocationIcon::ALL {
			assert_eq!(AllocationIcon::parse(icon.as_str()).unwrap(), icon);
		}
	}

	#[test]
	fn the_icon_vocabulary_lists_every_variant_exactly_once() {
		// `ALL` is walked from the `next` chain, so this is what proves the walk visits
		// each variant once rather than looping or stopping short.
		assert_eq!(AllocationIcon::ALL.len(), AllocationIcon::COUNT);
		for (i, icon) in AllocationIcon::ALL.iter().enumerate() {
			assert_eq!(AllocationIcon::ALL.iter().filter(|other| *other == icon).count(), 1, "{icon:?} appears more than once");
			assert!(!AllocationIcon::ALL[..i].contains(icon));
		}
		assert_eq!(AllocationIcon::ALL[0], AllocationIcon::default(), "the picker opens on the default");
	}

	#[test]
	fn an_unknown_icon_is_refused_rather_than_defaulted() {
		// A client sending an icon this build cannot draw is a contract mismatch, not a
		// product that quietly renders as something else.
		assert!(AllocationIcon::parse("rocket").is_err());
		assert!(AllocationIcon::parse("").is_err());
		assert!(AllocationIcon::parse("Fund").is_err(), "the wire form is lowercase snake_case");
		assert!(AllocationIcon::parse("realEstate").is_err());
	}

	#[test]
	fn the_default_icon_is_fund() {
		// What a product lands on when the operator picks nothing, and what the column
		// and the pre-icon event payloads backfill to.
		assert_eq!(AllocationIcon::default(), AllocationIcon::Fund);
		assert_eq!(AllocationIcon::default().as_str(), "fund");
	}

	#[test]
	fn the_icon_serializes_to_its_wire_string() {
		// The stored event payload spells the icon exactly as `as_str` does — the guard
		// against the serde rename and the manual table drifting apart.
		for icon in [AllocationIcon::RealEstate, AllocationIcon::Arbitrage, AllocationIcon::Fund] {
			let json = serde_json::to_string(&icon).unwrap();
			assert_eq!(json, format!("\"{}\"", icon.as_str()));
			assert_eq!(serde_json::from_str::<AllocationIcon>(&json).unwrap(), icon);
		}
	}

	#[test]
	fn a_pre_icon_event_payload_still_deserializes() {
		// `event_log` rows written before this field existed carry no `icon` key. Without
		// the serde default they would stop deserializing forever, and an audit read of
		// the registry's history would fail on every historical row.
		let mut allocation = registered();
		let registered_json = serde_json::to_string(&allocation.drain_events().pop().unwrap()).unwrap();
		allocation.update_details("Renamed", "", Some(AllocationIcon::Venture)).unwrap();
		let updated_json = serde_json::to_string(&allocation.drain_events().pop().unwrap()).unwrap();
		for json in [registered_json, updated_json] {
			let legacy = json.replace(r#","icon":"trading""#, "").replace(r#","icon":"venture""#, "");
			assert!(!legacy.contains("icon"), "the legacy payload must actually lack the key: {legacy}");
			let back: AllocationEvent = serde_json::from_str(&legacy).unwrap();
			let icon = match back {
				AllocationEvent::Registered { icon, .. } | AllocationEvent::DetailsUpdated { icon, .. } => icon,
				other => panic!("unexpected event: {other:?}"),
			};
			assert_eq!(icon, AllocationIcon::Fund, "a pre-icon row reads back as the default");
		}
	}

	#[test]
	fn register_starts_in_draft_and_emits_registered() {
		let mut allocation = registered();
		assert_eq!(allocation.state(), AllocationState::Draft);
		assert!(!allocation.is_listed_for(AllocationAccess::Invest));
		let events = allocation.drain_events();
		assert_eq!(events.len(), 1);
		assert!(matches!(events[0], AllocationEvent::Registered { .. }));
		assert!(allocation.drain_events().is_empty());
	}

	#[test]
	fn a_draft_takes_no_money_until_opened() {
		let mut allocation = registered();
		assert!(allocation.ensure_subscribable(AllocationAccess::Invest).is_err());
		allocation.open();
		assert!(allocation.ensure_subscribable(AllocationAccess::Invest).is_ok());
		assert!(allocation.is_listed_for(AllocationAccess::Invest));
	}

	#[test]
	fn closing_blocks_new_money_but_never_traps_an_investor() {
		let mut allocation = registered();
		allocation.open();
		allocation.close();
		assert!(allocation.ensure_subscribable(AllocationAccess::Invest).is_err());
		// The whole point of the split gate: units already minted can still be redeemed.
		assert!(allocation.ensure_redeemable().is_ok());
		assert!(!allocation.is_listed_for(AllocationAccess::Invest));
	}

	#[test]
	fn allocation_access_strings_are_canonical_and_ranked() {
		// Wire contract: these must match `evbanking_contracts::allocation::access`.
		assert_eq!(AllocationAccess::Hidden.as_str(), "hidden");
		assert_eq!(AllocationAccess::View.as_str(), "view");
		assert_eq!(AllocationAccess::Invest.as_str(), "invest");
		for level in [AllocationAccess::Hidden, AllocationAccess::View, AllocationAccess::Invest] {
			assert_eq!(AllocationAccess::parse(level.as_str()).unwrap(), level);
			assert_eq!(serde_json::to_string(&level).unwrap(), format!("\"{}\"", level.as_str()));
		}
		assert!(AllocationAccess::parse("public").is_err());
		assert!(AllocationAccess::parse("Invest").is_err(), "the wire form is lowercase");
		// The derived order IS the policy — `effective` is a plain `max` over it.
		assert!(AllocationAccess::Hidden < AllocationAccess::View);
		assert!(AllocationAccess::View < AllocationAccess::Invest);
	}

	#[test]
	fn a_registration_lands_visible_but_locked() {
		// "Closed by default": the product shows in the catalog once opened, and takes
		// no money until an operator raises the default or grants the investor.
		let mut allocation = registered();
		assert_eq!(allocation.access(), AllocationAccess::View);
		assert_eq!(AllocationAccess::default(), AllocationAccess::DEFAULT);
		allocation.open();
		assert!(allocation.is_listed_for(allocation.access()));
		let err = allocation.ensure_subscribable(allocation.access()).unwrap_err();
		assert!(matches!(err, DomainError::Precondition(_)), "locked is a precondition, not bad input: {err:?}");
	}

	#[test]
	fn the_effective_level_is_the_higher_of_default_and_grant() {
		use AllocationAccess::{Hidden, Invest, View};
		assert_eq!(AllocationAccess::effective(Hidden, None), Hidden);
		assert_eq!(AllocationAccess::effective(Hidden, Some(View)), View);
		assert_eq!(AllocationAccess::effective(Hidden, Some(Invest)), Invest);
		assert_eq!(AllocationAccess::effective(View, Some(View)), View);
		assert_eq!(AllocationAccess::effective(View, Some(Invest)), Invest);
		// A grant never lowers: an `invest` default with a `view` grant is still `invest`.
		assert_eq!(AllocationAccess::effective(Invest, Some(View)), Invest);
		assert_eq!(AllocationAccess::effective(Invest, None), Invest);
	}

	#[test]
	fn the_subscribe_gate_refuses_on_state_before_access_and_by_different_kinds() {
		let mut allocation = registered();
		// Closed wins over locked: the answer that stays true after any grant.
		allocation.close();
		let err = allocation.ensure_subscribable(AllocationAccess::View).unwrap_err();
		assert!(matches!(err, DomainError::Validation(ref m) if m.contains("closed")), "{err:?}");

		allocation.open();
		assert!(matches!(allocation.ensure_subscribable(AllocationAccess::Hidden).unwrap_err(), DomainError::Precondition(_)));
		assert!(matches!(allocation.ensure_subscribable(AllocationAccess::View).unwrap_err(), DomainError::Precondition(_)));
		assert!(allocation.ensure_subscribable(AllocationAccess::Invest).is_ok());
	}

	#[test]
	fn access_never_gates_the_way_out() {
		let mut allocation = registered();
		allocation.open();
		allocation.set_access(AllocationAccess::Hidden);
		// An investor holding units in a product that was locked after they bought in
		// must still be able to leave; `ensure_redeemable` takes no access at all.
		assert!(allocation.ensure_redeemable().is_ok());
		assert!(!allocation.is_listed_for(AllocationAccess::Hidden));
	}

	#[test]
	fn setting_access_is_idempotent_and_audited() {
		let mut allocation = registered();
		allocation.drain_events();
		allocation.set_access(AllocationAccess::View);
		assert!(allocation.drain_events().is_empty(), "re-setting the level it holds raises nothing");
		allocation.set_access(AllocationAccess::Invest);
		allocation.set_access(AllocationAccess::Invest);
		let events = allocation.drain_events();
		assert_eq!(events.len(), 1);
		assert!(matches!(
			events[0],
			AllocationEvent::AccessChanged {
				access: AllocationAccess::Invest,
				..
			}
		));
		assert_eq!(allocation.access(), AllocationAccess::Invest);
	}

	#[test]
	fn a_grant_carries_who_and_refuses_hidden() {
		let mut allocation = registered();
		allocation.drain_events();
		let (investor, operator) = (UserId::new(), UserId::new());
		// A grant only ever adds; `hidden` would be a no-op that reads as a lockout.
		assert!(allocation.grant_access(investor, AllocationAccess::Hidden, operator).is_err());
		assert!(allocation.drain_events().is_empty(), "a refused grant leaves no fact behind");

		allocation.grant_access(investor, AllocationAccess::Invest, operator).unwrap();
		allocation.revoke_access(investor, operator);
		let events = allocation.drain_events();
		assert_eq!(events.len(), 2);
		assert!(matches!(&events[0], AllocationEvent::AccessGranted { user_id, level: AllocationAccess::Invest, granted_by, .. } if *user_id == investor && *granted_by == operator));
		assert!(matches!(&events[1], AllocationEvent::AccessRevoked { user_id, revoked_by, .. } if *user_id == investor && *revoked_by == operator));
		// Grants live beside the aggregate, so the default is untouched by either.
		assert_eq!(allocation.access(), AllocationAccess::View);
	}

	#[test]
	fn a_pre_access_registered_payload_still_deserializes() {
		// `event_log` rows written before this field existed carry no `access` key.
		let mut allocation = registered();
		let json = serde_json::to_string(&allocation.drain_events().pop().unwrap()).unwrap();
		let legacy = json.replace(r#","access":"view""#, "");
		assert!(!legacy.contains("access"), "the legacy payload must actually lack the key: {legacy}");
		let back: AllocationEvent = serde_json::from_str(&legacy).unwrap();
		let AllocationEvent::Registered { access, .. } = back else {
			panic!("expected Registered, got {back:?}")
		};
		assert_eq!(access, AllocationAccess::DEFAULT);
	}

	#[test]
	fn a_draft_is_not_redeemable_because_it_can_hold_no_units() {
		assert!(registered().ensure_redeemable().is_err());
	}

	#[test]
	fn a_registration_starts_at_the_default_cap() {
		let allocation = registered();
		assert_eq!(allocation.unit_cap(), DEFAULT_UNIT_CAP);
		assert_eq!(DEFAULT_UNIT_CAP.to_decimal_string(), "100000000");
	}

	#[test]
	fn capacity_admits_up_to_the_cap_and_refuses_the_unit_past_it() {
		let mut allocation = registered();
		allocation.set_unit_cap(Shares::parse_decimal("1000").unwrap()).unwrap();
		let issued = Shares::parse_decimal("999").unwrap();
		// Landing exactly on the cap is allowed; it is the cap, not a ceiling to stay under.
		assert!(allocation.ensure_capacity(issued, Shares::parse_decimal("1").unwrap()).is_ok());
		// One base unit more is not. The message names the headroom the caller has left.
		let err = allocation.ensure_capacity(issued, Shares::from_base_units(1_000_000_000_000_000_000 + 1)).unwrap_err();
		assert!(matches!(err, DomainError::Validation(ref m) if m.contains("1 units left")), "{err:?}");
	}

	#[test]
	fn a_cap_below_the_issued_supply_stops_issuance_without_trapping_anyone() {
		let mut allocation = registered();
		allocation.open();
		allocation.set_unit_cap(Shares::parse_decimal("100").unwrap()).unwrap();
		let issued = Shares::parse_decimal("500").unwrap();
		assert_eq!(allocation.remaining_capacity(issued), Shares::ZERO);
		assert!(allocation.ensure_capacity(issued, Shares::from_base_units(1)).is_err());
		// Narrowing the cap is a supply decision, not a lifecycle one — holders still exit.
		assert!(allocation.ensure_redeemable().is_ok());
	}

	#[test]
	fn setting_the_cap_is_idempotent_validated_and_audited() {
		let mut allocation = registered();
		allocation.drain_events();
		// Zero would read as "unset" while silently refusing every subscription.
		assert!(allocation.set_unit_cap(Shares::ZERO).is_err());
		allocation.set_unit_cap(Shares::parse_decimal("1000").unwrap()).unwrap();
		allocation.set_unit_cap(Shares::parse_decimal("1000").unwrap()).unwrap();
		let events = allocation.drain_events();
		assert_eq!(events.len(), 1, "re-setting the same cap raises nothing");
		assert!(matches!(events[0], AllocationEvent::CapUpdated { .. }));
		assert_eq!(allocation.unit_cap(), Shares::parse_decimal("1000").unwrap());
	}

	#[test]
	fn capacity_refuses_a_mint_it_cannot_even_add_up() {
		let allocation = registered();
		// `issued + minting` overflowing u128 is refused, never wrapped into "under cap".
		assert!(allocation.ensure_capacity(Shares::from_base_units(u128::MAX), Shares::from_base_units(1)).is_err());
	}

	#[test]
	fn transitions_are_idempotent_and_reopenable() {
		let mut allocation = registered();
		allocation.drain_events();
		allocation.open();
		allocation.open();
		assert_eq!(allocation.drain_events().len(), 1, "re-opening an open allocation raises nothing");
		allocation.close();
		allocation.close();
		assert_eq!(allocation.drain_events().len(), 1);
		allocation.open();
		assert_eq!(allocation.state(), AllocationState::Open);
		assert_eq!(allocation.drain_events().len(), 1, "closed → open re-opens");
	}

	#[test]
	fn details_are_trimmed_and_bounded() {
		let mut allocation = registered();
		allocation
			.update_details("  EV Real Estate  ", "  Income-producing property  ", Some(AllocationIcon::RealEstate))
			.unwrap();
		assert_eq!(allocation.title(), "EV Real Estate");
		assert_eq!(allocation.summary(), "Income-producing property");
		assert_eq!(allocation.icon(), AllocationIcon::RealEstate);
		assert!(allocation.update_details("", "", Some(AllocationIcon::Fund)).is_err());
		assert!(allocation.update_details(&"x".repeat(MAX_TITLE_LEN + 1), "", Some(AllocationIcon::Fund)).is_err());
		assert!(allocation.update_details("ok", &"x".repeat(MAX_SUMMARY_LEN + 1), Some(AllocationIcon::Fund)).is_err());
		// An empty summary is legitimate — not every product needs a one-liner.
		assert!(allocation.update_details("ok", "", Some(AllocationIcon::Fund)).is_ok());
	}

	#[test]
	fn title_refuses_anything_a_mail_would_link() {
		let mut allocation = registered();
		for title in ["Fund https://evil.example", "See HTTP://x", "WWW.evil", "mail ops@evil.example"] {
			assert!(allocation.update_details(title, "", None).is_err(), "{title} must be refused");
		}
		for title in ["EV Fund v2.0", "Acme Inc.", "Aquafix.ae", "St.Petersburg", "Service-Arb 80/20"] {
			assert!(allocation.update_details(title, "", None).is_ok(), "{title} must be accepted");
		}
		assert!(Allocation::register(AllocationId::new(), svc(), "go to https://x", "", AllocationIcon::Fund).is_err());
	}

	#[test]
	fn an_update_that_names_no_icon_keeps_the_one_the_operator_picked() {
		// The regression this `Option` exists for. proto3 gives a scalar `string` no field
		// presence, so before this an older consumer, a stale browser bundle or a pod still
		// running the previous release would send a body with no icon, the boundary would
		// read that as "chose nothing", and a title edit would quietly restyle the product
		// back to `fund` — reporting success the whole way.
		let mut allocation = registered();
		assert_eq!(allocation.icon(), AllocationIcon::Trading);

		allocation.update_details("Renamed", "Still trading", None).unwrap();
		assert_eq!(allocation.icon(), AllocationIcon::Trading, "an absent icon leaves the pick alone");
		assert_eq!(allocation.title(), "Renamed", "the fields the caller DID name still change");

		// `Some` is an explicit set, and `Some(default)` is how an operator resets to the
		// neutral glyph — which stays reachable precisely because it is not what absence means.
		allocation.update_details("Renamed", "", Some(AllocationIcon::Venture)).unwrap();
		assert_eq!(allocation.icon(), AllocationIcon::Venture);
		allocation.update_details("Renamed", "", Some(AllocationIcon::default())).unwrap();
		assert_eq!(allocation.icon(), AllocationIcon::Fund);
	}

	#[test]
	fn the_details_event_carries_the_icon_the_product_ends_up_with() {
		// Not the one the request named: an audit read answers "what does this card look
		// like now" off the last `DetailsUpdated`, so an edit that said nothing about the
		// icon must still record the standing pick rather than a default.
		let mut allocation = registered();
		allocation.drain_events();
		allocation.update_details("Renamed", "", None).unwrap();
		let event = allocation.drain_events().pop().unwrap();
		let AllocationEvent::DetailsUpdated { icon, .. } = event else {
			panic!("expected DetailsUpdated, got {event:?}")
		};
		assert_eq!(icon, AllocationIcon::Trading);
	}

	#[test]
	fn updating_details_leaves_state_alone() {
		let mut allocation = registered();
		allocation.open();
		allocation.drain_events();
		allocation.update_details("Renamed", "", Some(AllocationIcon::Trading)).unwrap();
		assert_eq!(allocation.state(), AllocationState::Open);
		let events = allocation.drain_events();
		assert_eq!(events.len(), 1);
		assert!(matches!(events[0], AllocationEvent::DetailsUpdated { .. }));
	}

	#[test]
	fn event_round_trips_through_json() {
		let mut allocation = registered();
		let event = allocation.drain_events().pop().unwrap();
		let json = serde_json::to_string(&event).unwrap();
		let back: AllocationEvent = serde_json::from_str(&json).unwrap();
		assert!(matches!(back, AllocationEvent::Registered { .. }));
	}

	#[test]
	fn allocation_backing_strings_are_canonical() {
		// Wire contract: these must match `evbanking_contracts::allocation::backing`.
		assert_eq!(AllocationBacking::Cash.as_str(), "cash");
		assert_eq!(AllocationBacking::InKind.as_str(), "in_kind");
		for backing in [AllocationBacking::Cash, AllocationBacking::InKind] {
			assert_eq!(AllocationBacking::parse(backing.as_str()).unwrap(), backing);
			assert_eq!(serde_json::to_string(&backing).unwrap(), format!("\"{}\"", backing.as_str()));
		}
		assert!(AllocationBacking::parse("asset").is_err());
		assert!(AllocationBacking::parse("InKind").is_err(), "the wire form is lowercase snake_case");
		assert_eq!(AllocationBacking::default(), AllocationBacking::Cash, "a registration is cash-backed until a mint says otherwise");
	}

	#[test]
	fn a_registration_is_cash_backed_and_redeemable_until_marked_in_kind() {
		let mut allocation = registered();
		allocation.open();
		allocation.drain_events();
		assert_eq!(allocation.backing(), AllocationBacking::Cash);
		assert!(allocation.ensure_cash_backed().is_ok());

		allocation.mark_in_kind();
		allocation.mark_in_kind();
		assert_eq!(allocation.backing(), AllocationBacking::InKind);
		let events = allocation.drain_events();
		assert_eq!(events.len(), 1, "the second mint on an in-kind product raises nothing");
		assert!(matches!(
			events[0],
			AllocationEvent::BackingChanged {
				backing: AllocationBacking::InKind,
				..
			}
		));
		// The state gate still passes — the product is open — and the backing gate is
		// what refuses, as a precondition an operator can lift, pointing at the book.
		assert!(allocation.ensure_redeemable().is_ok());
		let err = allocation.ensure_cash_backed().unwrap_err();
		assert!(
			matches!(err, DomainError::Precondition(ref m) if m.contains("not backed by fund cash") && m.contains("book")),
			"{err:?}"
		);

		// The operator's way back, idempotent and audited like every other setter.
		allocation.set_backing(AllocationBacking::Cash);
		allocation.set_backing(AllocationBacking::Cash);
		assert_eq!(allocation.drain_events().len(), 1);
		assert!(allocation.ensure_cash_backed().is_ok());
	}

	#[test]
	fn a_pre_backing_registered_payload_still_deserializes() {
		// `event_log` rows written before this field existed carry no `backing` key.
		let mut allocation = registered();
		let json = serde_json::to_string(&allocation.drain_events().pop().unwrap()).unwrap();
		let legacy = json.replace(r#","backing":"cash""#, "");
		assert!(!legacy.contains("backing"), "the legacy payload must actually lack the key: {legacy}");
		let back: AllocationEvent = serde_json::from_str(&legacy).unwrap();
		let AllocationEvent::Registered { backing, .. } = back else {
			panic!("expected Registered, got {back:?}")
		};
		assert_eq!(backing, AllocationBacking::Cash);
	}
}
