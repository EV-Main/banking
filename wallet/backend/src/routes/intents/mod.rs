//! Wallet actions as intents: `GET /api/wallet/csrf`, `POST /api/wallet/intents` and
//! `POST /api/wallet/intents/{id}/confirm` (`docs/WALLET-INTENTS.md`).
//!
//! Every money-out act goes through these three routes, from the wallet's own page and from a
//! trusted host's alike; a host cannot call the first-party routes (`routes::money`). A host
//! cannot read the `ev_csrf` cookie, so the CSRF token here is an HMAC the BFF hands out per
//! account and page, the same on both surfaces; and since a cookie still rides along, every
//! request must also prove it comes from that exact page in a browser that calls it same-site.

#[cfg(test)]
mod tests;

use std::time::Duration;

use axum::{
	Json, Router,
	body::Bytes,
	extract::{Path, Request, State},
	http::{HeaderMap, HeaderValue, Method, StatusCode, header},
	middleware::{self, Next},
	response::{IntoResponse, Response},
	routing::{get, post},
};
use axum_extra::extract::cookie::CookieJar;
use domain::money::{Network, Usdt, WalletAddress};
use evbanking_contracts::banking::v1 as bk;
use evconcierge_auth::{
	Claims,
	stepup::{Aal, Expected},
};
use serde::{Deserialize, Serialize};
use tonic::{Code, Status};
use tracing::Instrument;

use crate::{
	dto,
	error::ApiError,
	routes::{Peer, audit_context, money_token_for, require_identity},
	state::AppState,
	util::now_secs,
	wallet_intents::{
		Assurance, IntentKind, IntentPage, Reach, WalletActions,
		intent::{self, Action, IntentRecord, MandateTerms, SessionBinding, Summary},
		keys,
		store::{self, IntentStatus, StoreError},
	},
};

/// How long a finished intent is kept after it expires, so a late confirm is told what
/// happened (`409`) rather than that nothing exists (`404`).
const RETAIN_AFTER_EXPIRY_SECS: u64 = 300;

/// The largest body either POST takes; an intent is a few short strings and a proof.
const MAX_BODY_BYTES: usize = 8 * 1024;

const ALLOW_HEADERS: &str = "content-type, x-ev-csrf";

pub fn routes(state: &AppState) -> Router<AppState> {
	Router::new()
		.route(
			"/api/wallet/csrf",
			get(csrf).options(preflight_refused).route_layer(middleware::from_fn_with_state(state.clone(), cors_for_get)),
		)
		.route(
			"/api/wallet/intents",
			post(create).options(preflight_refused).route_layer(middleware::from_fn_with_state(state.clone(), cors_for_post)),
		)
		.route(
			"/api/wallet/intents/{id}/confirm",
			post(confirm).options(preflight_refused).route_layer(middleware::from_fn_with_state(state.clone(), cors_for_post)),
		)
}

// ── CORS ─────────────────────────────────────────────────────────────────────

async fn cors_for_get(State(st): State<AppState>, request: Request, next: Next) -> Response {
	cors(&st, Method::GET, request, next).await
}

async fn cors_for_post(State(st): State<AppState>, request: Request, next: Next) -> Response {
	cors(&st, Method::POST, request, next).await
}

/// CORS for one route: a preflight is answered here and only for a scope host asking for
/// this route's method and no header beyond the two the routes read; any other answer
/// carries no CORS header, so the browser keeps it from the page. The wallet's own page is
/// same-origin and needs none.
async fn cors(st: &AppState, method: Method, request: Request, next: Next) -> Response {
	let origin = request.headers().get(header::ORIGIN).cloned();
	let enabled = origin
		.as_ref()
		.and_then(|value| value.to_str().ok())
		.and_then(|value| st.wallet_actions.page(value))
		.is_some_and(|page| page.reach != Reach::Wallet);
	let (mut response, allow) = if request.method() == Method::OPTIONS {
		if enabled && preflight_fits(request.headers(), &method) {
			let mut response = StatusCode::NO_CONTENT.into_response();
			let headers = response.headers_mut();
			headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static(if method == Method::GET { "GET" } else { "POST" }));
			headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static(ALLOW_HEADERS));
			headers.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("60"));
			(response, true)
		} else {
			(StatusCode::FORBIDDEN.into_response(), false)
		}
	} else {
		(next.run(request).await, enabled)
	};
	let headers = response.headers_mut();
	// The answer differs by `Origin`, so no cache may hand one host's answer to another; and a
	// CSRF token or an intent's terms belong to one person, so none may keep it at all.
	headers.append(header::VARY, HeaderValue::from_static("origin"));
	headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	if let (true, Some(origin)) = (allow, origin) {
		headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
		headers.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
	}
	response
}

