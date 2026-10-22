//! The signed-in governance surface: `/api/consilium/**` (money) and `/api/owners/**`
//! (ownership).
//!
//! Two planes, two tokens, and the split is the point. Moving or sharing the platform's
//! own money — a payment out of an allocation's claim, a holder seated in `fee` or `fund`,
//! a seed of `fund` — is authorized in the MONEY plane, against the owner roster it already
//! mirrors, because `docs/ARCHITECTURE.md` refuses to let a concierge-signed artifact move
//! money — and a consilium verdict is exactly such an artifact. Moving a seat — granting
//! one or taking one away — is authorized in the OWNERSHIP plane, because `Role::Owner`
//! is a concierge-owned fact and the bridge between the planes is one-way. So the money
//! handlers forward the banking token and never the concierge one; the ownership handlers
//! forward the concierge token and never the banking one. Neither plane trusts the other's
//! verdict.

use axum::{
	Json,
	body::Bytes,
	extract::{Path, Query, State},
	http::HeaderMap,
};
use axum_extra::extract::cookie::CookieJar;
use evbanking_contracts::banking::v1 as bk;
use evconcierge_contracts::concierge::v1 as cc;
use serde::Deserialize;

use crate::{
	dto,
	error::ApiError,
	governance::{AdmissionVote, ProposalKind, ProposalVote, RemovalVote},
	routes::{parse_body, require_money_token, require_token, required, verify_csrf},
	state::AppState,
};

#[derive(Deserialize)]
pub struct LimitQuery {
	limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct ProposalQuery {
	limit: Option<u32>,
	/// `suspension` / `reinstatement` / `admin_admission`; absent lists every kind.
	kind: Option<String>,
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
/// `fee` or `fund`, `user_id` the id the console carries (resolved hub-side like every
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

// ── ownership plane: the roster, its removals and its admissions ─────────────

/// `GET /api/owners` — the current roster, with the below-three-owners warning that says
/// the fund can no longer authorize a payout.
pub async fn owners(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::OwnerList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let owners = st.grpc.list_owners(&token).await.map_err(|s| ApiError::read(s, "owners unavailable"))?;
	Ok(Json(owners.into()))
}

/// `POST /api/owners/resign` — CSRF-checked: give up your own seat. No consilium, but the
/// floor still applies, and `confirm_email` must be the caller's own address — resigning
/// cannot be a stray click.
pub async fn resign(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::OwnerList>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let Some(confirm_email) = required(&parse_body(&body), "confirm_email") else {
		return Err(ApiError::BadRequest("confirm_email is required".into()));
	};
	Ok(Json(st.grpc.resign_ownership(&token, &confirm_email).await?.into()))
}

/// `GET /api/owners/removals` — every proposal, open and closed. Nothing is deleted: a
/// rejected, expired or void removal stays readable.
pub async fn list_removals(State(st): State<AppState>, jar: CookieJar, Query(q): Query<LimitQuery>) -> Result<Json<dto::OwnerRemovalList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let list = st
		.grpc
		.list_owner_removals(&token, q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "removals unavailable"))?;
	Ok(Json(list.into()))
}

/// `POST /api/owners/removals` — CSRF-checked: propose removing an owner. The reason is
/// required here as well as upstream: it is shown to the target and to every peer, and a
/// seat taken away without a stated reason is not a record anyone can audit later.
pub async fn open_removal(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::OwnerRemoval>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(target_user_id), Some(reason)) = (required(&v, "target_user_id"), required(&v, "reason")) else {
		return Err(ApiError::BadRequest("target_user_id and reason are required".into()));
	};
	Ok(Json(st.grpc.open_owner_removal(&token, &target_user_id, &reason).await?.into()))
}

/// `POST /api/owners/removals/{id}/vote` — CSRF-checked: a peer's live vote, cast in the
/// cabinet rather than from a mailbox. The plane refuses the target and the initiator.
pub async fn vote_removal(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> Result<Json<dto::OwnerRemoval>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let Some(vote) = required(&parse_body(&body), "vote").as_deref().and_then(RemovalVote::parse) else {
		return Err(ApiError::BadRequest("vote must be \"remove\" or \"keep\"".into()));
	};
	Ok(Json(st.grpc.submit_peer_vote(&token, &id, vote.wire()).await?.into()))
}

/// `POST /api/owners/removals/{id}/cancel` — CSRF-checked: withdraw a proposal the caller
/// opened, while it is still open.
pub async fn cancel_removal(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<dto::OwnerRemoval>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	Ok(Json(st.grpc.cancel_owner_removal(&token, &id).await?.into()))
}

