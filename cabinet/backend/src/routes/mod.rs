pub mod admin;
pub mod approval;
pub mod book;
pub mod book_ws;
pub mod consilium;
pub mod governance_ws;
pub mod grants;
pub mod identity;
pub mod money;
pub mod notifications;
pub mod payments;
pub mod platform;
pub mod system;
#[cfg(test)]
mod test_support;
pub mod ws;

use std::time::Duration;

use axum::{
	Router,
	body::Bytes,
	extract::{Request, State},
	http::{HeaderMap, StatusCode},
	middleware::{self, Next},
	response::Response,
	routing::{get, post},
};
use axum_extra::extract::cookie::CookieJar;
use concierge_iam::{Permission, PermissionSet};
use evconcierge_auth::Claims;
use serde_json::Value;
use subtle::ConstantTimeEq;
use tonic::Status;
use tower_http::{
	timeout::TimeoutLayer,
	trace::{DefaultOnFailure, TraceLayer},
};

use crate::{error::ApiError, session::MoneyToken, state::AppState};

/// Outer per-request deadline: a handler that is still awaiting an upstream plane past
/// this bound is aborted and the response becomes a 504, so a wedged plane can never hold
/// a browser connection (or, via the per-session refresh lock, sibling requests) open
/// indefinitely. Looser than the upstream per-RPC [`REQUEST_TIMEOUT`] so an upstream stall
/// normally surfaces as a gRPC error first; this is the backstop for everything else.
///
/// [`REQUEST_TIMEOUT`]: crate::state
const REQUEST_DEADLINE: Duration = Duration::from_secs(15);

/// Mount every BFF endpoint. Paths and methods mirror the old Next.js route handlers
/// 1:1 so the frontend's same-origin `/api/*` calls are unchanged.
///
/// The websockets are merged in AFTER [`REQUEST_DEADLINE`] is applied, so the layer
/// wraps every request-shaped route and none of the long-lived ones. A deadline is
/// exactly right for a request that must finish and exactly wrong for a socket that must
/// not: inside it, the live consilium page and the live book would be dropped every 15
/// seconds. (The upstream gRPC channel's own per-RPC timeout does not bound a
/// server-stream either — it covers the response future, which resolves when the headers
/// arrive, not the body.)
pub fn router(state: AppState) -> Router {
	let websockets = Router::new()
		.route("/api/owners/consilium/ws", get(governance_ws::upgrade))
		.route("/api/book/ws", get(book_ws::upgrade))
		.with_state(state.clone());

	requests(state)
		.merge(websockets)
		.layer(TraceLayer::new_for_http().on_failure(DefaultOnFailure::new().level(tracing::Level::WARN))) // recurring ones escalate in devops alerting
}

