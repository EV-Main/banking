//! The PUBLIC approval surface — `/api/approval/removal/{token}`.
//!
//! Reached from an email by someone who may not be signed in. It carries NO session and
//! requires none: the emailed token IS the credential, exactly as the concierge's removal
//! approval service is mounted outside its user-auth layer.
//!
//! Consequences that shape everything below:
//!
//! - **The GET is strictly read-only.** Gmail, Outlook SafeLinks and corporate gateways
//!   issue automatic `GET`s for every URL in a message, so a scanner must not be able to
//!   move a removal. The vote is a separate `POST` carrying the secret code from the
//!   same mail — that is what turns a scanned link into a deliberate human act.
//! - **No CSRF check.** Double-submit protects a session's authority from another origin;
//!   there is no session here, and the token in the path is not ambient authority a
//!   browser attaches on its own. The POST is bounded by [`AttemptLimiter`] instead.
//! - **One identical refusal.** Unknown, expired, spent, burned and wrong-state tokens
//!   must be indistinguishable — see [`opaque`].
//! - **Every response is sealed**: `Referrer-Policy: no-referrer` so the token in the URL
//!   never rides a `Referer` to a third party, and `Cache-Control: no-store` so it never
//!   lands in a shared cache.

use std::{
	collections::HashMap,
	net::SocketAddr,
	sync::Mutex,
	time::{Duration, Instant},
};

use axum::{
	Json,
	body::Bytes,
	extract::{ConnectInfo, FromRequestParts, Path, State},
	http::{HeaderMap, HeaderValue, header, request::Parts},
	response::{IntoResponse, Response},
};
use evconcierge_contracts::concierge::v1 as cc;
use serde::Serialize;
use tonic::{Code, Status};

use crate::{
	dto,
	error::ApiError,
	governance::RemovalVote,
	routes::{parse_body, trusted_client_ip},
	state::AppState,
};

/// The body every refusal on this surface carries, whatever the token actually was.
const NOT_FOUND: &str = "invitation not found";
/// Audit fields are attacker-supplied text; they are stored, never interpreted, and are
/// clamped here so a crafted header cannot bloat an audit row.
const MAX_IP: usize = 64;
const MAX_USER_AGENT: usize = 256;

// ── the endpoints ────────────────────────────────────────────────────────────

/// `GET /api/approval/removal/{token}` — the redacted removal invitation, for its target.
pub async fn removal_invitation(State(st): State<AppState>, Path(token): Path<String>) -> Response {
	respond(st.grpc.removal_invitation(&token).await.map(dto::OwnerRemovalInvitation::from))
}

/// `POST /api/approval/removal/{token}` — the target accepting or refusing their own
/// removal.
pub async fn removal_decision(State(st): State<AppState>, Path(token): Path<String>, peer: Peer, headers: HeaderMap, body: Bytes) -> Response {
	let (ballot, audit) = match accept(&st, &headers, peer, &body) {
		Ok(accepted) => accepted,
		Err(refusal) => return refusal,
	};
	let request = cc::SubmitSelfDecisionRequest {
		token,
		code: ballot.code,
		vote: ballot.decision.removal_vote().wire() as i32,
		client_ip: audit.client_ip,
		user_agent: audit.user_agent,
	};
	respond(st.grpc.submit_self_decision(request).await.map(dto::RemovalDecision::from))
}

// ── the shared shape ─────────────────────────────────────────────────────────

/// A vote as the browser sends it: the secret code from the mail, plus one of two answers.
struct Ballot {
	code: String,
	decision: Decision,
}

#[derive(Clone, Copy)]
enum Decision {
	Approve,
	Reject,
}

impl Decision {
	fn parse(raw: &str) -> Option<Self> {
		match raw {
			"approve" => Some(Self::Approve),
			"reject" => Some(Self::Reject),
			_ => None,
		}
	}

	/// On a removal the target is answering about themselves, so approving IS consenting
	/// to be removed and rejecting is refusing to go.
	fn removal_vote(self) -> RemovalVote {
		match self {
			Self::Approve => RemovalVote::Remove,
			Self::Reject => RemovalVote::Keep,
		}
	}
}

