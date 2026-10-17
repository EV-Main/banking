//! `allocations` context — the registry of investable products.
//!
//! Reads are open to any authenticated user and filtered to what that user may see;
//! every write, and the unfiltered view of the catalog, is gated on
//! `bank:allocation:manage` (Admin/Owner) — the same trust seam as posting a
//! valuation, because registering a product is what brings a fund into existence at
//! all, and opening it to an investor is what lets their money in.
//!
//! No money crosses this surface — with one deliberate exception. [`IssueUnits`]
//! (`AllocationsService::issue_units`) mints units **in kind**, with no cash leg: it is a
//! registry decision about who holds what, made by the same manager who sizes the
//! product, so it lives beside the cap rather than on the investor's dealing surface.
//! [`RetireUnits`] is the mint's mirror, a holder's units burnt. They and
//! [`ListUnitHolders`] are the only handlers here that read the ledger or notify the
//! relay. Both name a PERSON and nothing else: the company is not a holder (#245), and
//! the reserved `fee` allocation comes to hold a product's fee class through the fee
//! accrual, never through an operator's mint.
//!
//! [`IssueUnits`]: AllocationsService::issue_units
//! [`RetireUnits`]: AllocationsService::retire_units
//! [`ListUnitHolders`]: AllocationsService::list_unit_holders
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use concierge_domain::authz::bank;
use domain::{
	allocations::{AllocationAccess, AllocationBacking, AllocationIcon},
	balance::ServiceId,
	issuance::{IdempotencyKey, UnitHolder},
	money::{Shares, Usdt},
	users::{ConciergeUserId, UserId},
};
use evbanking_contracts::{
	allocation::{access as wire_access, backing as wire_backing, state as wire_state},
	banking::v1::{self as pb, allocations_service_server::AllocationsService},
};
use tonic::{Request, Response, Status};

use crate::{
	AppState,
	application::{
		allocations as allocations_app,
		funds::{self as funds_app, FundPorts},
		issuance::{self as issuance_app, IssueUnitsRequest, RetireUnitsRequest, UnitHoldersView, UnitHolding},
	},
	ports::{
		allocations::{AllocationAccessGrant, AllocationRecord},
		issuance::UnitIssuanceRecord,
	},
	services::support::{caller_id, holds_permission, map_err, optional, require_not_paused, require_permission, resolve_target_user, unix_now},
};

#[derive(Clone)]
pub struct AllocationsSvc {
	pub state: AppState,
}

impl AllocationsSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}

	/// The response every write handler ends on: the product re-read as the caller
	/// sees it. The aggregate a transition returns is deliberately clock-free and knows
	/// nothing of the caller's grant, so rather than answer with zeroed timestamps and a
	/// guessed `caller_access`, the handler pays one indexed read for an honest row —
	/// these are operator commands, not the hot path. Unrestricted: the caller just
	/// proved `AllocationManage`, so a product they hid from themselves still renders.
	async fn manager_view(&self, caller: UserId, service: &ServiceId) -> Result<Response<pb::Allocation>, Status> {
		let record = allocations_app::get_for(self.state.allocations.as_ref(), service, caller, true).await.map_err(map_err)?;
		Ok(Response::new(record_to_proto(&record)))
	}

	/// The holder a mint or a retirement names: always a person. The console names
	/// investors by their concierge id; resolve the way every admin RPC does, so the
	/// units land on (or leave) the money-plane row the holder redeems from. `NOT_FOUND`
	/// here is the existence gate the use case repeats. An empty id names nobody — a
	/// malformed request, never a mint to (or a burn of) nobody's units.
	async fn resolve_holder(&self, user_id: &str) -> Result<UnitHolder, Status> {
		let raw = optional(user_id).ok_or_else(|| Status::invalid_argument("user_id is required: units are issued to, and retired from, a person"))?;
		Ok(UnitHolder::User(resolve_target_user(&self.state, raw).await?))
	}
}

