//! Admin-console routes — the operator surface behind `/api/admin/*`.
//!
//! Every handler is gated first by [`require_permission`] on a named permission — the
//! `/api/admin/fees/*` routes on `bank:allocation:manage`, the one the money plane checks
//! for fees — and then forwards the correct plane token — the concierge identity token for
//! identity/platform RPCs, the banking money token for money/treasury RPCs — which the
//! owning plane re-checks against the specific permission (defense in depth; a missing
//! one surfaces as 403). Mutations verify CSRF right after the gate,
//! exactly like the money routes.

use axum::{
	Json,
	body::Bytes,
	extract::{Query, State},
	http::HeaderMap,
};
use axum_extra::extract::cookie::CookieJar;
use concierge_domain::authz::{Platform, Users, bank};
use evbanking_contracts::{
	allocation::{access as wire_access, backing as wire_backing, icon as wire_icon},
	banking::v1 as bk,
};
use evconcierge_contracts::concierge::v1 as cc;
use serde::Deserialize;
use serde_json::{Value, json};
use tonic::Status;

use crate::{
	dto,
	error::ApiError,
	routes::{editable, parse_body, require_identity, require_money_token, require_permission, require_token, required, required_u32, verify_csrf},
	state::AppState,
};

#[derive(Deserialize)]
pub struct UserIdQuery {
	user_id: Option<String>,
}

#[derive(Deserialize)]
struct SetRailFrozen {
	network: String,
	frozen: bool,
}

#[derive(Deserialize)]
pub struct FeeServiceQuery {
	service: Option<String>,
}

#[derive(Deserialize)]
pub struct ListUsersQuery {
	query: Option<String>,
	role: Option<String>,
	status: Option<String>,
	limit: Option<u32>,
	offset: Option<u32>,
}