/// Who cast the vote, as far as the edge can tell. Both fields are AUDIT-only: the header
/// is caller-controlled and the socket peer is the reverse proxy, so neither is ever an
/// authorization input — the token and the code are.
struct Audit {
	client_ip: String,
	user_agent: String,
}

/// The socket peer, when the server was built with connect info. Optional on purpose: a
/// request that arrives without it must still be served, because the address is an audit
/// field and not a gate — a missing one may not turn a vote into a 500.
pub struct Peer(Option<SocketAddr>);

impl Peer {
	pub fn addr(&self) -> Option<SocketAddr> {
		self.0
	}
}

impl<S: Send + Sync> FromRequestParts<S> for Peer {
	type Rejection = std::convert::Infallible;

	async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
		Ok(Self(parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|ConnectInfo(addr)| *addr)))
	}
}

/// The audit fields of a signed-in answer: where it came from and with what. Recorded beside
/// the decision, never an authorization input.
pub fn audit_fields(headers: &HeaderMap, Peer(peer): Peer, trusted_hops: u8) -> (String, String) {
	let user_agent = clamp(headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default(), MAX_USER_AGENT);
	(client_ip(headers, peer, trusted_hops), user_agent)
}

/// The vote POST's preamble: the per-IP bound, then the ballot, then the audit
/// fields. Returns the finished refusal response when either check fails.
// A `Response` is a large error type; returning `ApiError` instead would move the sealing
// back into the handler, the one thing on this surface that must not be forgettable.
#[allow(clippy::result_large_err)]
fn accept(st: &AppState, headers: &HeaderMap, Peer(peer): Peer, body: &Bytes) -> Result<(Ballot, Audit), Response> {
	let client_ip = client_ip(headers, peer, st.config.trusted_proxy_hops);
	if !st.approvals.allow(&client_ip) {
		return Err(sealed(ApiError::Grpc(Status::resource_exhausted("too many attempts; try again shortly")).into_response()));
	}
	let body = parse_body(body);
	let code = body.get("code").and_then(|v| v.as_str()).unwrap_or_default();
	let decision = body.get("decision").and_then(|v| v.as_str()).unwrap_or_default();
	let (Some(decision), false) = (Decision::parse(decision), code.is_empty()) else {
		return Err(sealed(ApiError::BadRequest("code and decision are required".into()).into_response()));
	};
	let audit = Audit {
		client_ip,
		user_agent: clamp(headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default(), MAX_USER_AGENT),
	};
	Ok((Ballot { code: code.to_string(), decision }, audit))
}

/// Render one upstream result as this surface's response: the payload as JSON, or the
/// single opaque refusal — sealed either way.
fn respond<T: Serialize>(result: Result<T, Status>) -> Response {
	sealed(match result {
		Ok(payload) => Json(payload).into_response(),
		Err(status) => opaque(status).into_response(),
	})
}

/// The two headers that keep an emailed token out of `Referer` chains and out of shared
/// caches. Applied to refusals as well as payloads — a 404 for a token is exactly as
/// worth not leaking as the invitation behind it.
fn sealed(mut response: Response) -> Response {
	let headers = response.headers_mut();
	headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
	headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	response
}

/// Fold every unusable-token verdict into ONE identical 404: unknown, expired, spent,
/// burned and wrong-state must be indistinguishable, or this endpoint becomes an oracle
/// for which tokens exist.
///
/// The planes' half of the contract is to express all of those as `NOT_FOUND`; the extra
/// codes folded in here are defense in depth against one of them ever arriving as
/// something else — a burned token surfacing as `ResourceExhausted`, say. Two codes stay
/// as they are: `InvalidArgument` is the wrong-code answer, and a human who mistyped a
/// code has to be told so (its bound is the five-attempt burn, not this mapping); and a
/// transport-class failure keeps its own status, because answering 404 for a plane that
/// is merely down would be a lie that tells an attacker nothing about any token anyway.
fn opaque(status: Status) -> ApiError {
	match status.code() {
		Code::NotFound | Code::PermissionDenied | Code::FailedPrecondition | Code::AlreadyExists | Code::ResourceExhausted => ApiError::ReadFailed {
			code: Code::NotFound,
			message: NOT_FOUND.to_string(),
		},
		_ => ApiError::Grpc(status),
	}
}