fn preflight_fits(headers: &HeaderMap, method: &Method) -> bool {
	let asks_method = headers
		.get(header::ACCESS_CONTROL_REQUEST_METHOD)
		.and_then(|value| value.to_str().ok())
		.is_some_and(|value| value == method.as_str());
	let asks_headers = headers
		.get_all(header::ACCESS_CONTROL_REQUEST_HEADERS)
		.iter()
		.filter_map(|value| value.to_str().ok())
		.flat_map(|value| value.split(','))
		.map(|name| name.trim().to_ascii_lowercase())
		.filter(|name| !name.is_empty())
		.all(|name| name == "content-type" || name == "x-ev-csrf");
	asks_method && asks_headers
}

/// Never reached: [`cors`] answers every preflight. It exists because a route layer runs
/// only for a method the route has.
async fn preflight_refused() -> StatusCode {
	StatusCode::FORBIDDEN
}

// ── the gate every route shares ─────────────────────────────────────────────

/// A verified caller on an intent page.
struct Caller {
	page: IntentPage,
	/// The concierge access token, for the identity re-check.
	token: String,
	claims: Claims,
}

/// The page is exactly an intent page, the browser says the call is same-site, and the
/// session cookie verifies. Unlike the other first-party routes, a missing `Sec-Fetch-Site`
/// is a refusal here: every browser that can run the wallet sends it, and without it the
/// `Origin` check would stand alone.
async fn page_caller(st: &AppState, jar: &CookieJar, headers: &HeaderMap) -> Result<Caller, ApiError> {
	let origin = headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()).ok_or(ApiError::Csrf)?;
	let page = st.wallet_actions.page(origin).cloned().ok_or(ApiError::Csrf)?;
	let same_site = matches!(headers.get("sec-fetch-site").map(|value| value.to_str()), Some(Ok("same-origin" | "same-site")));
	if !same_site {
		return Err(ApiError::Csrf);
	}
	let (token, claims) = require_identity(st, jar).await?;
	Ok(Caller { page, token, claims })
}

/// The rest of a POST's gate, after the session: the HMAC token for this account and page,
/// a body declared exactly `application/json`, and the per-user money budget.
fn post_gate(st: &AppState, caller: &Caller, headers: &HeaderMap, body: &Bytes) -> Result<(), ApiError> {
	let token = headers.get("x-ev-csrf").and_then(|value| value.to_str().ok()).unwrap_or_default();
	if !st.wallet_actions.keys.csrf_valid(token, &caller.claims.sub, &caller.page.origin, unix_now()) {
		return Err(ApiError::Csrf);
	}
	let declared = headers
		.get(header::CONTENT_TYPE)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.split(';').next())
		.map(|value| value.trim().to_ascii_lowercase());
	if declared.as_deref() != Some("application/json") {
		return Err(ApiError::UnsupportedMediaType);
	}
	if body.len() > MAX_BODY_BYTES {
		return Err(ApiError::BadRequest("invalid_request".into()));
	}
	if !st.money_mutations.allow(&caller.claims.sub) {
		return Err(ApiError::Grpc(Status::resource_exhausted("too many money operations; try again shortly")));
	}
	Ok(())
}

/// The account as concierge holds it now, not as the access token remembers it: still
/// active, and its `token_version` not raised since the token was minted ("sign out
/// everywhere" raises it, and an access token minted before still verifies for its TTL).
async fn account_in_good_standing(st: &AppState, caller: &Caller) -> Result<(), ApiError> {
	let me = st.grpc.get_me(&caller.token).await.map_err(|status| match status.code() {
		Code::Unauthenticated => ApiError::Unauthenticated,
		// Concierge refuses a suspended or unknown account outright on this read.
		Code::PermissionDenied | Code::NotFound => ApiError::Refused(StatusCode::FORBIDDEN, "account_not_active"),
		Code::Ok
		| Code::Cancelled
		| Code::Unknown
		| Code::InvalidArgument
		| Code::DeadlineExceeded
		| Code::AlreadyExists
		| Code::ResourceExhausted
		| Code::FailedPrecondition
		| Code::Aborted
		| Code::OutOfRange
		| Code::Unimplemented
		| Code::Internal
		| Code::Unavailable
		| Code::DataLoss => {
			tracing::warn!(code = ?status.code(), detail = %status.message(), "identity re-check failed");
			ApiError::Refused(StatusCode::SERVICE_UNAVAILABLE, "identity_unavailable")
		}
	})?;
	if me.user_id != caller.claims.sub || me.status != "active" {
		return Err(ApiError::Refused(StatusCode::FORBIDDEN, "account_not_active"));
	}
	if me.token_version != caller.claims.token_version {
		return Err(ApiError::Unauthenticated);
	}
	Ok(())
}

fn unix_now() -> u64 {
	now_secs().max(0) as u64
}

/// A fault on the BFF's side: the detail goes to the log, the page gets the generic answer.
fn internal(detail: impl Into<String>) -> ApiError {
	tracing::error!(detail = %detail.into(), "wallet intent failed");
	ApiError::Internal("request failed".into())
}