/// Every request-shaped endpoint: served under the outer deadline.
fn requests(state: AppState) -> Router {
	Router::new()
		.route("/api/health", get(system::health))
		// The kubelet's probe target: no session, no CSRF, no upstream. `/api/health` is
		// the deep check and must NOT be a probe — see `system::live`.
		.route("/api/health/live", get(system::live))
		.route("/api/mfe-registry", get(system::mfe_registry))
		.route("/api/platform", get(platform::status))
		.route("/api/users", get(identity::get_me).patch(identity::update_profile))
		// Notifications — self-service only: concierge resolves the subscriber from the
		// forwarded user token, so no route here names one.
		.route("/api/notifications", get(notifications::list))
		.route("/api/notifications/unread-count", get(notifications::unread_count))
		.route("/api/notifications/read", post(notifications::mark_read))
		.route("/api/notifications/settings", get(notifications::settings))
		.route("/api/notifications/settings/channel", post(notifications::set_channel))
		.route("/api/notifications/settings/topic", post(notifications::set_topic))
		.route("/api/wallet", get(money::get_wallet))
		.route("/api/wallet/deposit-address", get(money::deposit_address))
		.route("/api/wallet/withdrawals", get(money::list_withdrawals).post(money::request_withdrawal))
		.route("/api/wallet/withdrawals/cancel", post(money::cancel_withdrawal))
		.route("/api/wallet/deposits", get(money::list_deposits))
		.route("/api/wallet/invoices/{id}", get(money::get_payable_invoice))
		.route("/api/wallet/invoices/{id}/pay", post(money::pay_invoice))
		.route("/api/wallet/mandates", get(money::list_mandates).post(money::grant_mandate))
		.route("/api/wallet/mandates/{id}/revoke", post(money::revoke_mandate))
		.route("/api/allocations", get(money::list_allocations))
		.route("/api/allocations/detail", get(money::get_allocation))
		.route("/api/operations", get(money::list_operations))
		.route("/api/funds/nav", get(money::fund_nav))
		.route("/api/funds/nav/history", get(money::fund_nav_history))
		.route("/api/funds/positions", get(money::list_positions))
		.route("/api/funds/fee-policy", get(money::fee_policy))
		.route("/api/funds/accrued-fees", get(money::accrued_fees))
		.route("/api/funds/redemptions", get(money::list_redemptions))
		.route("/api/funds/redemptions/cancel", post(money::cancel_redemption))
		.route("/api/funds/subscribe", post(money::subscribe))
		.route("/api/funds/redeem", post(money::redeem))
		// The book — holders trading an allocation's units with each other. Money plane;
		// the live feed is `/api/book/ws`, mounted with the other socket below.
		.route("/api/book", get(book::get_book))
		.route("/api/book/trades", get(book::list_trades))
		.route("/api/book/candles", get(book::list_candles))
		.route("/api/book/policy", get(book::get_policy))
		.route("/api/book/orders", get(book::list_open_orders).post(book::place_order))
		.route("/api/book/orders/cancel", post(book::cancel_order))
		.route("/api/book/orders/history", get(book::list_order_history))
		.route("/api/book/fills", get(book::list_fills))
		// Admin console — gated at the BFF on a named permission per route AND re-checked
		// per-permission by the owning plane (defense in depth). Identity/platform routes hit concierge; money/treasury routes
		// hit the piggybank money plane.
		.route("/api/admin/users", get(admin::list_users))
		.route("/api/admin/users/detail", get(admin::get_user))
		.route("/api/admin/users/role", post(admin::set_role))
		// `/hold` and not `/suspend`: the plane split the verb, and the path says which half
		// this is. Permanent suspension is a proposal, under `/api/owners/proposals`.
		.route("/api/admin/users/hold", post(admin::hold_user))
		.route("/api/admin/users/reinstate", post(admin::reinstate_user))
		.route("/api/admin/users/revoke", post(admin::revoke_sessions))
		.route("/api/admin/users/kyc", post(admin::set_kyc))
		.route("/api/admin/users/balance", get(admin::user_balance))
		.route("/api/admin/treasury", get(admin::treasury))
		.route("/api/admin/treasury/record-deposit", post(admin::record_treasury_deposit))
		.route("/api/admin/treasury/seed-capital", post(admin::seed_capital))
		.route("/api/admin/allocations", get(admin::list_allocations))
		.route("/api/admin/allocations/register", post(admin::register_allocation))
		.route("/api/admin/allocations/update", post(admin::update_allocation))
		.route("/api/admin/allocations/state", post(admin::set_allocation_state))
		.route("/api/admin/allocations/cap", post(admin::set_allocation_unit_cap))
		.route("/api/admin/allocations/access", post(admin::set_allocation_access))
		.route("/api/admin/allocations/grants", get(admin::list_allocation_access_grants))
		.route("/api/admin/allocations/grants/grant", post(admin::grant_allocation_access))
		.route("/api/admin/allocations/grants/revoke", post(admin::revoke_allocation_access))
		.route("/api/admin/allocations/issue", post(admin::issue_units))
		.route("/api/admin/allocations/retire", post(admin::retire_units))
		.route("/api/admin/allocations/backing", post(admin::set_allocation_backing))
		.route("/api/admin/allocations/holders", get(admin::list_unit_holders))
		.route("/api/admin/allocations/book", post(admin::set_book_policy))
		// Gated on the session alone: a tenant's delegate usually holds no global role, so
		// concierge — not a coarse role gate here — decides.
		.route("/api/admin/tenants/{namespace}/grants", get(grants::list).post(grants::grant).delete(grants::revoke))
		.route("/api/admin/fees/policies", get(admin::list_fee_policies))
		.route("/api/admin/fees/policy", post(admin::schedule_fee_policy))
		.route("/api/admin/fees/policy/cancel", post(admin::cancel_fee_policy_change))
		.route("/api/admin/fees/policy/acknowledge-notices", post(admin::acknowledge_undelivered_notices))
		.route("/api/admin/fees/changes", get(admin::list_fee_policy_changes))
		.route("/api/admin/fees/shares", get(admin::fee_shares))
		.route("/api/admin/fees/settle", post(admin::settle_fee_shares))
		.route("/api/admin/fees/assessments", get(admin::fund_fee_assessments))
		.route("/api/admin/valuation/queue", get(admin::redemption_queue))
		.route("/api/admin/valuation/post", post(admin::post_valuation))
		.route("/api/admin/valuation/override", post(admin::propose_valuation_override))
		.route("/api/admin/valuation/settle", post(admin::settle_redemption))
		.route("/api/admin/valuation/fail", post(admin::fail_redemption))
		.route("/api/admin/withdrawals/queue", get(admin::withdrawal_queue))
		.route("/api/admin/withdrawals/dispatch", post(admin::dispatch_withdrawal))
		.route("/api/admin/withdrawals/settle", post(admin::settle_withdrawal))
		.route("/api/admin/withdrawals/fail", post(admin::fail_withdrawal))
		.route("/api/admin/revenue", get(admin::fund_revenue))
		.route("/api/admin/outbox/parked", get(admin::parked_events))
		.route("/api/admin/outbox/unpark", post(admin::unpark_event))
		.route("/api/admin/cabinet", get(admin::cabinet_config))
		.route("/api/admin/cabinet/maintenance", post(admin::set_maintenance))
		.route("/api/admin/cabinet/read-only", post(admin::set_read_only))
		.route("/api/admin/rails", get(admin::rails))
		.route("/api/admin/rails/frozen", post(admin::set_rail_frozen))
		.route("/api/admin/cabinet/announcement", post(admin::set_announcement))
		.route("/api/admin/cabinet/flag", post(admin::set_flag))
		// Payments — an order between two named ends. Money plane, Admin|Owner; the plane
		// seats the one approval it needs (owner consilium or subject consent) on open.
		.route("/api/admin/payments", get(payments::list).post(payments::open))
		.route("/api/admin/payments/{id}", get(payments::get))
		.route("/api/admin/payments/{id}/cancel", post(payments::cancel))
		// Consilium — the platform's own money moving or changing hands, gated on a quorum
		// of owners. Money plane: the tally is computed and verified where the money is.
		.route("/api/consilium", get(consilium::list))
		.route("/api/consilium/holder-grant", post(consilium::open_holder_grant))
		.route("/api/consilium/{id}", get(consilium::get))
		.route("/api/consilium/{id}/cancel", post(consilium::cancel))
		// Ownership — seats, and the two consilia that move them. Concierge plane:
		// `Role::Owner` is its fact. Admission is the only way a seat is GRANTED.
		.route("/api/owners", get(consilium::owners))
		.route("/api/owners/resign", post(consilium::resign))
		.route("/api/owners/removals", get(consilium::list_removals).post(consilium::open_removal))
		.route("/api/owners/removals/{id}/vote", post(consilium::vote_removal))
		.route("/api/owners/removals/{id}/cancel", post(consilium::cancel_removal))
		.route("/api/owners/admissions", get(consilium::list_admissions).post(consilium::open_admission))
		// User proposals — the owners' verdict over one PERSON's standing: the permanent
		// half of the split blocking verb, its undo, and the admin seat. Listed, voted and
		// withdrawn together, because a hold lapses in 24h and a family you can open but
		// not ratify is a dead end.
		.route("/api/owners/proposals", get(consilium::list_proposals))
		.route("/api/owners/proposals/suspension", post(consilium::open_suspension))
		.route("/api/owners/proposals/reinstatement", post(consilium::open_reinstatement))
		.route("/api/owners/proposals/admin-admission", post(consilium::open_admin_admission))
		.route("/api/owners/proposals/{id}/vote", post(consilium::vote_proposal))
		.route("/api/owners/proposals/{id}/cancel", post(consilium::cancel_proposal))
		.route("/api/owners/admissions/{id}/vote", post(consilium::vote_admission))
		.route("/api/owners/admissions/{id}/cancel", post(consilium::cancel_admission))
		// The public approval surface. NO session, NO CSRF, and no session cookie is even
		// read: the emailed token in the path is the whole credential. See `approval`.
		.route("/api/approval/payout/{token}", get(approval::payout_invitation).post(approval::payout_decision))
		.route("/api/approval/removal/{token}", get(approval::removal_invitation).post(approval::removal_decision))
		.route("/api/approval/consent/{token}", get(approval::consent_invitation).post(approval::consent_decision))
		.layer(middleware::from_fn_with_state(state.clone(), evict_banking_pair_on_unauthorized))
		.with_state(state)
		.layer(TimeoutLayer::with_status_code(StatusCode::GATEWAY_TIMEOUT, REQUEST_DEADLINE))
}

