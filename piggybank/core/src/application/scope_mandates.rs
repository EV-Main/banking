//! A user's mandates — the consent a scope's service draws on their compartment under
//! (`banking.v1.CompartmentService.GrantMandate/RevokeMandate/ListMandates`). Every use case
//! acts on the caller's own mandates; what a draw may take is decided under the hold store's
//! locks, not here.
//!
//! A grant names a client that banking's own grants let touch users' compartments in the
//! scope — the user cannot hand their money to a client banking never allowed there. A revoke
//! voids the client's holds on the compartment, so the relay is nudged to release them at once.
//!
//! A service asks for a mandate first (`scope.v1.MandateRequestService`): banking stores the
//! terms and the user grants by the request's id, so the limits on the consent screen are the
//! server's, never a page's.

use domain::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, Grants, IDEMPOTENCY_CONFLICT, MandateOperation},
	scope_mandates::{MandateCaps, MandateRequest, MandateRequestTerms, MandateTerms},
	scopes::ScopeId,
	users::UserId,
};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{
	application::scope_ledger::check_key,
	ports::{
		compartments::CompartmentStore,
		scope_mandates::{Granted, MandateRequestStore, MandateStore, RecordedMandate, Requested},
	},
};

/// The driven ports and the configuration the mandate use cases borrow.
pub struct MandatePorts<'a> {
	/// Banking's grants to service clients, the same the scope ledger serves under.
	pub grants: &'a Grants,
	pub store: &'a dyn MandateStore,
	/// The scope registry: a mandate is granted only in a scope banking knows.
	pub scopes: &'a dyn CompartmentStore,
	/// Nudged after a revoke so the relay releases the voided holds at once.
	pub relay: &'a Notify,
}

/// What a user asks of `GrantMandate`, parsed.
#[derive(Clone, Debug)]
pub struct GrantRequest {
	pub scope: ScopeId,
	pub client: ClientId,
	pub cap_per_op: Usdt,
	pub cap_24h: Usdt,
	/// `None` = `cap_24h`.
	pub cap_outstanding: Option<Usdt>,
	pub cap_book_fill_24h: Option<Usdt>,
	pub operations: Vec<MandateOperation>,
	/// Unix seconds; `None` = until revoked.
	pub expires_at: Option<i64>,
	pub idempotency_key: String,
}

/// Grant `request` as the user's mandate for the client in the scope, replacing any current
/// one. A repeat of the key with the same terms returns the original mandate, whatever happened
/// to it since; other terms are a conflict.
pub async fn grant(ports: &MandatePorts<'_>, user: UserId, request: GrantRequest) -> Result<RecordedMandate, DomainError> {
	check_key(&request.idempotency_key)?;
	let terms = MandateTerms::new(
		user,
		request.scope,
		request.client,
		MandateCaps {
			per_op: request.cap_per_op,
			per_24h: request.cap_24h,
			outstanding: request.cap_outstanding.unwrap_or(request.cap_24h),
			book_fill_24h: request.cap_book_fill_24h,
		},
		&request.operations,
		request.expires_at,
	)?;
	// INVALID_ARGUMENT, not a permission refusal: the user did nothing they lack the right to;
	// the client they named is not one banking lets near users' money in this scope.
	if !ports.grants.compartments(&terms.client, &terms.scope) {
		return Err(DomainError::Validation(format!("client {} may not draw on compartments in scope {}", terms.client, terms.scope)));
	}
	if ports.scopes.scope(&terms.scope).await?.is_none() {
		return Err(DomainError::NotFound {
			entity: "scope",
			id: terms.scope.to_string(),
		});
	}
	match ports.store.grant(&terms, &request.idempotency_key).await? {
		Granted::New(mandate) => Ok(mandate),
		Granted::Existing { terms: stored, mandate } if stored == terms.canonical() => Ok(mandate),
		Granted::Existing { .. } => Err(DomainError::Conflict(IDEMPOTENCY_CONFLICT.into())),
	}
}

/// End the user's mandate `id`; one already ended is returned as it is.
pub async fn revoke(ports: &MandatePorts<'_>, user: UserId, id: Uuid) -> Result<RecordedMandate, DomainError> {
	let revoked = ports.store.revoke(user, id).await?.ok_or_else(|| DomainError::NotFound {
		entity: "mandate",
		id: id.to_string(),
	})?;
	ports.relay.notify_one();
	Ok(revoked)
}

