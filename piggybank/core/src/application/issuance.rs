//! Unit issuance use cases — an operator minting fund units **in kind**.
//!
//! The one supply path beside [`subscribe`](super::funds::subscribe), and deliberately
//! shaped like it: the registry is resolved first, the mint is priced at a fresh NAV,
//! the cap is checked against what the ledger says is issued, and the relay posts the
//! units after the control-plane commit. What differs is what is *absent* — no cash
//! leg, no claim read, no access gate — because the operator is not an investor
//! spending a balance: they are recording that a holder owns a share of an asset the
//! product was registered against.
//!
//! Admin-only at the boundary (`bank:allocation:manage`). The allocation must be
//! registered but may be in any state: a product sized and seeded before it opens is the
//! normal case, and a closed one may still need its cap table corrected.
//!
//! [`retire_units`] is the mint's mirror — a holder's units burnt with no cash leg —
//! with a Read-First on what the holder has free and a closed-door gate that `force`
//! overrides. The hand-over out of the company's stake (the stake-transfer RPC) was
//! retired with the company holder itself (#245); the two rows it and the company mint
//! wrote are history ([`StoredIssuance::RetiredCompany`]), and their key is taken for good.
//!
//! **The reserved allocations are not the operator's to mint.** `fee` and `fund` are the
//! platform's own money, held by people (#245); a unit of either dilutes every holder
//! already there, and who holds the owners' money is the owners' call, not one
//! administrator's. [`issue_units`] and [`retire_units`] therefore refuse a reserved
//! `service` outright, and the one door that mints reserved units — [`grant_units`] — is
//! reached by no RPC. A holder LEAVES a reserved allocation by redemption at its NAV, never
//! by an operator's burn.
//!
//! A mint also flips the product's backing to `in_kind` (see
//! [`AllocationBacking`](domain::allocations::AllocationBacking)): the units it creates
//! have no cash in the fund's claim, so the redeem path has to know before the first
//! holder asks to be paid out of it.

use domain::{
	allocations::{AllocationBacking, AllocationState},
	balance::ServiceId,
	error::DomainError,
	issuance::{IdempotencyKey, IssuanceSource, UnitHolder, UnitIssuance, UnitIssuanceId},
	money::{Shares, Usdt},
};

use crate::{
	application::{
		allocations as allocations_app,
		funds::{self as funds_app, FundPorts},
		ownership as ownership_app,
	},
	ports::{
		UnitIssuanceRepository, UserRepository,
		allocations::AllocationRegistry,
		issuance::{IssueOutcome, StoredIssuance, UnitIssuanceRecord},
		ledger::Ledger,
	},
};

/// One issuance as the operator asked for it. `cost_basis: None` prices it as
/// `units × NAV` at the dealing mark; `Some` is taken as given, zero included.
pub struct IssueUnitsRequest {
	pub service: ServiceId,
	pub holder: UnitHolder,
	pub units: Shares,
	pub cost_basis: Option<Usdt>,
	pub idempotency_key: IdempotencyKey,
}

/// One retirement as the operator asked for it. `cost_basis: None` records `units × NAV`
/// as the book value written off; `force` burns out of a product that is not `closed`.
pub struct RetireUnitsRequest {
	pub service: ServiceId,
	pub holder: UnitHolder,
	pub units: Shares,
	pub cost_basis: Option<Usdt>,
	pub idempotency_key: IdempotencyKey,
	pub force: bool,
}

/// A fund's issued supply and who holds it — the cap table, every line read from the
/// ledger (issue #245: every unit has a holder, and the table is a sum of what is, not
/// a remainder). A person's line is their free units plus the units resting in their
/// sell orders; the `fee` allocation's line is the product's fee class. Holders with
/// nothing left are not listed.
///
/// `queued_units` is the one figure not read from the ledger: mints recorded but not yet
/// posted by the relay. The settled supply is what `ensure_capacity` reads, so an
/// operator pinning the cap to it while this is non-zero pins it below where the supply
/// is about to land.
pub struct UnitHoldersView {
	pub service: ServiceId,
	pub units_outstanding: Shares,
	/// Largest holding first; ties broken by the holder's stored identity, so the table
	/// reads the same on every refresh.
	pub holders: Vec<UnitHolding>,
	pub queued_units: Shares,
}

/// One line of the cap table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitHolding {
	pub holder: UnitHolder,
	pub units: Shares,
}

