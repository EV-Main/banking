pub mod approval;
pub mod consilium;
pub mod cors;
pub mod identity;
pub mod intents;
pub mod money;
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
use evbanking_contracts::banking::v1 as bk;
use evconcierge_auth::Claims;
use serde_json::Value;
use subtle::ConstantTimeEq;
use tonic::Status;
use tower_http::{
	timeout::TimeoutLayer,
	trace::{DefaultOnFailure, TraceLayer},
};

use crate::{error::ApiError, routes::approval::AttemptLimiter, session::MoneyToken, state::AppState};

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
pub fn router(state: AppState) -> Router {
	requests(state).layer(TraceLayer::new_for_http().on_failure(DefaultOnFailure::new().level(tracing::Level::WARN))) // recurring ones escalate in devops alerting
}

/// Every request-shaped endpoint: served under the outer deadline.
fn requests(state: AppState) -> Router {
	Router::new()
		.route("/api/health", get(system::health))
		// The kubelet's probe target: no session, no CSRF, no upstream. `/api/health` is
		// the deep check and must NOT be a probe — see `system::live`.
		.route("/api/health/live", get(system::live))
		.route("/api/users", get(identity::get_me))
		// The reads a scope host shows; see `cors` for why nothing else is listed.
		.route("/api/wallet", cors::scope_readable(&state, get(money::get_wallet)))
		.route("/api/wallet/compartments", cors::scope_readable(&state, get(money::list_compartments)))
		.route("/api/wallet/compartment", cors::scope_readable(&state, get(money::get_compartment)))
		.route("/api/wallet/compartments/move", post(money::move_funds))
		.route("/api/wallet/deposit-address", get(money::deposit_address))
		.route("/api/wallet/withdrawals", get(money::list_withdrawals).post(money::request_withdrawal))
		.route("/api/wallet/withdrawals/cancel", post(money::cancel_withdrawal))
		.route("/api/wallet/deposits", get(money::list_deposits))
		.route("/api/wallet/invoices/{id}", get(money::get_payable_invoice))
		.route("/api/wallet/invoices/{id}/pay", post(money::pay_invoice))
		.route("/api/wallet/mandates", get(money::list_mandates).post(money::grant_mandate))
		.route("/api/wallet/mandate-requests/{id}", get(money::get_mandate_request))
		.route("/api/wallet/mandates/{id}/revoke", post(money::revoke_mandate))
		.route("/api/wallet/payments", get(money::list_payment_confirmations))
		.route("/api/wallet/payments/{id}", get(money::get_payment_confirmation))
		.route("/api/wallet/payments/{id}/answer", post(money::answer_payment_confirmation))
		.route("/api/operations", cors::scope_readable(&state, get(money::list_operations)))
		// Consilium — the platform's own money moving or changing hands, gated on a quorum
		// of owners. Money plane: the tally is computed and verified where the money is.
		.route("/api/consilium", get(consilium::list))
		.route("/api/consilium/holder-grant", post(consilium::open_holder_grant))
		.route("/api/consilium/{id}", get(consilium::get))
		.route("/api/consilium/{id}/cancel", post(consilium::cancel))
		// The public approval surface. NO session, NO CSRF, and no session cookie is even
		// read: the emailed token in the path is the whole credential. See `approval`.
		.route("/api/approval/payout/{token}", get(approval::payout_invitation).post(approval::payout_decision))
		.route("/api/approval/removal/{token}", get(approval::removal_invitation).post(approval::removal_decision))
		.route("/api/approval/consent/{token}", get(approval::consent_invitation).post(approval::consent_decision))
		// A trusted host's wallet actions: the one cross-origin surface that may move money,
		// mounted only when `WALLET_ACTION_ORIGINS` names a host. See `intents`.
		.merge(intents::routes(&state))
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
/// out the token BEFORE the RPC, and the verdict comes back through every money handler's
/// `map_err`, each of which would have to be taught to report it. The status of the
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
	money_token_for(state, &claims).await
}

async fn money_token_for(state: &AppState, claims: &Claims) -> Result<String, ApiError> {
	match state.banking.token_for(&claims.sub, &state.grpc).await {
		MoneyToken::Token(token) => Ok(token),
		MoneyToken::NotIssued => Err(ApiError::NotConfigured),
	}
}

