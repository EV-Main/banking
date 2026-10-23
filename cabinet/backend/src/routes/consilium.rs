//! The signed-in consilium surface: `/api/consilium/**`, on the money plane.
//!
//! Moving or sharing the platform's own money — a payment out of an allocation's claim, a
//! holder seated in `fee` or `fund`, a seed of `fund` — is authorized in the MONEY plane,
//! against the owner roster it already mirrors, because `docs/ARCHITECTURE.md` refuses to
//! let a concierge-signed artifact move money — and a consilium verdict is exactly such an
//! artifact. So these handlers forward the banking token and never the concierge one.

use axum::{
	Json,
	body::Bytes,
	extract::{Path, Query, State},
	http::HeaderMap,
};
use axum_extra::extract::cookie::CookieJar;
use evbanking_contracts::banking::v1 as bk;
use serde::Deserialize;

use crate::{
	dto,
	error::ApiError,
	routes::{parse_body, require_money_token, required, verify_csrf},
	state::AppState,
};

#[derive(Deserialize)]
pub struct LimitQuery {
	limit: Option<u32>,
}

// ── money plane: the owners' consilium ───────────────────────────────────────

/// `GET /api/consilium` — the governance history, newest first. An absent `limit` falls
/// through to the plane's default page.
pub async fn list(State(st): State<AppState>, jar: CookieJar, Query(q): Query<LimitQuery>) -> Result<Json<dto::ConsiliumList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.list_consilia(&token, q.limit.unwrap_or(0)).await.map_err(|s| ApiError::read(s, "consilia unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/consilium/{id}` — one consilium in full, including per-voter state.
pub async fn get(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>) -> Result<Json<dto::Consilium>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let consilium = st.grpc.get_consilium(&token, &id).await.map_err(|s| ApiError::read(s, "consilium unavailable"))?;
	Ok(Json(consilium.into()))
}

/// `POST /api/consilium/holder-grant` — CSRF-checked: put a new holder of a reserved
/// allocation to the owners (#245). Body `{allocation, user_id, units}`: `allocation` is
/// `fee` or `fund`, `user_id` the person's id (resolved hub-side like every
/// admin target), `units` a positive decimal. The plane refuses a roster too small to
/// reach quorum, a person who is not an active user, and a proposer who holds no owner
/// seat; the terms are immutable once open, so a correction means cancel and reopen.
pub async fn open_holder_grant(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Consilium>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(allocation), Some(user_id), Some(units)) = (required(&v, "allocation"), required(&v, "user_id"), required(&v, "units")) else {
		return Err(ApiError::BadRequest("allocation, user_id and units are required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	let terms = bk::HolderGrantTerms { allocation, user_id, units };
	Ok(Json(st.grpc.open_holder_grant(&token, terms).await?.into()))
}

/// `POST /api/consilium/{id}/cancel` — CSRF-checked: the initiator withdraws their own
/// still-open invoice.
pub async fn cancel(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<dto::Consilium>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	Ok(Json(st.grpc.cancel_consilium(&token, &id).await?.into()))
}

#[cfg(test)]
mod route_tests {
	use std::{collections::HashMap, sync::Arc};

	use axum::{
		Router,
		body::Body,
		http::{Request, StatusCode, header},
	};
	use evconcierge_auth::{TokenType, Verifier, VerifierConfig};
	use tower::ServiceExt;

	use crate::{
		config::AppConfig,
		cookies::CookieNames,
		routes::{approval::AttemptLimiter, router},
		session::BankingTokens,
		state::{AppState, Grpc},
	};

	/// A port nothing listens on: every one of these tests asserts on a decision the BFF
	/// makes BEFORE it reaches a plane, so the plane only has to be unreachable — and an
	/// unreachable one is exactly what proves a request got past the gates.
	const BLACK_HOLE: &str = "http://127.0.0.1:1";

	fn app() -> Router {
		let env = HashMap::from([
			("PIGGYBANK_GRPC_ADDR", BLACK_HOLE),
			("BANKING_AUTH_GRPC_ADDR", BLACK_HOLE),
			("CONCIERGE_GRPC_ADDR", BLACK_HOLE),
			("BANKING_ISSUANCE_TOKEN", "test-issuance"),
			("AUTH_ISSUER", "https://auth.test"),
			("AUTH_CLIENT_AUDIENCE", "concierge"),
			("MFE_REGISTRY_PATH", "/mfe-registry.json"),
			("APP_ENV", "development"),
		]);
		let config = AppConfig::from_source(|var| env.get(var).map(|value| (*value).to_string())).expect("the test env loads");
		let verifier = Verifier::try_new(VerifierConfig {
			issuer: config.auth_issuer.clone(),
			audiences: vec![config.auth_client_audience.clone()],
			allowed_types: vec![TokenType::Access],
			jwks_grpc_endpoint: BLACK_HOLE.into(),
		})
		.expect("build the verifier");

		router(AppState {
			cookies: Arc::new(CookieNames::new(config.cookie_secure())),
			banking: Arc::new(BankingTokens::new()),
			approvals: Arc::new(AttemptLimiter::default()),
			money_mutations: Arc::new(crate::routes::money_mutation_limiter()),
			wallet_actions: None,
			verifier,
			grpc: Grpc::connect_lazy(BLACK_HOLE, BLACK_HOLE, BLACK_HOLE, Some("test-issuance".into())).expect("build the lazy channels"),
			config: Arc::new(config),
		})
	}

	async fn send(method: &str, uri: &str, body: Option<&str>) -> (StatusCode, HashMap<String, String>) {
		let request = Request::builder().method(method).uri(uri);
		let request = match body {
			Some(json) => request.header(header::CONTENT_TYPE, "application/json").body(Body::from(json.to_owned())),
			None => request.body(Body::empty()),
		}
		.expect("build the request");
		let response = app().oneshot(request).await.expect("the router responds");
		let headers = response
			.headers()
			.iter()
			.map(|(name, value)| (name.as_str().to_string(), value.to_str().unwrap_or_default().to_string()))
			.collect();
		(response.status(), headers)
	}

	/// The consilium surface is owner-only, so an anonymous caller is refused before any
	/// plane is touched — and in particular before a money-plane token could be minted.
	#[tokio::test]
	async fn the_signed_in_surface_refuses_an_anonymous_caller() {
		let reads = [("GET", "/api/consilium"), ("GET", "/api/consilium/c-1")];
		for (method, uri) in reads {
			let (status, _) = send(method, uri, None).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
		}
	}

	/// Every mutation on the signed-in surface goes through the double-submit check, and
	/// it runs FIRST — a request without the header is refused as CSRF, not as a missing
	/// session, which is what proves the check is not somewhere further down the handler.
	#[tokio::test]
	async fn every_signed_in_mutation_is_csrf_gated() {
		let mutations = [
			("/api/consilium/holder-grant", r#"{"allocation":"fee","user_id":"u-1","units":"100"}"#),
			("/api/consilium/c-1/cancel", "{}"),
		];
		for (uri, body) in mutations {
			let (status, _) = send("POST", uri, Some(body)).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "POST {uri} must be refused as CSRF");
		}
	}

	/// The emailed surface is the opposite: no session, no CSRF, and it must still reach
	/// the plane. With the plane unreachable that shows up as a transport failure — which
	/// only a request that passed every gate could possibly have produced.
	#[tokio::test]
	async fn the_emailed_surface_needs_no_session_and_no_csrf() {
		let calls = [
			("GET", "/api/approval/payout/tok-1", None),
			("POST", "/api/approval/payout/tok-1", Some(r#"{"code":"ABCDE12345","decision":"approve"}"#)),
			("GET", "/api/approval/consent/tok-1", None),
			("POST", "/api/approval/consent/tok-1", Some(r#"{"code":"ABCDE12345","decision":"approve"}"#)),
		];
		for (method, uri, body) in calls {
			let (status, headers) = send(method, uri, body).await;
			assert_ne!(status, StatusCode::UNAUTHORIZED, "{method} {uri} must not ask for a session");
			assert_ne!(status, StatusCode::FORBIDDEN, "{method} {uri} must not ask for a CSRF token");
			assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method} {uri} reached the plane and found it down");
			assert_eq!(headers.get("referrer-policy").map(String::as_str), Some("no-referrer"));
			assert_eq!(headers.get("cache-control").map(String::as_str), Some("no-store"));
		}
	}

	/// A vote with no code cannot be spent as one: the malformed body is refused here,
	/// before it can burn one of the token's five attempts upstream.
	#[tokio::test]
	async fn a_vote_without_a_code_never_reaches_the_plane() {
		for uri in ["/api/approval/payout/tok-1", "/api/approval/consent/tok-1"] {
			let (status, _) = send("POST", uri, Some(r#"{"decision":"approve"}"#)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "POST {uri}");
		}
	}
}
