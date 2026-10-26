//! Allocation registry use cases — the catalog of investable products.
//!
//! Pure control plane: nothing here touches TigerBeetle or the relay, so no handler
//! notifies it. The write side is Admin/Owner-gated at the service boundary
//! (`bank:allocation:manage`); the read side is open to any authenticated user,
//! filtered to what that user may see, with the unrestricted view behind the same
//! permission.
//!
//! The reserved `fee` and `fund` allocations (#245) are registered by migration 0045 and
//! are NOT an operator's to manage: every write here refuses them first
//! ([`refuse_on_reserved`]). Their holders are seated by the owners' quorum, their state,
//! access, backing and cap are what the migration wrote, and a single `AllocationManage`
//! holder can neither let themselves in nor lock the holders out.
//!
//! [`require_subscribable`] / [`require_redeemable`] are the reason the context exists:
//! they are the gate the fund use cases run before any money moves, turning "this slug
//! parses" into "an operator registered and opened this product — and let this
//! investor in". The two are asymmetric on purpose: the subscribe gate consults the
//! caller's access, the redeem gate never does — it consults the product's *backing*
//! instead, because a redemption pays cash the fund has to actually hold.

use domain::{
	allocations::{Allocation, AllocationAccess, AllocationBacking, AllocationIcon, AllocationId},
	balance::ServiceId,
	error::DomainError,
	money::Shares,
	users::UserId,
};

use crate::ports::allocations::{AllocationAccessGrant, AllocationRecord, AllocationRegistry};

/// The one gate every operator write runs first: a reserved allocation is the platform's
/// own money, held by people the owners seated, and nothing about it is one administrator's
/// to change. Opening the catalog's `invest` to themselves would let them buy in past the
/// quorum; closing it, changing its backing or its cap would let them lock the holders in.
/// `Forbidden`, not `Validation`: the request is well-formed and the caller is not
/// entitled to it.
pub(crate) fn refuse_on_reserved(service: &ServiceId) -> Result<(), DomainError> {
	if service.is_reserved() {
		return Err(DomainError::Forbidden(format!(
			"'{service}' is a reserved allocation: its state, access, backing and cap are not an operator's to change"
		)));
	}
	Ok(())
}

/// Register `service` as a new investable product, in `draft` and at the default
/// access level (`view`: listed once opened, locked until an operator says otherwise).
/// A slug already in the registry is a [`DomainError::Conflict`] — registration never
/// silently overwrites a live product's title or state.
pub async fn register(allocations: &dyn AllocationRegistry, service: ServiceId, title: &str, summary: &str, icon: AllocationIcon) -> Result<Allocation, DomainError> {
	let mut allocation = Allocation::register(AllocationId::new(), service, title, summary, icon)?;
	allocations.register(&mut allocation).await?;
	Ok(allocation)
}

/// Replace an allocation's presentation fields — title, summary and icon. State and
/// identity are untouched. `icon: None` leaves the stored pick alone (the request never
/// mentioned it); `Some` sets it, including a `Some(default)` reset.
pub async fn update_details(allocations: &dyn AllocationRegistry, service: &ServiceId, title: &str, summary: &str, icon: Option<AllocationIcon>) -> Result<Allocation, DomainError> {
	refuse_on_reserved(service)?;
	allocations.update_details(service, title, summary, icon).await
}

/// Resize an allocation's authorised unit supply (idempotent). Lifecycle is untouched:
/// this bounds how many units may still be minted, not whether the product is open.
pub async fn set_unit_cap(allocations: &dyn AllocationRegistry, service: &ServiceId, unit_cap: Shares) -> Result<Allocation, DomainError> {
	refuse_on_reserved(service)?;
	allocations.set_unit_cap(service, unit_cap).await
}

/// Open an allocation for subscriptions (idempotent).
pub async fn open(allocations: &dyn AllocationRegistry, service: &ServiceId) -> Result<Allocation, DomainError> {
	refuse_on_reserved(service)?;
	allocations.open(service).await
}

/// Close an allocation to new subscriptions (idempotent). Redemptions keep working —
/// see [`Allocation::ensure_redeemable`].
pub async fn close(allocations: &dyn AllocationRegistry, service: &ServiceId) -> Result<Allocation, DomainError> {
	refuse_on_reserved(service)?;
	allocations.close(service).await
}

/// Set an allocation's default access level (idempotent). Lifecycle and grants are
/// untouched: this decides who the product deals with by default, not whether it deals.
pub async fn set_access(allocations: &dyn AllocationRegistry, service: &ServiceId, access: AllocationAccess) -> Result<Allocation, DomainError> {
	refuse_on_reserved(service)?;
	allocations.set_access(service, access).await
}

/// Declare what stands behind an allocation's units (idempotent): `cash`, so holders
/// may redeem, or `in_kind`, so they may not and exit through the book. The operator's
/// command; the first in-kind mint sets `in_kind` through the same port on its own.
pub async fn set_backing(allocations: &dyn AllocationRegistry, service: &ServiceId, backing: AllocationBacking) -> Result<Allocation, DomainError> {
	refuse_on_reserved(service)?;
	allocations.set_backing(service, backing).await
}