#[tonic::async_trait]
impl AllocationsService for AllocationsSvc {
	async fn list_allocations(&self, request: Request<pb::ListAllocationsRequest>) -> Result<Response<pb::AllocationList>, Status> {
		let caller = caller_id(&request)?;
		let include_unlisted = request.get_ref().include_unlisted;
		// Refuse rather than silently downgrade to the visible list: a caller that asked
		// for drafts and got a filtered list would read it as "there are none".
		if include_unlisted {
			require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		}
		let records = allocations_app::list_for(self.state.allocations.as_ref(), caller, include_unlisted).await.map_err(map_err)?;
		Ok(Response::new(pb::AllocationList {
			allocations: records.iter().map(record_to_proto).collect(),
		}))
	}

	async fn get_allocation(&self, request: Request<pb::GetAllocationRequest>) -> Result<Response<pb::Allocation>, Status> {
		// Any authenticated user, any state — an investor holding units of a closed
		// product still has to render it. A product hidden from THIS caller is NOT_FOUND,
		// unless they hold AllocationManage — a question here, not a gate, because the
		// handler serves everyone and merely widens for a manager — or unless they hold
		// its units: a holder of the hidden `fee`/`fund` allocation reads the title of
		// what they own (#245), the same as their position card shows its price.
		let caller = caller_id(&request)?;
		let unrestricted = holds_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let record = funds_app::allocation_for_holder(self.state.allocations.as_ref(), self.state.ledger.as_ref(), &service, caller, unrestricted)
			.await
			.map_err(map_err)?;
		Ok(Response::new(record_to_proto(&record)))
	}

