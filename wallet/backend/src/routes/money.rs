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
	routes::{Peer, audit_context, first_party_request, money_gate, money_token_for, parse_json_body, require_money_token, required},
	state::AppState,
	wallet_intents::Assurance,
};

/// The machine-readable body the wallet host keys on to offer verification instead of reporting
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
		audit: Some(audit_context(&st, &headers, peer.addr(), Assurance::NONE)),
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

/// `POST /api/wallet/mandates/{id}/revoke` — first-party, CSRF-checked and rate-limited: end a mandate; the
/// service's holds on the caller's compartment are released.
pub async fn revoke_mandate(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>, peer: Peer, headers: HeaderMap) -> Result<Json<dto::Mandate>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	let token = money_token_for(&st, &claims).await?;
	Ok(Json(
		st.grpc.revoke_mandate(&token, &id, audit_context(&st, &headers, peer.addr(), Assurance::NONE)).await?.into(),
	))
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
/// is not the caller's, is not answered in the wallet host, or does not exist is the same 404.
pub async fn get_payment_confirmation(State(st): State<AppState>, jar: CookieJar, Path(id): Path<String>) -> Result<Json<dto::PaymentConfirmation>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let payment = st.grpc.get_payment_confirmation(&token, &id).await.map_err(|s| ApiError::read(s, "payment unavailable"))?;
	Ok(Json(payment.into()))
}