/// `GET /api/owners/admissions` — every admission, open and closed.
pub async fn list_admissions(State(st): State<AppState>, jar: CookieJar, Query(q): Query<LimitQuery>) -> Result<Json<dto::OwnerAdmissionList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let list = st
		.grpc
		.list_owner_admissions(&token, q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "admissions unavailable"))?;
	Ok(Json(list.into()))
}

/// `POST /api/owners/admissions` — CSRF-checked: propose granting a seat. This is the ONLY
/// way `Role::Owner` is granted — the directory's `SetRole` refuses it outside an executed
/// admission — and it passes only on unanimity of every other owner, so a minority can
/// never grow itself into the majority a payout needs.
pub async fn open_admission(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::OwnerAdmission>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(candidate_user_id), Some(reason)) = (required(&v, "candidate_user_id"), required(&v, "reason")) else {
		return Err(ApiError::BadRequest("candidate_user_id and reason are required".into()));
	};
	Ok(Json(st.grpc.open_owner_admission(&token, &candidate_user_id, &reason).await?.into()))
}

/// `POST /api/owners/admissions/{id}/vote` — CSRF-checked: a peer's vote on an admission.
/// Its own vocabulary, not the removal one: "admit"/"reject" is a different question from
/// "remove"/"keep", and a page that submitted the wrong verb here would seat someone.
pub async fn vote_admission(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> Result<Json<dto::OwnerAdmission>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let Some(vote) = required(&parse_body(&body), "vote").as_deref().and_then(AdmissionVote::parse) else {
		return Err(ApiError::BadRequest("vote must be \"admit\" or \"reject\"".into()));
	};
	Ok(Json(st.grpc.submit_admission_vote(&token, &id, vote.wire()).await?.into()))
}

/// `POST /api/owners/admissions/{id}/cancel` — CSRF-checked: withdraw an admission the
/// caller opened, while it is still open.
pub async fn cancel_admission(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<dto::OwnerAdmission>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	Ok(Json(st.grpc.cancel_owner_admission(&token, &id).await?.into()))
}

// ── ownership plane: the owners' verdict over one PERSON's standing ──────────
//
// The other half of the split blocking verb, plus the admin seat. `UserDirectory.HoldUser`
// freezes an account NOW and lapses in 24h; these are what make it stay, lift it again
// when the owners themselves imposed it, and grant `Role::Admin` — all three refused to
// one actor acting alone. Three kinds share one route family because they share one
// message: the kind is a field, not a path.
//
// Without the vote route below the whole family is a dead end — a hold would lapse after
// 24 hours with nobody able to ratify it — so the listing, the vote and the withdrawal
// ship together.

/// `GET /api/owners/proposals` — every user proposal, open and closed. `kind` narrows the
/// listing; an absent one lists all three.
pub async fn list_proposals(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ProposalQuery>) -> Result<Json<dto::UserProposalList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	// An unrecognised word is refused rather than widened to "every kind": a filter that
	// silently stops filtering shows the reader rows they did not ask for.
	let kind = match q.kind.as_deref() {
		None => cc::UserProposalKind::Unspecified,
		Some(raw) => match ProposalKind::parse(raw) {
			Some(kind) => kind.wire(),
			None => return Err(ApiError::BadRequest("kind must be \"suspension\", \"reinstatement\" or \"admin_admission\"".into())),
		},
	};
	let list = st
		.grpc
		.list_user_proposals(&token, q.limit.unwrap_or(0), kind)
		.await
		.map_err(|s| ApiError::read(s, "proposals unavailable"))?;
	Ok(Json(list.into()))
}