fn bool_field(v: &Value, key: &str) -> bool {
	v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// The top of the KYC tier ladder, as written down in
/// `contracts/proto/banking/v1/users.proto`: 0 registered · 1 verified · 2 enhanced ·
/// 3 elevated. Checked here rather than left to the identity plane because a tier the
/// operator mistyped is their mistake to see, and the refusal is more legible one hop
/// from the console than five.
const MAX_KYC_LEVEL: u32 = 3;

/// A REQUIRED basis-point rate: `None` when the field is missing, or is not a whole
/// non-negative number that fits a `u32`.
///
/// Deliberately not a defaulting missing-is-zero read, which is right for an optional
/// knob and wrong for a price. A rate that arrives as `"200"`, as `2.5`, or not at all is
/// a client that does not know what it is asking for, and coercing it to zero would set
/// the fund to charge NOTHING while answering "saved" — the exact silent-zero state this
/// screen exists to end. The hub caps the value at 10000 bps; refusing a malformed one is
/// this layer's half.
fn rate_field(v: &Value, key: &str) -> Option<u32> {
	required_u32(v, key)
}

// ── users (concierge identity plane) ───────────────────────────────────────────

/// `GET /api/admin/users` — paginated/filtered user list.
pub async fn list_users(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ListUsersQuery>) -> Result<Json<dto::AdminUserList>, ApiError> {
	require_permission(&st, &jar, Users::Read).await?;
	let token = require_token(&st, &jar).await?;
	let req = cc::ListUsersRequest {
		query: q.query.unwrap_or_default(),
		role: q.role.unwrap_or_default(),
		status: q.status.unwrap_or_default(),
		limit: q.limit.unwrap_or(0),
		offset: q.offset.unwrap_or(0),
	};
	let list = st.grpc.admin_list_users(&token, req).await.map_err(|s| ApiError::read(s, "users unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/admin/users/detail?user_id=` — any user's full profile.
pub async fn get_user(State(st): State<AppState>, jar: CookieJar, Query(q): Query<UserIdQuery>) -> Result<Json<dto::UserProfile>, ApiError> {
	require_permission(&st, &jar, Users::Read).await?;
	let token = require_token(&st, &jar).await?;
	let user_id = q.user_id.unwrap_or_default();
	let profile = st.grpc.admin_get_user(&token, &user_id).await.map_err(|s| ApiError::read(s, "user unavailable"))?;
	Ok(Json(profile.into()))
}

/// `POST /api/admin/users/role` — grant a role (Owner-only at the plane).
pub async fn set_role(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, Users::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(user_id), Some(role)) = (required(&v, "user_id"), required(&v, "role")) else {
		return Err(ApiError::BadRequest("user_id and role are required".into()));
	};
	let res = st.grpc.admin_set_role(&token, &user_id, &role, &editable(&v, "reason")).await?;
	Ok(Json(json!({ "role": res.role })))
}

/// `POST /api/admin/users/hold` — the emergency brake: freeze the account NOW, lapsing on
/// its own after 24h unless the owners ratify it.
///
/// Replaces the old `/suspend`, which called `DisableUser` — a verb the identity plane now
/// always refuses, because it was both the instant freeze and a permanent verdict one
/// person made. The two need opposite treatments, so the console has to say which it
/// means: this, or `POST /api/owners/proposals/suspension`.
///
/// The reason is REQUIRED here rather than forwarded empty, and it is the only one of the
/// identity mutations for which that is true. It is what the owners asked to ratify the
/// hold are reading, and the refusal is more legible one hop from the console than five.
pub async fn hold_user(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, Users::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(user_id), Some(reason)) = (required(&v, "user_id"), required(&v, "reason")) else {
		return Err(ApiError::BadRequest("user_id and reason are required".into()));
	};
	let res = st.grpc.admin_hold_user(&token, &user_id, &reason).await?;
	// The lapse stamp is the answer, not an acknowledgement: the console has to tell the
	// operator when the brake they just pulled comes off by itself.
	Ok(Json(json!({ "hold_expires_at": res.hold_expires_at.to_string() })))
}

/// `POST /api/admin/users/reinstate` — lift a HOLD in one act.
///
/// The plane refuses when the suspension was the owners' verdict (`suspended_by =
/// "governance"`), naming `OpenUserReinstatement` — otherwise one admin could overturn a
/// consilium and the consilium would be advisory. That refusal reaches the browser as the
/// plane's own 4xx; the console reads `suspended_by` and offers the proposal instead.
pub async fn reinstate_user(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, Users::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let Some(user_id) = required(&v, "user_id") else {
		return Err(ApiError::BadRequest("user_id is required".into()));
	};
	st.grpc.admin_reinstate_user(&token, &user_id, &editable(&v, "reason")).await?;
	Ok(Json(json!({ "ok": true })))
}

/// `POST /api/admin/users/revoke` — revoke all of a user's sessions (bump token_version).
pub async fn revoke_sessions(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, Users::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let Some(user_id) = required(&v, "user_id") else {
		return Err(ApiError::BadRequest("user_id is required".into()));
	};
	// Optional, unlike the hold's: revoking sessions is the reflex you want an operator to
	// reach for without composing a sentence first.
	let res = st.grpc.admin_revoke_tokens(&token, &user_id, &editable(&v, "reason")).await?;
	Ok(Json(json!({ "token_version": res.token_version.to_string() })))
}

/// The error string `set_kyc` answers when the caller named themselves.
///
/// A CODE, not a sentence, and that is the difference from the role gates in
/// `routes/mod.rs`, which answer prose. Those name a role and are read by whoever tripped
/// them; this one is read by an operator mid-task who needs to know who CAN do it instead,
/// and a sentence fixed in English here would be English in all five locales. The cabinet
/// maps it in the `FRIENDLY` table of `cabinet/frontend/shared/lib/api-client.ts` — a
/// client that does not know the code still gets a 403 and the code itself, which is worse
/// prose than this file could write but is never the wrong language.
const KYC_SELF_DENIED: &str = "kyc_self";

/// `POST /api/admin/users/kyc` — set a user's KYC level.
///
/// The tier is REQUIRED and range-checked here, before the request costs anything
/// upstream. Reading it through a missing-is-zero default made every unparseable body — a
/// typo, an absent field, a form post sending `"2"` — settle on tier 0 and answer
/// `200 {"kyc_level":0}`: banking gates deposit-address issuance and withdrawals on
/// `>= 1`, so the operator's mistake silently locked the user out of their own money
/// while the console reported success.
///
/// **The caller may not name themselves.** A `KycManage` holder who can raise their own
/// tier can walk themselves past every `>= 1` gate banking has, so the identity plane
/// refuses it — and this refuses it one hop earlier, the same way the fee gate refuses an
/// operator rather than letting the console discover it from the plane's 403. The hub
/// never sees the request.
///
/// The comparison is against the VERIFIED JWT subject, never a field of the body: the
/// signature check in [`require_identity`] is the whole reason the left side of it means
/// anything. Both sides are concierge ids — `user_id` is the handle the console carries
/// from the plane's `ListUsers`, which is what `sub` is minted from.
///
/// Decided after the CSRF check rather than before it, which is the one place this departs
/// from the fee gate's written rule. That gate reads a role and needs no body; this one is
/// a fact ABOUT the body, and acting on a body whose double-submit has not been checked is
/// acting on a form a third-party page may have composed. The part of the rule that
/// matters is kept: nothing upstream is called, and the answer names who can act.
pub async fn set_kyc(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, Users::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let (token, claims) = require_identity(&st, &jar).await?;
	let v = parse_body(&body);
	let Some(user_id) = required(&v, "user_id") else {
		return Err(ApiError::BadRequest("user_id is required".into()));
	};
	if user_id == claims.sub {
		return Err(ApiError::Grpc(Status::permission_denied(KYC_SELF_DENIED)));
	}
	let Some(kyc_level) = required_u32(&v, "kyc_level") else {
		return Err(ApiError::BadRequest(format!("kyc_level is required and must be a whole number in 0..={MAX_KYC_LEVEL}")));
	};
	if kyc_level > MAX_KYC_LEVEL {
		return Err(ApiError::BadRequest(format!("kyc_level {kyc_level} is outside the tier ladder 0..={MAX_KYC_LEVEL}")));
	}
	let res = st.grpc.admin_set_kyc_level(&token, &user_id, kyc_level, &editable(&v, "reason")).await?;
	Ok(Json(json!({ "kyc_level": res.kyc_level })))
}

/// `GET /api/admin/users/balance?user_id=` — any user's live balance (money plane).
pub async fn user_balance(State(st): State<AppState>, jar: CookieJar, Query(q): Query<UserIdQuery>) -> Result<Json<dto::UserBalance>, ApiError> {
	require_permission(&st, &jar, bank::UserBalance::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	let user_id = q.user_id.unwrap_or_default();
	let balance = st.grpc.admin_user_balance(&token, &user_id).await.map_err(|s| ApiError::read(s, "balance unavailable"))?;
	Ok(Json(balance.into()))
}

// ── treasury + valuation (banking money plane) ─────────────────────────────────

/// `GET /api/admin/treasury` — the two-layer chart of accounts.
pub async fn treasury(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::Treasury>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	let treasury = st.grpc.treasury(&token).await.map_err(|s| ApiError::read(s, "treasury unavailable"))?;
	Ok(Json(treasury.into()))
}

/// `POST /api/admin/treasury/record-deposit` — write a ledger fact for USDT that arrived
/// on a user's deposit address out of band.
///
/// Chain-proven and idempotent by `tx_ref`: the amount and the credited party are read off
/// the chain, never taken from the form — the chain names the deposit address's owner, so
/// this always credits a person. A transfer to a rail's treasury address is nobody's until
/// the owners say whose: it is refused here and proposed through
/// `/api/admin/treasury/seed-capital` instead. Pass the real on-chain reference
/// (`txhash:logIndex` on an EVM rail) so a re-submission, and any watcher that later scans
/// the same transfer, collapse onto the same key.
pub async fn record_treasury_deposit(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(tx_ref), Some(network)) = (required(&v, "tx_ref"), required(&v, "network")) else {
		return Err(ApiError::BadRequest("tx_ref and network are required".into()));
	};
	// Amount is an optional ASSERTION, never an input — the money plane reads the real one off
	// the chain. Forwarded so a typo fails loudly there instead of crediting another transfer.
	let req = bk::RecordDepositRequest {
		tx_ref,
		network,
		expected_amount: required(&v, "expected_amount").unwrap_or_default(),
	};
	let res = st.grpc.record_deposit(&token, req).await?;
	// `false` is a successful no-op, not a failure: the tx_ref was already recorded. The
	// caller needs the difference to tell "credited" from "already credited" — and the
	// amount/party come back from the chain, so the operator sees what was actually booked.
	Ok(Json(json!({
		"recorded": res.recorded,
		"amount": res.amount,
		"party_kind": res.party_kind,
		"party_id": res.party_id,
	})))
}

/// `POST /api/admin/treasury/seed-capital` — propose a seed of the platform's capital
/// (#245): a transfer that reached a rail's treasury address, proven against the chain,
/// attributed to a person as their deposit and subscription into `fund`. Body: `tx_ref`,
/// `network`, `expected_amount` (required — the figure the owners approve), and
/// `depositor_user_id` (the id the console carries, resolved hub-side; empty = the
/// caller). Opens a consilium and books nothing: the answer is `recorded: false`, the
/// amount the terms carry, and the `consilium_id` the owners' room shows. The plane
/// re-checks `CapitalManage` and refuses a proposer who holds no owner seat.
pub async fn seed_capital(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::SeedCapitalProposal>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(tx_ref), Some(network), Some(expected_amount)) = (required(&v, "tx_ref"), required(&v, "network"), required(&v, "expected_amount")) else {
		return Err(ApiError::BadRequest("tx_ref, network and expected_amount are required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let req = bk::SeedCapitalRequest {
		network,
		tx_ref,
		expected_amount,
		depositor_user_id: required(&v, "depositor_user_id").unwrap_or_default(),
	};
	Ok(Json(st.grpc.seed_capital(&token, req).await?.into()))
}

// ── fees ─────────────────────────────────────────────────────────────────────
//
// The fee plane shipped with no operator surface at all: the sweeper ran hourly and
// nothing could give a fund a policy for it to act on, so no fund ever charged anything.
// These are the five calls that make it operable. Paying the collected revenue OUT is
// not among them: what a settlement converts lands in the `fee` allocation's claim, and
// that leaves only by a holder's redemption or a payment the owners approve.

/// `GET /api/admin/fees/policies` — every fund's fee terms, for the fees table.
pub async fn list_fee_policies(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::FeePolicyList>, ApiError> {
	require_permission(&st, &jar, bank::Allocation::Manage).await?;
	let token = require_money_token(&st, &jar).await?;
	let mut list: dto::FeePolicyList = st.grpc.fee_policies(&token).await.map_err(|s| ApiError::read(s, "fee policies unavailable"))?.into();
	name_notice_waivers(&st, &jar, list.policies.iter_mut().filter_map(|policy| policy.pending.as_mut())).await?;
	Ok(Json(list))
}

/// `POST /api/admin/fees/policy` — propose a change of a fund's terms.
///
/// Every rate is required rather than patch-style optional. A fee schedule is read as a
/// whole — "2 and 20 over a 5% hurdle, annual" is one statement — and letting an operator
/// change the management rate while leaving an unseen crystallization period in place is
/// how a fund ends up charging terms nobody chose. `effective_from` (unix seconds, omitted
/// or 0 = as soon as the notice allows) and `reason` (required by the hub when the change
/// needs the owners) travel as given; the hub decides the requirement and the moment.
pub async fn schedule_fee_policy(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::FeePolicyChange>, ApiError> {
	require_permission(&st, &jar, bank::Allocation::Manage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	// Validated before the money token is minted: a request we are going to refuse on its
	// own contents should not cost an `IssueUserToken` round trip at the banking plane.
	let v = parse_body(&body);
	let (Some(service), Some(basis), Some(crystallization)) = (required(&v, "service"), required(&v, "basis"), required(&v, "crystallization")) else {
		return Err(ApiError::BadRequest("service, basis and crystallization are required".into()));
	};
	let (Some(management_bps), Some(performance_bps), Some(hurdle_bps)) = (rate_field(&v, "management_bps"), rate_field(&v, "performance_bps"), rate_field(&v, "hurdle_bps")) else {
		return Err(ApiError::BadRequest(
			"management_bps, performance_bps and hurdle_bps are required, each a whole number of basis points".into(),
		));
	};
	// Optional, but when present it must be a whole number of unix seconds: a moment this
	// layer cannot read must not become "now" while the operator believes it is next month.
	let effective_from = match v.get("effective_from") {
		None | Some(Value::Null) => 0,
		Some(raw) => raw
			.as_i64()
			.filter(|secs| *secs >= 0)
			.ok_or_else(|| ApiError::BadRequest("effective_from must be a whole number of unix seconds".into()))?,
	};
	let token = require_money_token(&st, &jar).await?;
	let req = bk::ScheduleFeePolicyRequest {
		service,
		management_bps,
		performance_bps,
		hurdle_bps,
		basis,
		crystallization,
		effective_from,
		reason: editable(&v, "reason"),
	};
	let change = st.grpc.schedule_fee_policy(&token, req).await?;
	Ok(Json(change.into()))
}

/// `POST /api/admin/fees/policy/cancel` — withdraw a pending change of terms.
pub async fn cancel_fee_policy_change(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::FeePolicyChange>, ApiError> {
	require_permission(&st, &jar, bank::Allocation::Manage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(change_id)) = (required(&v, "service"), required(&v, "change_id")) else {
		return Err(ApiError::BadRequest("service and change_id are required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let mut change: dto::FeePolicyChange = st.grpc.cancel_fee_policy_change(&token, &service, &change_id).await?.into();
	name_notice_waivers(&st, &jar, std::iter::once(&mut change)).await?;
	Ok(Json(change))
}

/// `POST /api/admin/fees/policy/acknowledge-notices` — take responsibility for the holders
/// of a scheduled change who could not be told, so a tightening binds over them. The hub
/// decides who may (the requester or an owner) and whether there is anything to acknowledge.
pub async fn acknowledge_undelivered_notices(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::FeePolicyChange>, ApiError> {
	require_permission(&st, &jar, bank::Allocation::Manage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(change_id)) = (required(&v, "service"), required(&v, "change_id")) else {
		return Err(ApiError::BadRequest("service and change_id are required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let mut change: dto::FeePolicyChange = st.grpc.acknowledge_undelivered_notices(&token, &service, &change_id).await?.into();
	name_notice_waivers(&st, &jar, std::iter::once(&mut change)).await?;
	Ok(Json(change))
}

/// `GET /api/admin/fees/changes?service=` — a fund's whole history of terms, newest first.
pub async fn list_fee_policy_changes(State(st): State<AppState>, jar: CookieJar, Query(q): Query<FeeServiceQuery>) -> Result<Json<dto::FeePolicyChangeList>, ApiError> {
	require_permission(&st, &jar, bank::Allocation::Manage).await?;
	let Some(service) = q.service.filter(|s| !s.trim().is_empty()) else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let mut list: dto::FeePolicyChangeList = st
		.grpc
		.fee_policy_changes(&token, &service)
		.await
		.map_err(|s| ApiError::read(s, "fee policy history unavailable"))?
		.into();
	name_notice_waivers(&st, &jar, list.changes.iter_mut()).await?;
	Ok(Json(list))
}

/// `GET /api/admin/fees/shares?service=` — uncollected fee units in one fund, and their value.
pub async fn fee_shares(State(st): State<AppState>, jar: CookieJar, Query(q): Query<FeeServiceQuery>) -> Result<Json<dto::FeeShares>, ApiError> {
	require_permission(&st, &jar, bank::Allocation::Manage).await?;
	let Some(service) = q.service.filter(|s| !s.trim().is_empty()) else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let shares = st.grpc.fee_shares(&token, &service).await.map_err(|s| ApiError::read(s, "fee shares unavailable"))?;
	Ok(Json(shares.into()))
}

/// `POST /api/admin/fees/settle` — convert a fund's accumulated fee units into cash.
///
/// Omitting `units` settles the whole balance, which is the ordinary end-of-period call.
/// Refused rather than queued when the fund's claim cannot cover it on top of its queued
/// redemptions — the manager is paid last.
pub async fn settle_fee_shares(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::FeeSettlement>, ApiError> {
	require_permission(&st, &jar, bank::Allocation::Manage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let settlement = st.grpc.settle_fee_shares(&token, &service, &required(&v, "units").unwrap_or_default()).await?;
	Ok(Json(settlement.into()))
}

/// `GET /api/admin/fees/assessments?service=` — every charge this fund has made.
pub async fn fund_fee_assessments(State(st): State<AppState>, jar: CookieJar, Query(q): Query<FeeServiceQuery>) -> Result<Json<dto::FeeAssessmentList>, ApiError> {
	require_permission(&st, &jar, bank::Allocation::Manage).await?;
	let Some(service) = q.service.filter(|s| !s.trim().is_empty()) else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let list = st
		.grpc
		.fund_fee_assessments(&token, &service)
		.await
		.map_err(|s| ApiError::read(s, "fee assessments unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/admin/valuation/queue` — the cross-user redemption queue awaiting settle.
pub async fn redemption_queue(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::RedemptionQueue>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	let queue = st.grpc.redemption_queue(&token).await.map_err(|s| ApiError::read(s, "redemption queue unavailable"))?;
	Ok(Json(queue.into()))
}

/// `GET /api/admin/allocations` — the full catalog, drafts and closed included.
pub async fn list_allocations(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::AllocationList>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.list_allocations(&token, true).await.map_err(|s| ApiError::read(s, "allocations unavailable"))?;
	Ok(Json(list.into()))
}

/// The `icon` field of a register/update body, validated against the wire contract.
///
/// Three distinct answers, and keeping them distinct is the point:
/// - key absent (or `null`) ⇒ `None`, "the caller is not talking about the icon". On
///   `/update` that reaches the hub as an unset `optional` field and the stored pick
///   survives. Curl, a script, or a browser tab holding a bundle from before this field
///   existed all land here — and none of them silently restyle a live product.
/// - present and empty ⇒ `Some("")`, an explicit reset to `fund`.
/// - present and outside the vocabulary ⇒ 400. An operator who mistypes an icon must be
///   told, not handed back a product quietly wearing the wrong picture. The hub refuses
///   it too; this just answers in the shape the admin console can render.
///
/// So `title` still 400s when omitted while `icon` does not, and that asymmetry is now
/// principled rather than accidental: neither field lets a caller destroy stored state
/// by staying silent. `title` has no wire value meaning "unchanged", so silence there is
/// a client bug and is refused; `icon` has one, so silence is honoured.
fn allocation_icon(v: &Value) -> Result<Option<String>, ApiError> {
	let Some(raw) = v.get("icon").filter(|value| !value.is_null()) else {
		return Ok(None);
	};
	let unknown = |icon: &str| ApiError::BadRequest(format!("unknown allocation icon '{icon}' — expected one of {}", wire_icon::ALL.join(", ")));
	match raw.as_str() {
		Some("") => Ok(Some(String::new())),
		Some(icon) if wire_icon::is_known(icon) => Ok(Some(icon.to_owned())),
		Some(icon) => Err(unknown(icon)),
		// A non-string `icon` was previously read as absent and silently defaulted; it is
		// as much of a client bug as a misspelled one.
		None => Err(unknown(&raw.to_string())),
	}
}

/// `POST /api/admin/allocations/register` — register a new investable product (`draft`).
/// This is the only way a fund comes into existence.
pub async fn register_allocation(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(service), Some(title)) = (required(&v, "service"), required(&v, "title")) else {
		return Err(ApiError::BadRequest("service and title are required".into()));
	};
	let req = bk::RegisterAllocationRequest {
		service,
		title,
		summary: required(&v, "summary").unwrap_or_default(),
		// A registration creates the row, so there is no earlier pick that an absent
		// field could destroy: "" is the wire's "chose nothing" and the hub lands it on
		// the default. Only `/update` needs presence.
		icon: allocation_icon(&v)?.unwrap_or_default(),
	};
	Ok(Json(st.grpc.register_allocation(&token, req).await?.into()))
}

/// `POST /api/admin/allocations/update` — edit an allocation's presentation fields.
pub async fn update_allocation(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(service), Some(title)) = (required(&v, "service"), required(&v, "title")) else {
		return Err(ApiError::BadRequest("service and title are required".into()));
	};
	let req = bk::UpdateAllocationRequest {
		service,
		title,
		summary: required(&v, "summary").unwrap_or_default(),
		// `None` travels as an unset `optional` field, which the hub reads as "leave the
		// stored icon alone" — the whole reason this route can be called by a client that
		// predates the field without quietly wiping an operator's choice.
		icon: allocation_icon(&v)?,
	};
	Ok(Json(st.grpc.update_allocation(&token, req).await?.into()))
}

/// `POST /api/admin/allocations/state` — open or close an allocation. Closing stops new
/// subscriptions only; redemptions keep working so no investor is trapped.
pub async fn set_allocation_state(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(service), Some(state)) = (required(&v, "service"), required(&v, "state")) else {
		return Err(ApiError::BadRequest("service and state are required".into()));
	};
	let req = bk::SetAllocationStateRequest { service, state };
	Ok(Json(st.grpc.set_allocation_state(&token, req).await?.into()))
}

/// `POST /api/admin/allocations/cap` — resize an allocation's authorised unit supply.
/// Separate from `/update` because this one gates money: the hub refuses a subscription
/// that would carry the issued supply past it.
pub async fn set_allocation_unit_cap(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(service), Some(unit_cap)) = (required(&v, "service"), required(&v, "unit_cap")) else {
		return Err(ApiError::BadRequest("service and unit_cap are required".into()));
	};
	let req = bk::SetAllocationUnitCapRequest { service, unit_cap };
	Ok(Json(st.grpc.set_allocation_unit_cap(&token, req).await?.into()))
}

/// An access level a body named, checked against the wire vocabulary before the hub
/// sees it. `grantable` narrows to what a per-investor grant may carry (`view` |
/// `invest`): `hidden` is a real level for the product's default and not for a grant,
/// which only ever raises. The hub refuses both cases too; this answers in the shape
/// the console can render, and before a money-plane token is minted for a request
/// that was never going to be served.
fn allocation_access_level(v: &Value, key: &str, grantable: bool) -> Result<String, ApiError> {
	let expected: &[&str] = if grantable { &wire_access::GRANTABLE } else { &wire_access::ALL };
	let Some(level) = required(v, key) else {
		return Err(ApiError::BadRequest(format!("{key} is required — one of {}", expected.join(", "))));
	};
	let known = if grantable { wire_access::is_grantable(&level) } else { wire_access::is_known(&level) };
	if !known {
		return Err(ApiError::BadRequest(format!("unknown access level '{level}' — expected one of {}", expected.join(", "))));
	}
	Ok(level)
}

/// `POST /api/admin/allocations/access` — set a product's default access level
/// (`hidden` | `view` | `invest`). Orthogonal to `/state`: that says whether the product
/// deals, this says with whom by default. Per-investor grants are untouched.
pub async fn set_allocation_access(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let access = allocation_access_level(&v, "access", false)?;
	let token = require_money_token(&st, &jar).await?;
	let req = bk::SetAllocationAccessRequest { service, access };
	Ok(Json(st.grpc.set_allocation_access(&token, req).await?.into()))
}

/// `POST /api/admin/allocations/backing` — declare what stands behind a product's units
/// (`cash` | `in_kind`). Orthogonal to `/state` and `/access`: this decides only whether
/// `Redeem` may pay cash out for the units. The hub flips a product to `in_kind` on its
/// own at the first mint; this is how an operator says the fund now holds cash for the
/// units — or corrects a product back. Idempotent. The vocabulary is checked here, like
/// an access level, so a typo is refused before a money-plane token is minted for it.
pub async fn set_allocation_backing(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let Some(backing) = required(&v, "backing") else {
		return Err(ApiError::BadRequest(format!("backing is required — one of {}", wire_backing::ALL.join(", "))));
	};
	if !wire_backing::is_known(&backing) {
		return Err(ApiError::BadRequest(format!("unknown backing '{backing}' — expected one of {}", wire_backing::ALL.join(", "))));
	}
	let token = require_money_token(&st, &jar).await?;
	Ok(Json(st.grpc.set_allocation_backing(&token, &service, &backing).await?.into()))
}

/// `GET /api/admin/allocations/grants?service=` — every investor raised above the
/// product's default, with who granted it and when, each carrying the investor's email.
///
/// The money plane names the investor by the id the console carries but stores no
/// email, so it is looked up here on the identity plane — one `GetUser` per grant,
/// fanned out under a small bound, with the concierge token (`GetUser` is an identity
/// RPC; the money token stays on the hub call). The lookup is best-effort: an id the
/// directory cannot name — a banking-only mirror, a lapsed account, an outage — lists
/// with `email: null` rather than hiding a grant that stands (banking#252).
pub async fn list_allocation_access_grants(State(st): State<AppState>, jar: CookieJar, Query(q): Query<FeeServiceQuery>) -> Result<Json<dto::AllocationAccessGrantList>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let Some(service) = q.service.filter(|s| !s.trim().is_empty()) else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let money_token = require_money_token(&st, &jar).await?;
	let identity_token = require_token(&st, &jar).await?;
	let mut list: dto::AllocationAccessGrantList = st
		.grpc
		.list_allocation_access_grants(&money_token, &service)
		.await
		.map_err(|s| ApiError::read(s, "allocation access grants unavailable"))?
		.into();

	let ids: Vec<String> = list.grants.iter().map(|grant| grant.user_id.clone()).collect();
	for (grant, email) in list.grants.iter_mut().zip(lookup_emails(&st, &identity_token, ids).await) {
		grant.email = email;
	}
	Ok(Json(list))
}

/// How the identity plane names each of `ids`, in the same order — one `GetUser` per id,
/// fanned out under a small bound, with the concierge token (`GetUser` is an identity RPC;
/// the money token stays on the hub call). Best-effort by design: an id the directory
/// cannot name — a banking-only mirror, a lapsed account, an outage — answers `None`, so
/// the caller lists what stands rather than hiding it or failing the page.
async fn lookup_emails(st: &AppState, identity_token: &str, ids: Vec<String>) -> Vec<Option<String>> {
	use std::sync::Arc;

	use tokio::{sync::Semaphore, task::JoinSet};

	/// A console page names a handful of people, but the bound is what keeps one page
	/// from turning into a burst against the identity plane if it ever does not.
	const LOOKUPS_IN_FLIGHT: usize = 8;

	let identity_token: Arc<str> = identity_token.into();
	let mut emails: Vec<Option<String>> = vec![None; ids.len()];

	// The permit is taken BEFORE the spawn and moved into the task, so the loop itself
	// blocks at the bound; the `JoinSet` aborts whatever is still in flight if the
	// caller goes away mid-page.
	let lookups = Arc::new(Semaphore::new(LOOKUPS_IN_FLIGHT));
	let mut set = JoinSet::new();
	for (index, user_id) in ids.iter().cloned().enumerate() {
		let permit = lookups.clone().acquire_owned().await.expect("the lookup semaphore is never closed");
		let (grpc, token) = (st.grpc.clone(), identity_token.clone());
		set.spawn(async move {
			let looked_up = grpc.admin_get_user(&token, &user_id).await;
			drop(permit);
			(index, looked_up)
		});
	}
	while let Some(joined) = set.join_next().await {
		match joined {
			Ok((index, Ok(profile))) => emails[index] = Some(profile.email).filter(|email| !email.is_empty()),
			Ok((index, Err(status))) => {
				tracing::debug!(code = ?status.code(), user_id = %ids[index], "user not resolvable in the directory; listing without email");
			}
			Err(join) => tracing::warn!(error = %join, "an email lookup task ended abnormally; listing without email"),
		}
	}
	emails
}

/// Names on the identity plane whoever acknowledged each change's undelivered notices.
///
/// `notices_waived_by` is the banking user id the hub recorded, and the console showed it
/// raw — "Notices waived by 78670fa7-…" (banking#328). Same best-effort lookup as the
/// grants list, one `GetUser` per distinct id: a waiver the directory cannot name keeps
/// `notices_waived_by_email: null`, and a directory outage never fails the page. Changes
/// nobody has acknowledged are skipped, so a history with no waivers costs no lookup.
async fn name_notice_waivers<'a>(st: &AppState, jar: &CookieJar, changes: impl IntoIterator<Item = &'a mut dto::FeePolicyChange>) -> Result<(), ApiError> {
	use std::collections::HashMap;

	let waived: Vec<&mut dto::FeePolicyChange> = changes.into_iter().filter(|change| change.notices_waived_by.is_some()).collect();
	if waived.is_empty() {
		return Ok(());
	}
	let identity_token = require_token(st, jar).await?;
	let mut ids: Vec<String> = waived.iter().filter_map(|change| change.notices_waived_by.clone()).collect();
	ids.sort_unstable();
	ids.dedup();
	let emails: HashMap<String, Option<String>> = ids.clone().into_iter().zip(lookup_emails(st, &identity_token, ids).await).collect();
	for change in waived {
		change.notices_waived_by_email = change.notices_waived_by.as_ref().and_then(|id| emails.get(id).cloned().flatten());
	}
	Ok(())
}

/// `POST /api/admin/allocations/grants/grant` — raise one investor to `view` | `invest`
/// on a product. A repeat for the same investor overwrites the level. `user_id` is the
/// id the console carries for the user (concierge-first, banking as a fallback — the
/// hub resolves it the way `/users/balance` does).
pub async fn grant_allocation_access(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::AllocationAccessGrant>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(user_id)) = (required(&v, "service"), required(&v, "user_id")) else {
		return Err(ApiError::BadRequest("service and user_id are required".into()));
	};
	let level = allocation_access_level(&v, "level", true)?;
	let token = require_money_token(&st, &jar).await?;
	let req = bk::GrantAllocationAccessRequest { service, user_id, level };
	Ok(Json(st.grpc.grant_allocation_access(&token, req).await?.into()))
}

/// `POST /api/admin/allocations/grants/revoke` — take an investor's grant back; they
/// fall to the product's default. Idempotent: revoking a grant that does not stand is
/// still `ok`. Units they already hold are untouched and stay redeemable.
pub async fn revoke_allocation_access(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(user_id)) = (required(&v, "service"), required(&v, "user_id")) else {
		return Err(ApiError::BadRequest("service and user_id are required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	st.grpc.revoke_allocation_access(&token, bk::RevokeAllocationAccessRequest { service, user_id }).await?;
	Ok(Json(json!({ "ok": true })))
}

/// `POST /api/admin/allocations/issue` — mint units in kind to a person, with no cash
/// leg. Body: `service`, `units`, `idempotency_key`, `user_id` (the id the console
/// carries — concierge-first, banking as a fallback, resolved hub-side like
/// `/users/balance`); `cost_basis` is optional and defaults hub-side to `units × NAV`.
/// Only a person: the company is not a holder (#245), and the reserved allocations are
/// seated through `/api/consilium/holder-grant`. The key is the retry contract — the
/// console generates one per form submission and re-sends the same one on a timeout, so
/// a double click lands one mint.
pub async fn issue_units(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::UnitIssuance>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(user_id), Some(units), Some(idempotency_key)) = (required(&v, "service"), required(&v, "user_id"), required(&v, "units"), required(&v, "idempotency_key"))
	else {
		// The hub refuses a request naming nobody; answering here spares it a money-plane
		// token for a form the console never filled in.
		return Err(ApiError::BadRequest("service, user_id, units and idempotency_key are required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let req = bk::IssueUnitsRequest {
		service,
		user_id,
		units,
		cost_basis: editable(&v, "cost_basis"),
		idempotency_key,
	};
	Ok(Json(st.grpc.issue_units(&token, req).await?.into()))
}

/// `GET /api/admin/allocations/holders?service=` — the product's cap table: the settled
/// supply and every holder of it (people and the `fee` allocation), largest first.
pub async fn list_unit_holders(State(st): State<AppState>, jar: CookieJar, Query(q): Query<FeeServiceQuery>) -> Result<Json<dto::UnitHolders>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let Some(service) = q.service.filter(|s| !s.trim().is_empty()) else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let holders = st.grpc.list_unit_holders(&token, &service).await.map_err(|s| ApiError::read(s, "unit holders unavailable"))?;
	Ok(Json(holders.into()))
}

/// `POST /api/admin/allocations/retire` — burn a holder's units with no cash leg, the
/// reverse of `/allocations/issue`: the units leave the holder's account and the supply
/// shrinks by the same amount. Body: `service`, `user_id` (resolved hub-side like
/// `/allocations/issue`), `units`, `idempotency_key`; `cost_basis` is optional and
/// defaults hub-side to `units × NAV`; `force` (default `false`) is the operator's
/// explicit override to burn units out of a live product — without it the hub answers
/// 412 unless the allocation is `closed`. Answers the same `UnitIssuance` shape as a
/// mint, with `source: "retire"` and positive `units`. The key is the same retry
/// contract, in the same per-product key space as `/allocations/issue`.
pub async fn retire_units(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::UnitIssuance>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(user_id), Some(units), Some(idempotency_key)) = (required(&v, "service"), required(&v, "user_id"), required(&v, "units"), required(&v, "idempotency_key"))
	else {
		// Same reason as the mint: a burn of nobody's units is a form the console never
		// filled in, and refusing it here spares the hub a money-plane token.
		return Err(ApiError::BadRequest("service, user_id, units and idempotency_key are required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let req = bk::RetireUnitsRequest {
		service,
		user_id,
		units,
		cost_basis: editable(&v, "cost_basis"),
		idempotency_key,
		force: bool_field(&v, "force"),
	};
	Ok(Json(st.grpc.retire_units(&token, req).await?.into()))
}

/// `POST /api/admin/allocations/book` — replace a product's trading terms: whether its
/// secondary book is open, the taker fee, the price tick, the lot size and how far past
/// the best quote a market order may fill. Closing the book stops new orders; resting
/// ones stay and may still be cancelled.
///
/// Read as a whole, like the fee schedule: `book_open` and `taker_fee_bps` must be
/// present and well-typed, because a missing flag coerced to `false` would close a book
/// the operator meant to leave open, and a missing fee read as zero would price the book
/// at nothing while the console answers "saved". `price_tick` and `lot_size` may be left
/// out (the hub's defaults, `0.01` and `0.0001`); `market_slippage_bps` may be left out
/// and is then zero, but if it is sent it has to be a whole number of bps.
/// `allow_unbacked_trading` may be left out and is then `false` — the operator's
/// acknowledgement that the book trades units the fund holds no cash for is given, never
/// presumed; the hub refuses to open an `in_kind` product's book without it.
pub async fn set_book_policy(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::BookPolicy>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let Some(book_open) = v.get("book_open").and_then(Value::as_bool) else {
		return Err(ApiError::BadRequest("book_open is required and must be a boolean".into()));
	};
	let Some(taker_fee_bps) = required_u32(&v, "taker_fee_bps") else {
		return Err(ApiError::BadRequest("taker_fee_bps is required and must be a whole number of basis points".into()));
	};
	let market_slippage_bps = match v.get("market_slippage_bps") {
		None => 0,
		Some(_) => required_u32(&v, "market_slippage_bps").ok_or_else(|| ApiError::BadRequest("market_slippage_bps must be a whole number of basis points".into()))?,
	};
	let token = require_money_token(&st, &jar).await?;
	let req = bk::SetBookPolicyRequest {
		service,
		book_open,
		taker_fee_bps,
		price_tick: editable(&v, "price_tick"),
		lot_size: editable(&v, "lot_size"),
		market_slippage_bps,
		allow_unbacked_trading: bool_field(&v, "allow_unbacked_trading"),
	};
	Ok(Json(st.grpc.set_book_policy(&token, req).await?.into()))
}

/// `POST /api/admin/valuation/post` — post a fund NAV, inside the move guard. There is no
/// `override` in the body any more (banking#232): a mark the guard refuses goes through
/// [`propose_valuation_override`] and the owners' vote.
pub async fn post_valuation(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::FundNav>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(service), Some(aum)) = (required(&v, "service"), required(&v, "aum")) else {
		return Err(ApiError::BadRequest("service and aum are required".into()));
	};
	let req = bk::PostFundValuationRequest { service, aum };
	Ok(Json(st.grpc.post_valuation(&token, req).await?.into()))
}

/// `POST /api/admin/valuation/override` — put a NAV mark the move guard refuses to the
/// owners. Body `{service, aum}`; the answer is the consilium the owners' room and the
/// emailed invitations then show. Same admin + CSRF gate as `/post`; the plane re-checks
/// `ValuationPost` and refuses a proposer who holds no owner seat.
pub async fn propose_valuation_override(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Consilium>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(aum)) = (required(&v, "service"), required(&v, "aum")) else {
		return Err(ApiError::BadRequest("service and aum are required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let terms = bk::ValuationOverrideTerms { service, aum };
	Ok(Json(st.grpc.open_valuation_override(&token, terms).await?.into()))
}

/// `POST /api/admin/valuation/settle` — settle a queued redemption.
pub async fn settle_redemption(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let Some(id) = required(&parse_body(&body), "redemption_id") else {
		return Err(ApiError::BadRequest("redemption_id is required".into()));
	};
	Ok(Json(st.grpc.settle_redemption(&token, &id).await?.into()))
}

/// `POST /api/admin/valuation/fail` — fail (void + refund units) a queued redemption.
pub async fn fail_redemption(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let Some(id) = required(&parse_body(&body), "redemption_id") else {
		return Err(ApiError::BadRequest("redemption_id is required".into()));
	};
	Ok(Json(st.grpc.fail_redemption(&token, &id).await?.into()))
}

// ── withdrawals (banking money plane, operator actions) ────────────────────────

/// `GET /api/admin/withdrawals/queue` — cross-user withdrawals awaiting operator
/// action (queued / processing), oldest first.
pub async fn withdrawal_queue(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::WithdrawalQueue>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	let queue = st.grpc.withdrawal_queue(&token).await.map_err(|s| ApiError::read(s, "withdrawal queue unavailable"))?;
	Ok(Json(queue.into()))
}

/// `POST /api/admin/withdrawals/dispatch` — dispatch a queued withdrawal whose rail
/// now has liquidity.
pub async fn dispatch_withdrawal(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let Some(id) = required(&parse_body(&body), "withdrawal_id") else {
		return Err(ApiError::BadRequest("withdrawal_id is required".into()));
	};
	st.grpc.dispatch_withdrawal(&token, &id).await?;
	Ok(Json(json!({ "ok": true })))
}

/// `POST /api/admin/withdrawals/settle` — settle a processing withdrawal with its
/// mined on-chain tx reference.
pub async fn settle_withdrawal(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let Some(id) = required(&v, "withdrawal_id") else {
		return Err(ApiError::BadRequest("withdrawal_id is required".into()));
	};
	let Some(tx_ref) = required(&v, "tx_ref") else {
		return Err(ApiError::BadRequest("tx_ref is required".into()));
	};
	st.grpc.settle_withdrawal(&token, &id, &tx_ref).await?;
	Ok(Json(json!({ "ok": true })))
}

/// `POST /api/admin/withdrawals/fail` — fail a withdrawal that NEVER reached the
/// chain (voids the reservation, refunding the user). The hub refuses when a
/// broadcast row exists; the reason is an audit note.
pub async fn fail_withdrawal(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let Some(id) = required(&v, "withdrawal_id") else {
		return Err(ApiError::BadRequest("withdrawal_id is required".into()));
	};
	let reason = required(&v, "reason").unwrap_or_default();
	st.grpc.fail_withdrawal(&token, &id, &reason).await?;
	Ok(Json(json!({ "ok": true })))
}

// ── revenue (banking money plane, the `fee` allocation) ───────────────────────

/// `GET /api/admin/revenue` — what the platform has earned: the `fee` allocation in the
/// treasury's shape (cash, supply, price, holders). Admin/Owner at the plane
/// (`ConsiliumManage`); an Operator who may read the treasury is refused here. Nothing
/// pays it out from this surface (#245): a holder redeems, or the owners approve a
/// payment out of `service:fee` through `/api/admin/payments`.
pub async fn fund_revenue(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::AllocationTreasury>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	let revenue = st.grpc.fund_revenue(&token).await.map_err(|s| ApiError::read(s, "fund revenue unavailable"))?;
	Ok(Json(revenue.into()))
}

// ── outbox (banking money plane) ────────────────────────────────────────────────

/// `GET /api/admin/outbox/parked` — outbox rows the relay parked (needs-intervention).
pub async fn parked_events(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::ParkedEventList>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.parked_events(&token).await.map_err(|s| ApiError::read(s, "parked events unavailable"))?;
	Ok(Json(list.into()))
}

/// `POST /api/admin/outbox/unpark` — CSRF-checked: clear a park so the relay re-drives
/// the event (the hub refuses compensated/dispatched rows).
pub async fn unpark_event(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let Some(seq) = required(&parse_body(&body), "seq").and_then(|s| s.parse::<i64>().ok()) else {
		return Err(ApiError::BadRequest("seq is required".into()));
	};
	st.grpc.unpark_event(&token, seq).await?;
	Ok(Json(json!({ "ok": true })))
}

// ── cabinet (concierge platform config + banking read-only kill-switch) ─────────

/// `GET /api/admin/cabinet` — platform config (concierge) + the money-plane read-only
/// flag (banking, best-effort). The MFE registry is served separately at
/// `/api/mfe-registry`.
pub async fn cabinet_config(State(st): State<AppState>, jar: CookieJar) -> Result<Json<Value>, ApiError> {
	require_permission(&st, &jar, Platform::Read).await?;
	let token = require_token(&st, &jar).await?;
	let config: dto::PlatformConfig = st.grpc.platform_config(&token).await.map_err(|s| ApiError::read(s, "platform config unavailable"))?.into();
	// The read-only kill-switch lives on the money plane; fetch best-effort so the screen
	// still renders if the money token isn't available.
	let read_only = match require_money_token(&st, &jar).await {
		Ok(mt) => st.grpc.operations_mode(&mt).await.map(|m| m.read_only).unwrap_or(false),
		Err(_) => false,
	};
	Ok(Json(json!({ "platform": config, "read_only": read_only })))
}

/// `POST /api/admin/cabinet/maintenance` — toggle the cabinet maintenance holding page.
pub async fn set_maintenance(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::PlatformConfig>, ApiError> {
	require_permission(&st, &jar, Platform::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let config = st.grpc.set_maintenance_mode(&token, bool_field(&parse_body(&body), "enabled")).await?;
	Ok(Json(config.into()))
}

/// `POST /api/admin/cabinet/read-only` — toggle the money-plane read-only kill-switch.
pub async fn set_read_only(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::OperationsMode>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let mode = st.grpc.set_operations_mode(&token, bool_field(&parse_body(&body), "read_only")).await?;
	Ok(Json(mode.into()))
}

/// `GET /api/admin/rails` — every rail with whether it is run and whether it is frozen.
pub async fn rails(State(st): State<AppState>, jar: CookieJar) -> Result<Json<Vec<dto::Rail>>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	let rails = st.grpc.rails(&token).await.map_err(|s| ApiError::read(s, "rails unavailable"))?;
	Ok(Json(rails.rails.into_iter().map(Into::into).collect()))
}

/// `POST /api/admin/rails/frozen` — freeze or unfreeze one rail; answers the whole list.
pub async fn set_rail_frozen(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<Vec<dto::Rail>>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let req: SetRailFrozen = serde_json::from_slice(&body).map_err(|e| ApiError::BadRequest(format!("expected {{ network, frozen }}: {e}")))?;
	let token = require_money_token(&st, &jar).await?;
	let rails = st
		.grpc
		.set_rail_frozen(
			&token,
			bk::SetRailFrozenRequest {
				network: req.network,
				frozen: req.frozen,
			},
		)
		.await?;
	Ok(Json(rails.rails.into_iter().map(Into::into).collect()))
}

/// `POST /api/admin/cabinet/announcement` — set/clear the live announcement banner.
pub async fn set_announcement(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::PlatformConfig>, ApiError> {
	require_permission(&st, &jar, Platform::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let req = cc::SetAnnouncementRequest {
		title: editable(&v, "title"),
		body: editable(&v, "body"),
		active: bool_field(&v, "active"),
	};
	let config = st.grpc.set_announcement(&token, req).await?;
	Ok(Json(config.into()))
}

/// `POST /api/admin/cabinet/flag` — upsert a feature flag.
pub async fn set_flag(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::PlatformConfig>, ApiError> {
	require_permission(&st, &jar, Platform::Read).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let Some(key) = required(&v, "key") else {
		return Err(ApiError::BadRequest("key is required".into()));
	};
	// A rollout that doesn't arrive as a whole number is a client that doesn't know what
	// it's asking for — coercing it to 0 would silently disable the flag while answering
	// "saved". Required rather than defaulted, same as the fee rates below.
	let Some(rollout) = required_u32(&v, "rollout") else {
		return Err(ApiError::BadRequest("rollout is required, and must be a whole number".into()));
	};
	if rollout > 100 {
		return Err(ApiError::BadRequest("rollout must be between 0 and 100".into()));
	}
	let req = cc::SetFeatureFlagRequest {
		key,
		description: editable(&v, "description"),
		enabled: bool_field(&v, "enabled"),
		rollout,
	};
	let config = st.grpc.set_feature_flag(&token, req).await?;
	Ok(Json(config.into()))
}

/// The five `/api/admin/fees/*` routes plus `/api/admin/users/kyc`, driven end to end
/// through the real router.
///
/// These are the calls the operator's fees screen makes, and they were the one part of
/// the fee plane with no automated coverage: the hub's own charging is exercised against
/// Postgres + TigerBeetle in `piggybank/core/tests/fee_policy.rs`, but everything between
/// the browser and that hub — the admin gate, the CSRF double-submit, the "reject before
/// any upstream call" validation, and the proto→JSON mapping the screen reads — was only
/// ever tried by hand against a stub.
///
/// So the test stands up the whole seam in process: a tonic server that plays all three
/// upstreams (the concierge JWKS + directory, the banking token-issuance seam, and the
/// piggybank fees service), a real `AppState` pointed at it, and the real
/// [`router`](crate::routes::router). Nothing below the HTTP boundary is faked out, so a
/// handler that forwards the wrong field, forgets a gate, or renames a JSON key fails
/// here rather than on the screen.
///
/// The KYC route joined them because it shares the seam and the failure mode: a tier the
/// BFF cannot parse must be refused here, and the assertion that matters is that the hub
/// never saw a request at all.
#[cfg(test)]
mod admin_route_tests {
	// `Status` is a large error type tonic mandates in handler signatures.
	#![allow(clippy::result_large_err)]

	use std::{
		net::SocketAddr,
		sync::{Arc, Mutex},
	};

	use axum::{
		Router,
		body::Body,
		http::{Request, StatusCode, header},
	};
	use evbanking_contracts::banking::v1::{
		allocations_service_server::{AllocationsService, AllocationsServiceServer},
		auth_service_server::{AuthService as BkAuthService, AuthServiceServer as BkAuthServiceServer},
		fees_service_server::{FeesService, FeesServiceServer},
	};
	use evconcierge_auth::{TokenType, Verifier, VerifierConfig};
	use evconcierge_contracts::concierge::v1::{
		auth_service_server::AuthServiceServer as CcAuthServiceServer,
		user_directory_server::{UserDirectory, UserDirectoryServer},
	};
	use serde_json::Value;
	use tokio::net::TcpListener;
	use tokio_stream::wrappers::TcpListenerStream;
	use tonic::{Code, Request as GrpcRequest, Response as GrpcResponse, Status, transport::Server};
	use tower::ServiceExt;

	use super::*;
	use crate::{
		config::AppConfig,
		cookies::CookieNames,
		routes::{
			router,
			test_support::{AUDIENCE, ConciergeJwks, ISSUER, access_token},
		},
		session::BankingTokens,
		state::{AppState, Grpc},
	};

	const CSRF: &str = "csrf-token-value";
	const SERVICE: &str = "quy-nhon";
	/// What the stub's exchange seam mints. The fees RPCs accept nothing else.
	const MONEY_TOKEN: &str = "banking-access-token";
	/// The one investor the stub directory can name, and how it names them.
	const KNOWN_INVESTOR: &str = "investor-7";
	const KNOWN_INVESTOR_EMAIL: &str = "investor7@example.test";
	/// A grant holder the directory has never heard of — a banking-only mirror.
	const UNMIRRORED_INVESTOR: &str = "investor-9";

	// ── the stub hub ────────────────────────────────────────────────────────────

	/// What the BFF actually forwarded upstream, so a test can assert on the request the
	/// hub would have seen rather than only on the response the browser gets.
	/// The change id the stub answers a schedule with.
	const CHANGE_ID: &str = "6f1c2a3e-0b4d-4c5e-8f9a-1b2c3d4e5f60";

	/// One row of the stub's fee-policy history — the shape the hub answers a schedule, a
	/// cancel and the history list with.
	#[allow(clippy::too_many_arguments)]
	fn stub_change(
		id: &str,
		state: &str,
		management_bps: u32,
		performance_bps: u32,
		hurdle_bps: u32,
		basis: &str,
		crystallization: &str,
		effective_from: i64,
		reason: &str,
	) -> bk::FeePolicyChange {
		bk::FeePolicyChange {
			id: id.into(),
			service: SERVICE.into(),
			version: 2,
			state: state.into(),
			management_bps,
			performance_bps,
			hurdle_bps,
			basis: basis.into(),
			crystallization: crystallization.into(),
			effective_from,
			requirement: "admin".into(),
			consilium_id: String::new(),
			requested_by: "user-1".into(),
			requested_at: 1_750_000_100,
			scheduled_at: 1_750_000_100,
			applied_at: 0,
			reason: reason.into(),
			notices_waived_by: String::new(),
			notices_waived_at: 0,
			notices_waived_users: Vec::new(),
			undelivered_notices: 0,
			notices_given_up: 0,
			notices_unacknowledged: 0,
		}
	}

	/// A tenant grant as the directory reports one.
	fn stub_grant(target: &str) -> cc::PermissionGrant {
		cc::PermissionGrant {
			id: 41,
			user_id: KNOWN_INVESTOR.into(),
			target: target.into(),
			granted_by: "user-1".into(),
			granted_at: 1_750_000_200,
			reason: "runs the calls".into(),
			orphaned: false,
		}
	}

	/// The stub's one holder the identity plane cannot reach.
	const UNTOLD_HOLDER: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

	#[derive(Default)]
	struct Seen {
		set_policy: Option<bk::ScheduleFeePolicyRequest>,
		cancel_change: Option<bk::CancelFeePolicyChangeRequest>,
		acknowledge: Option<bk::AcknowledgeUndeliveredNoticesRequest>,
		settle: Option<bk::SettleFeeSharesRequest>,
		set_kyc: Option<cc::SetKycLevelRequest>,
		grant_permission: Option<cc::GrantPermissionRequest>,
		revoke_permission: Option<cc::RevokePermissionRequest>,
		list_grants: Option<String>,
		set_access: Option<bk::SetAllocationAccessRequest>,
		grant: Option<bk::GrantAllocationAccessRequest>,
		revoke: Option<bk::RevokeAllocationAccessRequest>,
		set_backing: Option<bk::SetAllocationBackingRequest>,
		retire: Option<bk::RetireUnitsRequest>,
		issue: Option<bk::IssueUnitsRequest>,
		money_tokens_issued: usize,
	}

	#[derive(Clone)]
	struct Hub {
		role: String,
		/// What `GetMe` resolves the seat to — what [`require_permission`] gates on.
		permissions: Vec<String>,
		/// When set, every fees RPC fails with this code (the upstream-refusal cases).
		fail_with: Option<Code>,
		seen: Arc<Mutex<Seen>>,
	}

	impl Hub {
		fn new(role: &str) -> Self {
			Self {
				role: role.to_string(),
				permissions: concierge_domain::authz::Role::parse(role)
					.expect("stubs sit in concierge's seats")
					.permissions()
					.iter()
					.map(|p| (*p).to_owned())
					.collect(),
				fail_with: None,
				seen: Arc::new(Mutex::new(Seen::default())),
			}
		}

		fn failing(role: &str, code: Code) -> Self {
			Self {
				fail_with: Some(code),
				..Self::new(role)
			}
		}

		/// The fees plane is a MONEY plane: it must be reached with the banking
		/// (`aud=banking-core`) token minted through the exchange seam, never with the
		/// concierge identity JWT from the cookie. The two are cryptographically separate,
		/// so forwarding the wrong one is rejected in production on issuer/audience — but a
		/// stub that accepts any bearer would let a handler swapping `require_money_token`
		/// for `require_token` pass every test here and fail only in prod. So the stub
		/// checks, and the plane crossing is pinned like any other gate.
		fn guard_money_plane<T>(&self, request: &GrpcRequest<T>) -> Result<(), Status> {
			let presented = request.metadata().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
			if presented != format!("Bearer {MONEY_TOKEN}") {
				return Err(Status::unauthenticated("the fees plane requires the banking money token, not the concierge identity token"));
			}
			self.guard()
		}

		/// The mirror of [`Self::guard_money_plane`]: grants are identity-plane state, and
		/// concierge judges the CALLER, so the BFF must forward the caller's own concierge
		/// token — never the minted money token, which concierge would not accept.
		fn guard_identity_plane<T>(&self, request: &GrpcRequest<T>) -> Result<(), Status> {
			let presented = request.metadata().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
			if !presented.starts_with("Bearer ey") || presented == format!("Bearer {MONEY_TOKEN}") {
				return Err(Status::unauthenticated("the directory requires the caller's concierge identity token"));
			}
			self.guard()
		}

		fn guard(&self) -> Result<(), Status> {
			match self.fail_with {
				Some(code) => Err(Status::new(code, "upstream refused")),
				None => Ok(()),
			}
		}
	}

	#[tonic::async_trait]
	impl UserDirectory for Hub {
		/// `require_permission` reads the caller's permissions from here per request — the
		/// JWT carries none, so this is the only thing standing between an investor and the
		/// console.
		async fn get_me(&self, _: GrpcRequest<cc::GetMeRequest>) -> Result<GrpcResponse<cc::UserProfile>, Status> {
			Ok(GrpcResponse::new(cc::UserProfile {
				user_id: "user-1".into(),
				role: self.role.clone(),
				permissions: self.permissions.clone(),
				..Default::default()
			}))
		}

		async fn update_profile(&self, _: GrpcRequest<cc::UpdateProfileRequest>) -> Result<GrpcResponse<cc::UserProfile>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		async fn revoke_tokens(&self, _: GrpcRequest<cc::RevokeTokensRequest>) -> Result<GrpcResponse<cc::RevokeTokensResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		/// Kept in the contract rather than deleted so an older console gets an explanatory
		/// refusal instead of an UNIMPLEMENTED. Nothing in this BFF calls it any more.
		async fn disable_user(&self, _: GrpcRequest<cc::DisableUserRequest>) -> Result<GrpcResponse<cc::DisableUserResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		async fn hold_user(&self, _: GrpcRequest<cc::HoldUserRequest>) -> Result<GrpcResponse<cc::HoldUserResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		async fn reinstate_user(&self, _: GrpcRequest<cc::ReinstateUserRequest>) -> Result<GrpcResponse<cc::ReinstateUserResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		/// Records the tier it was asked for and echoes it back, exactly as the identity
		/// plane does. The recording is the point: a body the BFF should have refused must
		/// leave this untouched.
		async fn set_kyc_level(&self, request: GrpcRequest<cc::SetKycLevelRequest>) -> Result<GrpcResponse<cc::SetKycLevelResponse>, Status> {
			self.guard()?;
			let req = request.into_inner();
			let kyc_level = req.kyc_level;
			self.seen.lock().unwrap().set_kyc = Some(req);
			Ok(GrpcResponse::new(cc::SetKycLevelResponse { kyc_level }))
		}

		async fn list_users(&self, _: GrpcRequest<cc::ListUsersRequest>) -> Result<GrpcResponse<cc::ListUsersResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		/// The directory knows exactly one investor. The grants route asks here for each
		/// grant holder's email, and an id it cannot name must come back NOT_FOUND, the way
		/// the identity plane answers a banking-only mirror.
		async fn get_user(&self, request: GrpcRequest<cc::GetUserRequest>) -> Result<GrpcResponse<cc::UserProfile>, Status> {
			self.guard()?;
			let user_id = request.into_inner().user_id;
			if user_id != KNOWN_INVESTOR {
				return Err(Status::not_found("user"));
			}
			Ok(GrpcResponse::new(cc::UserProfile {
				user_id,
				email: KNOWN_INVESTOR_EMAIL.into(),
				role: "investor".into(),
				..Default::default()
			}))
		}

		async fn set_role(&self, _: GrpcRequest<cc::SetRoleRequest>) -> Result<GrpcResponse<cc::SetRoleResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		/// The grant RPCs decide authority themselves, so the stub records what it was asked
		/// and answers with `fail_with` when set — the concierge verdicts the routes relay.
		async fn grant_permission(&self, request: GrpcRequest<cc::GrantPermissionRequest>) -> Result<GrpcResponse<cc::GrantPermissionResponse>, Status> {
			self.guard_identity_plane(&request)?;
			let req = request.into_inner();
			let grant = stub_grant(&req.target);
			self.seen.lock().unwrap().grant_permission = Some(req);
			Ok(GrpcResponse::new(cc::GrantPermissionResponse { grant: Some(grant) }))
		}

		async fn revoke_permission(&self, request: GrpcRequest<cc::RevokePermissionRequest>) -> Result<GrpcResponse<cc::RevokePermissionResponse>, Status> {
			self.guard_identity_plane(&request)?;
			self.seen.lock().unwrap().revoke_permission = Some(request.into_inner());
			Ok(GrpcResponse::new(cc::RevokePermissionResponse {}))
		}

		async fn list_grants(&self, request: GrpcRequest<cc::ListGrantsRequest>) -> Result<GrpcResponse<cc::ListGrantsResponse>, Status> {
			self.guard_identity_plane(&request)?;
			self.seen.lock().unwrap().list_grants = Some(request.into_inner().namespace);
			Ok(GrpcResponse::new(cc::ListGrantsResponse {
				holders: vec![
					cc::GrantHolder {
						grant: Some(cc::PermissionGrant {
							orphaned: true,
							..stub_grant("sa:admin")
						}),
						email: KNOWN_INVESTOR_EMAIL.into(),
						legal_name: String::new(),
						preferred_name: "Seven".into(),
						username: String::new(),
					},
					// A row without its grant names nobody the tab could act on.
					cc::GrantHolder {
						grant: None,
						email: "orphan@example.test".into(),
						..Default::default()
					},
				],
			}))
		}
	}

	#[tonic::async_trait]
	impl BkAuthService for Hub {
		/// The concierge→banking exchange seam. Every fees route needs a money token, so
		/// counting the mints is also how these tests prove a rejected request never got
		/// far enough to ask for one.
		async fn issue_user_token(&self, _: GrpcRequest<bk::IssueUserTokenRequest>) -> Result<GrpcResponse<bk::TokenResponse>, Status> {
			self.seen.lock().unwrap().money_tokens_issued += 1;
			Ok(GrpcResponse::new(bk::TokenResponse {
				access_token: MONEY_TOKEN.into(),
				access_expires_at: now_secs() + 900,
				refresh_token: "banking-refresh-token".into(),
				refresh_expires_at: now_secs() + 86_400,
				user: None,
			}))
		}

		async fn refresh(&self, _: GrpcRequest<bk::RefreshRequest>) -> Result<GrpcResponse<bk::TokenResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		async fn logout(&self, _: GrpcRequest<bk::LogoutRequest>) -> Result<GrpcResponse<bk::LogoutResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		async fn list_sessions(&self, _: GrpcRequest<bk::ListSessionsRequest>) -> Result<GrpcResponse<bk::ListSessionsResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		async fn revoke_session(&self, _: GrpcRequest<bk::RevokeSessionRequest>) -> Result<GrpcResponse<bk::RevokeSessionResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}

		async fn jwks(&self, _: GrpcRequest<bk::JwksRequest>) -> Result<GrpcResponse<bk::JwksResponse>, Status> {
			Err(Status::unimplemented("not reached by the fees routes"))
		}
	}

	#[tonic::async_trait]
	impl FeesService for Hub {
		async fn list_fee_policies(&self, request: GrpcRequest<bk::ListFeePoliciesRequest>) -> Result<GrpcResponse<bk::FeePolicyList>, Status> {
			self.guard_money_plane(&request)?;
			Ok(GrpcResponse::new(bk::FeePolicyList {
				policies: vec![bk::FeePolicy {
					service: SERVICE.into(),
					configured: true,
					management_bps: 200,
					performance_bps: 2_000,
					hurdle_bps: 500,
					basis: "invested_capital".into(),
					crystallization: "annual".into(),
					updated_at: 1_750_000_000,
					version: 1,
					effective_from: 1_750_000_000,
					pending: Some(bk::FeePolicyChange {
						notices_waived_by: KNOWN_INVESTOR.into(),
						notices_waived_at: 1_750_050_000,
						notices_waived_users: vec![UNTOLD_HOLDER.into()],
						..stub_change(CHANGE_ID, "scheduled", 250, 2_000, 500, "invested_capital", "annual", 1_750_100_000, "")
					}),
				}],
			}))
		}

		async fn schedule_fee_policy(&self, request: GrpcRequest<bk::ScheduleFeePolicyRequest>) -> Result<GrpcResponse<bk::FeePolicyChange>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().set_policy = Some(req.clone());
			// The hub decides the moment; the stub echoes the request so a test can see what
			// was forwarded on the way back as well as in `seen`.
			Ok(GrpcResponse::new(stub_change(
				CHANGE_ID,
				"scheduled",
				req.management_bps,
				req.performance_bps,
				req.hurdle_bps,
				&req.basis,
				&req.crystallization,
				req.effective_from.max(1_750_086_400),
				&req.reason,
			)))
		}

		async fn cancel_fee_policy_change(&self, request: GrpcRequest<bk::CancelFeePolicyChangeRequest>) -> Result<GrpcResponse<bk::FeePolicyChange>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().cancel_change = Some(req.clone());
			Ok(GrpcResponse::new(stub_change(
				&req.change_id, "cancelled", 250, 2_000, 500, "invested_capital", "annual", 1_750_100_000, "",
			)))
		}

		async fn acknowledge_undelivered_notices(&self, request: GrpcRequest<bk::AcknowledgeUndeliveredNoticesRequest>) -> Result<GrpcResponse<bk::FeePolicyChange>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().acknowledge = Some(req.clone());
			Ok(GrpcResponse::new(bk::FeePolicyChange {
				notices_waived_by: "user-1".into(),
				notices_waived_at: 1_750_050_000,
				notices_waived_users: vec![UNTOLD_HOLDER.into()],
				undelivered_notices: 1,
				notices_given_up: 1,
				// Covered by the record just written: nothing left to offer.
				notices_unacknowledged: 0,
				..stub_change(&req.change_id, "scheduled", 250, 2_000, 500, "invested_capital", "annual", 1_750_100_000, "")
			}))
		}

		async fn list_fee_policy_changes(&self, request: GrpcRequest<bk::ListFeePolicyChangesRequest>) -> Result<GrpcResponse<bk::FeePolicyChangeList>, Status> {
			self.guard_money_plane(&request)?;
			Ok(GrpcResponse::new(bk::FeePolicyChangeList {
				changes: vec![
					// Acknowledged by the one person the directory can name…
					bk::FeePolicyChange {
						notices_waived_by: KNOWN_INVESTOR.into(),
						notices_waived_at: 1_750_050_000,
						notices_waived_users: vec![UNTOLD_HOLDER.into()],
						..stub_change(CHANGE_ID, "scheduled", 250, 2_000, 500, "invested_capital", "annual", 1_750_100_000, "")
					},
					// …and, back then, by one it cannot.
					bk::FeePolicyChange {
						version: 1,
						state: "active".into(),
						applied_at: 1_750_000_000,
						notices_waived_by: UNMIRRORED_INVESTOR.into(),
						notices_waived_at: 1_749_950_000,
						notices_waived_users: vec![UNTOLD_HOLDER.into()],
						..stub_change("0d5f3b7e-7d3c-4a58-9b2d-2c9f0c9f1e01", "active", 200, 2_000, 500, "invested_capital", "annual", 1_750_000_000, "")
					},
				],
			}))
		}

		async fn get_fee_shares(&self, request: GrpcRequest<bk::GetFeeSharesRequest>) -> Result<GrpcResponse<bk::FeeShares>, Status> {
			self.guard_money_plane(&request)?;
			Ok(GrpcResponse::new(bk::FeeShares {
				service: SERVICE.into(),
				units: "12.500000".into(),
				value: "1375.00".into(),
			}))
		}

		async fn settle_fee_shares(&self, request: GrpcRequest<bk::SettleFeeSharesRequest>) -> Result<GrpcResponse<bk::FeeSettlement>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().settle = Some(req.clone());
			Ok(GrpcResponse::new(bk::FeeSettlement {
				service: req.service,
				units: "12.500000".into(),
				nav: "110.00".into(),
				cash: "1375.00".into(),
			}))
		}

		async fn list_fund_fee_assessments(&self, request: GrpcRequest<bk::ListFundFeeAssessmentsRequest>) -> Result<GrpcResponse<bk::FeeAssessmentList>, Status> {
			self.guard_money_plane(&request)?;
			Ok(GrpcResponse::new(bk::FeeAssessmentList {
				assessments: vec![bk::FeeAssessment {
					service: SERVICE.into(),
					trigger: "period".into(),
					nav: "110.00".into(),
					management: "20.00".into(),
					performance: "80.00".into(),
					debt_opening: "0".into(),
					charged_units: "0.909090".into(),
					charged_cash: "100.00".into(),
					debt_carried: "0".into(),
					high_water_mark: "110.00".into(),
					assessed_at: 1_750_000_200,
				}],
			}))
		}

		async fn get_fee_policy(&self, _: GrpcRequest<bk::GetFeePolicyRequest>) -> Result<GrpcResponse<bk::FeePolicy>, Status> {
			Err(Status::unimplemented("not reached by the admin fees routes"))
		}

		async fn list_fee_assessments(&self, _: GrpcRequest<bk::ListFeeAssessmentsRequest>) -> Result<GrpcResponse<bk::FeeAssessmentList>, Status> {
			Err(Status::unimplemented("not reached by the admin fees routes"))
		}

		async fn get_accrued_fees(&self, _: GrpcRequest<bk::GetAccruedFeesRequest>) -> Result<GrpcResponse<bk::AccruedFees>, Status> {
			Err(Status::unimplemented("not reached by the admin fees routes"))
		}
	}

	/// The allocation the stub answers every access write with: the level the request
	/// named as the new default, and an HONEST `caller_access` for the admin — `view`,
	/// what they would get as an investor — so a test can see the BFF relays the two
	/// fields distinctly rather than collapsing them.
	fn stub_allocation(access: &str) -> bk::Allocation {
		bk::Allocation {
			service: SERVICE.into(),
			title: "Quy Nhon".into(),
			summary: String::new(),
			state: "open".into(),
			created_at: 1_750_000_000,
			updated_at: 1_750_000_300,
			unit_cap: "1000".into(),
			icon: "real_estate".into(),
			access: access.into(),
			caller_access: "view".into(),
			backing: "cash".into(),
		}
	}

	#[tonic::async_trait]
	impl AllocationsService for Hub {
		async fn set_allocation_access(&self, request: GrpcRequest<bk::SetAllocationAccessRequest>) -> Result<GrpcResponse<bk::Allocation>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().set_access = Some(req.clone());
			Ok(GrpcResponse::new(stub_allocation(&req.access)))
		}

		async fn grant_allocation_access(&self, request: GrpcRequest<bk::GrantAllocationAccessRequest>) -> Result<GrpcResponse<bk::AllocationAccessGrant>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().grant = Some(req.clone());
			Ok(GrpcResponse::new(bk::AllocationAccessGrant {
				service: req.service,
				user_id: req.user_id,
				level: req.level,
				granted_by: "user-1".into(),
				granted_at: 1_750_000_400,
			}))
		}

		async fn revoke_allocation_access(&self, request: GrpcRequest<bk::RevokeAllocationAccessRequest>) -> Result<GrpcResponse<bk::RevokeAllocationAccessResponse>, Status> {
			self.guard_money_plane(&request)?;
			self.seen.lock().unwrap().revoke = Some(request.into_inner());
			Ok(GrpcResponse::new(bk::RevokeAllocationAccessResponse {}))
		}

		/// Two standing grants: one holder the directory knows, one it does not.
		async fn list_allocation_access_grants(&self, request: GrpcRequest<bk::ListAllocationAccessGrantsRequest>) -> Result<GrpcResponse<bk::AllocationAccessGrantList>, Status> {
			self.guard_money_plane(&request)?;
			let service = request.into_inner().service;
			let grant = |user_id: &str| bk::AllocationAccessGrant {
				service: service.clone(),
				user_id: user_id.into(),
				level: "invest".into(),
				granted_by: "user-1".into(),
				granted_at: 1_750_000_400,
			};
			Ok(GrpcResponse::new(bk::AllocationAccessGrantList {
				grants: vec![grant(KNOWN_INVESTOR), grant(UNMIRRORED_INVESTOR)],
			}))
		}

		async fn list_allocations(&self, _: GrpcRequest<bk::ListAllocationsRequest>) -> Result<GrpcResponse<bk::AllocationList>, Status> {
			Err(Status::unimplemented("not reached by the access routes"))
		}

		async fn get_allocation(&self, _: GrpcRequest<bk::GetAllocationRequest>) -> Result<GrpcResponse<bk::Allocation>, Status> {
			Err(Status::unimplemented("not reached by the access routes"))
		}

		async fn register_allocation(&self, _: GrpcRequest<bk::RegisterAllocationRequest>) -> Result<GrpcResponse<bk::Allocation>, Status> {
			Err(Status::unimplemented("not reached by the access routes"))
		}

		async fn update_allocation(&self, _: GrpcRequest<bk::UpdateAllocationRequest>) -> Result<GrpcResponse<bk::Allocation>, Status> {
			Err(Status::unimplemented("not reached by the access routes"))
		}

		async fn set_allocation_state(&self, _: GrpcRequest<bk::SetAllocationStateRequest>) -> Result<GrpcResponse<bk::Allocation>, Status> {
			Err(Status::unimplemented("not reached by the access routes"))
		}

		async fn set_allocation_unit_cap(&self, _: GrpcRequest<bk::SetAllocationUnitCapRequest>) -> Result<GrpcResponse<bk::Allocation>, Status> {
			Err(Status::unimplemented("not reached by the access routes"))
		}

		async fn set_allocation_backing(&self, request: GrpcRequest<bk::SetAllocationBackingRequest>) -> Result<GrpcResponse<bk::Allocation>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().set_backing = Some(req.clone());
			Ok(GrpcResponse::new(bk::Allocation {
				backing: req.backing,
				..stub_allocation("view")
			}))
		}

		async fn retire_units(&self, request: GrpcRequest<bk::RetireUnitsRequest>) -> Result<GrpcResponse<bk::UnitIssuance>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().retire = Some(req.clone());
			Ok(GrpcResponse::new(bk::UnitIssuance {
				id: "9e2f".into(),
				service: req.service,
				holder_kind: "user".into(),
				holder_id: req.user_id,
				units: req.units,
				nav: "1.25".into(),
				cost_basis: if req.cost_basis.is_empty() { "16250".into() } else { req.cost_basis },
				state: "queued".into(),
				created_at: 1_750_000_500,
				source: "retire".into(),
			}))
		}

		async fn issue_units(&self, request: GrpcRequest<bk::IssueUnitsRequest>) -> Result<GrpcResponse<bk::UnitIssuance>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().issue = Some(req.clone());
			Ok(GrpcResponse::new(bk::UnitIssuance {
				id: "7c1e".into(),
				service: req.service,
				holder_kind: "user".into(),
				holder_id: req.user_id,
				units: req.units,
				nav: "1.25".into(),
				cost_basis: if req.cost_basis.is_empty() { "16250".into() } else { req.cost_basis },
				state: "queued".into(),
				created_at: 1_750_000_600,
				source: "mint".into(),
			}))
		}

		/// The cap table the hub answers with: a person and the `fee` allocation, each a
		/// line — the shape the holders screen has to render without summing anything.
		async fn list_unit_holders(&self, request: GrpcRequest<bk::ListUnitHoldersRequest>) -> Result<GrpcResponse<bk::UnitHolders>, Status> {
			self.guard_money_plane(&request)?;
			let holding = |kind: &str, id: &str, units: &str| bk::UnitHolding {
				holder: Some(bk::UnitHolderRef { kind: kind.into(), id: id.into() }),
				units: units.into(),
			};
			Ok(GrpcResponse::new(bk::UnitHolders {
				service: request.into_inner().service,
				units_outstanding: "13250".into(),
				queued_units: "100".into(),
				holders: vec![holding("user", KNOWN_INVESTOR, "13000"), holding("allocation", "fee", "250")],
			}))
		}
	}

	// ── harness ─────────────────────────────────────────────────────────────────

	fn now_secs() -> i64 {
		std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
	}

	/// Serve the stub on an ephemeral port, handing tonic the listener we already hold.
	///
	/// Binding to claim a port and then letting `serve(addr)` bind it again would open a
	/// window in which a sibling test — these run in parallel, in one process — is handed
	/// the port this one just released. The loser's `serve` would fail inside a spawned
	/// task, where the panic is swallowed, and its connect check would then succeed
	/// against the WINNER's hub: a test asserting on an `investor` role would quietly be
	/// talking to an `admin` stub. Never re-binding is what rules that out.
	async fn serve(hub: Hub) -> SocketAddr {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("claim an ephemeral port");
		let addr = listener.local_addr().expect("the listener has an address");

		tokio::spawn(async move {
			Server::builder()
				.add_service(CcAuthServiceServer::new(ConciergeJwks))
				.add_service(UserDirectoryServer::new(hub.clone()))
				.add_service(BkAuthServiceServer::new(hub.clone()))
				.add_service(AllocationsServiceServer::new(hub.clone()))
				.add_service(FeesServiceServer::new(hub))
				.serve_with_incoming(TcpListenerStream::new(listener))
				.await
				.expect("the stub hub serves");
		});

		addr
	}

	/// The real router over a real `AppState`, with all three upstream channels pointed at
	/// the one stub. Cookies take their insecure (unprefixed) names, as in development.
	fn app(addr: SocketAddr) -> Router {
		let env = |var: &str| -> Option<String> {
			Some(match var {
				"PIGGYBANK_GRPC_ADDR" | "BANKING_AUTH_GRPC_ADDR" | "CONCIERGE_GRPC_ADDR" => format!("http://{addr}"),
				"BANKING_ISSUANCE_TOKEN" => "test-issuance".into(),
				"AUTH_ISSUER" => ISSUER.into(),
				"AUTH_CLIENT_AUDIENCE" => AUDIENCE.into(),
				"MFE_REGISTRY_PATH" => "/mfe-registry.json".into(),
				"APP_ENV" => "development".into(),
				_ => return None,
			})
		};
		let config = AppConfig::from_source(env).expect("the test env loads");
		let verifier = Verifier::try_new(VerifierConfig {
			issuer: ISSUER.into(),
			audiences: vec![AUDIENCE.into()],
			allowed_types: vec![TokenType::Access],
			jwks_grpc_endpoint: format!("http://{addr}"),
		})
		.expect("build the verifier");
		let endpoint = format!("http://{addr}");

		router(AppState {
			cookies: Arc::new(CookieNames::new(config.cookie_secure())),
			banking: Arc::new(BankingTokens::new()),
			approvals: Arc::new(crate::routes::approval::AttemptLimiter::default()),
			money_mutations: Arc::new(crate::routes::money_mutation_limiter()),
			wallet_actions: None,
			verifier,
			grpc: Grpc::connect_lazy(&endpoint, &endpoint, &endpoint, Some("test-issuance".into())).expect("build the lazy channels"),
			config: Arc::new(config),
		})
	}

	/// A request carrying the signed session cookie (and, for mutations, the matching
	/// CSRF pair) — what the browser actually sends.
	fn signed(method: &str, uri: &str, body: Option<&str>, csrf: bool) -> Request<Body> {
		let cookie = format!("ev_access={}; ev_csrf={CSRF}", access_token());
		let mut builder = Request::builder().method(method).uri(uri).header(header::COOKIE, cookie);
		if csrf {
			builder = builder.header("x-ev-csrf", CSRF);
		}
		match body {
			Some(json) => builder.header(header::CONTENT_TYPE, "application/json").body(Body::from(json.to_owned())),
			None => builder.body(Body::empty()),
		}
		.expect("build the request")
	}

	async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
		let response = app.clone().oneshot(request).await.expect("the router responds");
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.expect("read the body");
		(status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
	}

	// ── the gates ───────────────────────────────────────────────────────────────

	/// Every fees route is behind the session cookie. Without one nothing is read, nothing
	/// is written, and no money-plane token is ever minted for the caller.
	#[tokio::test]
	async fn no_session_reaches_nothing() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for (method, uri, body) in [
			("GET", "/api/admin/fees/policies", None),
			("GET", "/api/admin/fees/shares?service=quy-nhon", None),
			("GET", "/api/admin/fees/assessments?service=quy-nhon", None),
			("GET", "/api/admin/fees/changes?service=quy-nhon", None),
			("POST", "/api/admin/fees/policy", Some("{}")),
			("POST", "/api/admin/fees/policy/cancel", Some("{}")),
			("POST", "/api/admin/fees/policy/acknowledge-notices", Some("{}")),
			("POST", "/api/admin/fees/settle", Some("{}")),
		] {
			let mut builder = Request::builder().method(method).uri(uri);
			if body.is_some() {
				builder = builder.header(header::CONTENT_TYPE, "application/json");
			}
			let request = builder.body(body.map_or_else(Body::empty, |b: &str| Body::from(b.to_owned()))).unwrap();
			let (status, _) = send(&app, request).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri} must refuse an unauthenticated caller");
		}

		let seen = seen.lock().unwrap();
		assert_eq!(seen.money_tokens_issued, 0, "an unauthenticated request must never mint a money-plane token");
		assert!(seen.set_policy.is_none() && seen.settle.is_none(), "an unauthenticated request must never reach the hub");
	}

	/// A plain investor holds a perfectly valid session — the role is what stops them, and
	/// it is read from the directory per request rather than trusted from the JWT.
	#[tokio::test]
	async fn an_investor_is_refused_the_console() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, _) = send(&app, signed("GET", "/api/admin/fees/policies", None, false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "an investor must not read the operator's terms table");

		let body = r#"{"service":"quy-nhon","management_bps":200,"performance_bps":2000,"hurdle_bps":0,"basis":"invested_capital","crystallization":"annual"}"#;
		let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy", Some(body), true)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "an investor must not price a fund");

		assert!(seen.lock().unwrap().set_policy.is_none(), "a refused caller must never reach the hub");
	}

	/// An `operator` passes the console but not the fee routes. The money plane refuses
	/// every fee call from an operator, so before this gate the console showed the fees
	/// screen and then answered 403 to everything on it. The refusal has to be decided at
	/// the BFF, before the CSRF check and before a money token is minted — the hub must not
	/// see the request at all, and the message must name what gets through, so the screen
	/// can say so instead of a bare "request failed".
	#[tokio::test]
	async fn an_operator_is_refused_the_fees_routes_but_not_the_console() {
		let hub = Hub::new("operator");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, response) = send(&app, signed("GET", "/api/admin/fees/policies", None, false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "an operator must not read the fees table");
		assert_eq!(response["error"], "requires bank:allocation:manage");

		let body = r#"{"service":"quy-nhon","management_bps":200,"performance_bps":2000,"hurdle_bps":0,"basis":"invested_capital","crystallization":"annual"}"#;
		let (status, response) = send(&app, signed("POST", "/api/admin/fees/policy", Some(body), true)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "an operator must not price a fund");
		assert_eq!(response["error"], "requires bank:allocation:manage");

		// The same session is still an operator elsewhere on the console: the refusal is
		// the fee routes' permission, not a demotion of the seat.
		let (status, _) = send(&app, signed("POST", "/api/admin/users/kyc", Some(r#"{"user_id":"u1","kyc_level":2}"#), true)).await;
		assert_eq!(status, StatusCode::OK, "an operator keeps the rest of the console");

		let seen = seen.lock().unwrap();
		assert!(seen.set_policy.is_none(), "the fee refusal must be decided before the hub is called");
		assert_eq!(seen.money_tokens_issued, 0, "a caller refused on role must not cost a money-token mint");
		assert!(seen.set_kyc.is_some(), "the operator's KYC call must still have reached the hub");
	}

	/// The fee gate is on every `/api/admin/fees/*` route, not on the ones the previous
	/// test happens to poke. The acknowledgement route once sat behind the coarse gate
	/// alone, so an operator passed the BFF, the CSRF check and a money-token mint, and
	/// only the hub said no — a 403 with the hub's wording instead of the one the screen
	/// reads the permission from. Every route here answers the same refusal before the hub.
	#[tokio::test]
	async fn an_operator_is_refused_every_fees_route_before_the_hub() {
		let hub = Hub::new("operator");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for (method, uri, body) in [
			("GET", "/api/admin/fees/policies", None),
			("GET", "/api/admin/fees/shares?service=quy-nhon", None),
			("GET", "/api/admin/fees/assessments?service=quy-nhon", None),
			("GET", "/api/admin/fees/changes?service=quy-nhon", None),
			("POST", "/api/admin/fees/policy", Some("{}")),
			("POST", "/api/admin/fees/policy/cancel", Some(r#"{"service":"quy-nhon","change_id":"c1"}"#)),
			("POST", "/api/admin/fees/policy/acknowledge-notices", Some(r#"{"service":"quy-nhon","change_id":"c1"}"#)),
			("POST", "/api/admin/fees/settle", Some(r#"{"service":"quy-nhon"}"#)),
		] {
			// The CSRF echo is sent so a 403 can only come from the role gate, and the
			// bodies are complete so nothing short-circuits on validation before it.
			let (status, response) = send(&app, signed(method, uri, body, true)).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri} must refuse an operator");
			assert_eq!(response["error"], "requires bank:allocation:manage", "{method} {uri} must name the permission that gets through");
		}

		let seen = seen.lock().unwrap();
		assert_eq!(seen.money_tokens_issued, 0, "a caller refused on role must not cost a money-token mint");
		assert!(
			seen.set_policy.is_none() && seen.cancel_change.is_none() && seen.acknowledge.is_none() && seen.settle.is_none(),
			"the fee refusal must be decided before the hub is called"
		);
	}

	/// The two roles the fee gate admits, both reading and writing. `owner` is here on
	/// purpose: a gate written as `role == "admin"` would lock the fund's owners out of
	/// their own fee terms, and nothing else on this seam would notice.
	#[tokio::test]
	async fn admins_and_owners_administer_fees() {
		for role in ["admin", "owner"] {
			let hub = Hub::new(role);
			let seen = hub.seen.clone();
			let app = app(serve(hub).await);

			let (status, _) = send(&app, signed("GET", "/api/admin/fees/policies", None, false)).await;
			assert_eq!(status, StatusCode::OK, "{role} must read the fees table");

			let body = r#"{"service":"quy-nhon","management_bps":200,"performance_bps":2000,"hurdle_bps":0,"basis":"invested_capital","crystallization":"annual"}"#;
			let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy", Some(body), true)).await;
			assert_eq!(status, StatusCode::OK, "{role} must be able to price a fund");

			assert!(seen.lock().unwrap().set_policy.is_some(), "{role}'s change of terms must reach the hub");
		}
	}

	/// The gate reads what concierge resolved the seat to, never the seat's name: an owner
	/// whose permissions concierge has not reported is refused like an investor.
	#[tokio::test]
	async fn a_seat_without_resolved_permissions_holds_nothing() {
		let hub = Hub {
			permissions: Vec::new(),
			..Hub::new("owner")
		};
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, _) = send(&app, signed("GET", "/api/admin/fees/policies", None, false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN);
		let (status, _) = send(&app, signed("POST", "/api/admin/users/kyc", Some(r#"{"user_id":"u1","kyc_level":2}"#), true)).await;
		assert_eq!(status, StatusCode::FORBIDDEN);

		let seen = seen.lock().unwrap();
		assert!(seen.set_kyc.is_none() && seen.set_policy.is_none(), "a refused caller must never reach the hub");
	}

	/// The double-submit gate on the two mutations. A valid session is not enough: a
	/// cross-site post carries the cookie but cannot read it to echo the header.
	#[tokio::test]
	async fn a_mutation_without_the_csrf_header_is_refused() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let body = r#"{"service":"quy-nhon","management_bps":200,"performance_bps":2000,"hurdle_bps":0,"basis":"invested_capital","crystallization":"annual"}"#;
		let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy", Some(body), false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "pricing a fund must require the CSRF echo");

		let (status, _) = send(&app, signed("POST", "/api/admin/fees/settle", Some(r#"{"service":"quy-nhon"}"#), false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "settling must require the CSRF echo");

		let seen = seen.lock().unwrap();
		assert!(seen.set_policy.is_none() && seen.settle.is_none(), "a CSRF failure must be decided before the hub is called");
	}

	// ── reading ─────────────────────────────────────────────────────────────────

	/// The terms table the screen opens on. `updated_at` crosses as a STRING even though
	/// the proto carries an int64 — the browser contract says string, and a silent switch
	/// to a JSON number is exactly the drift this pins.
	#[tokio::test]
	async fn the_policies_table_reaches_the_browser_intact() {
		let app = app(serve(Hub::new("admin")).await);

		let (status, body) = send(&app, signed("GET", "/api/admin/fees/policies", None, false)).await;
		assert_eq!(status, StatusCode::OK);

		let policy = &body["policies"][0];
		assert_eq!(policy["service"], SERVICE);
		assert_eq!(policy["configured"], true);
		assert_eq!(policy["management_bps"], 200);
		assert_eq!(policy["performance_bps"], 2_000);
		assert_eq!(policy["hurdle_bps"], 500);
		assert_eq!(policy["basis"], "invested_capital");
		assert_eq!(policy["crystallization"], "annual");
		assert_eq!(policy["updated_at"], "1750000000", "updated_at must serialize as a string, not a number");
	}

	/// Uncollected units and what they are worth — the right-hand card.
	#[tokio::test]
	async fn uncollected_units_carry_their_current_value() {
		let app = app(serve(Hub::new("admin")).await);

		let (status, body) = send(&app, signed("GET", "/api/admin/fees/shares?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["service"], SERVICE);
		assert_eq!(body["units"], "12.500000");
		assert_eq!(body["value"], "1375.00");
	}

	/// The audit trail, every column of it. `charged_cash` and `debt_opening` earn their
	/// place here as much as the rest: `charged_cash` falling short of
	/// `management + performance` is the ONLY thing that says a holding could not cover its
	/// charge, and dropping either from the DTO would empty a column of the operator's
	/// table while every other assertion still passed.
	#[tokio::test]
	async fn the_charge_history_keeps_every_column() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(&app, signed("GET", "/api/admin/fees/assessments?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);

		let row = &body["assessments"][0];
		assert_eq!(row["service"], SERVICE);
		assert_eq!(row["trigger"], "period");
		assert_eq!(row["nav"], "110.00");
		assert_eq!(row["management"], "20.00");
		assert_eq!(row["performance"], "80.00");
		assert_eq!(row["debt_opening"], "0");
		assert_eq!(row["charged_units"], "0.909090");
		assert_eq!(row["charged_cash"], "100.00");
		assert_eq!(row["debt_carried"], "0");
		assert_eq!(row["high_water_mark"], "110.00");
		assert_eq!(row["assessed_at"], "1750000200", "assessed_at must serialize as a string, not a number");

		// The mirror of the "rejected requests mint nothing" assertions: a read that DID
		// reach the hub must have crossed to the money plane to get there. Without this,
		// every `money_tokens_issued == 0` below would also pass if the seam were dead.
		assert_eq!(seen.lock().unwrap().money_tokens_issued, 1, "a served fees read must have minted the banking money token");
	}

	/// Both per-fund reads name their fund in the query string, and a missing one is a
	/// client error decided here — never a call to the hub with an empty service id.
	#[tokio::test]
	async fn a_per_fund_read_without_a_fund_is_rejected_locally() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for uri in [
			"/api/admin/fees/shares",
			"/api/admin/fees/assessments",
			"/api/admin/fees/shares?service=",
			"/api/admin/fees/assessments?service=%20",
		] {
			let (status, _) = send(&app, signed("GET", uri, None, false)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} must be rejected before the hub is called");
		}

		assert_eq!(seen.lock().unwrap().money_tokens_issued, 0, "a request rejected on its own input must not mint a money token");
	}

	// ── writing ─────────────────────────────────────────────────────────────────

	/// Pricing a fund. Every rate is forwarded as given — the handler must not reorder,
	/// default, or drop a field, because the tuple IS the fee schedule — and so are the
	/// requested moment and the reason, which the hub (not this layer) judges.
	#[tokio::test]
	async fn pricing_a_fund_forwards_the_whole_schedule() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let body = r#"{"service":"quy-nhon","management_bps":150,"performance_bps":1500,"hurdle_bps":500,"basis":"market_value","crystallization":"quarterly","effective_from":1750200000,"reason":"new mandate"}"#;
		let (status, response) = send(&app, signed("POST", "/api/admin/fees/policy", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK);
		// The answer is the CHANGE, not the live policy: the terms bind later.
		assert_eq!(response["id"], CHANGE_ID);
		assert_eq!(response["state"], "scheduled");
		assert_eq!(response["management_bps"], 150);
		assert_eq!(response["crystallization"], "quarterly");
		assert_eq!(response["effective_from"], "1750200000");
		assert_eq!(response["reason"], "new mandate");
		assert!(response["consilium_id"].is_null(), "an administrator's change waits on no consilium");

		let forwarded = seen.lock().unwrap().set_policy.clone().expect("the hub saw the write");
		assert_eq!(forwarded.service, SERVICE);
		assert_eq!(forwarded.management_bps, 150);
		assert_eq!(forwarded.performance_bps, 1_500);
		assert_eq!(forwarded.hurdle_bps, 500);
		assert_eq!(forwarded.basis, "market_value");
		assert_eq!(forwarded.crystallization, "quarterly");
		assert_eq!(forwarded.effective_from, 1_750_200_000);
		assert_eq!(forwarded.reason, "new mandate");
	}

	/// `effective_from` and `reason` are optional: omitted, they travel as the hub's "as
	/// soon as allowed" and "no reason given" — never invented. A moment this layer cannot
	/// read is refused rather than read as now.
	#[tokio::test]
	async fn an_omitted_moment_and_reason_travel_as_defaults_and_a_malformed_moment_is_refused() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let body = r#"{"service":"quy-nhon","management_bps":150,"performance_bps":1500,"hurdle_bps":500,"basis":"invested_capital","crystallization":"annual"}"#;
		let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK);
		let forwarded = seen.lock().unwrap().set_policy.clone().expect("the hub saw the write");
		assert_eq!(forwarded.effective_from, 0);
		assert_eq!(forwarded.reason, "");

		seen.lock().unwrap().set_policy = None;
		for body in [
			r#"{"service":"quy-nhon","management_bps":150,"performance_bps":1500,"hurdle_bps":500,"basis":"invested_capital","crystallization":"annual","effective_from":"tomorrow"}"#,
			r#"{"service":"quy-nhon","management_bps":150,"performance_bps":1500,"hurdle_bps":500,"basis":"invested_capital","crystallization":"annual","effective_from":-5}"#,
			r#"{"service":"quy-nhon","management_bps":150,"performance_bps":1500,"hurdle_bps":500,"basis":"invested_capital","crystallization":"annual","effective_from":1.5}"#,
		] {
			let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "an unreadable moment must be refused: {body}");
		}
		assert!(seen.lock().unwrap().set_policy.is_none(), "an unreadable moment must never reach the hub as now");
	}

	/// Withdrawing a pending change: the fund and the change id are both forwarded, and
	/// the answer is the change in its new state.
	#[tokio::test]
	async fn cancelling_a_change_forwards_the_fund_and_the_change_id() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy/cancel", Some(r#"{"service":"quy-nhon"}"#), true)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "a cancel without a change id is refused before the hub is called");
		assert!(seen.lock().unwrap().cancel_change.is_none());

		let body = format!(r#"{{"service":"quy-nhon","change_id":"{CHANGE_ID}"}}"#);
		let (status, response) = send(&app, signed("POST", "/api/admin/fees/policy/cancel", Some(&body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["state"], "cancelled");
		let forwarded = seen.lock().unwrap().cancel_change.clone().expect("the hub saw the cancel");
		assert_eq!(forwarded.service, SERVICE);
		assert_eq!(forwarded.change_id, CHANGE_ID);
	}

	/// Taking responsibility for untold holders: the fund and the change id are forwarded,
	/// and the answer carries the acknowledgement — who, when, whom for — and the figures
	/// the pending card reads, exactly as the hub states them.
	#[tokio::test]
	async fn acknowledging_undelivered_notices_forwards_the_change_and_reads_the_waiver_back() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy/acknowledge-notices", Some(r#"{"service":"quy-nhon"}"#), true)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "an acknowledgement without a change id is refused before the hub is called");
		assert!(seen.lock().unwrap().acknowledge.is_none());

		let body = format!(r#"{{"service":"quy-nhon","change_id":"{CHANGE_ID}"}}"#);
		let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy/acknowledge-notices", Some(&body), false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "the CSRF double-submit guards the acknowledgement as it guards every write");
		assert!(seen.lock().unwrap().acknowledge.is_none());

		let (status, response) = send(&app, signed("POST", "/api/admin/fees/policy/acknowledge-notices", Some(&body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["state"], "scheduled");
		assert_eq!(response["notices_waived_by"], "user-1");
		assert!(
			response["notices_waived_by_email"].is_null(),
			"a waiver the directory cannot name still reads back, without an email: {response}"
		);
		assert_eq!(response["notices_waived_at"], "1750050000");
		assert_eq!(response["notices_waived_users"], serde_json::json!([UNTOLD_HOLDER]));
		assert_eq!(response["undelivered_notices"], 1);
		assert_eq!(response["notices_given_up"], 1);
		assert_eq!(response["notices_unacknowledged"], 0);
		let forwarded = seen.lock().unwrap().acknowledge.clone().expect("the hub saw the acknowledgement");
		assert_eq!(forwarded.service, SERVICE);
		assert_eq!(forwarded.change_id, CHANGE_ID);
	}

	/// The history screen: newest first, every row carrying its full terms and state, and
	/// the live policy list carrying the pending change beside each fund.
	#[tokio::test]
	async fn the_history_and_the_pending_change_are_read_as_the_hub_states_them() {
		let app = app(serve(Hub::new("admin")).await);

		let (status, body) = send(&app, signed("GET", "/api/admin/fees/changes?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let changes = body["changes"].as_array().expect("a list of changes");
		assert_eq!(changes.len(), 2);
		assert_eq!(changes[0]["state"], "scheduled");
		assert_eq!(changes[0]["management_bps"], 250);
		assert_eq!(changes[1]["state"], "active");
		assert_eq!(changes[1]["version"], 1);
		assert_eq!(changes[1]["applied_at"], "1750000000");

		let (status, _) = send(&app, signed("GET", "/api/admin/fees/changes", None, false)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "the history is per fund");

		let (status, body) = send(&app, signed("GET", "/api/admin/fees/policies", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let policy = &body["policies"][0];
		assert_eq!(policy["version"], 1);
		assert_eq!(policy["effective_from"], "1750000000");
		assert_eq!(policy["pending"]["id"], CHANGE_ID);
		assert_eq!(policy["pending"]["state"], "scheduled");
	}

	/// The pending card and the history showed the acknowledger of undelivered notices as
	/// a raw id — "Notices waived by 78670fa7-…" (banking#328). The email comes from the
	/// identity plane beside the id, on every surface that carries a change; a waiver the
	/// directory cannot name keeps `notices_waived_by_email: null`, never a dropped row or a
	/// failed page, and a change nobody acknowledged asks the directory for nothing.
	#[tokio::test]
	async fn a_notice_waiver_is_named_by_email_when_the_directory_can() {
		let app = app(serve(Hub::new("admin")).await);

		let (status, body) = send(&app, signed("GET", "/api/admin/fees/changes?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let changes = body["changes"].as_array().expect("a list of changes");
		assert_eq!(changes.len(), 2, "an unresolvable waiver must not drop the change out of the history");
		assert_eq!(changes[0]["notices_waived_by"], KNOWN_INVESTOR);
		assert_eq!(changes[0]["notices_waived_by_email"], KNOWN_INVESTOR_EMAIL);
		assert_eq!(changes[1]["notices_waived_by"], UNMIRRORED_INVESTOR);
		assert!(
			changes[1]["notices_waived_by_email"].is_null(),
			"an id the directory cannot name has no email to show, not a made-up one: {}",
			changes[1]
		);
		assert!(
			changes[1].get("notices_waived_by_email").is_some(),
			"the key is present so the client can tell 'unknown' from 'not asked'"
		);

		let (status, body) = send(&app, signed("GET", "/api/admin/fees/policies", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let pending = &body["policies"][0]["pending"];
		assert_eq!(pending["notices_waived_by"], KNOWN_INVESTOR);
		assert_eq!(pending["notices_waived_by_email"], KNOWN_INVESTOR_EMAIL, "the pending card names the person too: {pending}");

		let body = format!(r#"{{"service":"quy-nhon","change_id":"{CHANGE_ID}"}}"#);
		let (status, response) = send(&app, signed("POST", "/api/admin/fees/policy/cancel", Some(&body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert!(response["notices_waived_by"].is_null());
		assert!(response["notices_waived_by_email"].is_null(), "no waiver, no name: {response}");
	}

	/// A fee schedule is read as a whole, so the write is all-or-nothing rather than a
	/// patch: omitting the fund, the basis or the period must fail loudly here, not
	/// silently leave a fund on terms nobody chose.
	#[tokio::test]
	async fn a_partial_schedule_is_refused_before_it_reaches_the_hub() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for body in [
			r#"{"management_bps":200,"performance_bps":2000,"hurdle_bps":0,"basis":"invested_capital","crystallization":"annual"}"#,
			r#"{"service":"quy-nhon","management_bps":200,"performance_bps":2000,"hurdle_bps":0,"crystallization":"annual"}"#,
			r#"{"service":"quy-nhon","management_bps":200,"performance_bps":2000,"hurdle_bps":0,"basis":"invested_capital"}"#,
			r#"{"service":"quy-nhon","management_bps":200,"performance_bps":2000,"hurdle_bps":0,"basis":"","crystallization":"annual"}"#,
		] {
			let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "an incomplete schedule must be refused: {body}");
		}

		let seen = seen.lock().unwrap();
		assert!(seen.set_policy.is_none(), "an incomplete schedule must never reach the hub");
		assert_eq!(seen.money_tokens_issued, 0, "a body we are going to refuse must not cost a money-token mint");
	}

	/// The rates are part of "the whole schedule", and a missing or malformed one is the
	/// dangerous case: coerced to zero it would price the fund at NOTHING while the screen
	/// answers "saved" — the very silent-zero state this console exists to end. A rate sent
	/// as a JSON string or a fraction is the same client bug wearing a different hat.
	#[tokio::test]
	async fn a_missing_or_malformed_rate_is_refused_rather_than_read_as_zero() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for body in [
			// No rates at all — the shape that would quietly zero a priced fund.
			r#"{"service":"quy-nhon","basis":"invested_capital","crystallization":"annual"}"#,
			// One rate missing.
			r#"{"service":"quy-nhon","management_bps":200,"hurdle_bps":0,"basis":"invested_capital","crystallization":"annual"}"#,
			// Numbers as strings — what a hand-rolled client or a form post sends.
			r#"{"service":"quy-nhon","management_bps":"200","performance_bps":"2000","hurdle_bps":"0","basis":"invested_capital","crystallization":"annual"}"#,
			// A fraction of a basis point is not a rate this plane can carry.
			r#"{"service":"quy-nhon","management_bps":2.5,"performance_bps":2000,"hurdle_bps":0,"basis":"invested_capital","crystallization":"annual"}"#,
			// Negative.
			r#"{"service":"quy-nhon","management_bps":-200,"performance_bps":2000,"hurdle_bps":0,"basis":"invested_capital","crystallization":"annual"}"#,
		] {
			let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "a rate that is not a whole number of bps must be refused: {body}");
		}

		assert!(seen.lock().unwrap().set_policy.is_none(), "a malformed rate must never reach the hub as a zero");
	}

	/// Zero IS a legitimate rate — a fund that charges no performance fee is a real
	/// product. Only an ABSENT or malformed rate is refused, so the guard above must not
	/// have made an explicit zero unwritable.
	#[tokio::test]
	async fn an_explicit_zero_rate_is_still_a_valid_schedule() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let body = r#"{"service":"quy-nhon","management_bps":0,"performance_bps":0,"hurdle_bps":0,"basis":"invested_capital","crystallization":"annual"}"#;
		let (status, _) = send(&app, signed("POST", "/api/admin/fees/policy", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK, "a deliberate zero-rate policy must be writable");

		let forwarded = seen.lock().unwrap().set_policy.clone().expect("the hub saw the write");
		assert_eq!((forwarded.management_bps, forwarded.performance_bps, forwarded.hurdle_bps), (0, 0, 0));
	}

	/// The ordinary end-of-period call the screen's one button makes: an empty `units`.
	/// That must forward an EMPTY units string — the hub's "settle the whole balance"
	/// sentinel — rather than a zero, which would settle nothing.
	#[tokio::test]
	async fn settling_without_an_amount_asks_for_the_whole_balance() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(&app, signed("POST", "/api/admin/fees/settle", Some(r#"{"service":"quy-nhon","units":""}"#), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["cash"], "1375.00");
		assert_eq!(body["nav"], "110.00");

		let forwarded = seen.lock().unwrap().settle.clone().expect("the hub saw the settle");
		assert_eq!(forwarded.service, SERVICE);
		assert_eq!(forwarded.units, "", "an omitted amount must settle the whole balance, not zero units");
	}

	/// A partial settlement is a deliberate act, and the amount must survive verbatim —
	/// a decimal string, never a float that could round the manager's draw.
	#[tokio::test]
	async fn a_partial_settlement_forwards_its_exact_amount() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, _) = send(&app, signed("POST", "/api/admin/fees/settle", Some(r#"{"service":"quy-nhon","units":"3.250000"}"#), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(seen.lock().unwrap().settle.clone().unwrap().units, "3.250000");
	}

	/// Settling without naming a fund is refused locally.
	#[tokio::test]
	async fn settling_must_name_its_fund() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, _) = send(&app, signed("POST", "/api/admin/fees/settle", Some(r#"{"units":"1"}"#), true)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);

		let seen = seen.lock().unwrap();
		assert!(seen.settle.is_none(), "a settle with no fund must never reach the hub");
		assert_eq!(seen.money_tokens_issued, 0, "a body we are going to refuse must not cost a money-token mint");
	}

	// ── what the hub says back ──────────────────────────────────────────────────

	/// The money plane re-checks the specific permission and can refuse a caller this
	/// coarse gate let through. That refusal must reach the browser as a 403 carrying the
	/// hub's own client-safe reason — the settle card shows it verbatim.
	#[tokio::test]
	async fn an_upstream_refusal_on_a_write_surfaces_with_its_reason() {
		let app = app(serve(Hub::failing("admin", Code::PermissionDenied)).await);

		let (status, body) = send(&app, signed("POST", "/api/admin/fees/settle", Some(r#"{"service":"quy-nhon","units":""}"#), true)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "the money plane's own permission check must surface as 403");
		assert_eq!(body["error"], "upstream refused", "a mutation relays the hub's client-safe detail");
	}

	/// A refused settlement — the fund's claim cannot cover it on top of the queued
	/// redemptions — is a precondition failure, not a queued job.
	#[tokio::test]
	async fn a_settlement_the_fund_cannot_cover_is_a_precondition_failure() {
		let app = app(serve(Hub::failing("admin", Code::FailedPrecondition)).await);

		let (status, _) = send(&app, signed("POST", "/api/admin/fees/settle", Some(r#"{"service":"quy-nhon","units":""}"#), true)).await;
		assert_eq!(status, StatusCode::PRECONDITION_FAILED, "an uncoverable settlement must be refused, never queued");
	}

	/// An `UNAUTHENTICATED` from the money plane — a banking token below the plane's
	/// revoke floor — must drop the cached pair, so the frontend's single heal-replay
	/// mints afresh instead of presenting the same refused token. Pinned by counting
	/// mints: the same two requests against a plane refusing for any OTHER reason share
	/// one mint, which is the cache doing its job.
	#[tokio::test]
	async fn an_unauthenticated_money_rpc_drops_the_cached_pair_so_the_replay_re_mints() {
		async fn mints_over_two_reads(hub: Hub, expected: StatusCode) -> usize {
			let seen = hub.seen.clone();
			let app = app(serve(hub).await);
			for _ in 0..2 {
				let (status, _) = send(&app, signed("GET", "/api/admin/fees/policies", None, false)).await;
				assert_eq!(status, expected);
			}
			seen.lock().unwrap().money_tokens_issued
		}

		assert_eq!(
			mints_over_two_reads(Hub::failing("admin", Code::Unauthenticated), StatusCode::UNAUTHORIZED).await,
			2,
			"the replay after a 401 must mint a new banking token"
		);
		assert_eq!(
			mints_over_two_reads(Hub::failing("admin", Code::PermissionDenied), StatusCode::FORBIDDEN).await,
			1,
			"any other refusal leaves the cached pair in place"
		);
	}

	/// Reads are the other half of the contract: a failing hub must map its code but
	/// surface the handler's fixed message, so internal detail never rides out on a GET.
	#[tokio::test]
	async fn a_failing_read_maps_the_code_but_hides_the_detail() {
		let app = app(serve(Hub::failing("admin", Code::Internal)).await);

		let (status, body) = send(&app, signed("GET", "/api/admin/fees/policies", None, false)).await;
		assert_eq!(status, StatusCode::BAD_GATEWAY);
		assert_eq!(body["error"], "fee policies unavailable", "a read must not relay the hub's own message");

		let (status, body) = send(&app, signed("GET", "/api/admin/fees/shares?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::BAD_GATEWAY);
		assert_eq!(body["error"], "fee shares unavailable");

		let (status, body) = send(&app, signed("GET", "/api/admin/fees/assessments?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::BAD_GATEWAY);
		assert_eq!(body["error"], "fee assessments unavailable");
	}

	// ── kyc ─────────────────────────────────────────────────────────────────────

	/// The tier is REQUIRED, and `as_u64` answers `None` for every shape below. Defaulting
	/// that `None` to zero is what let a mistyped console request read as "saved" while
	/// dropping the named user to tier 0 — which banking's `>= 1` gates turn into a
	/// lockout on both deposit-address issuance and withdrawals.
	#[tokio::test]
	async fn a_kyc_tier_the_bff_cannot_read_is_refused_rather_than_taken_as_zero() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for body in [
			// No tier at all — the shape that silently demoted whoever was named.
			r#"{"user_id":"u1"}"#,
			// Negative.
			r#"{"user_id":"u1","kyc_level":-1}"#,
			// A fraction of a tier is not a rung on the ladder.
			r#"{"user_id":"u1","kyc_level":2.5}"#,
			// A number as a string — what a hand-rolled client or a form post sends.
			r#"{"user_id":"u1","kyc_level":"2"}"#,
			// Past `u32::MAX`. These are the nastiest of the set: `as u32` truncated them
			// to 0 and to 1, so the overflow reached the hub as a plausible tier.
			r#"{"user_id":"u1","kyc_level":4294967296}"#,
			r#"{"user_id":"u1","kyc_level":4294967297}"#,
		] {
			let (status, _) = send(&app, signed("POST", "/api/admin/users/kyc", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "a tier that is not a whole number in range must be refused: {body}");
		}

		assert!(seen.lock().unwrap().set_kyc.is_none(), "a tier the BFF could not read must never reach the hub as a zero");
	}

	/// A tier above the ladder parses perfectly well as a `u32`, so only an explicit range
	/// check keeps a mistyped `4` from travelling. The ladder's top is written down in
	/// `contracts/proto/banking/v1/users.proto`; the console is the layer that should say
	/// so, rather than leaving the operator to read a refusal from two hops away.
	#[tokio::test]
	async fn a_kyc_tier_off_the_ladder_is_refused_locally() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for body in [r#"{"user_id":"u1","kyc_level":4}"#, r#"{"user_id":"u1","kyc_level":4294967295}"#] {
			let (status, _) = send(&app, signed("POST", "/api/admin/users/kyc", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "a tier above the ladder must be refused: {body}");
		}

		assert!(seen.lock().unwrap().set_kyc.is_none(), "an out-of-range tier must never reach the hub");
	}

	/// Every rung the ladder actually has stays settable — including 0, which is a real
	/// tier (a demotion after a failed review) and not merely the value the old default
	/// used to invent. The guard above must not have made a deliberate zero unwritable.
	#[tokio::test]
	async fn every_tier_on_the_ladder_is_still_settable() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for level in 0..=MAX_KYC_LEVEL {
			let body = format!(r#"{{"user_id":"u1","kyc_level":{level}}}"#);
			let (status, response) = send(&app, signed("POST", "/api/admin/users/kyc", Some(body.as_str()), true)).await;
			assert_eq!(status, StatusCode::OK, "tier {level} is on the ladder and must be writable");
			assert_eq!(response["kyc_level"], level, "the response carries the tier the hub applied");

			let forwarded = seen.lock().unwrap().set_kyc.clone().expect("the hub saw the write");
			assert_eq!((forwarded.user_id.as_str(), forwarded.kyc_level), ("u1", level), "the tier must reach the hub unchanged");
		}
	}

	/// The one target a `KycManage` holder may not name is themselves.
	///
	/// A tier they can raise on their own row is every `>= 1` gate banking has, walked past
	/// without anyone else in the room — so the identity plane refuses it. Before this the
	/// console forwarded the call anyway and the operator met the hub's 403 through a live
	/// selector; the refusal belongs here, where the screen can be told about it.
	///
	/// Pinned on all three halves of that: the status, the CODE the frontend translates
	/// (prose fixed here would be English in all five locales), and the hub having seen
	/// nothing at all. `user-1` is the subject the signed test cookie carries — every other
	/// KYC test targets `u1`, so the caller and the target were never the same account and
	/// the stub hub accepted what the real one now refuses.
	#[tokio::test]
	async fn an_operator_may_not_set_their_own_kyc_tier() {
		let hub = Hub::new("operator");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for level in 0..=MAX_KYC_LEVEL {
			let body = format!(r#"{{"user_id":"user-1","kyc_level":{level}}}"#);
			let (status, response) = send(&app, signed("POST", "/api/admin/users/kyc", Some(body.as_str()), true)).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "tier {level} on the caller's own row must be refused");
			assert_eq!(response["error"], KYC_SELF_DENIED, "the refusal must carry the code the console translates");
		}

		assert!(seen.lock().unwrap().set_kyc.is_none(), "a caller naming themselves must never reach the hub");

		// The gate is on the pairing, not on the route: the same session still administers
		// everybody else's tier.
		let (status, _) = send(&app, signed("POST", "/api/admin/users/kyc", Some(r#"{"user_id":"u1","kyc_level":2}"#), true)).await;
		assert_eq!(status, StatusCode::OK, "another account's tier is still this operator's to set");
		assert_eq!(seen.lock().unwrap().set_kyc.as_ref().expect("the hub saw the write").user_id, "u1");
	}

	// ── allocation access ───────────────────────────────────────────────────────

	/// The four access routes sit behind the same three gates as everything else on the
	/// console: session, role, CSRF on the mutations. Pinned here because these are the
	/// routes that decide who a product deals with — an investor reaching them would be
	/// letting themselves in.
	#[tokio::test]
	async fn the_access_routes_are_gated_like_the_rest_of_the_console() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, _) = send(&app, signed("GET", "/api/admin/allocations/grants?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "an investor must not read who was let in");
		for (uri, body) in [
			("/api/admin/allocations/access", r#"{"service":"quy-nhon","access":"invest"}"#),
			("/api/admin/allocations/grants/grant", r#"{"service":"quy-nhon","user_id":"investor-7","level":"invest"}"#),
			("/api/admin/allocations/grants/revoke", r#"{"service":"quy-nhon","user_id":"investor-7"}"#),
			(
				"/api/admin/allocations/issue",
				r#"{"service":"quy-nhon","user_id":"investor-7","units":"13000","idempotency_key":"k"}"#,
			),
			(
				"/api/admin/allocations/retire",
				r#"{"service":"quy-nhon","user_id":"investor-7","units":"13000","idempotency_key":"k"}"#,
			),
			("/api/admin/allocations/backing", r#"{"service":"quy-nhon","backing":"cash"}"#),
			(
				"/api/admin/treasury/seed-capital",
				r#"{"tx_ref":"0xabc:0","network":"bep20","expected_amount":"100","depositor_user_id":"investor-7"}"#,
			),
		] {
			let (status, _) = send(&app, signed("POST", uri, Some(body), true)).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "an investor must not change access or holdings: {uri}");
		}

		let admin = Hub::new("admin");
		let admin_seen = admin.seen.clone();
		let admin_app = self::app(serve(admin).await);
		let (status, _) = send(
			&admin_app,
			signed("POST", "/api/admin/allocations/access", Some(r#"{"service":"quy-nhon","access":"invest"}"#), false),
		)
		.await;
		assert_eq!(status, StatusCode::FORBIDDEN, "opening a product to everyone must require the CSRF echo");
		let (status, _) = send(
			&admin_app,
			signed(
				"POST",
				"/api/admin/allocations/grants/grant",
				Some(r#"{"service":"quy-nhon","user_id":"investor-7","level":"invest"}"#),
				false,
			),
		)
		.await;
		assert_eq!(status, StatusCode::FORBIDDEN, "granting must require the CSRF echo");

		for seen in [seen, admin_seen] {
			let seen = seen.lock().unwrap();
			assert!(
				seen.set_access.is_none() && seen.grant.is_none() && seen.revoke.is_none() && seen.retire.is_none() && seen.set_backing.is_none(),
				"a refused caller must never reach the hub"
			);
			assert_eq!(seen.money_tokens_issued, 0, "a refused request must not mint a money-plane token");
		}
	}

	/// Setting the default forwards the level verbatim, and the row comes back with BOTH
	/// access fields — the product's default and the caller's own effective level — kept
	/// distinct. Collapsing them would tell the console an admin "can invest" in a product
	/// they just locked.
	#[tokio::test]
	async fn setting_the_default_access_forwards_the_level_and_relays_both_access_fields() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(&app, signed("POST", "/api/admin/allocations/access", Some(r#"{"service":"quy-nhon","access":"hidden"}"#), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["service"], SERVICE);
		assert_eq!(body["access"], "hidden", "the product's default, as the hub applied it");
		assert_eq!(body["caller_access"], "view", "the caller's own level, not the default and not their permission");
		assert_eq!(body["updated_at"], "1750000300", "int64s cross as strings");

		let forwarded = seen.lock().unwrap().set_access.clone().expect("the hub saw the write");
		assert_eq!((forwarded.service.as_str(), forwarded.access.as_str()), (SERVICE, "hidden"));
	}

	/// A level outside the vocabulary — or, for a grant, `hidden`, which is a real level
	/// that a grant may not carry — is refused here, before a money token is minted for a
	/// request the hub was never going to serve. The message names what was expected.
	#[tokio::test]
	async fn an_access_level_outside_the_vocabulary_is_refused_locally() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for (uri, body) in [
			("/api/admin/allocations/access", r#"{"service":"quy-nhon","access":"public"}"#),
			("/api/admin/allocations/access", r#"{"service":"quy-nhon","access":""}"#),
			("/api/admin/allocations/access", r#"{"service":"quy-nhon"}"#),
			("/api/admin/allocations/access", r#"{"access":"invest"}"#),
			// `hidden` is the product-default floor, never a grant: a grant only raises.
			("/api/admin/allocations/grants/grant", r#"{"service":"quy-nhon","user_id":"investor-7","level":"hidden"}"#),
			("/api/admin/allocations/grants/grant", r#"{"service":"quy-nhon","user_id":"investor-7","level":"owner"}"#),
			("/api/admin/allocations/grants/grant", r#"{"service":"quy-nhon","user_id":"investor-7"}"#),
			("/api/admin/allocations/grants/grant", r#"{"service":"quy-nhon","level":"invest"}"#),
			("/api/admin/allocations/grants/revoke", r#"{"service":"quy-nhon"}"#),
			// A mint names a person, an amount and a retry key, or it is not a request. The
			// company is not a holder (#245): a stale console sending `company: true` in
			// place of a person is a form nobody filled in, not a mint to the company.
			("/api/admin/allocations/issue", r#"{"service":"quy-nhon","units":"13000","idempotency_key":"k"}"#),
			("/api/admin/allocations/issue", r#"{"service":"quy-nhon","company":true,"units":"13000","idempotency_key":"k"}"#),
			("/api/admin/allocations/issue", r#"{"service":"quy-nhon","user_id":"investor-7","idempotency_key":"k"}"#),
			("/api/admin/allocations/issue", r#"{"service":"quy-nhon","user_id":"investor-7","units":"13000"}"#),
			// A seed names the transfer, its rail and the figure the owners approve, or it
			// is not a proposal — `expected_amount` is not optional here as it is on a
			// deposit, because the amount goes under the owners' signature.
			("/api/admin/treasury/seed-capital", r#"{"network":"bep20","expected_amount":"100"}"#),
			("/api/admin/treasury/seed-capital", r#"{"tx_ref":"0xabc:0","expected_amount":"100"}"#),
			("/api/admin/treasury/seed-capital", r#"{"tx_ref":"0xabc:0","network":"bep20"}"#),
		] {
			let (status, response) = send(&app, signed("POST", uri, Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "must be refused before the hub is called: {uri} {body}");
			assert!(response["error"].is_string(), "the refusal says why: {response}");
		}
		let (status, body) = send(
			&app,
			signed(
				"POST",
				"/api/admin/allocations/grants/grant",
				Some(r#"{"service":"quy-nhon","user_id":"investor-7","level":"hidden"}"#),
				true,
			),
		)
		.await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		let reason = body["error"].as_str().unwrap_or_default();
		assert!(
			reason.contains("view") && reason.contains("invest") && !reason.contains("hidden, "),
			"a grant's refusal lists the grantable levels only: {reason}"
		);

		let seen = seen.lock().unwrap();
		assert!(
			seen.set_access.is_none() && seen.grant.is_none() && seen.revoke.is_none(),
			"a malformed level must never reach the hub"
		);
		assert_eq!(seen.money_tokens_issued, 0, "a body we are going to refuse must not cost a money-token mint");
	}

	/// Granting forwards the investor and the level as given and relays the grant the hub
	/// wrote — `granted_at` as a string like every other int64; revoking forwards the pair
	/// and answers `ok` (the hub is idempotent, so there is nothing else to say).
	#[tokio::test]
	async fn granting_and_revoking_forward_the_investor_and_relay_the_grant() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(
			&app,
			signed(
				"POST",
				"/api/admin/allocations/grants/grant",
				Some(r#"{"service":"quy-nhon","user_id":"investor-7","level":"view"}"#),
				true,
			),
		)
		.await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["service"], SERVICE);
		assert_eq!(body["user_id"], "investor-7");
		assert_eq!(body["level"], "view");
		assert_eq!(body["granted_by"], "user-1");
		assert_eq!(body["granted_at"], "1750000400", "granted_at must serialize as a string, not a number");
		let forwarded = seen.lock().unwrap().grant.clone().expect("the hub saw the grant");
		assert_eq!(
			(forwarded.service.as_str(), forwarded.user_id.as_str(), forwarded.level.as_str()),
			(SERVICE, "investor-7", "view")
		);

		let (status, body) = send(
			&app,
			signed("POST", "/api/admin/allocations/grants/revoke", Some(r#"{"service":"quy-nhon","user_id":"investor-7"}"#), true),
		)
		.await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["ok"], true);
		let forwarded = seen.lock().unwrap().revoke.clone().expect("the hub saw the revoke");
		assert_eq!((forwarded.service.as_str(), forwarded.user_id.as_str()), (SERVICE, "investor-7"));
	}

	/// A retirement forwards the person as `user_id` together with the `force` override,
	/// and relays the row the hub wrote with `source: "retire"` and POSITIVE units: the
	/// row is the magnitude, the source is the direction, and a console summing the
	/// history must not be handed a negative number to guess at.
	#[tokio::test]
	async fn retiring_forwards_the_holder_and_the_force_flag_and_relays_the_row() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(
			&app,
			signed(
				"POST",
				"/api/admin/allocations/retire",
				Some(r#"{"service":"quy-nhon","user_id":"investor-7","units":"13000","idempotency_key":"k-retire","force":true}"#),
				true,
			),
		)
		.await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["service"], SERVICE);
		assert_eq!(body["source"], "retire");
		assert_eq!(body["holder_kind"], "user");
		assert_eq!(body["holder_id"], "investor-7");
		assert_eq!(body["units"], "13000", "the magnitude, never signed");
		assert_eq!(body["cost_basis"], "16250", "the hub's `units × NAV` default when the body named none");
		assert_eq!(body["state"], "queued");
		assert_eq!(body["created_at"], "1750000500", "int64s cross as strings");

		let forwarded = seen.lock().unwrap().retire.clone().expect("the hub saw the retirement");
		assert_eq!(forwarded.service, SERVICE);
		assert_eq!(forwarded.user_id, "investor-7");
		assert_eq!((forwarded.units.as_str(), forwarded.idempotency_key.as_str()), ("13000", "k-retire"));
		assert!(forwarded.cost_basis.is_empty(), "an absent cost_basis crosses empty so the hub applies its default");
		assert!(forwarded.force, "the operator's override must reach the hub as given");
	}

	/// A mint forwards the person as `user_id` — the only holder a mint can name since
	/// #245 — and relays the row with `source: "mint"`. The console's `company` toggle is
	/// gone with the company holder; a body that still carries it is refused (see the
	/// malformed-body test), never quietly minted to somebody.
	#[tokio::test]
	async fn issuing_forwards_the_person_and_relays_the_mint() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(
			&app,
			signed(
				"POST",
				"/api/admin/allocations/issue",
				Some(r#"{"service":"quy-nhon","user_id":"investor-7","units":"13000","cost_basis":"0","idempotency_key":"k-mint"}"#),
				true,
			),
		)
		.await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(
			(body["holder_kind"].as_str(), body["holder_id"].as_str(), body["source"].as_str()),
			(Some("user"), Some("investor-7"), Some("mint"))
		);
		assert_eq!(body["cost_basis"], "0", "an explicit zero basis is a value, not an absence");

		let forwarded = seen.lock().unwrap().issue.clone().expect("the hub saw the mint");
		assert_eq!(
			(forwarded.service.as_str(), forwarded.user_id.as_str(), forwarded.units.as_str()),
			(SERVICE, "investor-7", "13000")
		);
		assert_eq!((forwarded.cost_basis.as_str(), forwarded.idempotency_key.as_str()), ("0", "k-mint"));
	}

	/// The cap table crosses as the hub lists it — one line per holder, a person and the
	/// `fee` allocation alike, with the kind that says how to render the id — and no
	/// class totals for the screen to reconcile against.
	#[tokio::test]
	async fn the_holders_route_relays_the_cap_table_line_by_line() {
		let app = app(serve(Hub::new("admin")).await);

		let (status, body) = send(&app, signed("GET", "/api/admin/allocations/holders?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(
			(body["service"].as_str(), body["units_outstanding"].as_str(), body["queued_units"].as_str()),
			(Some(SERVICE), Some("13250"), Some("100"))
		);
		let holders = body["holders"].as_array().expect("the cap table is a list");
		assert_eq!(holders.len(), 2);
		assert_eq!(
			(holders[0]["holder"]["kind"].as_str(), holders[0]["holder"]["id"].as_str(), holders[0]["units"].as_str()),
			(Some("user"), Some(KNOWN_INVESTOR), Some("13000"))
		);
		assert_eq!(
			(holders[1]["holder"]["kind"].as_str(), holders[1]["holder"]["id"].as_str(), holders[1]["units"].as_str()),
			(Some("allocation"), Some("fee"), Some("250"))
		);
		for retired in ["company_units", "fee_units", "investor_units"] {
			assert!(body.get(retired).is_none(), "the pre-#245 class total `{retired}` must not be on the wire");
		}

		let (status, _) = send(&app, signed("GET", "/api/admin/allocations/holders", None, false)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "a cap table is of one product");
	}

	/// `force` left out of the body is `false`: the console must not be able to burn units
	/// out of a live product by forgetting a field. A `cost_basis` the operator typed is
	/// forwarded verbatim.
	#[tokio::test]
	async fn retiring_a_user_defaults_force_to_false_and_keeps_the_typed_cost_basis() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(
			&app,
			signed(
				"POST",
				"/api/admin/allocations/retire",
				Some(r#"{"service":"quy-nhon","user_id":"investor-7","units":"100","cost_basis":"120","idempotency_key":"k2"}"#),
				true,
			),
		)
		.await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!((body["holder_kind"].as_str(), body["holder_id"].as_str()), (Some("user"), Some("investor-7")));
		assert_eq!(body["cost_basis"], "120");

		let forwarded = seen.lock().unwrap().retire.clone().expect("the hub saw the retirement");
		assert_eq!(forwarded.user_id, "investor-7");
		assert_eq!(forwarded.cost_basis, "120");
		assert!(!forwarded.force, "force absent from the body must cross as false");
	}

	/// A retirement names an amount, a retry key and a person, or it is not a request —
	/// and none of these malformed bodies may cost a money-token mint. The company is not
	/// a holder any more (#245): a stale console's `company: true` names nobody.
	#[tokio::test]
	async fn a_malformed_retirement_is_refused_before_the_hub_is_called() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for body in [
			r#"{"service":"quy-nhon","user_id":"investor-7","idempotency_key":"k"}"#,
			r#"{"service":"quy-nhon","user_id":"investor-7","units":"1"}"#,
			r#"{"user_id":"investor-7","units":"1","idempotency_key":"k"}"#,
			// Nobody's units — with or without the retired company flag — is a form nobody
			// filled in.
			r#"{"service":"quy-nhon","units":"1","idempotency_key":"k"}"#,
			r#"{"service":"quy-nhon","company":true,"units":"1","idempotency_key":"k"}"#,
			r#"{"service":"quy-nhon","user_id":"","units":"1","idempotency_key":"k"}"#,
		] {
			let (status, response) = send(&app, signed("POST", "/api/admin/allocations/retire", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "must be refused before the hub is called: {body}");
			assert!(response["error"].is_string(), "the refusal says why: {response}");
		}

		let seen = seen.lock().unwrap();
		assert!(seen.retire.is_none(), "a malformed retirement must never reach the hub");
		assert_eq!(seen.money_tokens_issued, 0, "a body we are going to refuse must not cost a money-token mint");
	}

	/// Setting the backing forwards the word verbatim and the row comes back carrying it —
	/// the field the client draws the redeem control off. A word outside `cash | in_kind`
	/// is refused here, naming what was expected, before a money token is minted for it.
	#[tokio::test]
	async fn setting_the_backing_forwards_it_and_the_allocation_carries_it() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(
			&app,
			signed("POST", "/api/admin/allocations/backing", Some(r#"{"service":"quy-nhon","backing":"in_kind"}"#), true),
		)
		.await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["service"], SERVICE);
		assert_eq!(body["backing"], "in_kind", "the backing as the hub applied it");
		assert_eq!(body["access"], "view", "the rest of the row rides along untouched");
		let forwarded = seen.lock().unwrap().set_backing.clone().expect("the hub saw the write");
		assert_eq!((forwarded.service.as_str(), forwarded.backing.as_str()), (SERVICE, "in_kind"));

		let minted_before = seen.lock().unwrap().money_tokens_issued;
		for body in [
			r#"{"service":"quy-nhon","backing":"asset"}"#,
			r#"{"service":"quy-nhon","backing":"InKind"}"#,
			r#"{"service":"quy-nhon","backing":""}"#,
			r#"{"service":"quy-nhon"}"#,
			r#"{"backing":"cash"}"#,
		] {
			let (status, response) = send(&app, signed("POST", "/api/admin/allocations/backing", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "must be refused before the hub is called: {body}");
			let reason = response["error"].as_str().unwrap_or_default();
			assert!(
				reason.contains("service") || (reason.contains("cash") && reason.contains("in_kind")),
				"the refusal names what was expected: {reason}"
			);
		}
		let seen = seen.lock().unwrap();
		assert_eq!(seen.set_backing.as_ref().map(|r| r.backing.as_str()), Some("in_kind"), "no malformed write reached the hub");
		assert_eq!(seen.money_tokens_issued, minted_before, "a body we are going to refuse must not cost a money-token mint");
	}

	/// The grant list names its fund in the query string; a missing one is decided here.
	#[tokio::test]
	async fn the_grant_list_names_its_fund_and_keeps_every_column() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for uri in [
			"/api/admin/allocations/grants",
			"/api/admin/allocations/grants?service=",
			"/api/admin/allocations/grants?service=%20",
		] {
			let (status, _) = send(&app, signed("GET", uri, None, false)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} must be rejected before the hub is called");
		}
		assert_eq!(seen.lock().unwrap().money_tokens_issued, 0);

		let (status, body) = send(&app, signed("GET", "/api/admin/allocations/grants?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let row = &body["grants"][0];
		assert_eq!(row["service"], SERVICE);
		assert_eq!(row["user_id"], KNOWN_INVESTOR);
		assert_eq!(row["level"], "invest");
		assert_eq!(row["granted_by"], "user-1");
		assert_eq!(row["granted_at"], "1750000400");
	}

	/// The grants table showed a raw id where the operator expected a person (banking#252).
	/// The email comes from the identity plane, per holder; a holder the directory cannot
	/// name keeps their row — with `email: null`, never a dropped grant or a failed page.
	#[tokio::test]
	async fn the_grant_list_names_each_holder_by_email_when_the_directory_can() {
		let app = app(serve(Hub::new("admin")).await);

		let (status, body) = send(&app, signed("GET", "/api/admin/allocations/grants?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let grants = body["grants"].as_array().expect("a list of grants");
		assert_eq!(grants.len(), 2, "an unresolvable holder must not drop out of the list");
		let known = grants.iter().find(|g| g["user_id"] == KNOWN_INVESTOR).expect("the mirrored investor lists");
		assert_eq!(known["email"], KNOWN_INVESTOR_EMAIL);
		let unmirrored = grants.iter().find(|g| g["user_id"] == UNMIRRORED_INVESTOR).expect("the banking-only investor lists too");
		assert!(
			unmirrored["email"].is_null(),
			"an id the directory cannot name has no email to show, not a made-up one: {unmirrored}"
		);
		assert!(unmirrored.get("email").is_some(), "the key is present so the client can tell 'unknown' from 'not asked'");
	}

	/// The hub re-checks `AllocationManage` and answers its own codes; a grant for a user
	/// the money plane does not know is NOT_FOUND, and that must reach the console as a
	/// 404 carrying the reason rather than as a generic upstream failure.
	#[tokio::test]
	async fn an_upstream_refusal_on_an_access_write_surfaces_with_its_code() {
		let app = app(serve(Hub::failing("admin", Code::NotFound)).await);
		let (status, body) = send(
			&app,
			signed(
				"POST",
				"/api/admin/allocations/grants/grant",
				Some(r#"{"service":"quy-nhon","user_id":"nobody","level":"invest"}"#),
				true,
			),
		)
		.await;
		assert_eq!(status, StatusCode::NOT_FOUND);
		assert_eq!(body["error"], "upstream refused");

		let app = self::app(serve(Hub::failing("admin", Code::Internal)).await);
		let (status, body) = send(&app, signed("GET", "/api/admin/allocations/grants?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::BAD_GATEWAY);
		assert_eq!(body["error"], "allocation access grants unavailable", "a read must not relay the hub's own message");
	}

	// ── tenant grants: what a user holds inside a tenant's namespace ──────────

	const GRANTS_ROUTE: &str = "/api/admin/tenants/sa/grants";

	/// Reads and writes of a namespace are behind the session like everything else, and a
	/// mutation checks the double-submit pair FIRST — a missing header is refused as CSRF
	/// even without a session, which proves the check sits at the top of the handler.
	#[tokio::test]
	async fn grants_need_a_session_and_csrf() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let anonymous = Request::builder().method("GET").uri(GRANTS_ROUTE).body(Body::empty()).unwrap();
		assert_eq!(send(&app, anonymous).await.0, StatusCode::UNAUTHORIZED);

		let body = r#"{"email":"investor7@example.test","target":"sa:operator"}"#;
		for method in ["POST", "DELETE"] {
			let (status, response) = send(&app, signed(method, GRANTS_ROUTE, Some(body), false)).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "{method} without the CSRF header");
			assert_eq!(response["error"], "csrf", "{method} must be refused as CSRF, not by the directory");

			let anonymous = Request::builder()
				.method(method)
				.uri(GRANTS_ROUTE)
				.header(header::COOKIE, format!("ev_csrf={CSRF}"))
				.header("x-ev-csrf", CSRF)
				.header(header::CONTENT_TYPE, "application/json")
				.body(Body::from(body))
				.unwrap();
			assert_eq!(send(&app, anonymous).await.0, StatusCode::UNAUTHORIZED, "{method} with CSRF but no session");
		}

		let seen = seen.lock().unwrap();
		assert!(
			seen.grant_permission.is_none() && seen.revoke_permission.is_none() && seen.list_grants.is_none(),
			"a refused request must never reach the directory"
		);
	}

	/// A tenant's delegate is usually a plain `investor` globally, so the BFF must NOT put
	/// the console's role gate in front of these routes: the directory judges the caller.
	/// It is asked with the caller's concierge token, and no money token is ever minted —
	/// tenant grants are not a money-plane matter.
	#[tokio::test]
	async fn a_delegate_without_a_global_role_reaches_the_directory() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(&app, signed("GET", GRANTS_ROUTE, None, false)).await;
		assert_eq!(status, StatusCode::OK, "{body}");
		let holders = body["holders"].as_array().expect("holders is a list");
		assert_eq!(holders.len(), 1, "a row without its grant is dropped: {body}");
		assert_eq!(holders[0]["email"], KNOWN_INVESTOR_EMAIL);
		assert_eq!(holders[0]["legal_name"], "");
		assert_eq!(holders[0]["preferred_name"], "Seven");
		assert_eq!(holders[0]["grant"]["target"], "sa:admin");
		assert_eq!(holders[0]["grant"]["orphaned"], true);
		assert_eq!(holders[0]["grant"]["id"], "41", "int64 crosses as a string");
		assert_eq!(holders[0]["grant"]["granted_at"], "1750000200", "int64 crosses as a string");

		let (status, body) = send(
			&app,
			signed(
				"POST",
				GRANTS_ROUTE,
				Some(r#"{"email":"investor7@example.test","target":"sa:operator","reason":"runs the calls"}"#),
				true,
			),
		)
		.await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["target"], "sa:operator");
		assert_eq!(body["user_id"], KNOWN_INVESTOR);

		let (status, body) = send(&app, signed("DELETE", GRANTS_ROUTE, Some(r#"{"user_id":"investor-7","target":"sa:work:*"}"#), true)).await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["ok"], true);

		let seen = seen.lock().unwrap();
		assert_eq!(seen.list_grants.as_deref(), Some("sa"));
		let grant = seen.grant_permission.as_ref().expect("the grant reached the directory");
		assert_eq!(grant.subject, Some(cc::grant_permission_request::Subject::Account("investor7@example.test".into())));
		assert_eq!(grant.target, "sa:operator");
		assert_eq!(grant.reason, "runs the calls");
		let revoke = seen.revoke_permission.as_ref().expect("the revocation reached the directory");
		assert_eq!(revoke.subject, Some(cc::revoke_permission_request::Subject::UserId("investor-7".into())));
		assert_eq!(revoke.target, "sa:work:*");
		assert_eq!(revoke.reason, "", "an untyped reason is forwarded empty, not refused");
		assert_eq!(seen.money_tokens_issued, 0, "tenant grants must never mint a money-plane token");
	}

	/// A malformed request is answered here, before the directory is asked: a namespace
	/// outside `[a-z0-9_]+`, a target outside the route's namespace, and a subject named
	/// twice or not at all.
	#[tokio::test]
	async fn a_malformed_grant_request_never_reaches_the_directory() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for namespace in ["s-a", "Sa", "a%3Ab"] {
			let uri = format!("/api/admin/tenants/{namespace}/grants");
			assert_eq!(send(&app, signed("GET", &uri, None, false)).await.0, StatusCode::BAD_REQUEST, "GET {uri}");
			let body = r#"{"email":"investor7@example.test","target":"sa:operator"}"#;
			assert_eq!(send(&app, signed("POST", &uri, Some(body), true)).await.0, StatusCode::BAD_REQUEST, "POST {uri}");
			assert_eq!(send(&app, signed("DELETE", &uri, Some(body), true)).await.0, StatusCode::BAD_REQUEST, "DELETE {uri}");
		}

		for body in [
			r#"{"email":"investor7@example.test","target":"bank:payment:open"}"#,
			r#"{"email":"investor7@example.test","target":"sab:operator"}"#,
			r#"{"email":"investor7@example.test","target":"operator"}"#,
			r#"{"email":"investor7@example.test"}"#,
			r#"{"email":"investor7@example.test","user_id":"investor-7","target":"sa:operator"}"#,
			r#"{"target":"sa:operator"}"#,
		] {
			for method in ["POST", "DELETE"] {
				let (status, _) = send(&app, signed(method, GRANTS_ROUTE, Some(body), true)).await;
				assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {body}");
			}
		}

		let seen = seen.lock().unwrap();
		assert!(
			seen.grant_permission.is_none() && seen.revoke_permission.is_none() && seen.list_grants.is_none(),
			"a malformed request must never reach the directory"
		);
	}

	/// The directory's verdict is the answer: its code maps to the BFF's usual status and
	/// its message — client-safe for these codes — reaches the console so it can say why.
	#[tokio::test]
	async fn the_directory_verdict_on_a_grant_surfaces_with_its_code() {
		let cases = [
			(Code::PermissionDenied, StatusCode::FORBIDDEN),
			(Code::NotFound, StatusCode::NOT_FOUND),
			(Code::FailedPrecondition, StatusCode::PRECONDITION_FAILED),
			(Code::InvalidArgument, StatusCode::BAD_REQUEST),
			(Code::ResourceExhausted, StatusCode::TOO_MANY_REQUESTS),
		];
		for (code, expected) in cases {
			let app = app(serve(Hub::failing("investor", code)).await);
			let body = r#"{"email":"investor7@example.test","target":"sa:operator"}"#;
			for (method, body) in [("GET", None), ("POST", Some(body)), ("DELETE", Some(body))] {
				let (status, response) = send(&app, signed(method, GRANTS_ROUTE, body, method != "GET")).await;
				assert_eq!(status, expected, "{method} under {code:?}");
				assert_eq!(response["error"], "upstream refused", "{method} under {code:?}");
			}
		}
	}

	/// `/api/users` carries the caller's permissions from GetMe, so the cabinet can offer
	/// the panel link without a second round trip.
	#[tokio::test]
	async fn the_profile_carries_the_callers_permissions() {
		let hub = Hub {
			permissions: vec!["sa:work:leads:read".into(), "bank:treasury:read".into()],
			..Hub::new("investor")
		};
		let app = app(serve(hub).await);
		let (status, body) = send(&app, signed("GET", "/api/users", None, false)).await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["permissions"], serde_json::json!(["sa:work:leads:read", "bank:treasury:read"]));
	}
}