	async fn register_allocation(&self, request: Request<pb::RegisterAllocationRequest>) -> Result<Response<pb::Allocation>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let caller = caller_id(&request)?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let icon = parse_icon(&req.icon)?;
		allocations_app::register(self.state.allocations.as_ref(), service.clone(), &req.title, &req.summary, icon)
			.await
			.map_err(map_err)?;
		self.manager_view(caller, &service).await
	}

	async fn update_allocation(&self, request: Request<pb::UpdateAllocationRequest>) -> Result<Response<pb::Allocation>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let caller = caller_id(&request)?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let icon = parse_icon_update(req.icon.as_deref())?;
		allocations_app::update_details(self.state.allocations.as_ref(), &service, &req.title, &req.summary, icon)
			.await
			.map_err(map_err)?;
		self.manager_view(caller, &service).await
	}

	async fn set_allocation_unit_cap(&self, request: Request<pb::SetAllocationUnitCapRequest>) -> Result<Response<pb::Allocation>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let caller = caller_id(&request)?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		// Parsed at the boundary, so a malformed cap is an `invalid_argument` about the
		// input rather than a validation error from inside the aggregate.
		let unit_cap = Shares::parse_decimal(&req.unit_cap).map_err(map_err)?;
		allocations_app::set_unit_cap(self.state.allocations.as_ref(), &service, unit_cap).await.map_err(map_err)?;
		self.manager_view(caller, &service).await
	}

	async fn set_allocation_state(&self, request: Request<pb::SetAllocationStateRequest>) -> Result<Response<pb::Allocation>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let caller = caller_id(&request)?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		match req.state.as_str() {
			wire_state::OPEN => allocations_app::open(self.state.allocations.as_ref(), &service).await,
			wire_state::CLOSED => allocations_app::close(self.state.allocations.as_ref(), &service).await,
			// `draft` is entered only by RegisterAllocation — a product that has taken
			// money must never be able to travel back to "never opened".
			other =>
				return Err(Status::invalid_argument(format!(
					"allocation state must be '{}' or '{}', got '{other}'",
					wire_state::OPEN,
					wire_state::CLOSED
				))),
		}
		.map_err(map_err)?;
		self.manager_view(caller, &service).await
	}

	async fn set_allocation_access(&self, request: Request<pb::SetAllocationAccessRequest>) -> Result<Response<pb::Allocation>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let caller = caller_id(&request)?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let access = parse_access(&req.access)?;
		allocations_app::set_access(self.state.allocations.as_ref(), &service, access).await.map_err(map_err)?;
		self.manager_view(caller, &service).await
	}

	async fn set_allocation_backing(&self, request: Request<pb::SetAllocationBackingRequest>) -> Result<Response<pb::Allocation>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let caller = caller_id(&request)?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let backing = parse_backing(&req.backing)?;
		allocations_app::set_backing(self.state.allocations.as_ref(), &service, backing).await.map_err(map_err)?;
		self.manager_view(caller, &service).await
	}

	async fn grant_allocation_access(&self, request: Request<pb::GrantAllocationAccessRequest>) -> Result<Response<pb::AllocationAccessGrant>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let granted_by = caller_id(&request)?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		// The console names investors by their concierge id; resolve the way every admin
		// RPC does, so a grant lands on the money-plane row the subscribe gate reads.
		let user = resolve_target_user(&self.state, &req.user_id).await?;
		let level = parse_access(&req.level)?;
		let grant = allocations_app::grant_access(self.state.allocations.as_ref(), &service, user, level, granted_by)
			.await
			.map_err(map_err)?;
		Ok(Response::new(grant_to_proto(&grant)))
	}

	async fn revoke_allocation_access(&self, request: Request<pb::RevokeAllocationAccessRequest>) -> Result<Response<pb::RevokeAllocationAccessResponse>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let revoked_by = caller_id(&request)?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let user = resolve_target_user(&self.state, &req.user_id).await?;
		allocations_app::revoke_access(self.state.allocations.as_ref(), &service, user, revoked_by)
			.await
			.map_err(map_err)?;
		Ok(Response::new(pb::RevokeAllocationAccessResponse {}))
	}

	async fn list_allocation_access_grants(&self, request: Request<pb::ListAllocationAccessGrantsRequest>) -> Result<Response<pb::AllocationAccessGrantList>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let grants = allocations_app::list_grants(self.state.allocations.as_ref(), &service).await.map_err(map_err)?;
		Ok(Response::new(pb::AllocationAccessGrantList {
			grants: grants.iter().map(grant_to_proto).collect(),
		}))
	}

	async fn issue_units(&self, request: Request<pb::IssueUnitsRequest>) -> Result<Response<pb::UnitIssuance>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		require_not_paused(&self.state).await?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let holder = self.resolve_holder(&req.user_id).await?;
		// Parsed at the boundary, so a malformed amount is an `invalid_argument` about the
		// input rather than a validation error from inside the aggregate.
		let units = Shares::parse_decimal(&req.units).map_err(map_err)?;
		let cost_basis = optional(&req.cost_basis).map(Usdt::parse_decimal).transpose().map_err(map_err)?;
		let idempotency_key = IdempotencyKey::parse(&req.idempotency_key).map_err(map_err)?;
		let ports = FundPorts {
			allocations: self.state.allocations.as_ref(),
			ledger: self.state.ledger.as_ref(),
			nav: self.state.nav.as_ref(),
			relay: &self.state.relay_notify,
		};
		let record = issuance_app::issue_units(
			&ports,
			self.state.issuances.as_ref(),
			self.state.users.as_ref(),
			IssueUnitsRequest {
				service,
				holder,
				units,
				cost_basis,
				idempotency_key,
			},
			unix_now(),
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(issuance_to_proto(&record)))
	}

	async fn retire_units(&self, request: Request<pb::RetireUnitsRequest>) -> Result<Response<pb::UnitIssuance>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		require_not_paused(&self.state).await?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let holder = self.resolve_holder(&req.user_id).await?;
		let units = Shares::parse_decimal(&req.units).map_err(map_err)?;
		let cost_basis = optional(&req.cost_basis).map(Usdt::parse_decimal).transpose().map_err(map_err)?;
		let idempotency_key = IdempotencyKey::parse(&req.idempotency_key).map_err(map_err)?;
		let ports = FundPorts {
			allocations: self.state.allocations.as_ref(),
			ledger: self.state.ledger.as_ref(),
			nav: self.state.nav.as_ref(),
			relay: &self.state.relay_notify,
		};
		let record = issuance_app::retire_units(
			&ports,
			self.state.issuances.as_ref(),
			self.state.users.as_ref(),
			RetireUnitsRequest {
				service,
				holder,
				units,
				cost_basis,
				idempotency_key,
				force: req.force,
			},
			unix_now(),
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(issuance_to_proto(&record)))
	}

	async fn list_unit_holders(&self, request: Request<pb::ListUnitHoldersRequest>) -> Result<Response<pb::UnitHolders>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let view = issuance_app::unit_holders(self.state.allocations.as_ref(), self.state.ledger.as_ref(), self.state.issuances.as_ref(), service)
			.await
			.map_err(map_err)?;
		Ok(Response::new(holders_to_proto(&view)))
	}
}

