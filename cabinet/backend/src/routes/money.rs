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
	routes::{first_party_origin, parse_body, require_money_token, required, verify_csrf},
	state::AppState,
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

/// `POST /api/wallet/withdrawals` — CSRF-checked: open a withdrawal of free balance.
pub async fn request_withdrawal(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Withdrawal>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(network), Some(address), Some(amount)) = (required(&v, "network"), required(&v, "address"), required(&v, "amount")) else {
		return Err(ApiError::BadRequest("network, address and amount are required".into()));
	};
	let req = bk::RequestWithdrawalRequest { network, address, amount };
	Ok(Json(st.grpc.request_withdrawal(&token, req).await?.into()))
}

/// `POST /api/wallet/withdrawals/cancel` — CSRF-checked: cancel a still-queued withdrawal.
pub async fn cancel_withdrawal(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Withdrawal>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
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

/// `POST /api/wallet/invoices/{id}/pay` — first-party and CSRF-checked: pay the invoice from
/// `main` or from the caller's compartment in its scope. The body names only the source and the
/// click's idempotency key; what is paid, how much and to whom is the server's invoice.
pub async fn pay_invoice(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> Result<Json<dto::PayableInvoice>, ApiError> {
	if !first_party_origin(&st, &headers) || !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
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

/// `POST /api/wallet/mandates` — first-party and CSRF-checked: let a scope's service draw on the
/// caller's compartment within the caps the body names. Consent to future debits, so only a
/// page on evinvest.ltd may ask for it — never a scope's own host.
pub async fn grant_mandate(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Mandate>, ApiError> {
	if !first_party_origin(&st, &headers) || !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(scope), Some(client_id), Some(cap_per_op), Some(cap_24h), Some(idempotency_key)) = (
		required(&v, "scope"),
		required(&v, "client_id"),
		required(&v, "cap_per_op"),
		required(&v, "cap_24h"),
		required(&v, "idempotency_key"),
	) else {
		return Err(ApiError::BadRequest("scope, client_id, cap_per_op, cap_24h and idempotency_key are required".into()));
	};
	let operations = mandate_operations(&v)?;
	let expires_at = match v.get("expires_at") {
		None | Some(serde_json::Value::Null) => 0,
		Some(raw) => raw.as_i64().filter(|at| *at >= 0).ok_or_else(|| ApiError::BadRequest("expires_at must be unix seconds".into()))?,
	};
	let req = bk::GrantMandateRequest {
		scope,
		client_id,
		cap_per_op,
		cap_24h,
		operations,
		expires_at,
		idempotency_key,
		cap_outstanding: required(&v, "cap_outstanding").unwrap_or_default(),
		cap_book_fill_24h: required(&v, "cap_book_fill_24h").unwrap_or_default(),
	};
	Ok(Json(st.grpc.grant_mandate(&token, req).await?.into()))
}

/// The body's `operations`, by the names [`dto::mandate_operation_name`] writes.
fn mandate_operations(v: &serde_json::Value) -> Result<Vec<i32>, ApiError> {
	let invalid = || ApiError::BadRequest("operations must be a non-empty list of reserve, charge, book_fill".into());
	let listed = v.get("operations").and_then(serde_json::Value::as_array).filter(|list| !list.is_empty()).ok_or_else(invalid)?;
	listed
		.iter()
		.map(|item| match item.as_str() {
			Some("reserve") => Ok(bk::MandateOperation::Reserve as i32),
			Some("charge") => Ok(bk::MandateOperation::Charge as i32),
			Some("book_fill") => Ok(bk::MandateOperation::BookFill as i32),
			_ => Err(invalid()),
		})
		.collect()
}

/// `POST /api/wallet/mandates/{id}/revoke` — first-party and CSRF-checked: end a mandate; the
/// service's holds on the caller's compartment are released.
pub async fn revoke_mandate(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<dto::Mandate>, ApiError> {
	if !first_party_origin(&st, &headers) || !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_money_token(&st, &jar).await?;
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

/// `PayInvoice` and the mandate routes through the real router, every plane unreachable: the
/// first-party gate answers before anything is dialled.
#[cfg(test)]
mod invoice_gate_tests {
	use std::{collections::HashMap, sync::Arc};

	use axum::{
		body::{Body, to_bytes},
		http::{Request, StatusCode},
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

	const BLACK_HOLE: &str = "http://127.0.0.1:1";
	const FIRST_PARTY: &str = "https://evinvest.ltd";
	const CSRF: &str = "csrf-token";

	fn app() -> axum::Router {
		let env = HashMap::from([
			("PIGGYBANK_GRPC_ADDR", BLACK_HOLE),
			("BANKING_AUTH_GRPC_ADDR", BLACK_HOLE),
			("CONCIERGE_GRPC_ADDR", BLACK_HOLE),
			("BANKING_ISSUANCE_TOKEN", "test-issuance"),
			("AUTH_ISSUER", "https://auth.test"),
			("AUTH_CLIENT_AUDIENCE", "concierge"),
			("MFE_REGISTRY_PATH", "/mfe-registry.json"),
			("CABINET_WS_ORIGIN", FIRST_PARTY),
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
			verifier,
			grpc: Grpc::connect_lazy(BLACK_HOLE, BLACK_HOLE, BLACK_HOLE, Some("test-issuance".into())).expect("build the lazy channels"),
			config: Arc::new(config),
		})
	}

	/// A pay request with a valid CSRF pair and no session, from `origin`.
	async fn pay_from(origin: Option<&str>) -> (StatusCode, String) {
		post_from(
			"/api/wallet/invoices/4f0c2d1e-0000-4000-8000-0123456789ab/pay",
			r#"{"pay_from":"main","idempotency_key":"k-1"}"#,
			origin,
		)
		.await
	}

	/// A POST with a valid CSRF pair and no session, from `origin`.
	async fn post_from(uri: &str, body: &'static str, origin: Option<&str>) -> (StatusCode, String) {
		let mut request = Request::builder()
			.method("POST")
			.uri(uri)
			.header("cookie", format!("ev_csrf={CSRF}"))
			.header("x-ev-csrf", CSRF)
			.header("content-type", "application/json");
		if let Some(origin) = origin {
			request = request.header("origin", origin);
		}
		let request = request.body(Body::from(body)).expect("build the request");
		let response = app().oneshot(request).await.expect("the router responds");
		let status = response.status();
		let body = to_bytes(response.into_body(), 1 << 16).await.expect("read the body");
		(status, String::from_utf8_lossy(&body).into_owned())
	}

	/// A scope's own host may read the wallet but never pay from it, and a request that does
	/// not say where it comes from is not first-party either.
	#[tokio::test]
	async fn only_evinvest_ltd_may_pay_an_invoice() {
		for origin in [Some("https://sa.evinvest.ltd"), Some("https://evil.example"), None] {
			let (status, body) = pay_from(origin).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "{origin:?}: {body}");
			assert!(body.contains("csrf"), "{origin:?}: {body}");
		}
		// Past the gate the next refusal is the missing session — the gate let it through.
		let (status, _) = pay_from(Some(FIRST_PARTY)).await;
		assert_eq!(status, StatusCode::UNAUTHORIZED);
	}

	/// Granting or revoking a mandate moves who may draw on the user's money: like paying, only
	/// a page on evinvest.ltd may do it, never a scope's own host.
	#[tokio::test]
	async fn only_evinvest_ltd_may_grant_or_revoke_a_mandate() {
		const GRANT: &str = r#"{"scope":"invest","client_id":"invest","cap_per_op":"1","cap_24h":"1","operations":["reserve"],"idempotency_key":"k-1"}"#;
		let routes = [("/api/wallet/mandates", GRANT), ("/api/wallet/mandates/4f0c2d1e-0000-4000-8000-0123456789ab/revoke", "")];
		for (uri, body) in routes {
			for origin in [Some("https://sa.evinvest.ltd"), Some("https://evil.example"), None] {
				let (status, response) = post_from(uri, body, origin).await;
				assert_eq!(status, StatusCode::FORBIDDEN, "{uri} from {origin:?}: {response}");
				assert!(response.contains("csrf"), "{uri} from {origin:?}: {response}");
			}
			let (status, _) = post_from(uri, body, Some(FIRST_PARTY)).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: past the gate the next refusal is the missing session");
		}
	}
}