fn store_error(error: StoreError) -> ApiError {
	tracing::error!(%error, "wallet intent store failed");
	ApiError::Refused(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable")
}

// ── GET /api/wallet/csrf ────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct CsrfToken {
	csrf: String,
	/// Unix seconds after which the token is refused.
	expires_at: u64,
	/// The kinds this page may create now, in matrix order. A host asks here rather than at
	/// build time, so narrowing `WALLET_ACTION_KINDS` turns its in-page forms off without a
	/// release; an empty list means the popup.
	kinds: Vec<IntentKind>,
}

/// The CSRF token for the signed-in account on this page: the page sends it back in
/// `x-ev-csrf`. In the body and never in a URL, so it reaches no log or `Referer`.
pub async fn csrf(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap) -> Result<Json<CsrfToken>, ApiError> {
	let caller = page_caller(&st, &jar, &headers).await?;
	let (csrf, expires_at) = st.wallet_actions.keys.csrf_token(&caller.claims.sub, &caller.page.origin, unix_now());
	Ok(Json(CsrfToken {
		csrf,
		expires_at,
		kinds: caller.page.kinds,
	}))
}

// ── POST /api/wallet/intents ────────────────────────────────────────────────

#[derive(Serialize)]
pub struct CreatedIntent {
	intent_id: String,
	/// What the page asks concierge to bind a step-up proof to.
	intent_hash: String,
	summary: Summary,
	expires_at: u64,
	/// The least assurance the confirm's proof must carry; `null` asks for no proof.
	min_aal: Option<Aal>,
	/// Whether a withdrawal goes to an address the account has not used: the hub decides it
	/// when the withdrawal is opened and exposes no read of it, so it is `null` here.
	new_recipient: Option<bool>,
	/// When a withdrawal to a new address is released: the hub's, for the same reason `null`.
	release_at: Option<u64>,
}

/// Compute and store an intent for `{kind, params}`. Nothing moves yet. The scopes are
/// judged by the page's reach, never by the body, and every amount and recipient shown is
/// the hub's record or the BFF's validated reading of the params.
pub async fn create(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<CreatedIntent>, ApiError> {
	let actions = &st.wallet_actions;
	let caller = page_caller(&st, &jar, &headers).await?;
	post_gate(&st, &caller, &headers, &body)?;
	let action: Action = serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest("invalid_request".into()))?;
	if !caller.page.kinds.contains(&action.kind()) {
		return Err(ApiError::Refused(StatusCode::FORBIDDEN, "kind_not_enabled"));
	}
	account_in_good_standing(&st, &caller).await?;
	let money_token = money_token_for(&st, &caller.claims).await?;
	let now = unix_now();
	let terms = terms_of(&st, &money_token, &caller.page, &action, now).await?;

	// Only a move can fall under the no-step-up line, so only a move reads what was moved.
	let moved = match action {
		Action::Move(_) => intent::from_micro(
			actions
				.store
				.moved_since(&caller.claims.sub, now.saturating_sub(store::BUDGET_WINDOW_SECS))
				.await
				.map_err(store_error)?,
		),
		Action::Withdraw(_) | Action::Pay(_) | Action::Mandate(_) | Action::ApprovePayment(_) => Usdt::ZERO,
	};
	let min_aal = intent::stepup_for(action.kind(), terms.amount, moved, &actions.policy);
	let record = IntentRecord {
		intent_id: intent::new_intent_id().map_err(|e| internal(format!("no randomness for an intent id: {e}")))?,
		sub: caller.claims.sub.clone(),
		origin: caller.page.origin.clone(),
		scope: terms.summary.scope.clone(),
		action,
		summary: terms.summary,
		min_aal,
		created_at: now,
		expires_at: now + intent::INTENT_TTL_SECS,
		session: SessionBinding {
			token_version: caller.claims.token_version,
			auth_time: caller.claims.auth_time,
		},
	};
	let bytes = record.to_bytes().map_err(|e| internal(format!("intent record does not serialise: {e}")))?;
	let hash = intent::intent_hash(&bytes);
	actions
		.store
		.put_intent(&record.intent_id, &bytes, &actions.keys.seal(&hash), intent::INTENT_TTL_SECS + RETAIN_AFTER_EXPIRY_SECS)
		.await
		.map_err(store_error)?;
	Ok(Json(CreatedIntent {
		intent_id: record.intent_id,
		intent_hash: keys::hex(&hash),
		summary: record.summary,
		expires_at: record.expires_at,
		min_aal: record.min_aal,
		new_recipient: None,
		release_at: None,
	}))
}

/// What a confirmation would carry out, and the amount the risk class is judged on.
struct Terms {
	summary: Summary,
	amount: Option<Usdt>,
}

fn refused(status: StatusCode, code: &'static str) -> ApiError {
	ApiError::Refused(status, code)
}