fn issuance_to_proto(record: &UnitIssuanceRecord) -> pb::UnitIssuance {
	let issuance = &record.issuance;
	pb::UnitIssuance {
		id: issuance.id().to_string(),
		service: issuance.service().to_string(),
		holder_kind: issuance.holder().kind_str().to_owned(),
		holder_id: holder_id_string(issuance.holder()),
		units: issuance.units().to_decimal_string(),
		nav: issuance.nav().to_decimal_string(),
		cost_basis: issuance.cost_basis().to_decimal_string(),
		state: issuance.state().as_str().to_owned(),
		created_at: record.created_at,
		source: issuance.source().as_str().to_owned(),
	}
}

/// The holder's identity as `holder_kind` says to read it: a user's id or an allocation's
/// slug (see `contracts::allocation::holder`).
fn holder_id_string(holder: &UnitHolder) -> String {
	match holder {
		UnitHolder::User(user) => user.to_string(),
		UnitHolder::Allocation(service) => service.to_string(),
	}
}

/// One line of a cap table on the wire. Shared with the treasury
/// (`BalanceService::GetTreasury`, `GetFundRevenue`), which lists every allocation's
/// holders in this same shape — one construction, so a holder is named the same way on
/// every screen.
pub(crate) fn holding_to_proto(line: &UnitHolding) -> pb::UnitHolding {
	pb::UnitHolding {
		holder: Some(pb::UnitHolderRef {
			kind: line.holder.kind_str().to_owned(),
			id: holder_id_string(&line.holder),
		}),
		units: line.units.to_decimal_string(),
	}
}

/// The cap table as the ledger holds it (#245): every holder and their units, largest
/// first, the way the use case orders them. Nothing is summed into classes here — a
/// person and the `fee` allocation are both lines, and the supply invariant is the
/// reconciliation's to check, not this view's to derive.
fn holders_to_proto(view: &UnitHoldersView) -> pb::UnitHolders {
	pb::UnitHolders {
		service: view.service.to_string(),
		units_outstanding: view.units_outstanding.to_decimal_string(),
		queued_units: view.queued_units.to_decimal_string(),
		holders: view.holders.iter().map(holding_to_proto).collect(),
	}
}

fn record_to_proto(record: &AllocationRecord) -> pb::Allocation {
	let allocation = &record.allocation;
	pb::Allocation {
		service: allocation.service().to_string(),
		title: allocation.title().to_owned(),
		summary: allocation.summary().to_owned(),
		state: allocation.state().as_str().to_owned(),
		created_at: record.created_at,
		updated_at: record.updated_at,
		unit_cap: allocation.unit_cap().to_decimal_string(),
		icon: allocation.icon().as_str().to_owned(),
		access: allocation.access().as_str().to_owned(),
		caller_access: record.caller_access.as_str().to_owned(),
		backing: allocation.backing().as_str().to_owned(),
	}
}

/// The ids cross the wire as the CONSOLE knows them — the concierge mirror when the
/// bridge has written one, the hub's own id otherwise. That is the exact order
/// [`resolve_target_user`] accepts on the way in, so a revoke that echoes a listed id
/// lands on the same row, and the console can look the person up in its directory
/// instead of showing a uuid no other screen recognises. The storage id stays what the
/// grants table holds; only the presentation changes.
fn grant_to_proto(grant: &AllocationAccessGrant) -> pb::AllocationAccessGrant {
	pb::AllocationAccessGrant {
		service: grant.service.to_string(),
		user_id: console_user_id(grant.concierge_user_id, grant.user_id),
		level: grant.level.as_str().to_owned(),
		granted_by: console_user_id(grant.granted_by_concierge_id, grant.granted_by),
		granted_at: grant.granted_at,
	}
}

fn console_user_id(concierge: Option<ConciergeUserId>, banking: UserId) -> String {
	concierge.map_or_else(|| banking.to_string(), |id| id.to_string())
}

/// An access level a request named, parsed strictly. Unlike the icon there is no
/// "chose nothing" here: an empty level is as much a client bug as an unknown one,
/// because every level gates money and none is a safe guess. The message names the
/// vocabulary so an operator sees what was expected, not just that it was wrong. The
/// grant handler runs the same parser — `hidden` is a real level, and refusing it FOR A
/// GRANT is the aggregate's rule, so it is refused there with its own reason.
fn parse_access(raw: &str) -> Result<AllocationAccess, Status> {
	AllocationAccess::parse(raw).map_err(|_| Status::invalid_argument(format!("access level must be one of {}, got '{raw}'", wire_access::ALL.join(", "))))
}