/// Raise one investor above the default on `service`. A repeat grant overwrites the
/// level; `hidden` is refused (a grant only ever adds).
pub async fn grant_access(
	allocations: &dyn AllocationRegistry,
	service: &ServiceId,
	user: UserId,
	level: AllocationAccess,
	granted_by: UserId,
) -> Result<AllocationAccessGrant, DomainError> {
	refuse_on_reserved(service)?;
	allocations.grant_access(service, user, level, granted_by).await
}

/// Take one investor's grant back (idempotent); they hold the default again.
pub async fn revoke_access(allocations: &dyn AllocationRegistry, service: &ServiceId, user: UserId, revoked_by: UserId) -> Result<(), DomainError> {
	refuse_on_reserved(service)?;
	allocations.revoke_access(service, user, revoked_by).await
}

/// Every standing grant on `service`. `NotFound` if never registered.
pub async fn list_grants(allocations: &dyn AllocationRegistry, service: &ServiceId) -> Result<Vec<AllocationAccessGrant>, DomainError> {
	allocations.list_grants(service).await
}

/// One allocation by service id, in any state, caller-agnostic. `NotFound` if never
/// registered. For anything shown to a caller, use [`get_for`] — this is the internal
/// resolver the valuation and fee paths use, where no investor is looking.
pub async fn get(allocations: &dyn AllocationRegistry, service: &ServiceId) -> Result<Allocation, DomainError> {
	allocations.find(service).await?.ok_or_else(|| not_found(service))
}

/// One allocation as `caller` sees it. `NotFound` if never registered — and, unless
/// `unrestricted`, also when the caller's effective level is `hidden`: a product the
/// caller may not see answers exactly as an unregistered slug does, so the catalog
/// cannot be probed for locked products. `unrestricted` is the `AllocationManage`
/// view (gated at the boundary): every product, still carrying the caller's honest
/// effective level rather than a courtesy `invest`.
pub async fn get_for(allocations: &dyn AllocationRegistry, service: &ServiceId, caller: UserId, unrestricted: bool) -> Result<AllocationRecord, DomainError> {
	let record = allocations.find_for(service, caller).await?.ok_or_else(|| not_found(service))?;
	if !unrestricted && !record.caller_access.permits_viewing() {
		return Err(not_found(service));
	}
	Ok(record)
}

/// The catalog as `caller` sees it: `open` products at effective level `view` or
/// above. `include_unlisted` lifts both filters (permission-gated at the boundary).
pub async fn list_for(allocations: &dyn AllocationRegistry, caller: UserId, include_unlisted: bool) -> Result<Vec<AllocationRecord>, DomainError> {
	allocations.list_for(caller, include_unlisted).await
}

/// Resolve `service` as `user` and assert it takes new money *from them*. The single
/// gate standing between a well-formed slug and a fund existing, now on two axes: an
/// unregistered service — or one hidden from this user — is `NotFound`; a draft/closed
/// one a validation error; an open one the user may only view a
/// [`DomainError::Precondition`], distinguishable by kind from both. Every subscribe
/// runs this **before** reading a balance or pricing a NAV, so an unregistered slug
/// never reaches the ledger at all.
///
/// Returns the resolved allocation because the caller needs it again: the *third* gate
/// ([`Allocation::ensure_capacity`]) can only run once the mint has been priced, and
/// re-reading the row for it would be a second trip to answer a question this one
/// already answered.
pub async fn require_subscribable(allocations: &dyn AllocationRegistry, service: &ServiceId, user: UserId) -> Result<Allocation, DomainError> {
	let record = get_for(allocations, service, user, false).await?;
	record.allocation.ensure_subscribable(record.caller_access)?;
	Ok(record.allocation)
}

/// Resolve `service` and assert investors can still exit it **for cash**. Deliberately
/// laxer than [`require_subscribable`] on the two axes it shares: a `closed` allocation
/// passes, and the caller's access is not consulted at all — refusing here would lock
/// units inside a wound-down or locked product. Hence no `user` parameter: there is
/// nothing about the caller to decide on.
///
/// Stricter on the one axis the subscribe gate never sees: the product's backing. A
/// redemption pays cash out of the fund's claim, and units minted in kind have none
/// there, so an `in_kind` product refuses ([`Allocation::ensure_cash_backed`],
/// `Precondition`) and points the holder at the book. One load answers both questions —
/// state first, so a draft answers "never open" rather than "not backed".
pub async fn require_redeemable(allocations: &dyn AllocationRegistry, service: &ServiceId) -> Result<(), DomainError> {
	let allocation = get(allocations, service).await?;
	allocation.ensure_redeemable()?;
	allocation.ensure_cash_backed()
}

fn not_found(service: &ServiceId) -> DomainError {
	DomainError::NotFound {
		entity: "allocation",
		id: service.to_string(),
	}
}