/// Money mutations a user may start per [`MONEY_MUTATION_WINDOW`]. A person moving, paying
/// or withdrawing does a handful a minute; the bound is far above that and far below what a
/// script holding a stolen session could otherwise push at the hub.
const MONEY_MUTATIONS_PER_WINDOW: u32 = 20;
const MONEY_MUTATION_WINDOW: Duration = Duration::from_secs(60);

/// The per-user limiter [`money_gate`] consults.
pub fn money_mutation_limiter() -> AttemptLimiter {
	AttemptLimiter::new(MONEY_MUTATION_WINDOW, MONEY_MUTATIONS_PER_WINDOW)
}

/// The preamble of a route that moves the caller's money or consents to it being drawn:
/// first-party origin, fetch metadata and CSRF, then the verified identity, then the per-user
/// bound. It stops short of the banking token so a route with one more check on the verified
/// claims (see [`require_fresh_login`]) runs it before an exchange RPC is spent; the route
/// then calls `money_token_for`.
pub async fn money_gate(state: &AppState, jar: &CookieJar, headers: &HeaderMap) -> Result<Claims, ApiError> {
	first_party_request(state, jar, headers)?;
	let (_token, claims) = require_identity(state, jar).await?;
	if !state.money_mutations.allow(&claims.sub) {
		return Err(ApiError::Grpc(Status::resource_exhausted("too many money operations; try again shortly")));
	}
	Ok(claims)
}

/// How recent the user's last interactive sign-in must be for an act that consents to future
/// debits. Long enough to read the terms, short enough that a script riding a stale session
/// cannot grant on its own.
pub const STEP_UP_MAX_AGE_SECS: u64 = 300;

/// `Ok` when the claims carry an `auth_time` within [`STEP_UP_MAX_AGE_SECS`] of `now`, else
/// [`ApiError::StepUpRequired`]. A token with no `auth_time` (minted before the claim, or by a
/// session older than it) is "no signal", not "just signed in", so it is refused too.
pub fn require_fresh_login(claims: &Claims, now: u64) -> Result<(), ApiError> {
	match claims.auth_time {
		Some(at) if now.saturating_sub(at) <= STEP_UP_MAX_AGE_SECS => Ok(()),
		_ => Err(ApiError::StepUpRequired),
	}
}

/// The first-party gate of a mutating wallet route (`wallet.proto`, FIRST-PARTY ONLY): the
/// request must come from a page on evinvest.ltd itself, so a scope's own host — which may
/// read the wallet cross-origin — can only send the user there, never move their money. The
/// origin is `CABINET_WS_ORIGIN` (`https://evinvest.ltd` in production and required there).
/// A request without `Origin` is refused: every browser sends it on a POST.
///
/// An unset origin refuses everything. Production cannot boot without it, so this only
/// changes a box run with `APP_ENV` unset or wrong — and there, a gate that opens because a
/// variable is missing is the one way a cross-site page gets to move money.
pub fn first_party_origin(state: &AppState, headers: &HeaderMap) -> bool {
	state.config.cabinet_ws_origin.as_deref().is_some_and(|expected| ws::origin_is(headers, expected))
}