/// The caller's address for the audit row and the rate-limit key: the entry the trusted proxy
/// chain appended ([`trusted_client_ip`], `TRUSTED_PROXY_HOPS`), not the leftmost
/// `X-Forwarded-For` the browser can type. Still never an authorization input, and the limiter
/// it keys is not the security boundary.
fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>, trusted_hops: u8) -> String {
	clamp(&trusted_client_ip(headers, peer, trusted_hops), MAX_IP)
}

/// Truncate on a character boundary — a byte slice would panic on multi-byte input, and
/// this text comes straight off the wire.
fn clamp(value: &str, max: usize) -> String {
	value.chars().take(max).collect()
}

// ── the per-client bound ─────────────────────────────────────────────────────

/// The vote POST's window, and how many attempts it admits per client.
const WINDOW: Duration = Duration::from_secs(60);
const MAX_ATTEMPTS: u32 = 10;
/// Above this many tracked clients the map is swept of expired windows, so a spray across
/// forged `X-Forwarded-For` values cannot grow it without bound.
const SWEEP_AT: usize = 4096;

/// A per-client fixed window. [`Default`] is the vote POST's quota, keyed by IP; the money
/// mutations key a second instance by the verified subject (see `AppState::money_mutations`).
///
/// In-process and per-replica ON PURPOSE. On the vote the real anti-brute-force bound is
/// the server-side five-attempt token burn, which is transactional, permanent and global
/// — 46 bits of code behind five tries needs no help from
/// the edge; on money the bounds that matter are the hub's own limits and idempotency.
/// This exists only so a script, or a client retrying a POST, cannot turn one session or
/// one leaked link into a flood of upstream RPCs; approximate, per-replica counting is
/// enough for that, and a shared store would put a dependency in front of paths that have
/// to keep working when the rest is degraded.
pub struct AttemptLimiter {
	window: Duration,
	max_attempts: u32,
	windows: Mutex<HashMap<String, Window>>,
}

struct Window {
	started: Instant,
	attempts: u32,
}

impl Default for AttemptLimiter {
	fn default() -> Self {
		Self::new(WINDOW, MAX_ATTEMPTS)
	}
}

impl AttemptLimiter {
	pub fn new(window: Duration, max_attempts: u32) -> Self {
		Self {
			window,
			max_attempts,
			windows: Mutex::new(HashMap::new()),
		}
	}

	/// Count one attempt from `client`, and say whether it may proceed.
	pub fn allow(&self, client: &str) -> bool {
		let mut windows = self.windows.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		let now = Instant::now();
		if windows.len() >= SWEEP_AT {
			windows.retain(|_, window| now.duration_since(window.started) < self.window);
		}
		let window = windows.entry(client.to_string()).or_insert(Window { started: now, attempts: 0 });
		if now.duration_since(window.started) >= self.window {
			*window = Window { started: now, attempts: 0 };
		}
		window.attempts += 1;
		window.attempts <= self.max_attempts
	}
}

#[cfg(test)]
mod tests {
	use axum::body::to_bytes;

	use super::*;