fn positive_amount(raw: &str) -> Result<Usdt, ApiError> {
	match Usdt::parse_decimal(raw) {
		Ok(amount) if !amount.is_zero() && raw.len() <= 64 => Ok(amount),
		_ => Err(ApiError::BadRequest("invalid amount".into())),
	}
}

/// A hub id as the routes pass it on: short and plain, so a crafted one never reaches the
/// store or the hub's parser.
fn plain_id(raw: &str) -> Result<(), ApiError> {
	if !raw.is_empty() && raw.len() <= 64 && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
		Ok(())
	} else {
		Err(ApiError::BadRequest("invalid id".into()))
	}
}

/// The terms of `action` on `page`, read from the hub where the hub holds them. Run at create
/// and again at confirm: a confirmation executes only the terms the person saw. Every scope
/// the act touches beside `main` must be in the page's reach: a host acting in another
/// host's scope is exactly the confused deputy of spec (d).
async fn terms_of(st: &AppState, money_token: &str, page: &IntentPage, action: &Action, now: u64) -> Result<Terms, ApiError> {
	let reach = page.reach;
	let summary = |amount: Option<String>, fee: Option<String>, net: Option<String>, recipient_full: String, network: Option<String>, scope: &str| Summary {
		amount,
		fee,
		net,
		recipient_full,
		network,
		scope: scope.to_owned(),
		host_origin: page.origin.clone(),
		mandate: None,
	};
	match action {
		// One side is a compartment and the other is `main`: the hub refuses a move between two
		// scopes anyway.
		Action::Move(p) => {
			let amount = positive_amount(&p.amount)?;
			let compartment = match (p.from_scope.as_str(), p.to_scope.as_str()) {
				("main", scope) | (scope, "main") if scope != "main" => scope,
				_ => return Err(refused(StatusCode::FORBIDDEN, "scope_mismatch")),
			};
			if !reach.covers(compartment) {
				return Err(refused(StatusCode::FORBIDDEN, "scope_mismatch"));
			}
			let amount_s = amount.to_decimal_string();
			// A move between compartments carries no fee (`CompartmentService.MoveFunds`).
			Ok(Terms {
				summary: summary(Some(amount_s.clone()), Some("0".into()), Some(amount_s), p.to_scope.clone(), None, compartment),
				amount: Some(amount),
			})
		}
		Action::Withdraw(p) => {
			let network = Network::parse(&p.network).map_err(|_| ApiError::BadRequest("unknown network".into()))?;
			let address = WalletAddress::parse(network, &p.address).map_err(|_| ApiError::BadRequest("invalid address".into()))?;
			let amount = positive_amount(&p.amount)?;
			let wallet = st.grpc.get_wallet(money_token).await.map_err(|s| ApiError::read(s, "wallet unavailable"))?;
			let rail = wallet
				.withdrawable
				.iter()
				.find(|rail| rail.network == network.as_str())
				.ok_or_else(|| refused(StatusCode::PRECONDITION_FAILED, "network_unavailable"))?;
			let hub_amount = |raw: &str| -> Result<Usdt, ApiError> {
				if raw.is_empty() {
					return Ok(Usdt::ZERO);
				}
				Usdt::parse_decimal(raw).map_err(|_| internal("the hub answered an unreadable withdrawal fee"))
			};
			let (fee, minimum) = (hub_amount(&rail.withdrawal_fee)?, hub_amount(&rail.min_withdrawal)?);
			if amount < minimum {
				return Err(refused(StatusCode::BAD_REQUEST, "below_minimum"));
			}
			let net = amount
				.checked_sub(fee)
				.filter(|net| !net.is_zero())
				.ok_or_else(|| refused(StatusCode::BAD_REQUEST, "below_fee"))?;
			Ok(Terms {
				summary: summary(
					Some(amount.to_decimal_string()),
					Some(fee.to_decimal_string()),
					Some(net.to_decimal_string()),
					address.as_str().to_owned(),
					Some(network.as_str().to_owned()),
					"main",
				),
				amount: Some(amount),
			})
		}
		// The invoice is the hub's, read server-side, and paid from `main` or from its own scope.
		Action::Pay(p) => {
			plain_id(&p.invoice_id)?;
			let invoice = st
				.grpc
				.get_payable_invoice(money_token, &p.invoice_id)
				.await
				.map_err(|s| ApiError::read(s, "invoice unavailable"))?;
			if !reach.covers(&invoice.scope) || (p.pay_from != "main" && p.pay_from != invoice.scope) {
				return Err(refused(StatusCode::FORBIDDEN, "scope_mismatch"));
			}
			if invoice.state != "open" {
				return Err(refused(StatusCode::PRECONDITION_FAILED, "invoice_not_open"));
			}
			let amount = Usdt::parse_decimal(&invoice.amount).map_err(|_| internal("the hub answered an unreadable invoice amount"))?;
			let recipient = if invoice.display_name.is_empty() {
				invoice.scope.clone()
			} else {
				invoice.display_name.clone()
			};
			// The invoice states no fee, so neither does the summary.
			Ok(Terms {
				summary: summary(Some(amount.to_decimal_string()), None, None, recipient, None, &invoice.scope),
				amount: Some(amount),
			})
		}
		Action::Mandate(p) => {
			plain_id(&p.request_id)?;
			let request = st
				.grpc
				.get_mandate_request(money_token, &p.request_id)
				.await
				.map_err(|s| ApiError::read(s, "mandate request unavailable"))?;
			if !reach.covers(&request.scope) {
				return Err(refused(StatusCode::FORBIDDEN, "scope_mismatch"));
			}
			if request.expires_at <= now as i64 {
				return Err(refused(StatusCode::PRECONDITION_FAILED, "mandate_request_expired"));
			}
			let recipient = if request.client_display_name.is_empty() {
				request.client_id.clone()
			} else {
				request.client_display_name.clone()
			};
			let mut summary = summary(None, None, None, recipient, None, &request.scope);
			summary.mandate = Some(MandateTerms {
				operations: request.operations().map(|op| op.as_str_name().to_owned()).collect(),
				client_id: request.client_id,
				client_display_name: request.client_display_name,
				cap_per_op: request.cap_per_op,
				cap_24h: request.cap_24h,
				cap_outstanding: request.cap_outstanding,
				cap_book_fill_24h: request.cap_book_fill_24h,
				mandate_expires_at: request.mandate_expires_at,
			});
			// A grant moves nothing now; its class does not depend on an amount.
			Ok(Terms { summary, amount: None })
		}
		// The order's terms as the hub states them, and still open to this seat's answer. It
		// spends the caller's claim, so its scope is `main`.
		Action::ApprovePayment(p) => {
			plain_id(&p.payment_id)?;
			let payment = st
				.grpc
				.get_payment_confirmation(money_token, &p.payment_id)
				.await
				.map_err(|s| ApiError::read(s, "payment unavailable"))?;
			let answerable = payment.state() == bk::PaymentState::Pending && payment.decision() == bk::ConsentDecision::Pending && !payment.invalidated && payment.expires_at > now as i64;
			if !answerable {
				return Err(refused(StatusCode::PRECONDITION_FAILED, "payment_not_answerable"));
			}
			let amount = Usdt::parse_decimal(&payment.amount).map_err(|_| internal("the hub answered an unreadable payment amount"))?;
			let destination = payment.destination.ok_or_else(|| internal("the hub answered a payment with no destination"))?;
			let (recipient, network) = if destination.kind == "external" {
				(destination.address, Some(destination.network))
			} else {
				(destination.label, None)
			};
			Ok(Terms {
				summary: summary(Some(amount.to_decimal_string()), None, None, recipient, network, "main"),
				amount: Some(amount),
			})
		}
	}
}

