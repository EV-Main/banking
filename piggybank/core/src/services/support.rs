//! Cross-cutting helpers shared by every context service: the auth gates
//! (`caller_id`/`require_permission`), the domain→status error mapper, id parsers, and
//! the small wire-shape utilities. Each context module ([`super::users`],
//! [`super::balance`], [`super::funds`], [`super::wallet`]) owns its own proto
//! mappers; only this genuinely shared surface lives here.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use std::time::{SystemTime, UNIX_EPOCH};

use concierge_iam::{Permission, PermissionSet};
use domain::{
	error::DomainError,
	money::Network,
	redemptions::RedemptionId,
	users::{ConciergeUserId, UserId},
	withdrawals::WithdrawalId,
};
use evbanking_auth::claims_of;
use tonic::{Request, Status};
use uuid::Uuid;

use crate::{
	AppState,
	application::withdrawals::{ACCOUNT_FROZEN, OUTFLOWS_PAUSED},
	ports::IssuanceTarget,
};

/// The authenticated caller's own user id (from the access-token `sub`).
///
/// Self-service RPCs act *as a user*, so only a `typ=access` token qualifies — a
/// `typ=service` token (an inter-service principal) is rejected here, matching the
/// authz matrix, independent of whether its `sub` happens to parse as a UUID.
pub(super) fn caller_id<T>(request: &Request<T>) -> Result<UserId, Status> {
	let claims = claims_of(request).ok_or_else(|| Status::unauthenticated("missing claims"))?;
	if !claims.is_access() {
		return Err(Status::permission_denied("access token required"));
	}
	parse_user_id(&claims.sub)
}

/// Gate a user-initiated money-moving RPC (`RequestWithdrawal` / `Subscribe` / `Redeem`)
/// on the caller's standing: the token has not been revoked, outflows are not paused, and
/// the account is not frozen. Returns the caller's id on success so the handler keeps its
/// existing `let user = ...?` shape.
///
/// Every user-INITIATED money mutation routes through here, so this is the single choke
/// point for all three. Inbound on-chain deposit crediting (the deposit watchers) is
/// deliberately NOT gated — funds already on-chain are still credited — and neither are
/// the cancel/read RPCs, so a frozen user can still unwind queued positions.
///
/// The revoke floor is the one `require_permission` already applies to operators: without
/// it here, a stolen access token kept moving money for its whole TTL after "sign out
/// everywhere". The freeze is the same `frozen OR disabled` fold issuance and the payout
/// re-check read ([`IssuanceTarget::disabled`]), and the pause is the same kill-switch the
/// dispatch path reads through [`OutflowPolicy`](crate::ports::OutflowPolicy). One resolve
/// per RPC; the decision itself is [`money_caller_gate`]. A control-plane read failure
/// fails CLOSED (UNAVAILABLE) — a money op never proceeds when the gate can't be read.
pub(super) async fn unfrozen_caller<T>(state: &AppState, request: &Request<T>) -> Result<UserId, Status> {
	gated_caller(state, request, MoneyGate::ALL).await
}

/// Which of the money-path gates a handler applies. Revocation always applies; the pause and
/// the freeze stop money that is about to move on the caller's say-so, and a handler whose
/// money never leaves the caller — `MoveFunds` out of a compartment back to `main` — or
/// that names the freeze in its own wire vocabulary (`user_frozen`) turns them off.
#[derive(Clone, Copy, Debug)]
pub(super) struct MoneyGate {
	pub pause: bool,
	pub freeze: bool,
}

impl MoneyGate {
	pub const ALL: Self = Self { pause: true, freeze: true };
}

