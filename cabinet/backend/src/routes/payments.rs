//! The payments console: `/api/admin/payments/**` — an order that moves money between
//! two named ends of the platform.
//!
//! Money plane, money token: the BFF admits `bank:treasury:read`, the plane holds
//! `bank:payment:open` and re-checks it per RPC. What the BFF decides here is only the shape — a destination that
//! names both an internal party and an address, or neither, is refused before the plane
//! is asked — and the vocabulary: the browser sends lowercase state names and gets them
//! back, exactly as the consilium surface does.
//!
//! There is deliberately no approve or execute verb on this surface. The requirement
//! (the owners' consilium or the subject's consent) is seated by the plane inside
//! `OpenPayment` and answered from a mailbox, through `/api/approval/**`.

use axum::{
	Json,
	body::Bytes,
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
	routes::{require_money_token, require_permission, require_token, verify_csrf},
	state::AppState,
};

#[derive(Deserialize)]
pub struct ListQuery {
	limit: Option<u32>,
	/// A lowercase [`bk::PaymentState`] name; absent lists every state.
	state: Option<String>,
}

/// `POST /api/admin/payments` as the browser sends it. Parsed strictly rather than through
/// the lenient `parse_body`: the destination is a nested either/or, and a lenient read that
/// defaulted half of it away would open an order nobody asked for.
#[derive(Deserialize)]
struct OpenPaymentBody {
	source: Party,
	destination: Destination,
	amount: String,
	reason: String,
}

#[derive(Deserialize)]
struct Party {
	kind: String,
	/// Empty for the two singleton claims, so a browser may omit it.
	#[serde(default)]
	id: String,
}

/// Exactly one of the two must be set — the proto's `oneof`, spelled as JSON.
#[derive(Deserialize)]
struct Destination {
	internal: Option<Party>,
	external: Option<ExternalDestination>,
}

#[derive(Deserialize)]
struct ExternalDestination {
	network: String,
	address: String,
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

/// `POST /api/admin/payments` — CSRF-checked: open an order. The plane derives the tier
/// and the requirement, checks solvency and seats the approval in one transaction; its
/// refusals (an uncovered source, an unconfigured rail, a second open order against the
/// same fund-owned source) come back with their reason.
pub async fn open(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Payment>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	require_permission(&st, &jar, bank::Treasury::Read).await?;
	let token = require_money_token(&st, &jar).await?;
	// The same session's concierge access token: the proof of who is asking, for a payment the
	// owners decide. Concierge accepts it only if minted within the last 120 seconds, and the
	// session renews its access token (every 15 minutes) only when it is about to expire — so
	// nothing here can make it fresh. A stale one fails the opening with that reason; a
	// forced-renewal endpoint on the identity plane is what would remove the failure.
	let proof = require_token(&st, &jar).await?;
	let req = wire(&body)?;
	Ok(Json(st.grpc.open_payment(&token, &proof, req).await?.into()))
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

/// The request body → the wire message, or the one `BadRequest` that names what is
/// missing. Everything the plane can judge better — the amount's precision, the rail, the
/// party's existence — is left to the plane.
fn wire(body: &Bytes) -> Result<bk::OpenPaymentRequest, ApiError> {
	let Ok(body) = serde_json::from_slice::<OpenPaymentBody>(body) else {
		return Err(ApiError::BadRequest("source, destination, amount and reason are required".into()));
	};
	if body.source.kind.is_empty() || body.amount.is_empty() || body.reason.is_empty() {
		return Err(ApiError::BadRequest("source.kind, amount and reason are required".into()));
	}
	let target = match (body.destination.internal, body.destination.external) {
		(Some(party), None) => bk::payment_destination::Target::Internal(party.into()),
		(None, Some(external)) => bk::payment_destination::Target::External(bk::ExternalDestination {
			network: external.network,
			address: external.address,
		}),
		(Some(_), Some(_)) | (None, None) => return Err(ApiError::BadRequest("destination must be exactly one of internal or external".into())),
	};
	Ok(bk::OpenPaymentRequest {
		source: Some(body.source.into()),
		destination: Some(bk::PaymentDestination { target: Some(target) }),
		amount: body.amount,
		reason: body.reason,
	})
}

impl From<Party> for bk::Party {
	fn from(p: Party) -> Self {
		Self { kind: p.kind, id: p.id }
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(json: &str) -> Result<bk::OpenPaymentRequest, ApiError> {
		wire(&Bytes::from(json.to_owned()))
	}

	fn parsed(json: &str) -> bk::OpenPaymentRequest {
		match parse(json) {
			Ok(req) => req,
			Err(ApiError::BadRequest(why)) => panic!("a well-formed order was refused: {why}"),
			Err(_) => panic!("a well-formed order was refused"),
		}
	}

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

	#[test]
	fn an_internal_destination_becomes_the_internal_arm() {
		let req = parsed(r#"{"source":{"kind":"service","id":"fund"},"destination":{"internal":{"kind":"user","id":"u-1"}},"amount":"10.00","reason":"rent"}"#);
		assert_eq!(req.source.as_ref().map(|p| (p.kind.as_str(), p.id.as_str())), Some(("service", "fund")));
		match req.destination.and_then(|d| d.target) {
			Some(bk::payment_destination::Target::Internal(party)) => assert_eq!((party.kind.as_str(), party.id.as_str()), ("user", "u-1")),
			other => panic!("expected the internal arm, got {other:?}"),
		}
		assert_eq!((req.amount.as_str(), req.reason.as_str()), ("10.00", "rent"));
	}

	#[test]
	fn an_external_destination_becomes_the_external_arm() {
		let req = parsed(r#"{"source":{"kind":"user","id":"u-1"},"destination":{"external":{"network":"BEP20","address":"0xabc"}},"amount":"10.00","reason":"rent"}"#);
		match req.destination.and_then(|d| d.target) {
			Some(bk::payment_destination::Target::External(ext)) => assert_eq!((ext.network.as_str(), ext.address.as_str()), ("BEP20", "0xabc")),
			other => panic!("expected the external arm, got {other:?}"),
		}
	}

	/// A destination naming both ends, or neither, is refused HERE: a lenient read that
	/// picked one would open an order the operator did not describe.
	#[test]
	fn a_destination_must_be_exactly_one_of_the_two() {
		let both = r#"{"source":{"kind":"service","id":"fund"},"destination":{"internal":{"kind":"user","id":"u-1"},"external":{"network":"BEP20","address":"0xabc"}},"amount":"10","reason":"rent"}"#;
		let neither = r#"{"source":{"kind":"service","id":"fund"},"destination":{},"amount":"10","reason":"rent"}"#;
		for body in [both, neither] {
			assert!(matches!(parse(body), Err(ApiError::BadRequest(_))), "{body}");
		}
	}

	#[test]
	fn a_malformed_or_hollow_order_is_refused_before_the_plane() {
		let cases = [
			"not json",
			"{}",
			r#"{"source":{"kind":""},"destination":{"internal":{"kind":"user","id":"u-1"}},"amount":"10","reason":"rent"}"#,
			r#"{"source":{"kind":"service","id":"fund"},"destination":{"internal":{"kind":"user","id":"u-1"}},"amount":"","reason":"rent"}"#,
			r#"{"source":{"kind":"service","id":"fund"},"destination":{"internal":{"kind":"user","id":"u-1"}},"amount":"10","reason":""}"#,
		];
		for body in cases {
			assert!(matches!(parse(body), Err(ApiError::BadRequest(_))), "{body}");
		}
	}
}