/// `POST /api/owners/proposals/suspension` — CSRF-checked: propose PERMANENTLY suspending
/// a user. The reason is required here as well as upstream: it is what the other owners
/// are voting on, and a decision to freeze somebody's account with no stated cause cannot
/// be reviewed afterwards.
pub async fn open_suspension(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::UserProposal>, ApiError> {
	let (token, user_id, reason) = proposal_open(&st, &jar, &headers, &body).await?;
	Ok(Json(st.grpc.open_user_suspension(&token, &user_id, &reason).await?.into()))
}

/// `POST /api/owners/proposals/reinstatement` — CSRF-checked: propose lifting a suspension
/// THE OWNERS imposed. A hold needs nothing from here — `/api/admin/users/reinstate` lifts
/// one in a single act, and the console decides which to offer from `suspended_by`.
pub async fn open_reinstatement(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::UserProposal>, ApiError> {
	let (token, user_id, reason) = proposal_open(&st, &jar, &headers, &body).await?;
	Ok(Json(st.grpc.open_user_reinstatement(&token, &user_id, &reason).await?.into()))
}

/// `POST /api/owners/proposals/admin-admission` — CSRF-checked: propose granting
/// `Role::Admin`. `SetRole` refuses that role in the GRANTING direction and names this.
/// Taking the role away is NOT here and stays one act on purpose — containing a rogue
/// operator must never be the slower path.
pub async fn open_admin_admission(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::UserProposal>, ApiError> {
	let (token, user_id, reason) = proposal_open(&st, &jar, &headers, &body).await?;
	Ok(Json(st.grpc.open_admin_admission(&token, &user_id, &reason).await?.into()))
}

/// `POST /api/owners/proposals/{id}/vote` — CSRF-checked: an owner's vote. Its own neutral
/// vocabulary, not either consilium's: three kinds share this vote, so the wire word says
/// which way the voter pushed and the surface renders the kind's verb.
pub async fn vote_proposal(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> Result<Json<dto::UserProposal>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let Some(vote) = required(&parse_body(&body), "vote").as_deref().and_then(ProposalVote::parse) else {
		return Err(ApiError::BadRequest("vote must be \"for\" or \"against\"".into()));
	};
	Ok(Json(st.grpc.submit_user_proposal_vote(&token, &id, vote.wire()).await?.into()))
}

/// `POST /api/owners/proposals/{id}/cancel` — CSRF-checked: withdraw a proposal the caller
/// opened, while it is still open.
pub async fn cancel_proposal(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<dto::UserProposal>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	Ok(Json(st.grpc.cancel_user_proposal(&token, &id).await?.into()))
}

/// The gate every proposal-opening route shares: CSRF, a session, and the two required
/// fields. Factored out because the three differ ONLY in which RPC they then call — and
/// because a reason that is required on one of them and forgotten on another is precisely
/// the divergence that makes an audit trail useless.
async fn proposal_open(st: &AppState, jar: &CookieJar, headers: &HeaderMap, body: &Bytes) -> Result<(String, String, String), ApiError> {
	if !verify_csrf(st, jar, headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(st, jar).await?;
	let v = parse_body(body);
	let (Some(user_id), Some(reason)) = (required(&v, "user_id"), required(&v, "reason")) else {
		return Err(ApiError::BadRequest("user_id and reason are required".into()));
	};
	Ok((token, user_id, reason))
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

	/// The governance surface is owner-only, so an anonymous caller is refused before any
	/// plane is touched — and in particular before a money-plane token could be minted.
	#[tokio::test]
	async fn the_signed_in_surface_refuses_an_anonymous_caller() {
		let reads = [
			("GET", "/api/consilium"),
			("GET", "/api/consilium/c-1"),
			("GET", "/api/owners"),
			("GET", "/api/owners/removals"),
			("GET", "/api/owners/admissions"),
			("GET", "/api/owners/proposals"),
			("GET", "/api/admin/payments"),
			("GET", "/api/admin/payments/p-1"),
		];
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
			("/api/owners/removals", r#"{"target_user_id":"u-2","reason":"inactive"}"#),
			("/api/owners/removals/r-1/vote", r#"{"vote":"remove"}"#),
			("/api/owners/removals/r-1/cancel", "{}"),
			("/api/owners/admissions", r#"{"candidate_user_id":"u-3","reason":"co-founder"}"#),
			("/api/owners/admissions/a-1/vote", r#"{"vote":"admit"}"#),
			("/api/owners/admissions/a-1/cancel", "{}"),
			("/api/owners/proposals/suspension", r#"{"user_id":"u-4","reason":"chargeback ring"}"#),
			("/api/owners/proposals/reinstatement", r#"{"user_id":"u-4","reason":"cleared"}"#),
			("/api/owners/proposals/admin-admission", r#"{"user_id":"u-5","reason":"ops lead"}"#),
			("/api/owners/proposals/p-1/vote", r#"{"vote":"for"}"#),
			("/api/owners/proposals/p-1/cancel", "{}"),
			("/api/owners/resign", r#"{"confirm_email":"ada@example.com"}"#),
			("/api/admin/payments/p-1/cancel", "{}"),
		];
		for (uri, body) in mutations {
			let (status, _) = send("POST", uri, Some(body)).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "POST {uri} must be refused as CSRF");
		}
	}

	/// Opening a payment is not served here any more (banking#565, F3): the old form relayed
	/// the session token as proof without a fresh sign-in gate. The path still serves GET,
	/// so the router's answer to a POST is 405, and it comes before any CSRF or session gate.
	#[tokio::test]
	async fn opening_a_payment_is_not_served() {
		let body = r#"{"source":{"kind":"service","id":"fee"},"destination":{"internal":{"kind":"user","id":"u-1"}},"amount":"10","reason":"rent"}"#;
		let (status, _) = send("POST", "/api/admin/payments", Some(body)).await;
		assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
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
