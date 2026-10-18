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
	routes::{
		approval::{Peer, audit_fields},
		audit_context, first_party_request, money_gate, money_token_for, parse_json_body, require_fresh_login, require_money_token, required,
	},
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
pub async fn move_funds(State(st): State<AppState>, jar: CookieJar, peer: Peer, headers: HeaderMap, body: Bytes) -> Result<Json<dto::CompartmentMove>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	let v = parse_json_body(&headers, &body)?;
	let token = money_token_for(&st, &claims).await?;
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
		audit: Some(audit_context(&st, &headers, peer.addr(), "none")),
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
/// withdrawal of free balance. Money leaving the platform, so a scope's host never asks, and
/// the session must have signed in interactively within [`STEP_UP_MAX_AGE_SECS`] — a stolen
/// cookie alone cannot empty the wallet. The `idempotency_key` is mandatory: a withdrawal is
/// the one act a retried POST must never repeat.
pub async fn request_withdrawal(State(st): State<AppState>, jar: CookieJar, peer: Peer, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Withdrawal>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	require_fresh_login(&claims, now_secs().max(0) as u64)?;
	let v = parse_json_body(&headers, &body)?;
	let (Some(network), Some(address), Some(amount)) = (required(&v, "network"), required(&v, "address"), required(&v, "amount")) else {
		return Err(ApiError::BadRequest("network, address and amount are required".into()));
	};
	let Some(idempotency_key) = required(&v, "idempotency_key") else {
		return Err(ApiError::BadRequest("idempotency_key is required".into()));
	};
	let token = money_token_for(&st, &claims).await?;
	let req = bk::RequestWithdrawalRequest {
		network,
		address,
		amount,
		idempotency_key,
		audit: Some(audit_context(&st, &headers, peer.addr(), "fresh_login")),
	};
	Ok(Json(st.grpc.request_withdrawal(&token, req).await?.into()))
}