/// Mint `request.units` of `request.service` to `request.holder` with no cash leg.
///
/// Idempotent by `(service, idempotency_key)`: a repeat returns the issuance the key
/// already names — the same row, one mint — provided it asks for the same holder and
/// units. A repeat that names the same key for a *different* request is a
/// [`DomainError::Conflict`]: an admin console retrying a timed-out call is the case
/// the key exists for, and a key reused for a new mint is the mistake it must catch.
///
/// Gates, in order: the key is read before anything is priced (a retry must succeed
/// even if the mark has since gone stale); the holder must be one the graph admits
/// ([`UnitHolder::ensure_may_hold`] — reserved holds product, never the reverse; pure,
/// so it runs before any read could fail for a reason that hides it); the allocation
/// must be registered, in any state; the holder must exist — a user (units minted to a
/// UUID nobody can sign in as are units nobody can redeem) or a registered reserved
/// allocation (the `fee` row migration `0045` wrote); the NAV must be fresh, because it
/// is recorded on the row and blended into the holder's high-water mark; and the cap
/// must hold
/// ([`Allocation::ensure_capacity`](domain::allocations::Allocation::ensure_capacity)),
/// with the same issuance-gate-not-invariant caveats as a subscription. An operator
/// sizing a product below what they mean to issue raises the cap first.
///
/// Once every gate has passed and before the row is written, a `cash` product is marked
/// `in_kind`: its units are about to include some the fund holds no cash for, and the
/// redeem gate must see that before a holder asks to be paid. Idempotent, so the second
/// mint on the same product touches nothing. The order is deliberate: if the flip lands
/// and the record then fails, the product is `in_kind` with no mint behind it — an
/// operator undoes that with one command, and nothing was at risk in between. The
/// reverse (a mint recorded on a product still `cash`) would let the next redemption
/// price units the fund cannot pay for, which is the failure this flag exists to stop.
///
/// A reserved allocation is refused before any of that (see the module header): its
/// units come only through [`grant_units`]. So is a reserved allocation AS THE HOLDER
/// (#245, L-1): the `fee` allocation's units of a product are its fee class, minted by
/// the fee accrual as the charge earns them, and an operator minting product units into
/// `fee` by hand would be inflating the owners' allocation at the product's investors'
/// expense with no fee behind it.
pub async fn issue_units(
	ports: &FundPorts<'_>,
	issuances: &dyn UnitIssuanceRepository,
	users: &dyn UserRepository,
	request: IssueUnitsRequest,
	now_unix: i64,
) -> Result<UnitIssuanceRecord, DomainError> {
	if request.service.is_reserved() {
		return Err(DomainError::Forbidden(format!(
			"'{}' is a reserved allocation: its units are never issued by hand",
			request.service
		)));
	}
	if let UnitHolder::Allocation(allocation) = &request.holder {
		return Err(DomainError::Forbidden(format!(
			"the '{allocation}' allocation's units of a product are its fee class, minted only by the fee accrual — never issued by hand"
		)));
	}
	mint(ports, issuances, users, request, now_unix).await
}

/// Mint units of a RESERVED allocation to a person. Everything [`issue_units`] checks is
/// checked here too (the key, the holder graph, the registry row, a fresh NAV, the cap);
/// what differs is who may reach it: no RPC does.
///
/// The backing stays `cash`, deliberately (see [`mint`]): a reserved allocation's units
/// are backed by the cash on its own claim, and its holders are paid out of exactly that.
pub async fn grant_units(
	ports: &FundPorts<'_>,
	issuances: &dyn UnitIssuanceRepository,
	users: &dyn UserRepository,
	request: IssueUnitsRequest,
	now_unix: i64,
) -> Result<UnitIssuanceRecord, DomainError> {
	if !request.service.is_reserved() {
		return Err(DomainError::Validation(format!(
			"'{}' is a product, not a reserved allocation: its units are issued, not granted",
			request.service
		)));
	}
	mint(ports, issuances, users, request, now_unix).await
}