// ── POST /api/wallet/intents/{id}/confirm ───────────────────────────────────

/// The only thing a confirmation may carry. Everything else — amounts, recipients, the
/// scope — is the stored intent's, so an unknown field is refused rather than ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmBody {
	#[serde(default)]
	proof: Option<String>,
}

#[derive(Serialize)]
pub struct ConfirmedIntent {
	intent_id: String,
	status: &'static str,
	/// The assurance the act was authorised with, as the proof achieved it.
	stepup_method: Assurance,
	/// The hub's answer, in the shape its matching read returns.
	result: serde_json::Value,
}

/// Carry out a stored intent, in the order concierge's proof contract prescribes: verify
/// the proof, win `created → authorized`, spend the proof's `jti`, execute. A lost race at
/// any step is a refusal, and a proof is never spent for an intent that did not move.
///
/// An intent left `executing` (the hub's answer was lost or unclear) is confirmed again by
/// sending the same act under the same idempotency key: the hub answers the first act, so
/// the page learns the outcome without a second withdrawal or move. No proof, `jti` or budget
/// is asked for or spent again; they were, when the act was first sent.
pub async fn confirm(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, peer: Peer, headers: HeaderMap, body: Bytes) -> Result<Json<ConfirmedIntent>, ApiError> {
	let started = tokio::time::Instant::now();
	let actions = &st.wallet_actions;
	let caller = page_caller(&st, &jar, &headers).await?;
	post_gate(&st, &caller, &headers, &body)?;
	let confirm: ConfirmBody = serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest("invalid_request".into()))?;
	let not_found = || refused(StatusCode::NOT_FOUND, "intent_not_found");
	if !intent::is_intent_id(&id) {
		return Err(not_found());
	}
	let stored = actions.store.get_intent(&id).await.map_err(store_error)?.ok_or_else(not_found)?;
	let hash = intent::intent_hash(&stored.record);
	if !actions.keys.seal_valid(&hash, &stored.seal) {
		tracing::error!(intent_id = %id, "a stored wallet intent does not match its seal");
		return Err(not_found());
	}
	let record: IntentRecord = serde_json::from_slice(&stored.record).map_err(|e| internal(format!("a sealed intent does not parse: {e}")))?;
	// Another account's or another page's intent is answered as if it did not exist, so the
	// route is no oracle for which ids are live.
	if record.intent_id != id || record.sub != caller.claims.sub || record.origin != caller.page.origin {
		return Err(not_found());
	}
	let binding = SessionBinding {
		token_version: caller.claims.token_version,
		auth_time: caller.claims.auth_time,
	};
	if record.session != binding {
		return Err(refused(StatusCode::FORBIDDEN, "session_changed"));
	}
	match stored.status {
		IntentStatus::Created => {}
		// Past `expires_at` too: the act was authorised in time, and refusing the resend would
		// leave the person a new intent — a new key, and a second act — as the only way to learn
		// what happened. The window is the store's keep of an executing intent.
		IntentStatus::Executing => {
			let assurance = stored.assurance.ok_or_else(|| internal(format!("executing intent {id} records no assurance")))?;
			let money_token = money_token_for(&st, &caller.claims).await?;
			let audit = audit_for(&st, &headers, &peer, &record, assurance);
			return finish(
				Execution {
					st: st.clone(),
					record,
					money_token,
					audit,
					assurance,
					first: None,
				},
				started,
			)
			.await;
		}
		// `executed` stays refused (concierge contract item 7): its result was answered once.
		IntentStatus::Authorized | IntentStatus::Executed | IntentStatus::Expired | IntentStatus::Cancelled => {
			return Err(refused(StatusCode::CONFLICT, "intent_not_pending"));
		}
	}
	let now = unix_now();
	if now >= record.expires_at {
		actions.store.transition(&id, IntentStatus::Created, IntentStatus::Expired).await.map_err(store_error)?;
		return Err(refused(StatusCode::GONE, "intent_expired"));
	}
	let demand = match (record.min_aal, &confirm.proof) {
		(Some(_), None) => return Err(refused(StatusCode::FORBIDDEN, "proof_required")),
		(None, Some(_)) => return Err(ApiError::BadRequest("proof_not_expected".into())),
		(Some(min_aal), Some(proof)) => Some((min_aal, proof)),
		(None, None) => None,
	};

	account_in_good_standing(&st, &caller).await?;
	let money_token = money_token_for(&st, &caller.claims).await?;
	if terms_of(&st, &money_token, &caller.page, &record.action, now).await?.summary != record.summary {
		actions.store.transition(&id, IntentStatus::Created, IntentStatus::Cancelled).await.map_err(store_error)?;
		return Err(refused(StatusCode::CONFLICT, "terms_changed"));
	}

	let proof = match demand {
		Some((min_aal, proof)) => {
			let expected = Expected {
				intent_hash: &hash,
				sub: &caller.claims.sub,
				origin: &record.origin,
				min_aal,
			};
			let claims = actions.proofs.verify(proof, &expected).await.map_err(|error| {
				tracing::warn!(intent_id = %id, %error, "step-up proof refused");
				refused(StatusCode::FORBIDDEN, "stepup_invalid")
			})?;
			// A proof minted before the intent existed cannot have been for it.
			if claims.iat < record.created_at {
				tracing::warn!(intent_id = %id, "step-up proof is older than its intent");
				return Err(refused(StatusCode::FORBIDDEN, "stepup_invalid"));
			}
			Some(claims)
		}
		None => None,
	};

	// The steps above can take seconds (`GetMe`, the money token, the terms, the JWKS); an
	// intent that expired meanwhile must not be authorised on a check made before them.
	if unix_now() >= record.expires_at {
		actions.store.transition(&id, IntentStatus::Created, IntentStatus::Expired).await.map_err(store_error)?;
		return Err(refused(StatusCode::GONE, "intent_expired"));
	}
	if !actions.store.transition(&id, IntentStatus::Created, IntentStatus::Authorized).await.map_err(store_error)? {
		return Err(refused(StatusCode::CONFLICT, "intent_not_pending"));
	}
	let assurance = proof.as_ref().map_or(Assurance::NONE, |claims| Assurance::from(claims.aal));
	let audit = audit_for(&st, &headers, &peer, &record, assurance);
	finish(
		Execution {
			st: st.clone(),
			record,
			money_token,
			audit,
			assurance,
			first: Some(FirstSend {
				proof: proof.map(|claims| (claims.jti, claims.exp)),
			}),
		},
		started,
	)
	.await
}