/// The user's live mandates, newest first.
pub async fn list(ports: &MandatePorts<'_>, user: UserId) -> Result<Vec<RecordedMandate>, DomainError> {
	ports.store.live(user).await
}

/// Where a user grants a request. First-party only, like [`super::scope_invoices::PAY_URL_BASE`]:
/// the page takes the request id and reads the terms from the server.
pub const GRANT_URL_BASE: &str = "https://evinvest.ltd/wallet/mandate?request=";

pub fn grant_url(id: Uuid) -> String {
	format!("{GRANT_URL_BASE}{id}")
}

/// How long a request can be granted. The service does not choose it: a request is not spent
/// by a grant, so its age is the only bound on how stale a consent screen can be.
pub const REQUEST_TTL_SECONDS: i64 = 24 * 60 * 60;

/// The ports the request use cases borrow.
pub struct RequestPorts<'a> {
	pub grants: &'a Grants,
	pub requests: &'a dyn MandateRequestStore,
	/// The scope registry: a request is made only in a scope banking knows.
	pub scopes: &'a dyn CompartmentStore,
}

/// What a service asks of `CreateMandateRequest`, parsed.
#[derive(Clone, Debug)]
pub struct CreateRequest {
	pub scope: ScopeId,
	pub cap_per_op: Usdt,
	pub cap_24h: Usdt,
	/// `None` = `cap_24h`.
	pub cap_outstanding: Option<Usdt>,
	pub cap_book_fill_24h: Option<Usdt>,
	pub operations: Vec<MandateOperation>,
	/// Unix seconds; `None` = until revoked.
	pub mandate_expires_at: Option<i64>,
	pub idempotency_key: String,
}

fn not_found(id: Uuid) -> DomainError {
	DomainError::NotFound {
		entity: "mandate request",
		id: id.to_string(),
	}
}

/// Record the client's request for a mandate. It must pass what a grant of the same terms
/// would pass, short of a user: a repeat of the key with the same terms returns the original
/// request, other terms are a conflict.
pub async fn create_request(ports: &RequestPorts<'_>, client: &ClientId, request: CreateRequest) -> Result<MandateRequest, DomainError> {
	check_key(&request.idempotency_key)?;
	let terms = MandateRequestTerms::new(
		request.scope,
		client.clone(),
		MandateCaps {
			per_op: request.cap_per_op,
			per_24h: request.cap_24h,
			outstanding: request.cap_outstanding.unwrap_or(request.cap_24h),
			book_fill_24h: request.cap_book_fill_24h,
		},
		&request.operations,
		request.mandate_expires_at,
	)?;
	if !ports.grants.compartments(&terms.client, &terms.scope) {
		return Err(DomainError::Validation(format!("client {} may not draw on compartments in scope {}", terms.client, terms.scope)));
	}
	if ports.scopes.scope(&terms.scope).await?.is_none() {
		return Err(DomainError::NotFound {
			entity: "scope",
			id: terms.scope.to_string(),
		});
	}
	match ports.requests.create(&terms, &request.idempotency_key, REQUEST_TTL_SECONDS).await? {
		Requested::New(created) => Ok(created),
		Requested::Existing { terms: stored, request } if stored == terms.canonical() => Ok(request),
		Requested::Existing { .. } => Err(DomainError::Conflict(IDEMPOTENCY_CONFLICT.into())),
	}
}

/// The request `id` for the user to read or grant. Unknown, expired and one whose client has
/// since lost its grant are the same NOT_FOUND: a screen for a request nobody can still honour
/// is worse than none.
pub async fn read_request(ports: &RequestPorts<'_>, id: Uuid) -> Result<MandateRequest, DomainError> {
	let request = ports.requests.get(id).await?.ok_or_else(|| not_found(id))?;
	if !ports.grants.compartments(&request.terms.client, &request.terms.scope) {
		return Err(not_found(id));
	}
	Ok(request)
}

/// The grant of exactly what `request` asked, under the user's own key.
pub fn grant_of(request: MandateRequest, idempotency_key: String) -> GrantRequest {
	let terms = request.terms;
	GrantRequest {
		scope: terms.scope,
		client: terms.client,
		cap_per_op: terms.caps.per_op,
		cap_24h: terms.caps.per_24h,
		cap_outstanding: Some(terms.caps.outstanding),
		cap_book_fill_24h: terms.caps.book_fill_24h,
		operations: terms.operations,
		expires_at: terms.mandate_expires_at,
		idempotency_key,
	}
}