/// [`unfrozen_caller`] with the gates `gate` names. One resolve per RPC; the pause is read
/// only when it applies.
pub(super) async fn gated_caller<T>(state: &AppState, request: &Request<T>, gate: MoneyGate) -> Result<UserId, Status> {
	let (is_access, sub, token_version) = {
		let claims = claims_of(request).ok_or_else(|| Status::unauthenticated("missing claims"))?;
		(claims.is_access(), claims.sub.clone(), claims.token_version)
	};
	if !is_access {
		return Err(Status::permission_denied("access token required"));
	}
	let user = parse_user_id(&sub)?;
	let target = state.users.resolve_issuance_by_banking_id(user).await.map_err(|_| Status::unavailable("internal error"))?;
	let paused = gate.pause && state.outflow.outflows_paused().await.map_err(|_| Status::unavailable("internal error"))?;
	money_caller_gate(token_version, target.as_ref(), paused, gate)?;
	Ok(user)
}

/// The money-path decision over facts already read — pure, so the ordering is testable
/// without an [`AppState`].
///
/// Revocation is answered first: a caller whose tokens were revoked is not authenticated,
/// and an unauthenticated caller learns nothing about the platform's state — not even that
/// outflows are paused. A missing row fails CLOSED like a freeze: every caller reaches this
/// with a minted money token, and minting resolved the same row, so `None` is not the
/// ordinary "not provisioned yet" but a state the gate cannot evaluate, on a path whose
/// next step moves money. Without the freeze gate a missing row passes: the use case
/// reads the caller's standing itself.
fn money_caller_gate(token_version: u64, target: Option<&IssuanceTarget>, paused: bool, gate: MoneyGate) -> Result<(), Status> {
	if target.is_some_and(|target| token_version < target.token_version) {
		return Err(Status::unauthenticated("tokens revoked"));
	}
	if gate.pause && paused {
		return Err(Status::failed_precondition(OUTFLOWS_PAUSED));
	}
	if !gate.freeze {
		return Ok(());
	}
	match target {
		Some(target) if !target.disabled => Ok(()),
		Some(_) | None => Err(Status::failed_precondition(ACCOUNT_FROZEN)),
	}
}

/// Gate an RPC on a `bank:*` permission, read from the caller's mirrored `users.permissions`
/// (concierge's resolution of their seat, over the bridge). Only a human access token
/// qualifies. Every path fails closed: no local row, or a row concierge has not yet told us
/// about (NULL), holds nothing; a non-UUID subject is `UNAUTHENTICATED`; a control-plane read
/// failure is `UNAVAILABLE`. Revoke and disable are answered before the permission, so
/// `RevokeTokens`/`DisableUser` bite on the most privileged principals too — revoke first, as
/// [`money_caller_gate`] answers an investor.
pub(super) async fn require_permission<T>(state: &AppState, request: &Request<T>, permission: impl Permission) -> Result<(), Status> {
	if holds_permission(state, request, permission).await? {
		Ok(())
	} else {
		Err(Status::permission_denied(format!("requires {}", permission.as_str())))
	}
}

/// [`require_permission`] as a question rather than a gate: the same resolution, the
/// same fail-closed answers for a disabled account, revoked tokens or an unreadable
/// control plane — only the final "no" comes back as `Ok(false)` instead of
/// `PERMISSION_DENIED`. For handlers that serve everyone and merely *widen* for a
/// permission holder (an investor's filtered catalog versus a manager's full one).
pub(super) async fn holds_permission<T>(state: &AppState, request: &Request<T>, permission: impl Permission) -> Result<bool, Status> {
	let (is_access, sub, token_version) = {
		let claims = claims_of(request).ok_or_else(|| Status::unauthenticated("missing claims"))?;
		(claims.is_access(), claims.sub.clone(), claims.token_version)
	};
	if !is_access {
		return Err(Status::permission_denied("access token required"));
	}
	let id = parse_user_id(&sub)?;
	let target = state.users.resolve_issuance_by_banking_id(id).await.map_err(|_| Status::unavailable("internal error"))?;
	let held = match target {
		Some(target) => {
			if token_version < target.token_version {
				return Err(Status::unauthenticated("tokens revoked"));
			}
			if target.disabled {
				return Err(Status::permission_denied("account is disabled"));
			}
			crate::infrastructure::bridge::permissions_of(&state.pool, id)
				.await
				.map_err(|_| Status::unavailable("internal error"))?
		}
		None => std::iter::empty::<&str>().collect::<PermissionSet>(),
	};
	Ok(held.may(permission))
}