/// Drop the caller's cached banking pair whenever a request ends in a 401.
///
/// The money plane holds a `token_version` floor that "log out of all devices" raises,
/// and refuses a banking token minted below it with `UNAUTHENTICATED`. [`BankingTokens`]
/// caches that token for its whole TTL, so without this the frontend's one heal-replay
/// would present the very same token, be refused again, and the cabinet would read
/// "could not confirm your session" for up to fifteen minutes after a re-login. Evicting
/// on the 401 itself is what makes the replay re-mint against the current floor.
///
/// One layer here rather than a check inside [`require_money_token`]: that helper hands
/// out the token BEFORE the RPC, and the verdict comes back through sixty-odd handlers'
/// `map_err`s, each of which would have to be taught to report it. The status of the
/// finished response is the one place every verdict passes through.
///
/// The identity is re-verified from the cookie rather than trusted from the request, and
/// only on a 401 — a JWKS-cached local check, so no round trip. When that verification
/// is itself what failed (no cookie, a bad signature) there is no subject to evict and
/// nothing happens; when it passes but the handler still said 401, the money plane did,
/// and the pair goes. A 401 the handler raised for another reason evicts a pair that a
/// later request re-mints at the cost of one exchange RPC — cheap, and never wrong.
///
/// [`BankingTokens`]: crate::session::BankingTokens
async fn evict_banking_pair_on_unauthorized(State(state): State<AppState>, jar: CookieJar, request: Request, next: Next) -> Response {
	let response = next.run(request).await;
	if response.status() == StatusCode::UNAUTHORIZED
		&& let Ok((_token, claims)) = require_identity(&state, &jar).await
	{
		state.banking.evict(&claims.sub).await;
	}
	response
}