/// The audit block the hub records beside a money act. Every field is the BFF's own reading of
/// the request, never a body field; `stepup_method` is what this route demanded of the session
/// (`fresh_login`, `passkey`, `email_code` or `none`), not something the browser claims.
/// `intent_id` is empty here: only `intents::confirm` executes an intent, and it sets it.
pub fn audit_context(state: &AppState, headers: &HeaderMap, peer: Option<std::net::SocketAddr>, stepup_method: &str) -> bk::AuditContext {
	let clamp = |value: &str, max: usize| value.chars().take(max).collect::<String>();
	bk::AuditContext {
		origin: clamp(headers.get(axum::http::header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or_default(), 128),
		client_ip: trusted_client_ip(headers, peer, state.config.trusted_proxy_hops),
		user_agent: clamp(headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default(), 256),
		intent_id: String::new(),
		stepup_method: stepup_method.to_owned(),
	}
}

/// The browser's address as the trusted proxy chain saw it: the `hops`-th entry of
/// `X-Forwarded-For` counted from the right. Each trusted proxy appends the peer it accepted
/// from, so the entries left of those hops are caller-controlled and the leftmost one in
/// particular is whatever the client typed. Fewer entries than hops (a request that skipped a
/// proxy) falls back to the socket peer rather than to a forgeable value.
pub fn trusted_client_ip(headers: &HeaderMap, peer: Option<std::net::SocketAddr>, hops: u8) -> String {
	let forwarded: Vec<&str> = headers
		.get_all("x-forwarded-for")
		.iter()
		.filter_map(|v| v.to_str().ok())
		.flat_map(|v| v.split(','))
		.map(str::trim)
		.filter(|v| !v.is_empty())
		.collect();
	let from_chain = (hops as usize)
		.checked_sub(1)
		.filter(|_| hops > 0)
		.and_then(|back| forwarded.len().checked_sub(back + 1))
		.and_then(|at| forwarded.get(at))
		.filter(|ip| ip.parse::<std::net::IpAddr>().is_ok());
	match from_chain {
		Some(ip) => (*ip).to_owned(),
		None => peer.map(|addr| addr.ip().to_string()).unwrap_or_default(),
	}
}

/// Everything a first-party mutation must show before its session is even read: the page's
/// own origin, a browser that says the request is not cross-site, and the CSRF pair.
///
/// Not a replacement for [`first_party_origin`] (kept for the routes and tests that name it):
/// this is that check plus [`fetch_site_allowed`] plus [`verify_csrf`] in one place, so a
/// mutation cannot take two of the three. A refusal of any of them is the same `csrf` 403.
pub fn first_party_request(state: &AppState, jar: &CookieJar, headers: &HeaderMap) -> Result<(), ApiError> {
	if first_party_origin(state, headers) && fetch_site_allowed(headers) && verify_csrf(state, jar, headers) {
		Ok(())
	} else {
		Err(ApiError::Csrf)
	}
}

/// `Sec-Fetch-Site` as a second, browser-enforced witness next to `Origin`. A browser sets it
/// and a page cannot forge or strip it, so `cross-site` (or `none`, a navigation, which never
/// POSTs JSON) is refused even if some future proxy rewrote `Origin`.
///
/// A request without the header passes: it comes from a client that predates fetch metadata
/// or from a non-browser caller, and either way `Origin` and the CSRF pair are still demanded
/// by [`first_party_request`] — so a missing header cannot open anything those two keep shut.
pub fn fetch_site_allowed(headers: &HeaderMap) -> bool {
	match headers.get("sec-fetch-site").map(|v| v.to_str()) {
		None => true,
		Some(Ok("same-origin" | "same-site")) => true,
		Some(_) => false,
	}
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
/// [`parse_body`] for a first-party money route: a body that is there must be declared
/// `application/json` (parameters such as `charset` allowed), or the answer is 415.
///
/// A form post or a `text/plain` fetch is a "simple" cross-site request a browser sends with
/// cookies and no preflight; refusing every media type but JSON takes that class off the
/// table whatever `Origin` did. An EMPTY body is not judged — body-less POSTs (a revoke) send no
/// `Content-Type`, and with nothing to read there is nothing to smuggle.
pub fn parse_json_body(headers: &HeaderMap, body: &Bytes) -> Result<Value, ApiError> {
	if !body.is_empty() {
		let declared = headers
			.get(axum::http::header::CONTENT_TYPE)
			.and_then(|v| v.to_str().ok())
			.and_then(|v| v.split(';').next())
			.map(|v| v.trim().to_ascii_lowercase());
		if declared.as_deref() != Some("application/json") {
			return Err(ApiError::UnsupportedMediaType);
		}
	}
	Ok(parse_body(body))
}
/// A required string field: `None` when missing OR empty (matches the TS `!body?.field`).
pub fn required(v: &Value, key: &str) -> Option<String> {
	v.get(key).and_then(|x| x.as_str()).map(str::to_string).filter(|s| !s.is_empty())
}
/// Constant-time string equality (after a length check, which only reveals length) as
/// defense-in-depth, matching the constant-time discipline used for secret comparisons.
fn ct_str_eq(a: &str, b: &str) -> bool {
	a.len() == b.len() && a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn forwarded(value: &'static str) -> HeaderMap {
		let mut headers = HeaderMap::new();
		headers.insert("x-forwarded-for", axum::http::HeaderValue::from_static(value));
		headers
	}

	#[test]
	fn the_audit_address_is_the_one_the_trusted_hop_appended_never_the_leftmost() {
		let peer: std::net::SocketAddr = "10.0.0.9:443".parse().unwrap();
		// The browser typed 6.6.6.6; Traefik appended the address it really saw.
		assert_eq!(trusted_client_ip(&forwarded("6.6.6.6, 203.0.113.9"), Some(peer), 1), "203.0.113.9");
		// Two trusted hops: the client is one further left.
		assert_eq!(trusted_client_ip(&forwarded("6.6.6.6, 203.0.113.9, 10.1.1.1"), Some(peer), 2), "203.0.113.9");
		// Fewer entries than trusted hops: the request skipped a proxy, so fall back to the socket.
		assert_eq!(trusted_client_ip(&forwarded("203.0.113.9"), Some(peer), 2), "10.0.0.9");
		assert_eq!(trusted_client_ip(&HeaderMap::new(), Some(peer), 1), "10.0.0.9");
		assert_eq!(trusted_client_ip(&HeaderMap::new(), None, 1), "");
		// A non-address in the trusted slot is not echoed into the audit row.
		assert_eq!(trusted_client_ip(&forwarded("<script>"), Some(peer), 1), "10.0.0.9");
		// Zero trusted hops means the header is not trusted at all.
		assert_eq!(trusted_client_ip(&forwarded("203.0.113.9"), Some(peer), 0), "10.0.0.9");
	}

	#[test]
	fn ct_str_eq_matches_plain_equality() {
		assert!(ct_str_eq("a3f9c0d1-token", "a3f9c0d1-token"));
		assert!(!ct_str_eq("a3f9c0d1-token", "a3f9c0d1-toked"));
		assert!(!ct_str_eq("short", "longer-value"));
		assert!(ct_str_eq("", ""));
	}

	fn claims_at(auth_time: Option<u64>) -> Claims {
		serde_json::from_value(serde_json::json!({
			"sub": "user-1", "iss": "i", "aud": "a", "exp": 0, "iat": 0, "typ": "access", "token_version": 0, "auth_time": auth_time,
		}))
		.expect("claims parse")
	}

	/// The edges a `<` / `<=` slip would move: exactly the limit passes, one second past it does
	/// not, and a missing claim is never a pass. A future `auth_time` passes by design — concierge
	/// signs it, so only its own clock skew can produce one.
	#[test]
	fn fetch_metadata_admits_same_site_and_a_missing_header_only() {
		let with = |value: Option<&str>| {
			let mut headers = HeaderMap::new();
			if let Some(value) = value {
				headers.insert("sec-fetch-site", value.parse().expect("a header value"));
			}
			fetch_site_allowed(&headers)
		};
		assert!(with(None));
		assert!(with(Some("same-origin")));
		assert!(with(Some("same-site")));
		assert!(!with(Some("cross-site")));
		assert!(!with(Some("none")));
		assert!(!with(Some("")));
	}

	#[test]
	fn a_body_must_be_declared_json_but_an_empty_one_is_not_judged() {
		let with = |content_type: Option<&str>, body: &'static str| {
			let mut headers = HeaderMap::new();
			if let Some(value) = content_type {
				headers.insert(axum::http::header::CONTENT_TYPE, value.parse().expect("a header value"));
			}
			parse_json_body(&headers, &Bytes::from_static(body.as_bytes())).is_ok()
		};
		assert!(with(Some("application/json"), "{}"));
		assert!(with(Some("Application/JSON; charset=utf-8"), "{}"));
		assert!(!with(Some("text/plain"), "{}"));
		assert!(!with(Some("application/x-www-form-urlencoded"), "a=b"));
		assert!(!with(Some("application/jsonp"), "{}"));
		assert!(!with(None, "{}"));
		assert!(with(None, ""));
	}

	#[test]
	fn fresh_login_is_judged_at_the_five_minute_edge() {
		let now = 1_000_000;
		let judged = |at| require_fresh_login(&claims_at(at), now).is_ok();
		assert!(judged(Some(now)));
		assert!(judged(Some(now - STEP_UP_MAX_AGE_SECS)));
		assert!(!judged(Some(now - STEP_UP_MAX_AGE_SECS - 1)));
		assert!(!judged(None));
		assert!(judged(Some(now + 30)));
	}
}