/// How long a confirmation waits for its act before it answers `intent_executing`: inside
/// the router's outer deadline, whose `504` carries no CORS header and so reaches the page as
/// a bare network failure.
const ANSWER_WITHIN: Duration = Duration::from_secs(13);

/// How long an `executing` intent is kept, counted from when the act was first sent: the
/// window in which a confirm can still send it again and learn the outcome.
pub const EXECUTING_KEEP_SECS: u64 = 86_400;

/// One act to send to the hub, and the writes around it.
struct Execution {
	st: AppState,
	record: IntentRecord,
	money_token: String,
	audit: bk::AuditContext,
	assurance: Assurance,
	/// `Some` for the first send, which still owes the `jti` and the budget; `None` for a
	/// resend of an `executing` intent, which already paid them.
	first: Option<FirstSend>,
}

struct FirstSend {
	/// The verified proof's `jti` and `exp`.
	proof: Option<(String, u64)>,
}

/// Run `execution` on its own task and wait for it within [`ANSWER_WITHIN`]. Neither a
/// dropped connection nor the router's deadline can then cut the act off from the status
/// write that records it; a confirm that stops waiting answers `intent_executing`, and the
/// task goes on to the end.
async fn finish(execution: Execution, started: tokio::time::Instant) -> Result<Json<ConfirmedIntent>, ApiError> {
	let intent_id = execution.record.intent_id.clone();
	let task = tokio::spawn(execution.run().in_current_span());
	match tokio::time::timeout_at(started + ANSWER_WITHIN, task).await {
		Ok(Ok(answer)) => answer.map(Json),
		Ok(Err(join)) => Err(internal(format!("the execution of intent {intent_id} failed: {join}"))),
		Err(_) => {
			tracing::warn!(%intent_id, "the hub did not answer in time; the intent is left executing for a resend");
			Err(refused(StatusCode::SERVICE_UNAVAILABLE, "intent_executing"))
		}
	}
}