/// `POST /api/wallet/withdrawals/cancel` — first-party and CSRF-checked: cancel a still-queued
/// withdrawal. Not rate-limited: it only ever returns money to the caller.
pub async fn cancel_withdrawal(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Withdrawal>, ApiError> {
	first_party_request(&st, &jar, &headers)?;
	let token = require_money_token(&st, &jar).await?;
	let Some(withdrawal_id) = required(&parse_json_body(&headers, &body)?, "withdrawal_id") else {
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
pub async fn pay_invoice(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, peer: Peer, headers: HeaderMap, body: Bytes) -> Result<Json<dto::PayableInvoice>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	let v = parse_json_body(&headers, &body)?;
	let token = money_token_for(&st, &claims).await?;
	let (Some(pay_from), Some(idempotency_key)) = (required(&v, "pay_from"), required(&v, "idempotency_key")) else {
		return Err(ApiError::BadRequest("pay_from and idempotency_key are required".into()));
	};
	// The amount is the server's invoice, never the page's: the step-up threshold is judged
	// on what the hub says will be debited.
	let invoice = st.grpc.get_payable_invoice(&token, &id).await.map_err(|s| ApiError::read(s, "invoice unavailable"))?;
	let stepped_up = exceeds(&invoice.amount, st.config.pay_stepup_threshold_usdt.as_str());
	if stepped_up {
		require_fresh_login(&claims, now_secs().max(0) as u64)?;
	}
	let req = bk::PayInvoiceRequest {
		invoice_id: id,
		pay_from,
		idempotency_key,
		audit: Some(audit_context(&st, &headers, peer.addr(), if stepped_up { "fresh_login" } else { "none" })),
	};
	Ok(Json(st.grpc.pay_invoice(&token, req).await?.into()))
}

/// Whether the decimal USDT string `amount` is strictly above `threshold`. Compared as
/// digits, never through a float: both are money strings. An amount that is not a plain
/// non-negative decimal counts as above — the step-up is the safe side of a value this layer
/// cannot read.
fn exceeds(amount: &str, threshold: &str) -> bool {
	fn split(raw: &str) -> Option<(&str, &str)> {
		let (whole, frac) = raw.split_once('.').unwrap_or((raw, ""));
		let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
		(!whole.is_empty() && digits(whole) && digits(frac)).then_some((whole, frac))
	}
	let (Some((aw, af)), Some((tw, tf))) = (split(amount), split(threshold)) else {
		return true;
	};
	let (aw, tw) = (aw.trim_start_matches('0'), tw.trim_start_matches('0'));
	let (af, tf) = (af.trim_end_matches('0'), tf.trim_end_matches('0'));
	let width = af.len().max(tf.len());
	let pad = |frac: &str| format!("{frac:0<width$}");
	(aw.len(), aw, pad(af)) > (tw.len(), tw, pad(tf))
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
pub async fn grant_mandate(State(st): State<AppState>, jar: CookieJar, peer: Peer, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Mandate>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	require_fresh_login(&claims, now_secs().max(0) as u64)?;
	let v = parse_json_body(&headers, &body)?;
	let token = money_token_for(&st, &claims).await?;
	let (Some(request_id), Some(idempotency_key)) = (required(&v, "request_id"), required(&v, "idempotency_key")) else {
		return Err(ApiError::BadRequest("request_id and idempotency_key are required".into()));
	};
	let req = bk::GrantMandateRequest {
		request_id,
		idempotency_key,
		audit: Some(audit_context(&st, &headers, peer.addr(), "fresh_login")),
	};
	Ok(Json(st.grpc.grant_mandate(&token, req).await?.into()))
}

/// `POST /api/wallet/mandates/{id}/revoke` — first-party, CSRF-checked and rate-limited: end a mandate; the
/// service's holds on the caller's compartment are released.
pub async fn revoke_mandate(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, peer: Peer, headers: HeaderMap) -> Result<Json<dto::Mandate>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	let token = money_token_for(&st, &claims).await?;
	Ok(Json(st.grpc.revoke_mandate(&token, &id, audit_context(&st, &headers, peer.addr(), "none")).await?.into()))
}

// ── payments out of the caller's own claim (their say-so, signed in) ──────────

/// `GET /api/wallet/payments` — the payments out of the caller's claim that wait for their
/// answer, newest first.
pub async fn list_payment_confirmations(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::PaymentConfirmationList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let list = st.grpc.list_payment_confirmations(&token).await.map_err(|s| ApiError::read(s, "payments unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/wallet/payments/{id}` — one such payment, in whatever state it is. An order that
/// is not the caller's, is not answered in the cabinet, or does not exist is the same 404.
pub async fn get_payment_confirmation(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>) -> Result<Json<dto::PaymentConfirmation>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let payment = st.grpc.get_payment_confirmation(&token, &id).await.map_err(|s| ApiError::read(s, "payment unavailable"))?;
	Ok(Json(payment.into()))
}

/// `POST /api/wallet/payments/{id}/answer` — first-party, CSRF-checked and rate-limited: the
/// caller's answer, `{"decision": "approve" | "reject"}`, on a payment out of their claim. The
/// body names only the decision: the terms are the stored order's, so a page cannot approve
/// more than the user was shown. An approval lets money leave, so it needs a fresh login like
/// granting a mandate does; a decline needs none — it is the way to stop an order they did
/// not mean, and must work from a stale session.
pub async fn answer_payment_confirmation(
	State(st): State<AppState>,
	jar: CookieJar,
	Path(id): Path<String>,
	peer: Peer,
	headers: HeaderMap,
	body: Bytes,
) -> Result<Json<dto::PaymentConfirmation>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	let v = parse_json_body(&headers, &body)?;
	let decision = match required(&v, "decision").as_deref() {
		Some("approve") => bk::ConsentDecision::Approve,
		Some("reject") => bk::ConsentDecision::Reject,
		_ => return Err(ApiError::BadRequest("decision must be approve or reject".into())),
	};
	if decision == bk::ConsentDecision::Approve {
		require_fresh_login(&claims, now_secs().max(0) as u64)?;
	}
	let token = money_token_for(&st, &claims).await?;
	let (client_ip, user_agent) = audit_fields(&headers, peer, st.config.trusted_proxy_hops);
	let req = bk::AnswerPaymentConfirmationRequest {
		payment_id: id,
		decision: decision as i32,
		client_ip,
		user_agent,
	};
	Ok(Json(st.grpc.answer_payment_confirmation(&token, req).await?.into()))
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

/// `POST /api/funds/subscribe` — first-party, CSRF-checked and rate-limited: subscribe free balance into a fund (mint units).
pub async fn subscribe(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Subscription>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	let v = parse_json_body(&headers, &body)?;
	let token = money_token_for(&st, &claims).await?;
	let (Some(service), Some(amount)) = (required(&v, "service"), required(&v, "amount")) else {
		return Err(ApiError::BadRequest("service and amount are required".into()));
	};
	let req = bk::SubscribeRequest { service, amount };
	Ok(Json(st.grpc.subscribe(&token, req).await?.into()))
}

/// `POST /api/funds/redeem` — first-party, CSRF-checked and rate-limited: redeem units back to cash (accept-and-queue).
pub async fn redeem(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	let v = parse_json_body(&headers, &body)?;
	let token = money_token_for(&st, &claims).await?;
	let (Some(service), Some(units)) = (required(&v, "service"), required(&v, "units")) else {
		return Err(ApiError::BadRequest("service and units are required".into()));
	};
	let req = bk::RedeemRequest { service, units };
	Ok(Json(st.grpc.redeem(&token, req).await?.into()))
}

/// `POST /api/funds/redemptions/cancel` — first-party and CSRF-checked: cancel a still-queued redemption.
pub async fn cancel_redemption(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	// Cancelling only returns money to the caller, so it is gated like the other first-party
	// mutations but, like `cancel_withdrawal`, kept out of the per-user budget.
	first_party_request(&st, &jar, &headers)?;
	let token = require_money_token(&st, &jar).await?;
	let Some(redemption_id) = required(&parse_json_body(&headers, &body)?, "redemption_id") else {
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
	use evbanking_contracts::banking::v1 as bk;
	use evconcierge_auth::{TokenType, Verifier, VerifierConfig};
	use evconcierge_contracts::concierge::v1::auth_service_server::AuthServiceServer;
	use tokio::net::TcpListener;
	use tokio_stream::wrappers::TcpListenerStream;
	use tonic::transport::Server;
	use tower::ServiceExt;

	use super::exceeds;
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
		util::now_secs,
	};

	const BLACK_HOLE: &str = "http://127.0.0.1:1";
	const FIRST_PARTY: &str = "https://evinvest.ltd";
	const SCOPE_HOST: &str = "https://sa.evinvest.ltd";
	const SCOPE_HOSTS: &str = "https://invest.evinvest.ltd,https://sa.evinvest.ltd";
	const CSRF: &str = "csrf-token";
	const MANDATE: &str = r#"{"request_id":"4f0c2d1e-0000-4000-8000-0123456789ab","idempotency_key":"k-1"}"#;
	const WITHDRAW: &str = r#"{"network":"bep20","address":"0xabc","amount":"1","idempotency_key":"k-1"}"#;
	const MOVE: &str = r#"{"from_scope":"main","to_scope":"sa","amount":"1","idempotency_key":"k-1"}"#;

	/// Every route that moves money or consents to it being drawn, with a body that passes
	/// the BFF's own field checks.
	const MONEY_MUTATIONS: [(&str, &str); 6] = [
		("/api/wallet/compartments/move", MOVE),
		("/api/wallet/withdrawals", WITHDRAW),
		("/api/wallet/invoices/4f0c2d1e-0000-4000-8000-0123456789ab/pay", r#"{"pay_from":"main","idempotency_key":"k-1"}"#),
		("/api/wallet/mandates", MANDATE),
		("/api/wallet/mandates/4f0c2d1e-0000-4000-8000-0123456789ab/revoke", ""),
		("/api/wallet/payments/4f0c2d1e-0000-4000-8000-0123456789ab/answer", r#"{"decision":"reject"}"#),
	];

	struct Env {
		ws_origin: Option<&'static str>,
		cors: &'static str,
		concierge: String,
		/// The stub hub serving both the money plane and the banking token exchange.
		hub: String,
	}

	impl Default for Env {
		fn default() -> Self {
			Self {
				ws_origin: Some(FIRST_PARTY),
				cors: "",
				concierge: BLACK_HOLE.into(),
				hub: BLACK_HOLE.into(),
			}
		}
	}

	fn state(env: Env) -> AppState {
		let mut vars = HashMap::from([
			("PIGGYBANK_GRPC_ADDR", env.hub.clone()),
			("BANKING_AUTH_GRPC_ADDR", env.hub.clone()),
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
			grpc: Grpc::connect_lazy(&env.hub, &env.hub, &env.concierge, Some("test-issuance".into())).expect("build the lazy channels"),
			config: Arc::new(config),
		}
	}

	/// The money plane as the BFF reaches it, for the routes whose verdict depends on what the
	/// hub says or on what reaches it: the banking token exchange, an invoice with a chosen
	/// amount, and a withdrawal that is only recorded. Everything else is unimplemented, so a
	/// route that strays past its gate onto another RPC fails loudly.
	#[derive(Clone)]
	struct StubHub {
		invoice_amount: &'static str,
		withdrawals: Arc<std::sync::Mutex<Vec<bk::RequestWithdrawalRequest>>>,
		payments: Arc<std::sync::Mutex<Vec<bk::PayInvoiceRequest>>>,
	}

	// `Status` is a large error type tonic mandates in handler signatures.
	#[allow(clippy::result_large_err)]
	fn unimplemented_rpc<T>() -> Result<tonic::Response<T>, tonic::Status> {
		Err(tonic::Status::unimplemented("not part of this stub"))
	}

	#[tonic::async_trait]
	impl bk::auth_service_server::AuthService for StubHub {
		async fn issue_user_token(&self, _: tonic::Request<bk::IssueUserTokenRequest>) -> Result<tonic::Response<bk::TokenResponse>, tonic::Status> {
			let now = now_secs();
			Ok(tonic::Response::new(bk::TokenResponse {
				access_token: "stub-money-token".into(),
				access_expires_at: now + 900,
				refresh_token: "stub-refresh".into(),
				refresh_expires_at: now + 3600,
				user: None,
			}))
		}

		async fn refresh(&self, _: tonic::Request<bk::RefreshRequest>) -> Result<tonic::Response<bk::TokenResponse>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn logout(&self, _: tonic::Request<bk::LogoutRequest>) -> Result<tonic::Response<bk::LogoutResponse>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn list_sessions(&self, _: tonic::Request<bk::ListSessionsRequest>) -> Result<tonic::Response<bk::ListSessionsResponse>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn revoke_session(&self, _: tonic::Request<bk::RevokeSessionRequest>) -> Result<tonic::Response<bk::RevokeSessionResponse>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn jwks(&self, _: tonic::Request<bk::JwksRequest>) -> Result<tonic::Response<bk::JwksResponse>, tonic::Status> {
			unimplemented_rpc()
		}
	}

	#[tonic::async_trait]
	impl bk::invoice_payment_service_server::InvoicePaymentService for StubHub {
		async fn get_payable_invoice(&self, request: tonic::Request<bk::GetPayableInvoiceRequest>) -> Result<tonic::Response<bk::PayableInvoice>, tonic::Status> {
			Ok(tonic::Response::new(bk::PayableInvoice {
				invoice_id: request.into_inner().invoice_id,
				amount: self.invoice_amount.into(),
				state: "open".into(),
				..Default::default()
			}))
		}

		async fn pay_invoice(&self, request: tonic::Request<bk::PayInvoiceRequest>) -> Result<tonic::Response<bk::PayableInvoice>, tonic::Status> {
			let request = request.into_inner();
			let invoice_id = request.invoice_id.clone();
			self.payments.lock().expect("the stub's lock").push(request);
			Ok(tonic::Response::new(bk::PayableInvoice {
				invoice_id,
				amount: self.invoice_amount.into(),
				state: "processing".into(),
				..Default::default()
			}))
		}
	}

	#[tonic::async_trait]
	impl bk::wallet_service_server::WalletService for StubHub {
		async fn get_wallet(&self, _: tonic::Request<bk::GetWalletRequest>) -> Result<tonic::Response<bk::Wallet>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn get_deposit_address(&self, _: tonic::Request<bk::GetDepositAddressRequest>) -> Result<tonic::Response<bk::DepositAddress>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn request_withdrawal(&self, request: tonic::Request<bk::RequestWithdrawalRequest>) -> Result<tonic::Response<bk::Withdrawal>, tonic::Status> {
			let request = request.into_inner();
			let reply = bk::Withdrawal {
				id: "w-1".into(),
				network: request.network.clone(),
				address: request.address.clone(),
				amount: request.amount.clone(),
				state: "queued".into(),
				..Default::default()
			};
			self.withdrawals.lock().expect("the stub's lock").push(request);
			Ok(tonic::Response::new(reply))
		}

		async fn cancel_withdrawal(&self, _: tonic::Request<bk::CancelWithdrawalRequest>) -> Result<tonic::Response<bk::Withdrawal>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn list_withdrawals(&self, _: tonic::Request<bk::ListWithdrawalsRequest>) -> Result<tonic::Response<bk::WithdrawalList>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn list_deposits(&self, _: tonic::Request<bk::ListDepositsRequest>) -> Result<tonic::Response<bk::DepositList>, tonic::Status> {
			unimplemented_rpc()
		}
	}

	struct Hub {
		addr: SocketAddr,
		withdrawals: Arc<std::sync::Mutex<Vec<bk::RequestWithdrawalRequest>>>,
		payments: Arc<std::sync::Mutex<Vec<bk::PayInvoiceRequest>>>,
	}

	async fn stub_hub(invoice_amount: &'static str) -> Hub {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("claim an ephemeral port");
		let addr = listener.local_addr().expect("the listener has an address");
		let hub = StubHub {
			invoice_amount,
			withdrawals: Arc::default(),
			payments: Arc::default(),
		};
		let (withdrawals, payments) = (hub.withdrawals.clone(), hub.payments.clone());
		tokio::spawn(async move {
			Server::builder()
				.add_service(bk::auth_service_server::AuthServiceServer::new(hub.clone()))
				.add_service(bk::invoice_payment_service_server::InvoicePaymentServiceServer::new(hub.clone()))
				.add_service(bk::wallet_service_server::WalletServiceServer::new(hub))
				.serve_with_incoming(TcpListenerStream::new(listener))
				.await
				.expect("the stub hub serves");
		});
		Hub { addr, withdrawals, payments }
	}

	/// An app wired to a stub concierge (for the session) and a stub hub (for the money plane).
	async fn app_with_hub(invoice_amount: &'static str) -> (axum::Router, Hub) {
		let concierge = jwks().await;
		let hub = stub_hub(invoice_amount).await;
		let app = router(state(Env {
			concierge: format!("http://{concierge}"),
			hub: format!("http://{}", hub.addr),
			..Env::default()
		}));
		(app, hub)
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

	/// Approving a payment out of the caller's own claim needs a recent interactive sign-in, as
	/// granting a mandate does; declining one — the way to stop an order they did not mean — and
	/// a body that names no decision are not held to it.
	#[tokio::test]
	async fn approving_a_payment_needs_a_fresh_login_but_declining_does_not() {
		let concierge = jwks().await;
		let app = router(state(Env {
			concierge: format!("http://{concierge}"),
			..Env::default()
		}));
		let now = jsonwebtoken::get_current_timestamp();
		let uri = "/api/wallet/payments/4f0c2d1e-0000-4000-8000-0123456789ab/answer";
		for stale in [Some(now - crate::routes::STEP_UP_MAX_AGE_SECS - 60), None] {
			let approve = request_with("POST", uri, r#"{"decision":"approve"}"#, Some(FIRST_PARTY), Some(access_token_at(stale)));
			let (status, _, response) = send(&app, approve).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "auth_time {stale:?}: {response}");
			assert!(response.contains("step_up_required"), "auth_time {stale:?}: {response}");

			let decline = request_with("POST", uri, r#"{"decision":"reject"}"#, Some(FIRST_PARTY), Some(access_token_at(stale)));
			let (_, _, response) = send(&app, decline).await;
			assert!(!response.contains("step_up_required"), "declining asks for no fresh login, auth_time {stale:?}: {response}");

			let nothing = request_with("POST", uri, "{}", Some(FIRST_PARTY), Some(access_token_at(stale)));
			let (status, _, response) = send(&app, nothing).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
		}
		let fresh = request_with("POST", uri, r#"{"decision":"approve"}"#, Some(FIRST_PARTY), Some(access_token_at(Some(now))));
		let (_, _, response) = send(&app, fresh).await;
		assert!(!response.contains("step_up_required"), "a fresh login passes the gate: {response}");
	}

	/// Money leaving the platform needs a recent interactive sign-in, like consent to future
	/// debits: a stale or absent `auth_time` is a `step_up_required` before any hub RPC, a fresh
	/// one reaches the hub with the client's key intact. Cancelling, which only returns money,
	/// is not held to it.
	#[tokio::test]
	async fn withdrawing_needs_a_fresh_login_but_cancelling_does_not() {
		let (app, hub) = app_with_hub("1").await;
		let now = jsonwebtoken::get_current_timestamp();
		for stale in [Some(now - crate::routes::STEP_UP_MAX_AGE_SECS - 60), None] {
			let withdraw = request_with("POST", "/api/wallet/withdrawals", WITHDRAW, Some(FIRST_PARTY), Some(access_token_at(stale)));
			let (status, _, response) = send(&app, withdraw).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "auth_time {stale:?}: {response}");
			assert!(response.contains("step_up_required"), "auth_time {stale:?}: {response}");

			let cancel = request_with(
				"POST",
				"/api/wallet/withdrawals/cancel",
				r#"{"withdrawal_id":"w-1"}"#,
				Some(FIRST_PARTY),
				Some(access_token_at(stale)),
			);
			let (_, _, response) = send(&app, cancel).await;
			assert!(!response.contains("step_up_required"), "cancelling asks for no fresh login, auth_time {stale:?}: {response}");
		}
		assert!(hub.withdrawals.lock().expect("the stub's lock").is_empty(), "a refused withdrawal never reaches the hub");

		let fresh = request_with("POST", "/api/wallet/withdrawals", WITHDRAW, Some(FIRST_PARTY), Some(access_token_at(Some(now))));
		let (status, _, response) = send(&app, fresh).await;
		assert_eq!(status, StatusCode::OK, "{response}");
		let seen = hub.withdrawals.lock().expect("the stub's lock");
		assert_eq!(seen.len(), 1);
		assert_eq!(seen[0].idempotency_key, "k-1", "the client's key reaches the hub unchanged");
	}

	/// A withdrawal without a key is refused here, not defaulted: a retried POST is exactly
	/// what the key exists to absorb. Missing, empty and non-string keys are all the same 400,
	/// and none reaches the hub.
	#[tokio::test]
	async fn a_withdrawal_without_an_idempotency_key_is_refused() {
		let (app, hub) = app_with_hub("1").await;
		let bodies = [
			r#"{"network":"bep20","address":"0xabc","amount":"1"}"#,
			r#"{"network":"bep20","address":"0xabc","amount":"1","idempotency_key":""}"#,
			r#"{"network":"bep20","address":"0xabc","amount":"1","idempotency_key":7}"#,
		];
		for body in bodies {
			let (status, _, response) = send(&app, request("POST", "/api/wallet/withdrawals", body, Some(FIRST_PARTY), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {response}");
			assert!(response.contains("idempotency_key"), "{body}: {response}");
		}
		assert!(hub.withdrawals.lock().expect("the stub's lock").is_empty());
	}

	/// An invoice above the threshold is paid only on a fresh sign-in; at or below it a session
	/// is enough. The amount judged is the hub's invoice, which the page cannot influence.
	#[tokio::test]
	async fn paying_above_the_threshold_needs_a_fresh_login() {
		let uri = "/api/wallet/invoices/4f0c2d1e-0000-4000-8000-0123456789ab/pay";
		let body = r#"{"pay_from":"main","idempotency_key":"k-1"}"#;
		let now = jsonwebtoken::get_current_timestamp();
		let stale = Some(access_token_at(Some(now - crate::routes::STEP_UP_MAX_AGE_SECS - 60)));

		for (amount, needs_step_up) in [
			("100.000001", true),
			("100.01", true),
			("5000", true),
			("100", false),
			("99.99", false),
			("100.000", false),
			("0.5", false),
		] {
			let (app, hub) = app_with_hub(amount).await;
			let (status, _, response) = send(&app, request_with("POST", uri, body, Some(FIRST_PARTY), stale.clone())).await;
			if needs_step_up {
				assert_eq!(status, StatusCode::UNAUTHORIZED, "{amount}: {response}");
				assert!(response.contains("step_up_required"), "{amount}: {response}");
				assert!(hub.payments.lock().expect("the stub's lock").is_empty(), "{amount}: nothing is paid without the step-up");
				let fresh = request_with("POST", uri, body, Some(FIRST_PARTY), Some(access_token_at(Some(now))));
				let (status, _, response) = send(&app, fresh).await;
				assert_eq!(status, StatusCode::OK, "{amount} with a fresh login: {response}");
			} else {
				assert_eq!(status, StatusCode::OK, "{amount} is under the line: {response}");
			}
			assert_eq!(hub.payments.lock().expect("the stub's lock").len(), 1, "{amount}");
		}
	}

	#[test]
	fn the_pay_threshold_is_compared_digit_for_digit() {
		for (amount, threshold, above) in [
			("100", "100", false),
			("100.0", "100", false),
			("100.000000000000000001", "100", true),
			("99.999999999999999999", "100", false),
			("1000", "999", true),
			("0100", "100", false),
			("0", "0", false),
			("0.01", "0", true),
			("", "100", true),
			("-5", "100", true),
			("1e9", "100", true),
			("abc", "100", true),
		] {
			assert_eq!(exceeds(amount, threshold), above, "{amount} against {threshold}");
		}
	}

	/// A body that is not declared `application/json` is a simple cross-site request in
	/// waiting, and every first-party route that reads one refuses it with 415 — after the
	/// session is proved, so the status says nothing to a stranger.
	#[tokio::test]
	async fn a_money_route_reads_only_declared_json() {
		let now = jsonwebtoken::get_current_timestamp();
		let routes = first_party_mutations().into_iter().filter(|(_, body)| !body.is_empty());
		for (uri, body) in routes {
			// A fresh app per route: the probes below would otherwise spend one user's budget.
			let (app, hub) = app_with_hub("1").await;
			for content_type in ["text/plain", "application/x-www-form-urlencoded", "multipart/form-data; boundary=x", "application/jsonp"] {
				let mut req = request_with("POST", uri, body, Some(FIRST_PARTY), Some(access_token_at(Some(now))));
				req.headers_mut().insert(header::CONTENT_TYPE, content_type.parse().expect("a header value"));
				let (status, _, response) = send(&app, req).await;
				assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{uri} as {content_type}: {response}");
			}
			let mut bare = request_with("POST", uri, body, Some(FIRST_PARTY), Some(access_token_at(Some(now))));
			bare.headers_mut().remove(header::CONTENT_TYPE);
			let (status, _, response) = send(&app, bare).await;
			assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{uri} with no content type: {response}");
			assert!(hub.withdrawals.lock().expect("the stub's lock").is_empty());
			assert!(hub.payments.lock().expect("the stub's lock").is_empty());
		}
	}

	/// The browser's own word that a request is cross-site beats a correct-looking `Origin`:
	/// every first-party mutation answers 403 `csrf`, a session or not. Same-origin and
	/// same-site pass the gate; so does a request that carries no such header at all.
	#[tokio::test]
	async fn a_cross_site_fetch_is_refused_whatever_its_origin_says() {
		let (app, hub) = app_with_hub("1").await;
		let routes = first_party_mutations();
		for (uri, body) in &routes {
			for site in ["cross-site", "none", "bogus"] {
				let mut req = request("POST", uri, body, Some(FIRST_PARTY), true);
				req.headers_mut().insert("sec-fetch-site", site.parse().expect("a header value"));
				let (status, _, response) = send(&app, req).await;
				assert_eq!(status, StatusCode::FORBIDDEN, "{uri} as {site}: {response}");
				assert!(response.contains("csrf"), "{uri} as {site}: {response}");
			}
			for site in [Some("same-origin"), Some("same-site"), None] {
				let mut req = request("POST", uri, body, Some(FIRST_PARTY), false);
				if let Some(site) = site {
					req.headers_mut().insert("sec-fetch-site", site.parse().expect("a header value"));
				}
				let (status, _, response) = send(&app, req).await;
				assert_eq!(
					status,
					StatusCode::UNAUTHORIZED,
					"{uri} as {site:?}: past the gate the next refusal is the missing session: {response}"
				);
			}
		}
		assert!(hub.withdrawals.lock().expect("the stub's lock").is_empty());
	}

	/// Every first-party mutation with a body, once: the six money routes, the withdrawal and
	/// redemption cancels, subscribe, redeem and the book's two.
	fn first_party_mutations() -> Vec<(&'static str, &'static str)> {
		MONEY_MUTATIONS
			.iter()
			.copied()
			.chain([
				("/api/wallet/withdrawals/cancel", r#"{"withdrawal_id":"w-1"}"#),
				("/api/funds/subscribe", r#"{"service":"s","amount":"1"}"#),
				("/api/funds/redeem", r#"{"service":"s","units":"1"}"#),
				("/api/funds/redemptions/cancel", r#"{"redemption_id":"r-1"}"#),
				("/api/book/orders", r#"{"service":"s","side":"buy","kind":"limit","size":"1","client_order_id":"c-1"}"#),
				("/api/book/orders/cancel", r#"{"order_id":"o-1"}"#),
			])
			.collect()
	}

	/// Subscribing, redeeming and trading the book move money exactly as a withdrawal or a move
	/// does, so they are first-party too: a scope's host, a stranger, a look-alike and a request
	/// with no `Origin` are all 403 `csrf`, and only evinvest.ltd gets as far as the session.
	#[tokio::test]
	async fn funds_and_the_book_are_first_party_only() {
		let lookalikes = ["https://evinvest.ltd.evil.example", "https://evinvest.ltd:8443", "https://evinvest.ltd/", "null", ""];
		for (uri, body) in first_party_mutations()
			.into_iter()
			.filter(|(uri, _)| uri.starts_with("/api/funds") || uri.starts_with("/api/book"))
		{
			for origin in [Some(SCOPE_HOST), Some("https://evil.example"), None].into_iter().chain(lookalikes.iter().map(|o| Some(*o))) {
				let (status, response) = post_from(uri, body, origin).await;
				assert_eq!(status, StatusCode::FORBIDDEN, "{uri} from {origin:?}: {response}");
				assert!(response.contains("csrf"), "{uri} from {origin:?}: {response}");
			}
			let (status, _) = post_from(uri, body, Some(FIRST_PARTY)).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: past the gate the next refusal is the missing session");
		}
	}

	/// Placing, subscribing and redeeming draw on the same per-user budget as every other
	/// money mutation; cancelling an order or a redemption does not, so a script that burned the
	/// budget cannot also keep the user from taking their own orders off the book.
	#[tokio::test]
	async fn placing_orders_spends_the_money_budget_but_cancelling_does_not() {
		let (app, _hub) = app_with_hub("1").await;
		let place = ("/api/book/orders", r#"{"service":"s","side":"buy","kind":"limit","size":"1","client_order_id":"c-1"}"#);
		for attempt in 0..crate::routes::MONEY_MUTATIONS_PER_WINDOW {
			let (status, _, response) = send(&app, request("POST", place.0, place.1, Some(FIRST_PARTY), true)).await;
			assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "attempt {attempt}: {response}");
		}
		let (status, _, response) = send(&app, request("POST", place.0, place.1, Some(FIRST_PARTY), true)).await;
		assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{response}");
		for (uri, body) in [
			("/api/book/orders/cancel", r#"{"order_id":"o-1"}"#),
			("/api/funds/redemptions/cancel", r#"{"redemption_id":"r-1"}"#),
		] {
			let (status, _, response) = send(&app, request("POST", uri, body, Some(FIRST_PARTY), true)).await;
			assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "{uri}: {response}");
		}
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