/// The shared body of [`issue_units`] and [`grant_units`], once the target has been
/// admitted: the key, the holder graph, the registry row, the holder, a fresh NAV, the
/// cap, the backing flip (products only) and the record.
async fn mint(
	ports: &FundPorts<'_>,
	issuances: &dyn UnitIssuanceRepository,
	users: &dyn UserRepository,
	request: IssueUnitsRequest,
	now_unix: i64,
) -> Result<UnitIssuanceRecord, DomainError> {
	let identity = RequestIdentity {
		service: &request.service,
		holder: &request.holder,
		source: IssuanceSource::Mint,
		units: request.units,
		idempotency_key: &request.idempotency_key,
	};
	if let Some(existing) = issuances.find_by_key(&request.service, &request.idempotency_key).await? {
		return identity.same_stored_request_or_conflict(existing);
	}
	// Pure and first: a holder the graph refuses is refused as such, before any read
	// could fail for a reason that hides it.
	request.holder.ensure_may_hold(&request.service)?;
	let allocation = allocations_app::get(ports.allocations, &request.service).await?;
	require_holder(ports.allocations, users, &request.holder).await?;
	let price = funds_app::dealing_nav(ports.nav, ports.ledger, &request.service, now_unix).await?;
	allocation.ensure_capacity(funds_app::issued_units(ports.ledger, &request.service).await?, request.units)?;
	let issuance = UnitIssuance::issue(
		UnitIssuanceId::new(),
		request.service.clone(),
		request.holder.clone(),
		request.units,
		price,
		request.cost_basis,
		request.idempotency_key.clone(),
	)?;
	// A reserved allocation stays `cash`: its units are backed by the cash on its own
	// claim (seed capital, settled fees), and a holder is paid out of exactly that —
	// flipping it would refuse the one exit its holders have (#245).
	if allocation.backing() == AllocationBacking::Cash && !request.service.is_reserved() {
		ports.allocations.set_backing(&request.service, AllocationBacking::InKind).await?;
	}
	record(ports, issuances, issuance, &identity).await
}

/// Burn `request.units` of `request.service` out of `request.holder`'s account: `Dr
/// SharesOutstanding / Cr <holder shares>` by the relay, supply shrinking by exactly
/// that, no cash moving — the mirror of [`issue_units`], for units that should never
/// have been minted or that stand for an asset the holder no longer owns.
///
/// Same record, same key contract as the mint (a repeat with the same holder and
/// units returns the row; the same key for a different request, a mint included, is a
/// [`DomainError::Conflict`]). Gates, in order: the key is read first; the allocation
/// must be registered and — unless `force` — `closed` ([`DomainError::Precondition`]
/// otherwise: burning a holder's units out of a live product is a decision that
/// deserves a closed door first, and `force` is the operator saying so explicitly); a
/// user holder must exist; the NAV must be fresh (recorded on the row, and the default
/// basis is `units × NAV`); and the holder must have at least `units` **available** on
/// the ledger (Read-First; a shortfall is a [`DomainError::Validation`]) — units
/// resting on the book or reserved by a redemption are spoken for and stay. No cap
/// check: supply only shrinks. The holder's account is debit-normal with the
/// non-negative flag, so an over-retire that races the read parks.
///
/// A reserved allocation is refused: its holders are people holding the platform's own
/// money, and they leave by redeeming at NAV — an operator has no burn over them.
pub async fn retire_units(
	ports: &FundPorts<'_>,
	issuances: &dyn UnitIssuanceRepository,
	users: &dyn UserRepository,
	request: RetireUnitsRequest,
	now_unix: i64,
) -> Result<UnitIssuanceRecord, DomainError> {
	if request.service.is_reserved() {
		return Err(DomainError::Forbidden(format!(
			"'{}' is a reserved allocation: a holder leaves it by redeeming at NAV, never by an operator's burn",
			request.service
		)));
	}
	let identity = RequestIdentity {
		service: &request.service,
		holder: &request.holder,
		source: IssuanceSource::Retire,
		units: request.units,
		idempotency_key: &request.idempotency_key,
	};
	if let Some(existing) = issuances.find_by_key(&request.service, &request.idempotency_key).await? {
		return identity.same_stored_request_or_conflict(existing);
	}
	request.holder.ensure_may_hold(&request.service)?;
	let allocation = allocations_app::get(ports.allocations, &request.service).await?;
	if allocation.state() != AllocationState::Closed && !request.force {
		return Err(DomainError::Precondition(format!(
			"allocation '{}' is open — close it before retiring units, or set force to retire on a live product",
			request.service
		)));
	}
	require_holder(ports.allocations, users, &request.holder).await?;
	let price = funds_app::dealing_nav(ports.nav, ports.ledger, &request.service, now_unix).await?;
	let held = Shares::from_base_units(ports.ledger.balance(&request.holder.shares_key(&request.service)?).await?.available());
	if held < request.units {
		return Err(DomainError::Validation(format!(
			"holder holds {} available units of '{}' (units resting on the book or reserved by a redemption cannot be retired), cannot retire {}",
			held.to_decimal_string(),
			request.service,
			request.units.to_decimal_string()
		)));
	}
	let issuance = UnitIssuance::retire(
		UnitIssuanceId::new(),
		request.service.clone(),
		request.holder.clone(),
		request.units,
		price,
		request.cost_basis,
		request.idempotency_key.clone(),
	)?;
	record(ports, issuances, issuance, &identity).await
}