impl Execution {
	async fn run(self) -> Result<ConfirmedIntent, ApiError> {
		let actions = &self.st.wallet_actions;
		let id = self.record.intent_id.as_str();
		if let Some(first) = &self.first {
			self.pay_the_first_send(actions, first).await?;
			if !actions.store.begin_execution(id, self.assurance, EXECUTING_KEEP_SECS).await.map_err(store_error)? {
				return Err(refused(StatusCode::CONFLICT, "intent_not_pending"));
			}
		}
		match execute(&self.st, &self.money_token, &self.record, self.audit.clone()).await {
			Ok(result) => {
				// The act is done whatever the store says now; the answer is the hub's. A lost
				// write leaves the intent `executing`, and a later confirm resends and gets the
				// same act back.
				match actions.store.transition(id, IntentStatus::Executing, IntentStatus::Executed).await {
					Ok(true) => {}
					Ok(false) => tracing::debug!(intent_id = %id, "another confirm recorded the execution first"),
					Err(error) => tracing::error!(intent_id = %id, %error, "the executed intent could not be recorded"),
				}
				Ok(ConfirmedIntent {
					intent_id: self.record.intent_id.clone(),
					status: IntentStatus::Executed.as_str(),
					stepup_method: self.assurance,
					result,
				})
			}
			// On the first send the hub had never seen the key, so a refusal means it did not
			// act. On a resend the first send may have acted, so the intent stays `executing`.
			Err(status) if hub_refused(status.code()) => {
				if self.first.is_some() {
					match actions.store.transition(id, IntentStatus::Executing, IntentStatus::Cancelled).await {
						Ok(_) => {}
						Err(error) => tracing::error!(intent_id = %id, %error, "a refused intent could not be recorded"),
					}
				}
				Err(ApiError::Grpc(status))
			}
			Err(status) => {
				tracing::warn!(intent_id = %id, code = ?status.code(), detail = %status.message(), "the hub's answer is unclear; the intent is left executing for a resend");
				Err(refused(StatusCode::SERVICE_UNAVAILABLE, "intent_executing"))
			}
		}
	}

	/// What the first send owes before the act: the proof's `jti` (concierge doc item 4) and the
	/// move budget. Each is counted once, here, and stays counted if the hub then refuses (the
	/// safe side).
	async fn pay_the_first_send(&self, actions: &WalletActions, first: &FirstSend) -> Result<(), ApiError> {
		let id = self.record.intent_id.as_str();
		let now = unix_now();
		if let Some((jti, exp)) = &first.proof
			&& !actions.store.spend_jti(jti, exp.saturating_sub(now) + 5).await.map_err(store_error)?
		{
			tracing::warn!(intent_id = %id, "step-up proof replayed");
			return Err(cancel(actions, id, "proof_replayed").await);
		}
		if let Action::Move(_) = self.record.action {
			let amount = sealed_amount(&self.record)?;
			// Only a move that needed no step-up is held to the line, again here and atomically:
			// two intents created side by side each saw the budget before the other spent it.
			// Every move is counted.
			let cap = self.record.min_aal.is_none().then(|| intent::micro_floor(actions.policy.no_stepup_24h));
			if !actions.store.count_move(&self.record.sub, id, intent::micro_ceil(amount), cap, now).await.map_err(store_error)? {
				return Err(cancel(actions, id, "limit_reached").await);
			}
		}
		Ok(())
	}
}