/// A backing a request named, parsed as strictly as an access level and for the same
/// reason: it gates money (the way out), so neither empty nor unknown is a safe guess.
fn parse_backing(raw: &str) -> Result<AllocationBacking, Status> {
	AllocationBacking::parse(raw).map_err(|_| Status::invalid_argument(format!("backing must be one of {}, got '{raw}'", wire_backing::ALL.join(", "))))
}

/// The icon a *request* named. Empty means "the operator chose nothing", which is
/// [`AllocationIcon::default`]. Anything else is parsed strictly: an icon this build
/// cannot draw is an `invalid_argument` about the request, never a product silently
/// stored as something the operator did not pick.
///
/// Strict here and lenient in the storage adapter is deliberate, and the direction is
/// what decides it: client → hub rejects a value it does not know, because the request
/// has not been acted on yet and the caller can be told. Hub ← storage does not, because
/// the row is already written and refusing a read would fail `find`/`list` — the money
/// plane's gate — over a presentation column. See
/// [`crate::infrastructure::allocations`].
fn parse_icon(raw: &str) -> Result<AllocationIcon, Status> {
	if raw.is_empty() {
		return Ok(AllocationIcon::default());
	}
	AllocationIcon::parse(raw).map_err(map_err)
}

/// The same, for `UpdateAllocationRequest`, where the field carries presence.
///
/// `None` (field unset) is "the caller did not talk about the icon" and leaves the
/// stored pick alone. `Some("")` is an explicit reset to the default — a distinction a
/// bare proto3 `string` cannot express, which is why the field is `optional`: without it
/// a consumer built before the icon existed, a stale browser bundle, or a pod still on
/// the previous release would erase the operator's choice on every title edit.
fn parse_icon_update(raw: Option<&str>) -> Result<Option<AllocationIcon>, Status> {
	raw.map(parse_icon).transpose()
}

/// The wire vocabulary in `evbanking_contracts` is what consumer repos match on; the
/// domain enum is what the hub stores. They are two halves of one contract, so drift
/// between them is a compile-and-test-time failure, not a runtime mystery.
#[cfg(test)]
mod tests {
	use domain::{
		allocations::{AllocationState, DEFAULT_UNIT_CAP},
		issuance::{IssuanceSource, IssuanceState},
	};
	// Only the guards below read these vocabularies — the handlers go through the domain
	// enums, which are the authority on what a stored value may be.
	use evbanking_contracts::allocation::{holder as wire_holder, icon as wire_icon, issuance_source as wire_issuance_source, issuance_state as wire_issuance_state};

	use super::*;

	#[test]
	fn domain_holders_and_issuance_states_match_the_wire_contract() {
		// The three vocabularies an in-kind issuance crosses the wire with, held to the same
		// standard as state/access/icon: the hub stores the domain enum, consumers match on
		// the wire constants, and drift between them is a test failure, not a mystery.
		let holders = [UnitHolder::User(UserId::new()), UnitHolder::Allocation(ServiceId::fee())];
		let as_wire: Vec<&str> = holders.iter().map(|holder| holder.kind_str()).collect();
		assert_eq!(as_wire.as_slice(), wire_holder::ALL.as_slice(), "the domain holder kinds and the wire vocabulary have drifted");
		for kind in wire_holder::ALL {
			assert!(wire_holder::is_known(kind));
		}
		let states = [IssuanceState::Queued, IssuanceState::Applied];
		let as_wire: Vec<&str> = states.iter().map(|state| state.as_str()).collect();
		assert_eq!(
			as_wire.as_slice(),
			wire_issuance_state::ALL.as_slice(),
			"the domain issuance states and the wire vocabulary have drifted"
		);
		for state in wire_issuance_state::ALL {
			assert_eq!(IssuanceState::parse(state).unwrap().as_str(), state);
		}
		let sources = [IssuanceSource::Mint, IssuanceSource::Retire];
		let as_wire: Vec<&str> = sources.iter().map(|source| source.as_str()).collect();
		assert_eq!(
			as_wire.as_slice(),
			wire_issuance_source::ALL.as_slice(),
			"the domain issuance sources and the wire vocabulary have drifted"
		);
		for source in wire_issuance_source::ALL {
			assert_eq!(IssuanceSource::parse(source).unwrap().as_str(), source);
		}
	}