	async fn rendered(response: Response) -> (u16, String, Vec<(String, String)>) {
		let status = response.status().as_u16();
		let headers = response
			.headers()
			.iter()
			.map(|(name, value)| (name.as_str().to_string(), value.to_str().unwrap_or_default().to_string()))
			.collect();
		let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, String::from_utf8_lossy(&bytes).to_string(), headers)
	}

	/// Every unusable-token verdict must be one response — same status, same bytes — or the
	/// endpoint tells a stranger which tokens exist.
	#[tokio::test]
	async fn every_dead_token_produces_the_identical_refusal() {
		let verdicts = [
			Status::not_found("no such token"),
			Status::permission_denied("token belongs to another voter"),
			Status::failed_precondition("removal already decided"),
			Status::already_exists("token already spent"),
			Status::resource_exhausted("token burned after 5 attempts"),
			Status::not_found("removal already cancelled"),
		];
		let mut answers = Vec::new();
		for verdict in verdicts {
			answers.push(rendered(respond::<()>(Err(verdict))).await);
		}
		for answer in &answers {
			assert_eq!(answer.0, 404, "every dead token is a 404");
			assert_eq!(answer.1, answers[0].1, "and every one of them is the SAME body");
			for detail in ["burned", "spent", "invalidated", "cancelled"] {
				assert!(!answer.1.contains(detail), "no upstream detail may survive: {}", answer.1);
			}
		}
	}

	/// The token rides in the URL, so no response on this surface may be cacheable or leak a
	/// `Referer` — the refusals included.
	#[tokio::test]
	async fn every_response_is_sealed_against_leaking_the_token() {
		for response in [respond(Ok(dto::OwnerRemovalInvitation::default())), respond::<()>(Err(Status::not_found("")))] {
			let (_, _, headers) = rendered(response).await;
			assert!(headers.contains(&("referrer-policy".to_string(), "no-referrer".to_string())), "{headers:?}");
			assert!(headers.contains(&("cache-control".to_string(), "no-store".to_string())), "{headers:?}");
		}
	}

	/// A plane that is merely down must not be reported as a missing invitation: that
	/// would tell the owner their link is dead when it is not, and it reveals nothing.
	#[tokio::test]
	async fn an_outage_is_not_disguised_as_a_dead_token() {
		let (status, _, _) = rendered(respond::<()>(Err(Status::unavailable("tcp connect error")))).await;
		assert_eq!(status, 503);
	}

	#[test]
	fn the_window_admits_its_quota_then_refuses() {
		let limiter = AttemptLimiter::default();
		for attempt in 1..=MAX_ATTEMPTS {
			assert!(limiter.allow("198.51.100.7"), "attempt {attempt} is within the window");
		}
		assert!(!limiter.allow("198.51.100.7"));
		// The bound is per client, so one noisy address cannot lock everyone else out.
		assert!(limiter.allow("198.51.100.8"));
	}

	#[test]
	fn the_audit_address_counts_trusted_hops_from_the_right() {
		let mut headers = HeaderMap::new();
		headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9, 10.0.0.1"));
		let peer: SocketAddr = "10.0.0.1:443".parse().unwrap();
		assert_eq!(client_ip(&headers, Some(peer), 1), "10.0.0.1");
		assert_eq!(client_ip(&headers, Some(peer), 2), "203.0.113.9");
		assert_eq!(client_ip(&HeaderMap::new(), Some(peer), 1), "10.0.0.1");
		assert_eq!(client_ip(&HeaderMap::new(), None, 1), "");
	}

	#[test]
	fn only_the_two_decisions_parse() {
		assert!(matches!(Decision::parse("approve"), Some(Decision::Approve)));
		assert!(matches!(Decision::parse("reject"), Some(Decision::Reject)));
		assert!(Decision::parse("pending").is_none());
		assert!(Decision::parse("").is_none());
	}
}

/// The surface as the mounted router serves it, not handler by handler: what these pin is
/// that no session or CSRF layer sits in front of these routes.
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

	use super::AttemptLimiter;
	use crate::{
		config::AppConfig,
		cookies::CookieNames,
		routes::router,
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

	/// No session, no CSRF, and the request must still reach the plane. With the plane
	/// unreachable that shows up as a transport failure — which only a request that passed
	/// every gate could possibly have produced.
	#[tokio::test]
	async fn the_emailed_surface_needs_no_session_and_no_csrf() {
		let calls = [
			("GET", "/api/approval/removal/tok-1", None),
			("POST", "/api/approval/removal/tok-1", Some(r#"{"code":"ABCDE12345","decision":"approve"}"#)),
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
		let (status, _) = send("POST", "/api/approval/removal/tok-1", Some(r#"{"decision":"approve"}"#)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
	}
}