/// `POST /api/wallet/payments/{id}/reject` — first-party, CSRF-checked and rate-limited: decline
/// a payment out of the caller's claim. No step-up: it only stops money leaving, and must work
/// from a stale session. The approval is an intent (`approve_payment`).
pub async fn reject_payment_confirmation(
	State(st): State<AppState>,
	jar: CookieJar,
	Path(id): Path<String>,
	peer: Peer,
	headers: HeaderMap,
) -> Result<Json<dto::PaymentConfirmation>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	let token = money_token_for(&st, &claims).await?;
	let req = bk::AnswerPaymentConfirmationRequest {
		payment_id: id,
		decision: bk::ConsentDecision::Reject as i32,
		audit: Some(audit_context(&st, &headers, peer.addr(), Assurance::NONE)),
	};
	Ok(Json(st.grpc.answer_payment_confirmation(&token, req).await?.into()))
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

	/// The wallet host offers verification off the back of this token. If it ever went back to
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
	use std::{
		net::SocketAddr,
		sync::{
			Arc,
			atomic::{AtomicUsize, Ordering},
		},
	};

	use axum::{
		body::{Body, to_bytes},
		http::{HeaderMap, Request, StatusCode, header},
	};
	use evbanking_contracts::banking::v1 as bk;
	use evconcierge_contracts::concierge::v1::auth_service_server::AuthServiceServer;
	use tokio::net::TcpListener;
	use tokio_stream::wrappers::TcpListenerStream;
	use tonic::{Code, transport::Server};
	use tower::ServiceExt;

	use crate::{
		routes::{
			first_party_origin, router,
			test_support::{self, ConciergeJwks, access_token, access_token_at},
		},
		state::AppState,
		util::now_secs,
		wallet_intents::store::MemoryStore,
	};

	const BLACK_HOLE: &str = "http://127.0.0.1:1";
	const FIRST_PARTY: &str = "https://evinvest.ltd";
	const SCOPE_HOST: &str = "https://sa.evinvest.ltd";
	const SCOPE_HOSTS: &str = "https://invest.evinvest.ltd,https://sa.evinvest.ltd";
	const CSRF: &str = "csrf-token";
	const MOVE: &str = r#"{"from_scope":"main","to_scope":"sa","amount":"1","idempotency_key":"k-1"}"#;
	const PAYMENT: &str = "4f0c2d1e-0000-4000-8000-0123456789ab";

	/// Every first-party route that moves money within the caller's claim or only reduces
	/// risk, with a body that passes the BFF's own field checks. Money out is an intent.
	const MONEY_MUTATIONS: [(&str, &str); 3] = [
		("/api/wallet/compartments/move", MOVE),
		("/api/wallet/mandates/4f0c2d1e-0000-4000-8000-0123456789ab/revoke", ""),
		("/api/wallet/payments/4f0c2d1e-0000-4000-8000-0123456789ab/reject", ""),
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
		let mut vars = test_support::env(&env.concierge, &env.hub);
		vars.insert("WALLET_CORS_ORIGINS", env.cors.into());
		if let Some(origin) = env.ws_origin {
			vars.insert("CABINET_WS_ORIGIN", origin.into());
		}
		test_support::state(&vars, Arc::new(MemoryStore::default()))
	}

	/// The money plane as the BFF reaches it, for the routes whose verdict depends on what the
	/// hub says or on what reaches it: the banking token exchange, a payment answer that is
	/// only recorded, and a wallet read refused with a chosen code. Everything else is
	/// unimplemented, so a route that strays past its gate onto another RPC fails loudly.
	#[derive(Clone)]
	struct StubHub {
		wallet_refusal: Code,
		mints: Arc<AtomicUsize>,
		answers: Arc<std::sync::Mutex<Vec<bk::AnswerPaymentConfirmationRequest>>>,
	}

	// `Status` is a large error type tonic mandates in handler signatures.
	#[allow(clippy::result_large_err)]
	fn unimplemented_rpc<T>() -> Result<tonic::Response<T>, tonic::Status> {
		Err(tonic::Status::unimplemented("not part of this stub"))
	}

	#[tonic::async_trait]
	impl bk::auth_service_server::AuthService for StubHub {
		async fn issue_user_token(&self, _: tonic::Request<bk::IssueUserTokenRequest>) -> Result<tonic::Response<bk::TokenResponse>, tonic::Status> {
			self.mints.fetch_add(1, Ordering::SeqCst);
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
	impl bk::payment_confirmation_service_server::PaymentConfirmationService for StubHub {
		async fn list_payment_confirmations(&self, _: tonic::Request<bk::ListPaymentConfirmationsRequest>) -> Result<tonic::Response<bk::PaymentConfirmationList>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn get_payment_confirmation(&self, _: tonic::Request<bk::GetPaymentConfirmationRequest>) -> Result<tonic::Response<bk::PaymentConfirmation>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn answer_payment_confirmation(&self, request: tonic::Request<bk::AnswerPaymentConfirmationRequest>) -> Result<tonic::Response<bk::PaymentConfirmation>, tonic::Status> {
			let request = request.into_inner();
			let reply = bk::PaymentConfirmation {
				payment_id: request.payment_id.clone(),
				state: bk::PaymentState::Rejected as i32,
				decision: request.decision,
				..Default::default()
			};
			self.answers.lock().expect("the stub's lock").push(request);
			Ok(tonic::Response::new(reply))
		}
	}

	#[tonic::async_trait]
	impl bk::wallet_service_server::WalletService for StubHub {
		async fn get_wallet(&self, _: tonic::Request<bk::GetWalletRequest>) -> Result<tonic::Response<bk::Wallet>, tonic::Status> {
			Err(tonic::Status::new(self.wallet_refusal, "the stub refuses the wallet"))
		}

		async fn get_deposit_address(&self, _: tonic::Request<bk::GetDepositAddressRequest>) -> Result<tonic::Response<bk::DepositAddress>, tonic::Status> {
			unimplemented_rpc()
		}

		async fn request_withdrawal(&self, _: tonic::Request<bk::RequestWithdrawalRequest>) -> Result<tonic::Response<bk::Withdrawal>, tonic::Status> {
			unimplemented_rpc()
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
		mints: Arc<AtomicUsize>,
		answers: Arc<std::sync::Mutex<Vec<bk::AnswerPaymentConfirmationRequest>>>,
	}

	async fn stub_hub(wallet_refusal: Code) -> Hub {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("claim an ephemeral port");
		let addr = listener.local_addr().expect("the listener has an address");
		let hub = StubHub {
			wallet_refusal,
			mints: Arc::default(),
			answers: Arc::default(),
		};
		let (mints, answers) = (hub.mints.clone(), hub.answers.clone());
		tokio::spawn(async move {
			Server::builder()
				.add_service(bk::auth_service_server::AuthServiceServer::new(hub.clone()))
				.add_service(bk::payment_confirmation_service_server::PaymentConfirmationServiceServer::new(hub.clone()))
				.add_service(bk::wallet_service_server::WalletServiceServer::new(hub))
				.serve_with_incoming(TcpListenerStream::new(listener))
				.await
				.expect("the stub hub serves");
		});
		Hub { addr, mints, answers }
	}

	/// An app wired to a stub concierge (for the session) and a stub hub (for the money plane).
	async fn app_with_hub() -> (axum::Router, Hub) {
		app_with_stub(Code::Unimplemented).await
	}

	async fn app_with_stub(wallet_refusal: Code) -> (axum::Router, Hub) {
		let concierge = jwks().await;
		let hub = stub_hub(wallet_refusal).await;
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
	/// `ev_access` cookie verifies. The listener is handed over, never re-bound: binding to
	/// claim a port and letting `serve` bind it again opens a window in which a sibling test,
	/// running in parallel in this process, is handed the port and talks to the wrong stub.
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

	/// Money out has one door, the intent routes: the first-party one-shot routes are gone, so
	/// a page that still posts to them is refused before any session is read.
	#[tokio::test]
	async fn the_one_shot_money_out_routes_are_gone() {
		let (app, hub) = app_with_hub().await;
		for (uri, gone) in [
			("/api/wallet/withdrawals", StatusCode::METHOD_NOT_ALLOWED),
			("/api/wallet/mandates", StatusCode::METHOD_NOT_ALLOWED),
			("/api/wallet/invoices/4f0c2d1e-0000-4000-8000-0123456789ab/pay", StatusCode::NOT_FOUND),
			("/api/wallet/payments/4f0c2d1e-0000-4000-8000-0123456789ab/answer", StatusCode::NOT_FOUND),
		] {
			let (status, _, response) = send(&app, request("POST", uri, r#"{"decision":"approve"}"#, Some(FIRST_PARTY), true)).await;
			assert_eq!(status, gone, "{uri}: {response}");
		}
		assert!(hub.answers.lock().expect("the stub's lock").is_empty());
	}

	/// Declining a payment stops money leaving, so it asks for no step-up and works from a
	/// session whose last sign-in is long past; the hub records it with no assurance.
	#[tokio::test]
	async fn a_payment_is_rejected_from_a_stale_session() {
		let (app, hub) = app_with_hub().await;
		let stale = access_token_at(Some(jsonwebtoken::get_current_timestamp() - 86_400));
		let uri = format!("/api/wallet/payments/{PAYMENT}/reject");
		let (status, _, response) = send(&app, request_with("POST", &uri, "", Some(FIRST_PARTY), Some(stale))).await;
		assert_eq!(status, StatusCode::OK, "{response}");
		let answers = hub.answers.lock().expect("the stub's lock");
		assert_eq!(answers.len(), 1);
		assert_eq!((answers[0].payment_id.as_str(), answers[0].decision()), (PAYMENT, bk::ConsentDecision::Reject));
		let audit = answers[0].audit.as_ref().expect("a rejection carries its audit");
		assert_eq!((audit.origin.as_str(), audit.stepup_method.as_str()), (FIRST_PARTY, "none"));
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
			let (app, _) = app_with_hub().await;
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
		}
	}

	/// The browser's own word that a request is cross-site beats a correct-looking `Origin`:
	/// every first-party mutation answers 403 `csrf`, a session or not. Same-origin and
	/// same-site pass the gate; so does a request that carries no such header at all.
	#[tokio::test]
	async fn a_cross_site_fetch_is_refused_whatever_its_origin_says() {
		let (app, hub) = app_with_hub().await;
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
		assert!(hub.answers.lock().expect("the stub's lock").is_empty());
	}

	/// Every first-party mutation, once: the money routes and the withdrawal cancel.
	fn first_party_mutations() -> Vec<(&'static str, &'static str)> {
		MONEY_MUTATIONS
			.iter()
			.copied()
			.chain([("/api/wallet/withdrawals/cancel", r#"{"withdrawal_id":"w-1"}"#)])
			.collect()
	}

	/// Moving draws on the per-user money budget; cancelling a withdrawal does not, so a script
	/// that burned the budget cannot also keep the user from taking their money back.
	#[tokio::test]
	async fn moving_spends_the_money_budget_but_cancelling_does_not() {
		let (app, _) = app_with_hub().await;
		for attempt in 0..crate::routes::MONEY_MUTATIONS_PER_WINDOW {
			let (status, _, response) = send(&app, request("POST", "/api/wallet/compartments/move", MOVE, Some(FIRST_PARTY), true)).await;
			assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "attempt {attempt}: {response}");
		}
		let (status, _, response) = send(&app, request("POST", "/api/wallet/compartments/move", MOVE, Some(FIRST_PARTY), true)).await;
		assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{response}");
		let cancel = request("POST", "/api/wallet/withdrawals/cancel", r#"{"withdrawal_id":"w-1"}"#, Some(FIRST_PARTY), true);
		let (status, _, response) = send(&app, cancel).await;
		assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "{response}");
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
	/// origin production would accept.
	#[tokio::test]
	async fn an_unset_first_party_origin_closes_the_money_gate() {
		let st = state(Env { ws_origin: None, ..Env::default() });
		let mut headers = HeaderMap::new();
		headers.insert(header::ORIGIN, FIRST_PARTY.parse().expect("a header value"));
		assert!(!first_party_origin(&st, &headers));

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

	/// An `UNAUTHENTICATED` from the money plane — a banking token below the plane's
	/// revoke floor — must drop the cached pair, so the frontend's single heal-replay
	/// mints afresh instead of presenting the same refused token. Pinned by counting
	/// mints: the same two reads against a plane refusing for any OTHER reason share
	/// one mint, which is the cache doing its job.
	#[tokio::test]
	async fn an_unauthenticated_money_rpc_drops_the_cached_pair_so_the_replay_re_mints() {
		async fn mints_over_two_reads(refusal: Code, expected: StatusCode) -> usize {
			let (app, hub) = app_with_stub(refusal).await;
			for _ in 0..2 {
				let (status, _, body) = send(&app, request("GET", "/api/wallet", "", None, true)).await;
				assert_eq!(status, expected, "{body}");
			}
			hub.mints.load(Ordering::SeqCst)
		}

		assert_eq!(
			mints_over_two_reads(Code::Unauthenticated, StatusCode::UNAUTHORIZED).await,
			2,
			"the replay after a 401 must mint a new banking token"
		);
		assert_eq!(
			mints_over_two_reads(Code::PermissionDenied, StatusCode::FORBIDDEN).await,
			1,
			"any other refusal leaves the cached pair in place"
		);
	}
}