	#[test]
	fn domain_states_match_the_wire_contract() {
		assert_eq!(AllocationState::Draft.as_str(), wire_state::DRAFT);
		assert_eq!(AllocationState::Open.as_str(), wire_state::OPEN);
		assert_eq!(AllocationState::Closed.as_str(), wire_state::CLOSED);
		for state in wire_state::ALL {
			assert_eq!(AllocationState::parse(state).unwrap().as_str(), state);
		}
	}

	#[test]
	fn domain_access_levels_match_the_wire_contract() {
		// The second axis, held to the same standard as state: the wire list is what
		// consumer repos match on, the domain enum is what the hub stores and RANKS, so
		// the two must agree member for member AND in order — the wire's `ALL` is
		// documented lowest-first, and a client that trusts that documentation would
		// otherwise rank a product wrong.
		let domain = [AllocationAccess::Hidden, AllocationAccess::View, AllocationAccess::Invest];
		let as_wire: Vec<&str> = domain.iter().map(|level| level.as_str()).collect();
		assert_eq!(as_wire.as_slice(), wire_access::ALL.as_slice(), "the domain enum and the wire vocabulary have drifted");
		for level in wire_access::ALL {
			assert_eq!(AllocationAccess::parse(level).unwrap().as_str(), level);
		}
		assert!(domain.windows(2).all(|pair| pair[0] < pair[1]), "the wire order must be the domain's rank order");
		assert_eq!(AllocationAccess::DEFAULT.as_str(), wire_access::DEFAULT);
		// The grantable subset is exactly what `Allocation::grant_access` admits.
		for level in wire_access::ALL {
			assert_eq!(AllocationAccess::parse(level).unwrap().permits_viewing(), wire_access::is_grantable(level), "{level}");
		}
		assert_eq!(parse_access(wire_access::INVEST).unwrap(), AllocationAccess::Invest);
		assert_eq!(parse_access("").unwrap_err().code(), tonic::Code::InvalidArgument, "no level is a safe guess");
		assert_eq!(parse_access("public").unwrap_err().code(), tonic::Code::InvalidArgument);
	}

	#[test]
	fn domain_backing_matches_the_wire_contract() {
		// The fourth vocabulary, and the one that gates the way OUT: a client draws the
		// redeem control off `wire_backing`, the hub refuses `Redeem` off the domain enum,
		// so the two must agree member for member.
		let domain = [AllocationBacking::Cash, AllocationBacking::InKind];
		let as_wire: Vec<&str> = domain.iter().map(|backing| backing.as_str()).collect();
		assert_eq!(as_wire.as_slice(), wire_backing::ALL.as_slice(), "the domain enum and the wire vocabulary have drifted");
		for backing in wire_backing::ALL {
			assert_eq!(AllocationBacking::parse(backing).unwrap().as_str(), backing);
		}
		assert_eq!(AllocationBacking::default().as_str(), wire_backing::DEFAULT);
		// The wire's "may redeem" is exactly the domain's `ensure_cash_backed`.
		assert!(wire_backing::permits_redemptions(AllocationBacking::Cash.as_str()));
		assert!(!wire_backing::permits_redemptions(AllocationBacking::InKind.as_str()));
		assert_eq!(parse_backing(wire_backing::IN_KIND).unwrap(), AllocationBacking::InKind);
		assert_eq!(parse_backing("").unwrap_err().code(), tonic::Code::InvalidArgument, "no backing is a safe guess");
		assert_eq!(parse_backing("asset").unwrap_err().code(), tonic::Code::InvalidArgument);
	}