/// Resolve the user an admin RPC names. The operator console carries CONCIERGE ids
/// (the identity plane's `ListUsers`) while money-plane callers (the redemption queue)
/// carry banking ids — so concierge-first via the bridge mirror, then the banking id;
/// an id matching neither is `NOT_FOUND`. `disabled`/`token_version` on the target are
/// deliberately ignored: an operator must be able to act on a frozen or disabled user.
///
/// A malformed id is `INVALID_ARGUMENT` — it is a field of the request, not the
/// caller's own subject, so [`parse_user_id`]'s `UNAUTHENTICATED` would be the wrong
/// thing to tell an operator who mistyped.
pub(super) async fn resolve_target_user(state: &AppState, raw: &str) -> Result<UserId, Status> {
	let raw = Uuid::parse_str(raw).map_err(|_| Status::invalid_argument("invalid user id"))?;
	if let Some(target) = state.users.resolve_issuance_by_concierge_id(ConciergeUserId::from_raw(raw)).await.map_err(map_err)? {
		return Ok(target.user_id);
	}
	state
		.users
		.resolve_issuance_by_banking_id(UserId::from_raw(raw))
		.await
		.map_err(map_err)?
		.map(|target| target.user_id)
		.ok_or_else(|| Status::not_found("user"))
}

pub(super) fn parse_user_id(raw: &str) -> Result<UserId, Status> {
	Uuid::parse_str(raw).map(UserId::from_raw).map_err(|_| Status::unauthenticated("subject is not a user id"))
}

pub(super) fn parse_redemption_id(raw: &str) -> Result<RedemptionId, Status> {
	Uuid::parse_str(raw).map(RedemptionId::from_raw).map_err(|_| Status::invalid_argument("invalid redemption id"))
}

pub(super) fn parse_withdrawal_id(raw: &str) -> Result<WithdrawalId, Status> {
	Uuid::parse_str(raw).map(WithdrawalId::from_raw).map_err(|_| Status::invalid_argument("invalid withdrawal id"))
}

/// Whether a rail's addresses are testnet-tagged. Only TON has a distinct testnet address
/// form (the tag is baked into the friendly encoding), so a client can render the raw
/// `workchain:hex` the hub stores in the right user-facing form; the other rails' addresses
/// are network-agnostic on the wire.
pub(super) fn rail_is_testnet(state: &AppState, network: Network) -> bool {
	matches!(network, Network::Ton) && state.ton_is_testnet
}

/// Treat an empty proto string field as an absent optional.
pub(super) fn optional(raw: &str) -> Option<&str> {
	if raw.is_empty() { None } else { Some(raw) }
}

/// Bound a caller-supplied audit string to `max_bytes`, cut on a character boundary.
///
/// The BFF already clamps what it forwards, but this surface is reachable by any gRPC
/// client and the value lands in an audit column: a megabyte of "user agent" must not be
/// what a single unauthenticated request can make the database store.
pub(super) fn clamp(value: String, max_bytes: usize) -> String {
	if value.len() <= max_bytes {
		return value;
	}
	let mut cut = max_bytes;
	while !value.is_char_boundary(cut) {
		cut -= 1;
	}
	value[..cut].to_owned()
}

/// The widest client IP an audit row keeps — an IPv6 with a zone id fits comfortably.
pub(super) const MAX_AUDIT_IP_BYTES: usize = 64;
/// The widest user agent an audit row keeps.
pub(super) const MAX_AUDIT_USER_AGENT_BYTES: usize = 512;

pub(super) fn unix_now() -> i64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default()
}