/// The verified concierge identity for a request: the shared `ev_access` JWT cookie
/// (set by the shell-owned auth surface) checked locally against the concierge JWKS.
/// Auth is shell-owned — the BFF runs no OAuth and holds no session; this cookie IS
/// the request's credential. Returns the raw token (for forwarding as a bearer) plus
/// its verified claims.
pub async fn require_identity(state: &AppState, jar: &CookieJar) -> Result<(String, Claims), ApiError> {
	let token = jar.get(&state.cookies.access).map(|c| c.value().to_string()).ok_or(ApiError::Unauthenticated)?;
	let claims = state.verifier.verify(&token).await.map_err(|_| ApiError::Unauthenticated)?;
	Ok((token, claims))
}

/// The verified **concierge** identity-plane access token for an authenticated request,
/// or `Unauthenticated`. Money RPCs must NOT use this — see [`require_money_token`].
pub async fn require_token(state: &AppState, jar: &CookieJar) -> Result<String, ApiError> {
	Ok(require_identity(state, jar).await?.0)
}

/// The fresh **banking** (`aud=banking-core`) access token for a money-plane RPC. The two
/// planes are cryptographically separated, so the BFF forwards the banking token here and
/// the concierge token to identity — never one plane's token to the other. The banking pair
/// is minted via the concierge→banking exchange seam (`IssueUserToken`) for the VERIFIED
/// JWT subject — the signature check above is what stops a forged cookie from minting
/// money-plane tokens for an arbitrary id. When none can be obtained (issuance
/// unconfigured, or the bridge hasn't mirrored the user yet) this surfaces
/// `NotConfigured` (503) rather than forwarding the wrong-plane token, which the money
/// verifier would reject on issuer/audience.
pub async fn require_money_token(state: &AppState, jar: &CookieJar) -> Result<String, ApiError> {
	let (_token, claims) = require_identity(state, jar).await?;
	match state.banking.token_for(&claims.sub, &state.grpc).await {
		MoneyToken::Token(token) => Ok(token),
		MoneyToken::NotIssued => Err(ApiError::NotConfigured),
	}
}

