//! A user's mandates — the consent a scope's service draws on their compartment under
//! (`banking.v1.CompartmentService.GrantMandate/RevokeMandate/ListMandates`). Every use case
//! acts on the caller's own mandates; what a draw may take is decided under the hold store's
//! locks, not here.
//!
//! A grant names a client that banking's own grants let touch users' compartments in the
//! scope — the user cannot hand their money to a client banking never allowed there. A revoke
//! voids the client's holds on the compartment, so the relay is nudged to release them at once.

use domain::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, Grants, MandateOperation},
	scope_mandates::{MandateCaps, MandateTerms},
	scopes::ScopeId,
	users::UserId,
};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{
	application::scope_ledger::check_key,
	ports::{
		compartments::CompartmentStore,
		scope_mandates::{Granted, MandateStore, RecordedMandate},
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
		Granted::Existing { .. } => Err(DomainError::Conflict("idempotency_key was used for a different mandate".into())),
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