/// The amount a sealed move confirms.
fn sealed_amount(record: &IntentRecord) -> Result<Usdt, ApiError> {
	match record.summary.amount.as_deref().map(Usdt::parse_decimal) {
		Some(Ok(amount)) => Ok(amount),
		Some(Err(e)) => Err(internal(format!("a sealed intent has an unreadable amount: {e}"))),
		None => Err(internal("a sealed intent has no amount")),
	}
}

/// The audit block of the act: the request's own reading, with the intent's origin and id and
/// the assurance the act was authorised with.
fn audit_for(st: &AppState, headers: &HeaderMap, peer: &Peer, record: &IntentRecord, assurance: Assurance) -> bk::AuditContext {
	let mut audit = audit_context(st, headers, peer.addr(), assurance);
	audit.origin = record.origin.clone();
	audit.intent_id = record.intent_id.clone();
	audit
}

/// `authorized → cancelled` for an intent that won its transition but may not go on, and
/// the refusal to answer with.
async fn cancel(actions: &WalletActions, id: &str, code: &'static str) -> ApiError {
	match actions.store.transition(id, IntentStatus::Authorized, IntentStatus::Cancelled).await {
		Ok(_) => refused(StatusCode::CONFLICT, code),
		Err(error) => store_error(error),
	}
}

/// Whether the hub's answer means it did not act on this send. Anything else leaves the
/// outcome unknown, and the intent stays `executing` for a resend under the same key.
fn hub_refused(code: Code) -> bool {
	match code {
		Code::InvalidArgument
		| Code::NotFound
		| Code::AlreadyExists
		| Code::PermissionDenied
		| Code::FailedPrecondition
		| Code::ResourceExhausted
		| Code::OutOfRange
		| Code::Unimplemented
		| Code::Unauthenticated => true,
		Code::Ok | Code::Cancelled | Code::Unknown | Code::DeadlineExceeded | Code::Aborted | Code::Internal | Code::Unavailable | Code::DataLoss => false,
	}
}

/// The act itself, with the intent id as the hub's idempotency key. Amounts and recipients
/// come from the sealed summary — the values the person confirmed — not from the params.
async fn execute(st: &AppState, money_token: &str, record: &IntentRecord, audit: bk::AuditContext) -> Result<serde_json::Value, Status> {
	let sealed = |value: &Option<String>, what: &str| value.clone().ok_or_else(|| Status::internal(format!("a sealed intent has no {what}")));
	let json = |value: Result<serde_json::Value, serde_json::Error>| value.map_err(|e| Status::internal(format!("the hub's answer does not serialise: {e}")));
	match &record.action {
		Action::Move(p) => {
			let req = bk::MoveFundsRequest {
				from_scope: p.from_scope.clone(),
				to_scope: p.to_scope.clone(),
				amount: sealed(&record.summary.amount, "amount")?,
				idempotency_key: record.intent_id.clone(),
				audit: Some(audit),
			};
			json(serde_json::to_value(dto::CompartmentMove::from(st.grpc.move_funds(money_token, req).await?)))
		}
		Action::Withdraw(_) => {
			let req = bk::RequestWithdrawalRequest {
				network: sealed(&record.summary.network, "network")?,
				address: record.summary.recipient_full.clone(),
				amount: sealed(&record.summary.amount, "amount")?,
				idempotency_key: record.intent_id.clone(),
				audit: Some(audit),
			};
			json(serde_json::to_value(dto::Withdrawal::from(st.grpc.request_withdrawal(money_token, req).await?)))
		}
		Action::Pay(p) => {
			let req = bk::PayInvoiceRequest {
				invoice_id: p.invoice_id.clone(),
				pay_from: p.pay_from.clone(),
				idempotency_key: record.intent_id.clone(),
				audit: Some(audit),
			};
			json(serde_json::to_value(dto::PayableInvoice::from(st.grpc.pay_invoice(money_token, req).await?)))
		}
		Action::Mandate(p) => {
			let req = bk::GrantMandateRequest {
				request_id: p.request_id.clone(),
				idempotency_key: record.intent_id.clone(),
				audit: Some(audit),
			};
			json(serde_json::to_value(dto::Mandate::from(st.grpc.grant_mandate(money_token, req).await?)))
		}
		// No idempotency key on the wire: the hub answers a repeated identical decision with the
		// order as it stands, so a resend learns the outcome without a second answer.
		Action::ApprovePayment(p) => {
			let req = bk::AnswerPaymentConfirmationRequest {
				payment_id: p.payment_id.clone(),
				decision: bk::ConsentDecision::Approve as i32,
				audit: Some(audit),
			};
			json(serde_json::to_value(dto::PaymentConfirmation::from(st.grpc.answer_payment_confirmation(money_token, req).await?)))
		}
	}
}