/// The console gate: the verified caller must hold `permission` among what concierge's
/// first-party `GetMe` resolves for their seat. Defense in depth — the owning plane re-checks
/// its own act; refusing here keeps an investor out before any privileged call, and answers
/// a screen whose plane would refuse everything at the BFF, where it can act on it. Read per
/// request: the JWT carries no permissions.
pub async fn require_permission(state: &AppState, jar: &CookieJar, permission: impl Permission) -> Result<(), ApiError> {
	let (token, _claims) = require_identity(state, jar).await?;
	let me = state.grpc.get_me(&token).await.map_err(|_| ApiError::Unauthenticated)?;
	if !me.permissions.into_iter().collect::<PermissionSet>().may(permission) {
		return Err(ApiError::Grpc(Status::permission_denied(format!("requires {}", permission.as_str()))));
	}
	Ok(())
}

/// The first-party gate of a mutating wallet route (`wallet.proto`, FIRST-PARTY ONLY): the
/// request must come from a page on evinvest.ltd itself, so a scope's own host — which may
/// read the wallet cross-origin — can only send the user there, never move their money. The
/// origin is the one the websockets admit (`CABINET_WS_ORIGIN`, `https://evinvest.ltd` in
/// production and required there); unset, as in local development, the gate is off. A
/// request without `Origin` is refused: every browser sends it on a POST.
pub fn first_party_origin(state: &AppState, headers: &HeaderMap) -> bool {
	ws::origin_allowed(state, headers)
}

/// CSRF double-submit: the `x-ev-csrf` header must equal the readable `ev_csrf` cookie.
pub fn verify_csrf(state: &AppState, jar: &CookieJar, headers: &HeaderMap) -> bool {
	let cookie = jar.get(&state.cookies.csrf).map(|c| c.value().to_string());
	let header = headers.get("x-ev-csrf").and_then(|v| v.to_str().ok());
	matches!((cookie.as_deref(), header), (Some(c), Some(h)) if !c.is_empty() && ct_str_eq(c, h))
}

/// Parse a request body leniently (a malformed/empty body becomes `{}`), matching the
/// old BFF's `req.json().catch(() => ({}))`.
pub fn parse_body(body: &Bytes) -> Value {
	serde_json::from_slice(body).unwrap_or_else(|_| Value::Object(Default::default()))
}
/// A required string field: `None` when missing OR empty (matches the TS `!body?.field`).
pub fn required(v: &Value, key: &str) -> Option<String> {
	v.get(key).and_then(|x| x.as_str()).map(str::to_string).filter(|s| !s.is_empty())
}
/// A REQUIRED whole-number field that fits a `u32`: `None` when the field is missing, or
/// is not a non-negative integer inside the `u32` range.
///
/// The numeric counterpart to [`required`], and deliberately not a defaulting read.
/// `serde_json`'s `as_u64` answers `None` for `-1`, for `2.5` and for a quoted `"2"`
/// alike, so a caller that unwraps it to zero cannot tell "the operator asked for zero"
/// from "this layer could not read what the operator asked for" — and zero is a
/// meaningful value on every ladder that crosses the BFF, so inventing one answers
/// "saved" to a request nobody understood. `u32::try_from` closes the other half: `as u32`
/// truncates silently, which turns 2^32 into a perfectly plausible 0.
pub fn required_u32(v: &Value, key: &str) -> Option<u32> {
	v.get(key).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())
}
/// An editable string field: missing ⇒ `""` (full-replace semantics; empty clears).
pub fn editable(v: &Value, key: &str) -> String {
	v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
/// Constant-time string equality (after a length check, which only reveals length) as
/// defense-in-depth, matching the constant-time discipline used for secret comparisons.
fn ct_str_eq(a: &str, b: &str) -> bool {
	a.len() == b.len() && a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ct_str_eq_matches_plain_equality() {
		assert!(ct_str_eq("a3f9c0d1-token", "a3f9c0d1-token"));
		assert!(!ct_str_eq("a3f9c0d1-token", "a3f9c0d1-toked"));
		assert!(!ct_str_eq("short", "longer-value"));
		assert!(ct_str_eq("", ""));
	}
}