	#[test]
	fn domain_icons_match_the_wire_contract() {
		// Same guard as the states above, for the second vocabulary: the client picks its
		// SVG off `wire_icon`, the hub stores the domain enum, and the DB CHECK in
		// migration 0027 spells the same strings. Drift here is a product that renders as
		// the wrong picture, or a row the CHECK refuses.
		//
		// Compared BOTH ways, and in order. The previous version only walked `wire_icon`
		// and parsed each entry, so a variant added to the domain (and to the CHECK, and
		// to the client) but forgotten in `wire_icon` left this green — the direction that
		// matters most, since `wire_icon` is what consumer repos match on.
		let domain: Vec<&str> = AllocationIcon::ALL.iter().map(|icon| icon.as_str()).collect();
		assert_eq!(domain.as_slice(), wire_icon::ALL.as_slice(), "the domain enum and the wire vocabulary have drifted");
		for icon in wire_icon::ALL {
			assert_eq!(AllocationIcon::parse(icon).unwrap().as_str(), icon);
		}
		for icon in AllocationIcon::ALL {
			assert!(wire_icon::is_known(icon.as_str()), "{icon:?} is a domain variant the wire contract does not name");
		}
		assert_eq!(AllocationIcon::default().as_str(), wire_icon::DEFAULT);
		// The empty wire value is "unset", not an icon — the boundary turns it into the
		// default, and everything else has to be a value the contract names.
		assert_eq!(parse_icon("").unwrap(), AllocationIcon::default());
		assert_eq!(parse_icon(wire_icon::REAL_ESTATE).unwrap(), AllocationIcon::RealEstate);
		assert_eq!(parse_icon("rocket").unwrap_err().code(), tonic::Code::InvalidArgument);
	}

	#[test]
	fn an_update_request_without_the_icon_field_asks_for_no_icon_change() {
		// The three states the `optional` field can arrive in. Absent must reach the
		// aggregate as `None` — anything else and an older consumer, a stale bundle or a
		// pod mid-rolling-deploy resets the operator's pick on every title edit.
		assert_eq!(parse_icon_update(None).unwrap(), None);
		assert_eq!(
			parse_icon_update(Some("")).unwrap(),
			Some(AllocationIcon::default()),
			"an explicit empty string is a reset, not a no-op"
		);
		assert_eq!(parse_icon_update(Some(wire_icon::VENTURE)).unwrap(), Some(AllocationIcon::Venture));
		// Strictness is unchanged for a value the caller did send.
		assert_eq!(parse_icon_update(Some("rocket")).unwrap_err().code(), tonic::Code::InvalidArgument);
	}

	#[test]
	fn a_grant_crosses_the_wire_under_the_id_the_console_carries() {
		// The console's user picker and `/users/detail` speak concierge ids; a listed grant
		// that answered with the hub's id was one the console could not name (banking#252).
		// The fallback must stay the hub id, because that is what `resolve_target_user`
		// tries second — so a revoke echoing either listed id still finds the row.
		let (investor, operator) = (UserId::new(), UserId::new());
		let (investor_cc, operator_cc) = (ConciergeUserId::new(), ConciergeUserId::new());
		let mirrored = AllocationAccessGrant {
			service: ServiceId::parse("quy-nhon").unwrap(),
			user_id: investor,
			concierge_user_id: Some(investor_cc),
			level: AllocationAccess::Invest,
			granted_by: operator,
			granted_by_concierge_id: Some(operator_cc),
			granted_at: 1_750_000_400,
		};
		let wire = grant_to_proto(&mirrored);
		assert_eq!(wire.user_id, investor_cc.to_string(), "a mirrored investor is named by the concierge id");
		assert_eq!(wire.granted_by, operator_cc.to_string(), "and so is the operator who let them in");
		assert_eq!((wire.service.as_str(), wire.level.as_str(), wire.granted_at), ("quy-nhon", wire_access::INVEST, 1_750_000_400));

		let unmirrored = AllocationAccessGrant {
			concierge_user_id: None,
			granted_by_concierge_id: None,
			..mirrored
		};
		let wire = grant_to_proto(&unmirrored);
		assert_eq!(wire.user_id, investor.to_string(), "without a mirror the hub id is the only name there is");
		assert_eq!(wire.granted_by, operator.to_string());
	}

	#[test]
	fn the_wire_default_cap_matches_the_domain() {
		// A consumer repo renders `contracts::allocation::DEFAULT_UNIT_CAP` before an
		// operator has sized a product; it has to be the number the hub actually stored.
		assert_eq!(DEFAULT_UNIT_CAP.to_decimal_string(), evbanking_contracts::allocation::DEFAULT_UNIT_CAP);
	}
}
