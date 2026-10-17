use axum::{
	Json,
	body::Bytes,
	extract::{Path, Query, State},
	http::HeaderMap,
};
use axum_extra::extract::cookie::CookieJar;
use evbanking_contracts::banking::v1 as bk;
use serde::Deserialize;
use tonic::{Code, Status};

use crate::{
	dto,
	error::ApiError,
	routes::{first_party_origin, money_gate, money_mutation, money_token_for, parse_body, require_fresh_login, require_money_token, required, verify_csrf},
	state::AppState,
	util::now_secs,
};

/// The machine-readable body the cabinet keys on to offer verification instead of reporting
/// a fault. A token rather than a sentence: it is matched in the browser and worded there,
/// the way the identity plane's own `kyc_unavailable` is.
pub const VERIFICATION_REQUIRED: &str = "verification_required";

#[derive(Deserialize)]
pub struct NetworkQuery {
	network: Option<String>,
}

#[derive(Deserialize)]
pub struct ScopeQuery {
	scope: Option<String>,
}

#[derive(Deserialize)]
pub struct LimitQuery {
	limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct ServiceQuery {
	service: Option<String>,
}

/// `?allocation=&from=&to=` — a history window over one allocation, unix seconds.
#[derive(Deserialize)]
pub struct HistoryQuery {
	allocation: Option<String>,
	from: Option<i64>,
	to: Option<i64>,
}

// ── wallet ───────────────────────────────────────────────────────────────────

/// `GET /api/wallet` — the unified lifecycle balance, deposit rails, and per-rail withdraw options.
pub async fn get_wallet(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::Wallet>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let wallet = st.grpc.get_wallet(&token).await.map_err(|s| ApiError::read(s, "wallet unavailable"))?;
	Ok(Json(wallet.into()))
}

/// `GET /api/wallet/deposit-address?network=` — the caller's deposit address on a network.
///
/// The one refusal named rather than flattened. Reads answer with a fixed generic string so
/// hub detail never leaks, but "you are not verified yet" is a state the caller can leave,
/// and reporting it as an outage sent every unverified user hunting for a fault that was
/// never theirs. `PermissionDenied` is unambiguous HERE and nowhere else: the hub raises it
/// on this read for the verification gate alone — an unconfigured rail answers `Ok(None)`
/// rather than an error, and this RPC carries no role check. The withdrawal path, where the
/// same code also means a suspended account or someone else's withdrawal, keeps relaying the
/// hub's own wording instead of guessing.
pub async fn deposit_address(State(st): State<AppState>, jar: CookieJar, Query(q): Query<NetworkQuery>) -> Result<Json<dto::DepositAddress>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let addr = st.grpc.deposit_address(&token, &q.network.unwrap_or_default()).await.map_err(deposit_address_error)?;
	Ok(Json(addr.into()))
}

fn deposit_address_error(status: Status) -> ApiError {
	match status.code() {
		Code::PermissionDenied => ApiError::read(status, VERIFICATION_REQUIRED),
		_ => ApiError::read(status, "deposit address unavailable"),
	}
}