/// Map a domain error to a gRPC status without leaking control-plane internals.
pub(super) fn map_err(err: DomainError) -> Status {
	match err {
		DomainError::NotFound { .. } => Status::not_found(err.to_string()),
		DomainError::Validation(_) => Status::invalid_argument(err.to_string()),
		DomainError::Forbidden(_) => Status::permission_denied(err.to_string()),
		DomainError::Precondition(_) => Status::failed_precondition(err.to_string()),
		DomainError::Conflict(_) => Status::already_exists(err.to_string()),
		DomainError::Repository(_) => Status::unavailable("internal error"),
	}
}

#[cfg(test)]
mod tests {
	use tonic::Code;

	use super::*;

	fn target(disabled: bool, token_version: u64) -> IssuanceTarget {
		IssuanceTarget {
			user_id: UserId::new(),
			email: "u@example.com".to_owned(),
			disabled,
			token_version,
		}
	}

	fn code(token_version: u64, target: Option<&IssuanceTarget>, paused: bool) -> Option<Code> {
		money_caller_gate(token_version, target, paused, MoneyGate::ALL).err().map(|status| status.code())
	}

	#[test]
	fn a_gate_without_pause_and_freeze_still_answers_revocation() {
		let none = MoneyGate { pause: false, freeze: false };
		let (live, frozen) = (target(false, 5), target(true, 5));
		let gate = |token_version, target, paused| money_caller_gate(token_version, target, paused, none).err().map(|status| status.code());
		assert_eq!(gate(4, Some(&live), false), Some(Code::Unauthenticated));
		assert_eq!(gate(5, Some(&frozen), true), None, "a frozen caller in a paused system passes");
		assert_eq!(gate(5, None, false), None, "the use case reads the standing itself");
		let pause_only = MoneyGate { pause: true, freeze: false };
		assert_eq!(
			money_caller_gate(5, Some(&target(true, 5)), true, pause_only).err().map(|s| s.code()),
			Some(Code::FailedPrecondition)
		);
	}

	#[test]
	fn a_revoked_token_is_unauthenticated_before_anything_else() {
		let floor = target(false, 5);
		assert_eq!(code(4, Some(&floor), false), Some(Code::Unauthenticated));
		// Revocation outranks the pause: an unauthenticated caller learns nothing about
		// the platform's state.
		assert_eq!(code(4, Some(&floor), true), Some(Code::Unauthenticated));
		// ...and outranks the freeze.
		assert_eq!(code(4, Some(&target(true, 5)), false), Some(Code::Unauthenticated));
	}

	#[test]
	fn the_pause_refuses_a_live_token() {
		assert_eq!(code(5, Some(&target(false, 5)), true), Some(Code::FailedPrecondition));
	}

	#[test]
	fn a_frozen_or_disabled_account_is_a_failed_precondition() {
		assert_eq!(code(5, Some(&target(true, 5)), false), Some(Code::FailedPrecondition));
	}

	#[test]
	fn a_missing_row_fails_closed() {
		assert_eq!(code(5, None, false), Some(Code::FailedPrecondition));
	}

	#[test]
	fn a_live_token_on_a_clean_account_passes() {
		assert_eq!(code(5, Some(&target(false, 5)), false), None, "at the floor");
		assert_eq!(code(6, Some(&target(false, 5)), false), None, "above the floor");
	}

	/// A signer `RequiresApproval` reaches the operator as `FailedPrecondition` with the
	/// custodian's activity id in the message — and ONLY in the message: the status is
	/// rebuilt from the domain error, so the signer's `turnkey-activity-id` trailer never
	/// crosses the hub's boundary.
	#[test]
	fn a_precondition_keeps_its_message_and_carries_no_signer_trailer() {
		let status = map_err(DomainError::Precondition(
			"signer rotation requires custodian approval (activity 0f6a2b3c-4d5e-4f70-8a9b-0c1d2e3f4a5b)".into(),
		));
		assert_eq!(status.code(), Code::FailedPrecondition);
		assert!(status.message().contains("0f6a2b3c-4d5e-4f70-8a9b-0c1d2e3f4a5b"), "{}", status.message());
		assert!(status.metadata().get("turnkey-activity-id").is_none());
	}
}
