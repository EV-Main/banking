//! The payments console: `/api/admin/payments/**`, the read side of an order that moves
//! money between two named ends of the platform.
//!
//! Money plane, money token: the BFF admits `bank:treasury:read`, and the plane re-checks
//! its own permission per RPC. The browser sends lowercase state names and gets them back,
//! exactly as the consilium surface does.
//!
//! There is deliberately no open verb here (banking#565, F3). The old form relayed the
//! session's access token as the opener's proof without the fresh sign-in gate that the
//! central admin (EV-Main/admin) puts in front of `OpenPayment`, so opening an order lives
//! only there. There is no approve or execute verb either: the plane seats the requirement
//! (the owners' consilium or the subject's consent) on open, and it is answered from a
//! mailbox, through `/api/approval/**`.

use axum::{
	Json,
	extract::{Path, Query, State},
	http::HeaderMap,
};
use axum_extra::extract::cookie::CookieJar;
use concierge_domain::authz::bank;
use evbanking_contracts::banking::v1 as bk;
use serde::Deserialize;

use crate::{
	dto,
	error::ApiError,
	routes::{require_money_token, require_permission, verify_csrf},
	state::AppState,
};

#[derive(Deserialize)]
pub struct ListQuery {
	limit: Option<u32>,
	/// A lowercase [`bk::PaymentState`] name; absent lists every state.
	state: Option<String>,
}

/// `GET /api/admin/payments?state=&limit=` — the history, newest first. Nothing is ever
/// deleted, so a closed order stays readable.
pub async fn list(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ListQuery>) -> Result<Json<dto::PaymentList>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	// An unrecognised word is refused rather than widened to "every state": a filter that
	// silently stops filtering shows the reader rows they did not ask for.
	let state = match q.state.as_deref() {
		None => bk::PaymentState::Unspecified,
		Some(raw) => parse_state(raw).ok_or_else(|| ApiError::BadRequest("state must be a payment state such as \"pending\" or \"executed\"".into()))?,
	};
	let req = bk::ListPaymentsRequest {
		limit: q.limit.unwrap_or(0),
		state: state as i32,
		party: None,
		fund_owned_only: false,
	};
	let list = st.grpc.list_payments(&token, req).await.map_err(|s| ApiError::read(s, "payments unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/admin/payments/{id}` — one order in full, including its consent seat.
pub async fn get(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>) -> Result<Json<dto::Payment>, ApiError> {
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	let payment = st.grpc.get_payment(&token, &id).await.map_err(|s| ApiError::read(s, "payment unavailable"))?;
	Ok(Json(payment.into()))
}

/// `POST /api/admin/payments/{id}/cancel` — CSRF-checked: the initiator withdraws their
/// own still-pending order. The plane refuses anyone else.
pub async fn cancel(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<dto::Payment>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	Ok(Json(st.grpc.cancel_payment(&token, &id).await?.into()))
}

/// The browser's lowercase state name → the proto enum, derived from the generated names
/// the same way `dto::enum_label` derives the outbound word, so a state added upstream is
/// filterable without a new arm here. `unspecified` is not a state and is refused.
fn parse_state(raw: &str) -> Option<bk::PaymentState> {
	let name = format!("PAYMENT_STATE_{}", raw.to_ascii_uppercase());
	bk::PaymentState::from_str_name(&name).filter(|state| *state != bk::PaymentState::Unspecified)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Every lowercase state the screen can filter by round-trips to the proto, and the
	/// two non-states are refused rather than read as "everything".
	#[test]
	fn the_state_filter_speaks_the_browser_vocabulary() {
		assert_eq!(parse_state("pending"), Some(bk::PaymentState::Pending));
		assert_eq!(parse_state("execution_failed"), Some(bk::PaymentState::ExecutionFailed));
		assert_eq!(parse_state("cancelled"), Some(bk::PaymentState::Cancelled));
		assert_eq!(parse_state("unspecified"), None);
		assert_eq!(parse_state("open"), None);
		assert_eq!(parse_state(""), None);
	}
}