/// `GET /api/wallet/withdrawals` — the caller's withdrawals, newest first.
pub async fn list_withdrawals(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::WithdrawalList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.list_withdrawals(&token).await.map_err(|s| ApiError::read(s, "withdrawals unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/wallet/deposits` — the caller's confirmed on-chain deposits, newest first.
pub async fn list_deposits(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::DepositList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.list_deposits(&token).await.map_err(|s| ApiError::read(s, "deposits unavailable"))?;
	Ok(Json(list.into()))
}

// ── compartments (main and one balance per scope) ────────────────────────────

/// `GET /api/wallet/compartments` — `main` first, then every scope with the caller's balance
/// there and its policy. Readable from the scope hosts (`routes::cors`).
pub async fn list_compartments(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::CompartmentList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.list_compartments(&token).await.map_err(|s| ApiError::read(s, "compartments unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/wallet/compartment?scope=` — the caller's balance in one compartment, for a
/// scope host that must show its own balance and not the sum over all of them. Readable from
/// the scope hosts like the list (`routes::cors`).
pub async fn get_compartment(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ScopeQuery>) -> Result<Json<dto::Compartment>, ApiError> {
	let scope = q.scope.filter(|scope| !scope.is_empty()).ok_or_else(|| ApiError::BadRequest("scope is required".into()))?;
	let token = require_money_token(&st, &jar).await?;
	let compartment = st.grpc.get_compartment(&token, &scope).await.map_err(|s| ApiError::read(s, "compartment unavailable"))?;
	Ok(Json(compartment.into()))
}

/// `POST /api/wallet/compartments/move` — first-party, CSRF-checked and rate-limited: move money
/// between `main` and one scope. The hub checks the policy, the balance and that one side is
/// `main`; the BFF only refuses a body that names nothing.
pub async fn move_funds(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::CompartmentMove>, ApiError> {
	let token = money_mutation(&st, &jar, &headers).await?;
	let v = parse_body(&body);
	let (Some(from_scope), Some(to_scope), Some(amount), Some(idempotency_key)) =
		(required(&v, "from_scope"), required(&v, "to_scope"), required(&v, "amount"), required(&v, "idempotency_key"))
	else {
		return Err(ApiError::BadRequest("from_scope, to_scope, amount and idempotency_key are required".into()));
	};
	let req = bk::MoveFundsRequest {
		from_scope,
		to_scope,
		amount,
		idempotency_key,
	};
	Ok(Json(st.grpc.move_funds(&token, req).await?.into()))
}

// ── operations ───────────────────────────────────────────────────────────────

/// `GET /api/operations?limit=` — the caller's unified activity timeline, newest first.
/// An absent `limit` falls through to the hub's default page; the hub clamps anything
/// oversized, so no bound is enforced twice.
pub async fn list_operations(State(st): State<AppState>, jar: CookieJar, Query(q): Query<LimitQuery>) -> Result<Json<dto::OperationList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let list = st
		.grpc
		.list_operations(&token, q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "operations unavailable"))?;
	Ok(Json(list.into()))
}

/// `POST /api/wallet/withdrawals` — first-party, CSRF-checked and rate-limited: open a
/// withdrawal of free balance. Money leaving the platform, so a scope's host never asks.
pub async fn request_withdrawal(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Withdrawal>, ApiError> {
	let token = money_mutation(&st, &jar, &headers).await?;
	let v = parse_body(&body);
	let (Some(network), Some(address), Some(amount)) = (required(&v, "network"), required(&v, "address"), required(&v, "amount")) else {
		return Err(ApiError::BadRequest("network, address and amount are required".into()));
	};
	// Minted by the withdraw page on load; absent from older clients, which keep the earlier
	// no-dedupe behaviour at the hub.
	let idempotency_key = required(&v, "idempotency_key").unwrap_or_default();
	let req = bk::RequestWithdrawalRequest {
		network,
		address,
		amount,
		idempotency_key,
	};
	Ok(Json(st.grpc.request_withdrawal(&token, req).await?.into()))
}

/// `POST /api/wallet/withdrawals/cancel` — first-party and CSRF-checked: cancel a still-queued
/// withdrawal. Not rate-limited: it only ever returns money to the caller.
pub async fn cancel_withdrawal(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Withdrawal>, ApiError> {
	if !first_party_origin(&st, &headers) || !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let Some(withdrawal_id) = required(&parse_body(&body), "withdrawal_id") else {
		return Err(ApiError::BadRequest("withdrawal_id is required".into()));
	};
	Ok(Json(st.grpc.cancel_withdrawal(&token, &withdrawal_id).await?.into()))
}

// ── invoices (a scope asking the caller to pay) ──────────────────────────────

/// `GET /api/wallet/invoices/{id}` — a scope's invoice as the caller, its would-be payer, sees
/// it. An unknown id and one restricted to another payer are the same 404.
pub async fn get_payable_invoice(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>) -> Result<Json<dto::PayableInvoice>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let invoice = st.grpc.get_payable_invoice(&token, &id).await.map_err(|s| ApiError::read(s, "invoice unavailable"))?;
	Ok(Json(invoice.into()))
}

/// `POST /api/wallet/invoices/{id}/pay` — first-party, CSRF-checked and rate-limited: pay the invoice from
/// `main` or from the caller's compartment in its scope. The body names only the source and the
/// click's idempotency key; what is paid, how much and to whom is the server's invoice.
pub async fn pay_invoice(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> Result<Json<dto::PayableInvoice>, ApiError> {
	let token = money_mutation(&st, &jar, &headers).await?;
	let v = parse_body(&body);
	let (Some(pay_from), Some(idempotency_key)) = (required(&v, "pay_from"), required(&v, "idempotency_key")) else {
		return Err(ApiError::BadRequest("pay_from and idempotency_key are required".into()));
	};
	let req = bk::PayInvoiceRequest {
		invoice_id: id,
		pay_from,
		idempotency_key,
	};
	Ok(Json(st.grpc.pay_invoice(&token, req).await?.into()))
}

// ── mandates (what a scope's service may draw on the caller's compartment) ──────

/// `GET /api/wallet/mandates` — the caller's live mandates, newest first.
pub async fn list_mandates(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::MandateList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.list_mandates(&token).await.map_err(|s| ApiError::read(s, "mandates unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/wallet/mandate-requests/{id}` — what a scope's service asked the caller to grant,
/// for the consent screen. An unknown, expired or malformed id is the same 404.
pub async fn get_mandate_request(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>) -> Result<Json<dto::MandateRequest>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let request = st.grpc.get_mandate_request(&token, &id).await.map_err(|s| ApiError::read(s, "mandate request unavailable"))?;
	Ok(Json(request.into()))
}

/// `POST /api/wallet/mandates` — first-party, CSRF-checked and rate-limited: grant the mandate a
/// scope's service asked for. The body names only the request and the click's idempotency key;
/// the limits are the stored request's, so a page cannot grant more than the user was shown.
/// Consent to future debits, so only a page on evinvest.ltd may ask for it — never a scope's own host.
pub async fn grant_mandate(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Mandate>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	require_fresh_login(&claims, now_secs().max(0) as u64)?;
	let token = money_token_for(&st, &claims).await?;
	let v = parse_body(&body);
	let (Some(request_id), Some(idempotency_key)) = (required(&v, "request_id"), required(&v, "idempotency_key")) else {
		return Err(ApiError::BadRequest("request_id and idempotency_key are required".into()));
	};
	let req = bk::GrantMandateRequest { request_id, idempotency_key };
	Ok(Json(st.grpc.grant_mandate(&token, req).await?.into()))
}

/// `POST /api/wallet/mandates/{id}/revoke` — first-party, CSRF-checked and rate-limited: end a mandate; the
/// service's holds on the caller's compartment are released.
pub async fn revoke_mandate(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<dto::Mandate>, ApiError> {
	let token = money_mutation(&st, &jar, &headers).await?;
	Ok(Json(st.grpc.revoke_mandate(&token, &id).await?.into()))
}

// ── allocations (the catalog of investable products) ─────────────────────────

/// `GET /api/allocations` — the investor-facing catalog: the `open` allocations the caller
/// may at least view. Each row carries `access` (the product's default) and `caller_access`
/// (what THIS investor holds) — the subscribe control keys off the latter. The unlisted
/// half (draft/closed/hidden) is admin-only and lives on the admin surface.
pub async fn list_allocations(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::AllocationList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.list_allocations(&token, false).await.map_err(|s| ApiError::read(s, "allocations unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/allocations/detail?service=` — one allocation, in any state. An investor
/// holding units of a closed product still has to render it. A product hidden from this
/// investor is a 404, exactly as an unknown service is.
pub async fn get_allocation(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ServiceQuery>) -> Result<Json<dto::Allocation>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let allocation = st
		.grpc
		.get_allocation(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "allocation unavailable"))?;
	Ok(Json(allocation.into()))
}

// ── funds (the service currency) ─────────────────────────────────────────────

/// `GET /api/funds/nav?service=` — the current NAV (price per share) of a fund.
pub async fn fund_nav(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ServiceQuery>) -> Result<Json<dto::FundNav>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let nav = st
		.grpc
		.fund_nav(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "fund nav unavailable"))?;
	Ok(Json(nav.into()))
}

/// `GET /api/funds/nav/history?allocation=&from=&to=` — the valuation log of one
/// allocation over `[from, to]` (unix seconds; absent = all-time / now) plus the caller's
/// own participation through it: the two series of the dashboard's performance chart.
/// Keyed by the allocation, not the service, so a hidden allocation can serve its holders
/// the same way later. A seed-only fund answers empty `marks`, not an error.
pub async fn fund_nav_history(State(st): State<AppState>, jar: CookieJar, Query(q): Query<HistoryQuery>) -> Result<Json<dto::FundNavHistory>, ApiError> {
	let allocation = q.allocation.filter(|a| !a.is_empty()).ok_or_else(|| ApiError::BadRequest("allocation is required".into()))?;
	let token = require_money_token(&st, &jar).await?;
	let req = bk::ListFundNavHistoryRequest {
		allocation,
		from: q.from.unwrap_or(0),
		to: q.to.unwrap_or(0),
	};
	let history = st.grpc.fund_nav_history(&token, req).await.map_err(|s| ApiError::read(s, "fund history unavailable"))?;
	Ok(Json(history.into()))
}

/// `GET /api/funds/fee-policy?service=` — what this fund charges. Not gated on holding a
/// position: the terms are part of deciding whether to buy in at all.
pub async fn fee_policy(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ServiceQuery>) -> Result<Json<dto::FeePolicy>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let policy = st
		.grpc
		.fee_policy(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "fee policy unavailable"))?;
	Ok(Json(policy.into()))
}

/// `GET /api/funds/accrued-fees?service=` — what the caller's holding owes right now,
/// uncharged. The disclosure behind showing a position's value net of fees.
pub async fn accrued_fees(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ServiceQuery>) -> Result<Json<dto::AccruedFees>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let accrued = st
		.grpc
		.accrued_fees(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "accrued fees unavailable"))?;
	Ok(Json(accrued.into()))
}

/// `GET /api/funds/positions` — the caller's fund positions.
pub async fn list_positions(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::PositionList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.list_positions(&token).await.map_err(|s| ApiError::read(s, "positions unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/funds/redemptions` — the caller's redemptions, newest first.
pub async fn list_redemptions(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::RedemptionList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.list_redemptions(&token).await.map_err(|s| ApiError::read(s, "redemptions unavailable"))?;
	Ok(Json(list.into()))
}

/// `POST /api/funds/subscribe` — CSRF-checked: subscribe free balance into a fund (mint units).
pub async fn subscribe(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Subscription>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(service), Some(amount)) = (required(&v, "service"), required(&v, "amount")) else {
		return Err(ApiError::BadRequest("service and amount are required".into()));
	};
	let req = bk::SubscribeRequest { service, amount };
	Ok(Json(st.grpc.subscribe(&token, req).await?.into()))
}

/// `POST /api/funds/redeem` — CSRF-checked: redeem units back to cash (accept-and-queue).
pub async fn redeem(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(service), Some(units)) = (required(&v, "service"), required(&v, "units")) else {
		return Err(ApiError::BadRequest("service and units are required".into()));
	};
	let req = bk::RedeemRequest { service, units };
	Ok(Json(st.grpc.redeem(&token, req).await?.into()))
}

/// `POST /api/funds/redemptions/cancel` — CSRF-checked: cancel a still-queued redemption.
pub async fn cancel_redemption(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let Some(redemption_id) = required(&parse_body(&body), "redemption_id") else {
		return Err(ApiError::BadRequest("redemption_id is required".into()));
	};
	Ok(Json(st.grpc.cancel_redemption(&token, &redemption_id).await?.into()))
}

#[cfg(test)]
mod tests {
	use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};

	use super::*;

	async fn render(err: ApiError) -> (StatusCode, String) {
		let resp = err.into_response();
		let status = resp.status();
		let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
		let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		(status, body["error"].as_str().unwrap().to_string())
	}

	/// The cabinet offers verification off the back of this token. If it ever went back to
	/// the generic string, the deposit screen would silently return to reporting the one
	/// refusal a user can clear themselves as an outage they cannot.
	#[tokio::test]
	async fn verification_refusal_is_named() {
		let hub = Status::permission_denied("identity verification required before a deposit address is issued");
		let (status, message) = render(deposit_address_error(hub)).await;
		assert_eq!(status, StatusCode::FORBIDDEN);
		assert_eq!(message, VERIFICATION_REQUIRED);
	}

	/// Everything else stays flattened — the hub's wording on a read is not the browser's
	/// business, and a signer or ledger fault must not arrive dressed as a KYC problem.
	#[tokio::test]
	async fn every_other_failure_stays_generic() {
		for hub in [
			Status::unavailable("internal error"),
			Status::internal("sqlx: connection refused"),
			Status::invalid_argument("unknown network"),
		] {
			let code = hub.code();
			let (_, message) = render(deposit_address_error(hub)).await;
			assert_eq!(message, "deposit address unavailable", "{code:?}");
		}
	}
}

/// The money mutations and the scope-readable reads through the real router. Every plane is
/// unreachable except, where a session is needed, a stub concierge that only publishes the
/// JWKS: each assertion is on a decision the BFF makes before it reaches the money plane.
#[cfg(test)]
mod wallet_gate_tests {
	use std::{collections::HashMap, net::SocketAddr, sync::Arc};

	use axum::{
		body::{Body, to_bytes},
		http::{HeaderMap, Request, StatusCode, header},
	};
	use evconcierge_auth::{TokenType, Verifier, VerifierConfig};
	use evconcierge_contracts::concierge::v1::auth_service_server::AuthServiceServer;
	use tokio::net::TcpListener;
	use tokio_stream::wrappers::TcpListenerStream;
	use tonic::transport::Server;
	use tower::ServiceExt;

	use crate::{
		config::AppConfig,
		cookies::CookieNames,
		routes::{
			approval::AttemptLimiter,
			first_party_origin, router,
			test_support::{AUDIENCE, ConciergeJwks, ISSUER, access_token, access_token_at},
			ws,
		},
		session::BankingTokens,
		state::{AppState, Grpc},
	};

	const BLACK_HOLE: &str = "http://127.0.0.1:1";
	const FIRST_PARTY: &str = "https://evinvest.ltd";
	const SCOPE_HOST: &str = "https://sa.evinvest.ltd";
	const SCOPE_HOSTS: &str = "https://invest.evinvest.ltd,https://sa.evinvest.ltd";
	const CSRF: &str = "csrf-token";
	const MANDATE: &str = r#"{"request_id":"4f0c2d1e-0000-4000-8000-0123456789ab","idempotency_key":"k-1"}"#;
	const MOVE: &str = r#"{"from_scope":"main","to_scope":"sa","amount":"1","idempotency_key":"k-1"}"#;

	/// Every route that moves money or consents to it being drawn, with a body that passes
	/// the BFF's own field checks.
	const MONEY_MUTATIONS: [(&str, &str); 5] = [
		("/api/wallet/compartments/move", MOVE),
		("/api/wallet/withdrawals", r#"{"network":"bep20","address":"0xabc","amount":"1"}"#),
		("/api/wallet/invoices/4f0c2d1e-0000-4000-8000-0123456789ab/pay", r#"{"pay_from":"main","idempotency_key":"k-1"}"#),
		("/api/wallet/mandates", MANDATE),
		("/api/wallet/mandates/4f0c2d1e-0000-4000-8000-0123456789ab/revoke", ""),
	];

	struct Env {
		ws_origin: Option<&'static str>,
		cors: &'static str,
		concierge: String,
	}

	impl Default for Env {
		fn default() -> Self {
			Self {
				ws_origin: Some(FIRST_PARTY),
				cors: "",
				concierge: BLACK_HOLE.into(),
			}
		}
	}

	fn state(env: Env) -> AppState {
		let mut vars = HashMap::from([
			("PIGGYBANK_GRPC_ADDR", BLACK_HOLE.to_string()),
			("BANKING_AUTH_GRPC_ADDR", BLACK_HOLE.to_string()),
			("CONCIERGE_GRPC_ADDR", env.concierge.clone()),
			("BANKING_ISSUANCE_TOKEN", "test-issuance".into()),
			("AUTH_ISSUER", ISSUER.into()),
			("AUTH_CLIENT_AUDIENCE", AUDIENCE.into()),
			("MFE_REGISTRY_PATH", "/mfe-registry.json".into()),
			("WALLET_CORS_ORIGINS", env.cors.into()),
			("APP_ENV", "development".into()),
		]);
		if let Some(origin) = env.ws_origin {
			vars.insert("CABINET_WS_ORIGIN", origin.into());
		}
		let config = AppConfig::from_source(|var| vars.get(var).cloned()).expect("the test env loads");
		let verifier = Verifier::try_new(VerifierConfig {
			issuer: ISSUER.into(),
			audiences: vec![AUDIENCE.into()],
			allowed_types: vec![TokenType::Access],
			jwks_grpc_endpoint: env.concierge.clone(),
		})
		.expect("build the verifier");
		AppState {
			cookies: Arc::new(CookieNames::new(config.cookie_secure())),
			banking: Arc::new(BankingTokens::new()),
			approvals: Arc::new(AttemptLimiter::default()),
			money_mutations: Arc::new(crate::routes::money_mutation_limiter()),
			verifier,
			grpc: Grpc::connect_lazy(BLACK_HOLE, BLACK_HOLE, &env.concierge, Some("test-issuance".into())).expect("build the lazy channels"),
			config: Arc::new(config),
		}
	}

	fn app() -> axum::Router {
		router(state(Env::default()))
	}

	/// A stub concierge on an ephemeral port that answers only `Jwks`, so a signed
	/// `ev_access` cookie verifies. The listener is handed over, never re-bound — see the
	/// same helper in `admin.rs` for the race that rules out.
	async fn jwks() -> SocketAddr {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("claim an ephemeral port");
		let addr = listener.local_addr().expect("the listener has an address");
		tokio::spawn(async move {
			Server::builder()
				.add_service(AuthServiceServer::new(ConciergeJwks))
				.serve_with_incoming(TcpListenerStream::new(listener))
				.await
				.expect("the stub concierge serves");
		});
		addr
	}

	fn request(method: &str, uri: &str, body: &str, origin: Option<&str>, session: bool) -> Request<Body> {
		request_with(method, uri, body, origin, session.then(access_token))
	}

	fn request_with(method: &str, uri: &str, body: &str, origin: Option<&str>, token: Option<String>) -> Request<Body> {
		let cookie = if let Some(token) = token {
			format!("ev_access={token}; ev_csrf={CSRF}")
		} else {
			format!("ev_csrf={CSRF}")
		};
		let mut request = Request::builder()
			.method(method)
			.uri(uri)
			.header(header::COOKIE, cookie)
			.header("x-ev-csrf", CSRF)
			.header(header::CONTENT_TYPE, "application/json");
		if let Some(origin) = origin {
			request = request.header(header::ORIGIN, origin);
		}
		request.body(Body::from(body.to_owned())).expect("build the request")
	}

	async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
		let response = app.clone().oneshot(request).await.expect("the router responds");
		let status = response.status();
		let headers = response.headers().clone();
		let body = to_bytes(response.into_body(), 1 << 16).await.expect("read the body");
		(status, headers, String::from_utf8_lossy(&body).into_owned())
	}

	/// A POST with a valid CSRF pair and no session, from `origin`.
	async fn post_from(uri: &str, body: &str, origin: Option<&str>) -> (StatusCode, String) {
		let (status, _, body) = send(&app(), request("POST", uri, body, origin, false)).await;
		(status, body)
	}

	/// A scope's own host may read the wallet but never act on it, and a request that does not
	/// say where it comes from is not first-party either. Cancelling a withdrawal is gated the
	/// same way (`wallet.proto` lists it), though it only returns money.
	#[tokio::test]
	async fn only_evinvest_ltd_may_move_pay_withdraw_or_change_a_mandate() {
		let cancel = [("/api/wallet/withdrawals/cancel", r#"{"withdrawal_id":"w-1"}"#)];
		for (uri, body) in MONEY_MUTATIONS.iter().chain(cancel.iter()) {
			for origin in [Some(SCOPE_HOST), Some("https://evil.example"), None] {
				let (status, response) = post_from(uri, body, origin).await;
				assert_eq!(status, StatusCode::FORBIDDEN, "{uri} from {origin:?}: {response}");
				assert!(response.contains("csrf"), "{uri} from {origin:?}: {response}");
			}
			let (status, _) = post_from(uri, body, Some(FIRST_PARTY)).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: past the gate the next refusal is the missing session");
		}
	}

	/// Consent to future debits needs a recent interactive sign-in. A session with a stale or
	/// absent `auth_time` is refused with the code the page keys on, before any money RPC is
	/// reached; revoking, which only ends a consent, is not held to it.
	#[tokio::test]
	async fn granting_a_mandate_needs_a_fresh_login_but_revoking_does_not() {
		let concierge = jwks().await;
		let app = router(state(Env {
			concierge: format!("http://{concierge}"),
			..Env::default()
		}));
		let now = jsonwebtoken::get_current_timestamp();
		let revoke = "/api/wallet/mandates/m-1/revoke";
		for stale in [Some(now - crate::routes::STEP_UP_MAX_AGE_SECS - 60), None] {
			let grant = request_with("POST", "/api/wallet/mandates", MANDATE, Some(FIRST_PARTY), Some(access_token_at(stale)));
			let (status, _, response) = send(&app, grant).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "auth_time {stale:?}: {response}");
			assert!(response.contains("step_up_required"), "auth_time {stale:?}: {response}");

			let revoke = request_with("POST", revoke, "", Some(FIRST_PARTY), Some(access_token_at(stale)));
			let (_, _, response) = send(&app, revoke).await;
			assert!(!response.contains("step_up_required"), "auth_time {stale:?}: {response}");
		}
		let fresh = request_with("POST", "/api/wallet/mandates", MANDATE, Some(FIRST_PARTY), Some(access_token_at(Some(now))));
		let (_, _, response) = send(&app, fresh).await;
		assert!(!response.contains("step_up_required"), "a fresh login passes the gate: {response}");
	}

	/// Reading a service's request for a mandate is a read, like an invoice: no CSRF token, no
	/// first-party origin, only a session.
	#[tokio::test]
	async fn a_mandate_request_is_read_with_a_session_alone() {
		let uri = "/api/wallet/mandate-requests/4f0c2d1e-0000-4000-8000-0123456789ab";
		for origin in [Some(FIRST_PARTY), Some(SCOPE_HOST), None] {
			let (status, _, response) = send(&app(), request("GET", uri, "", origin, false)).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "from {origin:?}: {response}");
		}
	}

	/// With no first-party origin configured the money gate refuses everything, even the
	/// origin production would accept; the websocket guard, which only reads, stays open
	/// for local development.
	#[tokio::test]
	async fn an_unset_first_party_origin_closes_the_money_gate_but_not_the_socket() {
		let st = state(Env { ws_origin: None, ..Env::default() });
		let mut headers = HeaderMap::new();
		headers.insert(header::ORIGIN, FIRST_PARTY.parse().expect("a header value"));
		assert!(!first_party_origin(&st, &headers));
		assert!(ws::origin_allowed(&st, &headers));

		let app = router(st);
		for (uri, body) in MONEY_MUTATIONS {
			let (status, _, response) = send(&app, request("POST", uri, body, Some(FIRST_PARTY), true)).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {response}");
		}
	}

	/// The per-user bound is one budget across every money mutation: a script cannot dodge it
	/// by spreading over routes. The limiter runs after the identity check, so it is spent
	/// only by a signed-in caller and keyed by who they are.
	#[tokio::test]
	async fn money_mutations_share_one_per_user_budget() {
		let concierge = jwks().await;
		let app = router(state(Env {
			concierge: format!("http://{concierge}"),
			..Env::default()
		}));
		for attempt in 0..crate::routes::MONEY_MUTATIONS_PER_WINDOW as usize {
			let (uri, body) = MONEY_MUTATIONS[attempt % MONEY_MUTATIONS.len()];
			let (status, _, response) = send(&app, request("POST", uri, body, Some(FIRST_PARTY), true)).await;
			assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "attempt {attempt} on {uri} is within the budget: {response}");
			assert_ne!(status, StatusCode::UNAUTHORIZED, "the stub session must verify: {response}");
		}
		for (uri, body) in MONEY_MUTATIONS {
			let (status, _, response) = send(&app, request("POST", uri, body, Some(FIRST_PARTY), true)).await;
			assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{uri}: {response}");
		}
		// Reads are not money mutations and do not draw on the budget.
		let (status, _, _) = send(&app, request("GET", "/api/wallet/withdrawals", "", Some(FIRST_PARTY), true)).await;
		assert_ne!(status, StatusCode::TOO_MANY_REQUESTS);
	}

	const SCOPE_READS: [&str; 3] = ["/api/wallet", "/api/wallet/compartments", "/api/operations"];

	fn cors_of(headers: &HeaderMap) -> (Option<&str>, Option<&str>) {
		let get = |name| headers.get(name).and_then(|v| v.to_str().ok());
		(get(header::ACCESS_CONTROL_ALLOW_ORIGIN), get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS))
	}

	/// A configured scope host reads the three wallet reads with credentials — its own origin
	/// echoed, never `*` — and the answer says it varies by `Origin`. Even the 401 carries the
	/// headers, so the scope host can tell "signed out" from a network failure.
	#[tokio::test]
	async fn a_scope_host_reads_the_wallet_with_credentials() {
		let app = router(state(Env {
			cors: SCOPE_HOSTS,
			..Env::default()
		}));
		for uri in SCOPE_READS {
			for origin in ["https://invest.evinvest.ltd", SCOPE_HOST] {
				let (_, headers, _) = send(&app, request("GET", uri, "", Some(origin), false)).await;
				assert_eq!(cors_of(&headers), (Some(origin), Some("true")), "{uri} from {origin}");
				assert!(headers.get_all(header::VARY).iter().any(|v| v == "origin"), "{uri}: {headers:?}");
			}
		}
	}

	/// Anyone else, any other route and every mutation get no CORS headers at all, so the
	/// browser hides the response from the calling page.
	#[tokio::test]
	async fn nothing_else_is_readable_cross_origin() {
		let app = router(state(Env {
			cors: SCOPE_HOSTS,
			..Env::default()
		}));
		for uri in SCOPE_READS {
			for origin in ["https://evil.example", "https://sa.evinvest.ltd.evil.example", "null", FIRST_PARTY] {
				let (_, headers, _) = send(&app, request("GET", uri, "", Some(origin), false)).await;
				assert_eq!(cors_of(&headers), (None, None), "{uri} from {origin}");
			}
			// No preflight is answered: only simple reads are served cross-origin.
			let (_, headers, _) = send(&app, request("OPTIONS", uri, "", Some(SCOPE_HOST), false)).await;
			assert_eq!(cors_of(&headers), (None, None), "OPTIONS {uri}");
		}
		for uri in [
			"/api/wallet/withdrawals",
			"/api/wallet/deposit-address",
			"/api/wallet/mandates",
			"/api/wallet/mandate-requests/4f0c2d1e-0000-4000-8000-0123456789ab",
			"/api/wallet/deposits",
		] {
			let (_, headers, _) = send(&app, request("GET", uri, "", Some(SCOPE_HOST), false)).await;
			assert_eq!(cors_of(&headers), (None, None), "{uri} is not a scope read");
		}
		for (uri, body) in MONEY_MUTATIONS {
			let (_, headers, _) = send(&app, request("POST", uri, body, Some(SCOPE_HOST), true)).await;
			assert_eq!(cors_of(&headers), (None, None), "POST {uri}");
		}
	}

	/// An empty list is CORS switched off, scope hosts included.
	#[tokio::test]
	async fn no_configured_hosts_means_no_cors() {
		let app = app();
		for uri in SCOPE_READS {
			let (_, headers, _) = send(&app, request("GET", uri, "", Some(SCOPE_HOST), false)).await;
			assert_eq!(cors_of(&headers), (None, None), "{uri}");
		}
	}
}