/// The holder must exist: units minted to a UUID nobody can sign in as are units nobody
/// can redeem, and units minted to an allocation with no registry row would trip the
/// `holder_service` foreign key as an opaque repository error instead of this
/// `NotFound`. A person must also be ACTIVE (#245, L-2): a frozen account can no more
/// redeem than a missing one, and a holder grant approved over an active person must not
/// mint to them once they have been disabled during the vote. Whether the holder may
/// hold at all ([`UnitHolder::ensure_may_hold`]) is checked by the use case before any
/// read.
pub(crate) async fn require_holder(allocations: &dyn AllocationRegistry, users: &dyn UserRepository, holder: &UnitHolder) -> Result<(), DomainError> {
	match holder {
		UnitHolder::User(user) => {
			let Some(row) = users.find_by_id(*user).await? else {
				return Err(DomainError::NotFound {
					entity: "user",
					id: user.to_string(),
				});
			};
			if !row.is_active() {
				return Err(DomainError::Precondition(format!("user {user} is not active, so no units can be minted to them")));
			}
			Ok(())
		}
		UnitHolder::Allocation(service) => allocations_app::get(allocations, service).await.map(drop),
	}
}

async fn record(ports: &FundPorts<'_>, issuances: &dyn UnitIssuanceRepository, mut issuance: UnitIssuance, identity: &RequestIdentity<'_>) -> Result<UnitIssuanceRecord, DomainError> {
	match issuances.issue(&mut issuance).await? {
		IssueOutcome::Recorded(record) => {
			ports.relay.notify_one();
			Ok(record)
		}
		// Lost the race to a concurrent send of the same request: the winner's row is
		// the answer, subject to the same parameter check as a sequential repeat.
		IssueOutcome::Existing(existing) => identity.same_request_or_conflict(existing),
	}
}

/// What makes two requests under one key "the same": the facts the operator chose,
/// not the ones the hub computed (NAV, a defaulted basis).
struct RequestIdentity<'a> {
	service: &'a ServiceId,
	holder: &'a UnitHolder,
	source: IssuanceSource,
	units: Shares,
	idempotency_key: &'a IdempotencyKey,
}

impl RequestIdentity<'_> {
	/// [`Self::same_request_or_conflict`] over what a key lookup found: a historical
	/// company-stake row is never the same request as anything asked today.
	fn same_stored_request_or_conflict(&self, existing: StoredIssuance) -> Result<UnitIssuanceRecord, DomainError> {
		match existing {
			StoredIssuance::Live(record) => self.same_request_or_conflict(record),
			StoredIssuance::RetiredCompany(_) => Err(DomainError::Conflict(format!(
				"idempotency key '{}' already names a historical company-stake issuance on '{}'",
				self.idempotency_key.as_str(),
				self.service
			))),
		}
	}

	fn same_request_or_conflict(&self, existing: UnitIssuanceRecord) -> Result<UnitIssuanceRecord, DomainError> {
		if existing.issuance.matches_request(self.holder, self.source, self.units) {
			Ok(existing)
		} else {
			Err(DomainError::Conflict(format!(
				"idempotency key '{}' already names a different issuance on '{}'",
				self.idempotency_key.as_str(),
				self.service
			)))
		}
	}
}

/// The settled supply of `service` and every holder of it, plus the mints still in
/// flight. Gated on the allocation existing, like the NAV view: a cap table for a
/// product no registry entry backs is a cap table for a fund that does not exist.
///
/// The holders are summed from the ledger's holding accounts by
/// [`ownership_app::allocation_ownership`], the one read of who holds an allocation.
/// The sum is read at one instant per account, so it can differ from
/// `units_outstanding` by a mint landing mid-scan — a read-only view over a moving
/// ledger reports both figures and lets the reader compare, rather than failing or
/// inventing a remainder.
pub async fn unit_holders(allocations: &dyn AllocationRegistry, ledger: &dyn Ledger, issuances: &dyn UnitIssuanceRepository, service: ServiceId) -> Result<UnitHoldersView, DomainError> {
	allocations_app::get(allocations, &service).await?;
	let queued = issuances.queued_mint_units(&service).await?;
	let ownership = ownership_app::allocation_ownership(ledger, service).await?;
	Ok(UnitHoldersView {
		service: ownership.service,
		units_outstanding: ownership.units_outstanding,
		holders: ownership.holders,
		queued_units: queued,
	})
}
