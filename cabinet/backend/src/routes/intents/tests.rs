//! The intent routes through the real router, against a stub concierge (JWKS + `GetMe`) and
//! a stub hub that records what reaches it. Each refusal is asserted together with "the hub
//! saw nothing", because a refusal that still executed would pass a status check alone.

// `Status` is a large error type tonic mandates in handler signatures.
#![allow(clippy::result_large_err)]

use std::{
	collections::HashMap,
	net::SocketAddr,
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
	},
	time::Duration,
};

use axum::{
	body::{Body, to_bytes},
	http::{HeaderMap, Request, StatusCode, header},
};
use evbanking_contracts::banking::v1 as bk;
use evconcierge_auth::{
	TokenType, Verifier, VerifierConfig,
	stepup::{StepupClaims, StepupMethod},
};
use evconcierge_contracts::concierge::v1::{
	self as cc,
	auth_service_server::{AuthService, AuthServiceServer},
	user_directory_server::{UserDirectory, UserDirectoryServer},
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request as GrpcRequest, Response as GrpcResponse, Status, transport::Server};
use tower::ServiceExt;

use crate::{
	config::AppConfig,
	cookies::CookieNames,
	routes::{
		approval::AttemptLimiter,
		router,
		test_support::{AUDIENCE, ConciergeJwks, ISSUER, access_token_for, sign, sign_with_kid},
	},
	session::BankingTokens,
	state::{AppState, Grpc},
	util::now_secs,
	wallet_intents::{
		WalletActions,
		intent::{self, Action, IntentRecord, MoveParams, SessionBinding, Stepup, Summary},
		keys::{self, IntentKeys},
		store::{self, BUDGET_WINDOW_SECS, IntentStatus, MemoryStore, WalletStore},
	},
};

const BLACK_HOLE: &str = "http://127.0.0.1:1";
const INVEST: &str = "https://invest.evinvest.ltd";
const SA: &str = "https://sa.evinvest.ltd";
const BOTH: &str = "https://invest.evinvest.ltd,https://sa.evinvest.ltd";
const KEY: &str = "test-wallet-intent-key-0123456789abcdef";
const USER: &str = "user-1";
const INVOICE: &str = "4f0c2d1e-0000-4000-8000-0123456789ab";
const MANDATE_REQUEST: &str = "5f0c2d1e-0000-4000-8000-0123456789ab";
const BEP20: &str = "0x00000000000000000000000000000000000000ab";

fn now() -> u64 {
	now_secs().max(0) as u64
}

// ── the stub concierge ───────────────────────────────────────────────────────

#[derive(Clone)]
struct Concierge {
	jwks_calls: Arc<AtomicUsize>,
	status: Arc<Mutex<&'static str>>,
	token_version: Arc<AtomicU64>,
}

impl Default for Concierge {
	fn default() -> Self {
		Self {
			jwks_calls: Arc::default(),
			status: Arc::new(Mutex::new("active")),
			token_version: Arc::default(),
		}
	}
}

#[tonic::async_trait]
impl AuthService for Concierge {
	async fn jwks(&self, request: GrpcRequest<cc::JwksRequest>) -> Result<GrpcResponse<cc::JwksResponse>, Status> {
		self.jwks_calls.fetch_add(1, Ordering::SeqCst);
		ConciergeJwks.jwks(request).await
	}

	async fn exchange_code(&self, request: GrpcRequest<cc::ExchangeCodeRequest>) -> Result<GrpcResponse<cc::ClientTokenResponse>, Status> {
		ConciergeJwks.exchange_code(request).await
	}

	async fn refresh_client_token(&self, request: GrpcRequest<cc::RefreshClientTokenRequest>) -> Result<GrpcResponse<cc::ClientTokenResponse>, Status> {
		ConciergeJwks.refresh_client_token(request).await
	}

	async fn refresh(&self, request: GrpcRequest<cc::RefreshRequest>) -> Result<GrpcResponse<cc::TokenResponse>, Status> {
		ConciergeJwks.refresh(request).await
	}

	async fn logout(&self, request: GrpcRequest<cc::LogoutRequest>) -> Result<GrpcResponse<cc::LogoutResponse>, Status> {
		ConciergeJwks.logout(request).await
	}

	async fn list_sessions(&self, request: GrpcRequest<cc::ListSessionsRequest>) -> Result<GrpcResponse<cc::ListSessionsResponse>, Status> {
		ConciergeJwks.list_sessions(request).await
	}

	async fn revoke_session(&self, request: GrpcRequest<cc::RevokeSessionRequest>) -> Result<GrpcResponse<cc::RevokeSessionResponse>, Status> {
		ConciergeJwks.revoke_session(request).await
	}

	async fn publish_catalog(&self, request: GrpcRequest<cc::PublishCatalogRequest>) -> Result<GrpcResponse<cc::PublishCatalogResponse>, Status> {
		ConciergeJwks.publish_catalog(request).await
	}
}

fn not_part_of_the_stub<T>() -> Result<GrpcResponse<T>, Status> {
	Err(Status::unimplemented("not part of this stub"))
}

#[tonic::async_trait]
impl UserDirectory for Concierge {
	async fn get_me(&self, _: GrpcRequest<cc::GetMeRequest>) -> Result<GrpcResponse<cc::UserProfile>, Status> {
		Ok(GrpcResponse::new(cc::UserProfile {
			user_id: USER.into(),
			status: (*self.status.lock().unwrap()).into(),
			token_version: self.token_version.load(Ordering::SeqCst),
			..Default::default()
		}))
	}

	async fn update_profile(&self, _: GrpcRequest<cc::UpdateProfileRequest>) -> Result<GrpcResponse<cc::UserProfile>, Status> {
		not_part_of_the_stub()
	}

	async fn revoke_tokens(&self, _: GrpcRequest<cc::RevokeTokensRequest>) -> Result<GrpcResponse<cc::RevokeTokensResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn disable_user(&self, _: GrpcRequest<cc::DisableUserRequest>) -> Result<GrpcResponse<cc::DisableUserResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn hold_user(&self, _: GrpcRequest<cc::HoldUserRequest>) -> Result<GrpcResponse<cc::HoldUserResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn reinstate_user(&self, _: GrpcRequest<cc::ReinstateUserRequest>) -> Result<GrpcResponse<cc::ReinstateUserResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn set_kyc_level(&self, _: GrpcRequest<cc::SetKycLevelRequest>) -> Result<GrpcResponse<cc::SetKycLevelResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn list_users(&self, _: GrpcRequest<cc::ListUsersRequest>) -> Result<GrpcResponse<cc::ListUsersResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn get_user(&self, _: GrpcRequest<cc::GetUserRequest>) -> Result<GrpcResponse<cc::UserProfile>, Status> {
		not_part_of_the_stub()
	}

	async fn set_role(&self, _: GrpcRequest<cc::SetRoleRequest>) -> Result<GrpcResponse<cc::SetRoleResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn grant_permission(&self, _: GrpcRequest<cc::GrantPermissionRequest>) -> Result<GrpcResponse<cc::GrantPermissionResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn revoke_permission(&self, _: GrpcRequest<cc::RevokePermissionRequest>) -> Result<GrpcResponse<cc::RevokePermissionResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn list_grants(&self, _: GrpcRequest<cc::ListGrantsRequest>) -> Result<GrpcResponse<cc::ListGrantsResponse>, Status> {
		not_part_of_the_stub()
	}
}

// ── the stub hub ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct Seen {
	/// The acts the hub opened: one per idempotency key, as the real hub keeps them.
	moves: Vec<bk::MoveFundsRequest>,
	withdrawals: Vec<bk::RequestWithdrawalRequest>,
	payments: Vec<bk::PayInvoiceRequest>,
	grants: Vec<bk::GrantMandateRequest>,
	/// Every key a move or a withdrawal was sent under, repeats included.
	keys_sent: Vec<String>,
	invoice_reads: usize,
}

impl Seen {
	fn acts(&self) -> usize {
		self.moves.len() + self.withdrawals.len() + self.payments.len() + self.grants.len()
	}
}

#[derive(Clone)]
struct Hub {
	seen: Arc<Mutex<Seen>>,
	invoice: Arc<Mutex<bk::PayableInvoice>>,
	mandate_request: Arc<Mutex<bk::MandateRequest>>,
	/// When set, the next move or withdrawal is committed and its answer then lost — what a
	/// dropped connection or a timeout after the commit looks like to the BFF.
	lose_next_answer: Arc<AtomicBool>,
	/// How long an invoice read takes, so a confirm can be made to cross its expiry.
	invoice_read_delay: Arc<Mutex<Duration>>,
}

impl Hub {
	fn answer(&self) -> Result<(), Status> {
		if self.lose_next_answer.swap(false, Ordering::SeqCst) {
			Err(Status::unavailable("connection reset after the commit"))
		} else {
			Ok(())
		}
	}
}

impl Default for Hub {
	fn default() -> Self {
		Self {
			lose_next_answer: Arc::default(),
			invoice_read_delay: Arc::default(),
			seen: Arc::default(),
			invoice: Arc::new(Mutex::new(bk::PayableInvoice {
				invoice_id: INVOICE.into(),
				scope: "invest".into(),
				display_name: "EV Invest".into(),
				amount: "100".into(),
				state: "open".into(),
				..Default::default()
			})),
			mandate_request: Arc::new(Mutex::new(bk::MandateRequest {
				request_id: MANDATE_REQUEST.into(),
				scope: "invest".into(),
				client_id: "invest-book".into(),
				client_display_name: "Invest book".into(),
				cap_per_op: "500".into(),
				cap_24h: "1000".into(),
				cap_outstanding: "1000".into(),
				operations: vec![bk::MandateOperation::Reserve as i32],
				expires_at: now_secs() + 3600,
				..Default::default()
			})),
		}
	}
}

#[tonic::async_trait]
impl bk::auth_service_server::AuthService for Hub {
	async fn issue_user_token(&self, _: GrpcRequest<bk::IssueUserTokenRequest>) -> Result<GrpcResponse<bk::TokenResponse>, Status> {
		Ok(GrpcResponse::new(bk::TokenResponse {
			access_token: "stub-money-token".into(),
			access_expires_at: now_secs() + 900,
			refresh_token: "stub-refresh".into(),
			refresh_expires_at: now_secs() + 3600,
			user: None,
		}))
	}

	async fn refresh(&self, _: GrpcRequest<bk::RefreshRequest>) -> Result<GrpcResponse<bk::TokenResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn logout(&self, _: GrpcRequest<bk::LogoutRequest>) -> Result<GrpcResponse<bk::LogoutResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn list_sessions(&self, _: GrpcRequest<bk::ListSessionsRequest>) -> Result<GrpcResponse<bk::ListSessionsResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn revoke_session(&self, _: GrpcRequest<bk::RevokeSessionRequest>) -> Result<GrpcResponse<bk::RevokeSessionResponse>, Status> {
		not_part_of_the_stub()
	}

	async fn jwks(&self, _: GrpcRequest<bk::JwksRequest>) -> Result<GrpcResponse<bk::JwksResponse>, Status> {
		not_part_of_the_stub()
	}
}

#[tonic::async_trait]
impl bk::compartment_service_server::CompartmentService for Hub {
	async fn list_compartments(&self, _: GrpcRequest<bk::ListCompartmentsRequest>) -> Result<GrpcResponse<bk::CompartmentList>, Status> {
		not_part_of_the_stub()
	}

	async fn get_compartment(&self, _: GrpcRequest<bk::GetCompartmentRequest>) -> Result<GrpcResponse<bk::Compartment>, Status> {
		not_part_of_the_stub()
	}

	async fn move_funds(&self, request: GrpcRequest<bk::MoveFundsRequest>) -> Result<GrpcResponse<bk::CompartmentMove>, Status> {
		let request = request.into_inner();
		let reply = bk::CompartmentMove {
			id: "m-1".into(),
			from_scope: request.from_scope.clone(),
			to_scope: request.to_scope.clone(),
			amount: request.amount.clone(),
			state: "pending".into(),
			..Default::default()
		};
		{
			let mut seen = self.seen.lock().unwrap();
			seen.keys_sent.push(request.idempotency_key.clone());
			if !seen.moves.iter().any(|first| first.idempotency_key == request.idempotency_key) {
				seen.moves.push(request);
			}
		}
		self.answer()?;
		Ok(GrpcResponse::new(reply))
	}

	async fn get_scope_policy(&self, _: GrpcRequest<bk::GetScopePolicyRequest>) -> Result<GrpcResponse<bk::ScopePolicy>, Status> {
		not_part_of_the_stub()
	}

	async fn get_mandate_request(&self, _: GrpcRequest<bk::GetMandateRequestRequest>) -> Result<GrpcResponse<bk::MandateRequest>, Status> {
		Ok(GrpcResponse::new(self.mandate_request.lock().unwrap().clone()))
	}

	async fn grant_mandate(&self, request: GrpcRequest<bk::GrantMandateRequest>) -> Result<GrpcResponse<bk::Mandate>, Status> {
		self.seen.lock().unwrap().grants.push(request.into_inner());
		Ok(GrpcResponse::new(bk::Mandate {
			mandate_id: "md-1".into(),
			scope: "invest".into(),
			..Default::default()
		}))
	}

	async fn revoke_mandate(&self, _: GrpcRequest<bk::RevokeMandateRequest>) -> Result<GrpcResponse<bk::Mandate>, Status> {
		not_part_of_the_stub()
	}

	async fn list_mandates(&self, _: GrpcRequest<bk::ListMandatesRequest>) -> Result<GrpcResponse<bk::MandateList>, Status> {
		not_part_of_the_stub()
	}
}

#[tonic::async_trait]
impl bk::invoice_payment_service_server::InvoicePaymentService for Hub {
	async fn get_payable_invoice(&self, _: GrpcRequest<bk::GetPayableInvoiceRequest>) -> Result<GrpcResponse<bk::PayableInvoice>, Status> {
		self.seen.lock().unwrap().invoice_reads += 1;
		let delay = *self.invoice_read_delay.lock().unwrap();
		tokio::time::sleep(delay).await;
		Ok(GrpcResponse::new(self.invoice.lock().unwrap().clone()))
	}

	async fn pay_invoice(&self, request: GrpcRequest<bk::PayInvoiceRequest>) -> Result<GrpcResponse<bk::PayableInvoice>, Status> {
		self.seen.lock().unwrap().payments.push(request.into_inner());
		let mut invoice = self.invoice.lock().unwrap().clone();
		invoice.state = "processing".into();
		Ok(GrpcResponse::new(invoice))
	}
}

#[tonic::async_trait]
impl bk::wallet_service_server::WalletService for Hub {
	async fn get_wallet(&self, _: GrpcRequest<bk::GetWalletRequest>) -> Result<GrpcResponse<bk::Wallet>, Status> {
		Ok(GrpcResponse::new(bk::Wallet {
			withdrawable: vec![bk::NetworkWithdrawable {
				network: "bep20".into(),
				withdrawable: "1000".into(),
				instant: "1000".into(),
				min_withdrawal: "5".into(),
				withdrawal_fee: "1".into(),
			}],
			..Default::default()
		}))
	}

	async fn get_deposit_address(&self, _: GrpcRequest<bk::GetDepositAddressRequest>) -> Result<GrpcResponse<bk::DepositAddress>, Status> {
		not_part_of_the_stub()
	}

	async fn request_withdrawal(&self, request: GrpcRequest<bk::RequestWithdrawalRequest>) -> Result<GrpcResponse<bk::Withdrawal>, Status> {
		let request = request.into_inner();
		let reply = bk::Withdrawal {
			id: "w-1".into(),
			network: request.network.clone(),
			address: request.address.clone(),
			amount: request.amount.clone(),
			state: "queued".into(),
			..Default::default()
		};
		{
			let mut seen = self.seen.lock().unwrap();
			seen.keys_sent.push(request.idempotency_key.clone());
			if !seen.withdrawals.iter().any(|first| first.idempotency_key == request.idempotency_key) {
				seen.withdrawals.push(request);
			}
		}
		self.answer()?;
		Ok(GrpcResponse::new(reply))
	}

	async fn cancel_withdrawal(&self, _: GrpcRequest<bk::CancelWithdrawalRequest>) -> Result<GrpcResponse<bk::Withdrawal>, Status> {
		not_part_of_the_stub()
	}

	async fn list_withdrawals(&self, _: GrpcRequest<bk::ListWithdrawalsRequest>) -> Result<GrpcResponse<bk::WithdrawalList>, Status> {
		not_part_of_the_stub()
	}

	async fn list_deposits(&self, _: GrpcRequest<bk::ListDepositsRequest>) -> Result<GrpcResponse<bk::DepositList>, Status> {
		not_part_of_the_stub()
	}
}

/// A listener handed straight to the server, never re-bound (see `admin.rs` for the race).
async fn listener() -> (TcpListener, SocketAddr) {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("claim an ephemeral port");
	let addr = listener.local_addr().expect("the listener has an address");
	(listener, addr)
}

async fn serve_concierge(concierge: Concierge) -> SocketAddr {
	let (listener, addr) = listener().await;
	tokio::spawn(async move {
		Server::builder()
			.add_service(AuthServiceServer::new(concierge.clone()))
			.add_service(UserDirectoryServer::new(concierge))
			.serve_with_incoming(TcpListenerStream::new(listener))
			.await
			.expect("the stub concierge serves");
	});
	addr
}

async fn serve_hub(hub: Hub) -> SocketAddr {
	let (listener, addr) = listener().await;
	tokio::spawn(async move {
		Server::builder()
			.add_service(bk::auth_service_server::AuthServiceServer::new(hub.clone()))
			.add_service(bk::compartment_service_server::CompartmentServiceServer::new(hub.clone()))
			.add_service(bk::invoice_payment_service_server::InvoicePaymentServiceServer::new(hub.clone()))
			.add_service(bk::wallet_service_server::WalletServiceServer::new(hub))
			.serve_with_incoming(TcpListenerStream::new(listener))
			.await
			.expect("the stub hub serves");
	});
	addr
}

// ── the app ──────────────────────────────────────────────────────────────────

fn state(origins: &str, kinds: &str, concierge: &str, hub: &str, store: Arc<MemoryStore>) -> AppState {
	let vars = HashMap::from([
		("PIGGYBANK_GRPC_ADDR", hub.to_owned()),
		("BANKING_AUTH_GRPC_ADDR", hub.to_owned()),
		("CONCIERGE_GRPC_ADDR", concierge.to_owned()),
		("BANKING_ISSUANCE_TOKEN", "test-issuance".into()),
		("AUTH_ISSUER", ISSUER.into()),
		("AUTH_CLIENT_AUDIENCE", AUDIENCE.into()),
		("MFE_REGISTRY_PATH", "/mfe-registry.json".into()),
		("CABINET_WS_ORIGIN", "https://evinvest.ltd".into()),
		("WALLET_ACTION_ORIGINS", origins.to_owned()),
		("WALLET_ACTION_KINDS", kinds.to_owned()),
		("WALLET_INTENT_KEY", KEY.into()),
		("APP_ENV", "development".into()),
	]);
	let config = AppConfig::from_source(|var| vars.get(var).cloned()).expect("the test env loads");
	config.check_wallet_actions().expect("the test env passes the boot check");
	let verifier = Verifier::try_new(VerifierConfig {
		issuer: ISSUER.into(),
		audiences: vec![AUDIENCE.into()],
		allowed_types: vec![TokenType::Access],
		jwks_grpc_endpoint: concierge.to_owned(),
	})
	.expect("build the verifier");
	let wallet_actions = (!origins.is_empty()).then(|| Arc::new(WalletActions::with_store(&config, store, concierge).expect("the wallet actions build")));
	AppState {
		cookies: Arc::new(CookieNames::new(config.cookie_secure())),
		banking: Arc::new(BankingTokens::new()),
		approvals: Arc::new(AttemptLimiter::default()),
		money_mutations: Arc::new(crate::routes::money_mutation_limiter()),
		verifier,
		grpc: Grpc::connect_lazy(hub, hub, concierge, Some("test-issuance".into())).expect("build the lazy channels"),
		wallet_actions,
		config: Arc::new(config),
	}
}

struct Harness {
	app: axum::Router,
	concierge: Concierge,
	hub: Hub,
	store: Arc<MemoryStore>,
	keys: IntentKeys,
}

async fn harness(origins: &str, kinds: &str) -> Harness {
	let (concierge, hub) = (Concierge::default(), Hub::default());
	let concierge_addr = format!("http://{}", serve_concierge(concierge.clone()).await);
	let hub_addr = format!("http://{}", serve_hub(hub.clone()).await);
	let store = Arc::new(MemoryStore::default());
	Harness {
		app: router(state(origins, kinds, &concierge_addr, &hub_addr, store.clone())),
		concierge,
		hub,
		store,
		keys: IntentKeys::derive(KEY.as_bytes()),
	}
}

/// An app whose planes are unreachable: for the refusals that come before any of them.
fn offline(origins: &str) -> axum::Router {
	router(state(origins, "", BLACK_HOLE, BLACK_HOLE, Arc::new(MemoryStore::default())))
}

/// One browser request to an intent route, with everything a real host page would send;
/// each test takes away or changes the one thing it is about.
#[derive(Clone)]
struct Call {
	method: &'static str,
	uri: String,
	body: String,
	origin: Option<String>,
	fetch_site: Option<&'static str>,
	content_type: Option<&'static str>,
	session: Option<String>,
	csrf: Option<String>,
}

impl Call {
	fn post(uri: &str, body: Value) -> Self {
		Self {
			method: "POST",
			uri: uri.to_owned(),
			body: body.to_string(),
			origin: Some(INVEST.into()),
			fetch_site: Some("same-site"),
			content_type: Some("application/json"),
			session: Some(access_token_for(USER, Some(1_790_000_000))),
			csrf: None,
		}
	}

	fn get(uri: &str) -> Self {
		Self {
			method: "GET",
			body: String::new(),
			content_type: None,
			..Self::post(uri, Value::Null)
		}
	}

	fn from(mut self, origin: Option<&str>) -> Self {
		self.origin = origin.map(str::to_owned);
		self
	}

	fn signed_in_as(mut self, sub: &str, auth_time: Option<u64>) -> Self {
		self.session = Some(access_token_for(sub, auth_time));
		self
	}

	fn csrf(mut self, token: &str) -> Self {
		self.csrf = Some(token.to_owned());
		self
	}

	/// The request, with a CSRF token minted for its own account and page unless the test
	/// chose one.
	fn build(self, keys: &IntentKeys) -> Request<Body> {
		let origin = self.origin.clone().unwrap_or_default();
		let sub = self.session.as_deref().map(sub_of).unwrap_or_default();
		let csrf = self.csrf.clone().unwrap_or_else(|| keys.csrf_token(&sub, &origin, now()).0);
		let mut request = Request::builder().method(self.method).uri(&self.uri).header("x-ev-csrf", csrf);
		if let Some(origin) = &self.origin {
			request = request.header(header::ORIGIN, origin);
		}
		if let Some(site) = self.fetch_site {
			request = request.header("sec-fetch-site", site);
		}
		if let Some(content_type) = self.content_type {
			request = request.header(header::CONTENT_TYPE, content_type);
		}
		if let Some(token) = &self.session {
			request = request.header(header::COOKIE, format!("ev_access={token}"));
		}
		request.body(Body::from(self.body)).expect("build the request")
	}
}

/// The `sub` of a test access token, read without verifying it.
fn sub_of(token: &str) -> String {
	let payload = token.split('.').nth(1).expect("a JWT");
	let bytes = base64_url(payload);
	serde_json::from_slice::<Value>(&bytes).expect("JSON claims")["sub"].as_str().expect("a sub").to_owned()
}

fn base64_url(input: &str) -> Vec<u8> {
	const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
	let mut bits = 0u32;
	let mut count = 0;
	let mut out = Vec::new();
	for byte in input.bytes() {
		let value = ALPHABET.iter().position(|&c| c == byte).expect("base64url") as u32;
		bits = (bits << 6) | value;
		count += 6;
		if count >= 8 {
			count -= 8;
			out.push((bits >> count) as u8);
		}
	}
	out
}

struct Answer {
	status: StatusCode,
	headers: HeaderMap,
	body: Value,
}

impl Answer {
	fn error(&self) -> &str {
		self.body["error"].as_str().unwrap_or_default()
	}
}

async fn send(app: &axum::Router, request: Request<Body>) -> Answer {
	let response = app.clone().oneshot(request).await.expect("the router responds");
	let status = response.status();
	let headers = response.headers().clone();
	let bytes = to_bytes(response.into_body(), 1 << 16).await.expect("read the body");
	let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
	Answer { status, headers, body }
}

impl Harness {
	async fn send(&self, call: Call) -> Answer {
		send(&self.app, call.build(&self.keys)).await
	}

	async fn create(&self, origin: &str, body: Value) -> Answer {
		self.send(Call::post("/api/wallet/intents", body).from(Some(origin))).await
	}

	/// Create an intent that must succeed, and return its answer.
	async fn intent(&self, origin: &str, body: Value) -> Value {
		let answer = self.create(origin, body.clone()).await;
		assert_eq!(answer.status, StatusCode::OK, "create {body}: {}", answer.body);
		answer.body
	}

	async fn confirm(&self, origin: &str, intent: &Value, body: Value) -> Answer {
		let uri = format!("/api/wallet/intents/{}/confirm", intent["intent_id"].as_str().expect("an intent id"));
		self.send(Call::post(&uri, body).from(Some(origin))).await
	}

	fn acts(&self) -> usize {
		self.hub.seen.lock().unwrap().acts()
	}
}

fn move_in(scope: &str, amount: &str) -> Value {
	json!({"kind": "move", "params": {"from_scope": "main", "to_scope": scope, "amount": amount}})
}

/// A step-up proof as concierge mints it; each test bends one claim.
struct Proof {
	sub: String,
	intent_hash: String,
	method: StepupMethod,
	origin: String,
	iat: u64,
	exp: u64,
	aud: String,
	jti: String,
}

impl Proof {
	fn for_intent(intent: &Value) -> Self {
		let iat = now();
		Self {
			sub: USER.into(),
			intent_hash: intent["intent_hash"].as_str().expect("an intent hash").into(),
			method: StepupMethod::Passkey,
			origin: INVEST.into(),
			iat,
			exp: iat + 120,
			aud: "banking-bff".into(),
			jti: intent::new_intent_id().expect("a random jti"),
		}
	}

	fn claims(&self) -> StepupClaims {
		StepupClaims {
			sub: self.sub.clone(),
			iss: ISSUER.into(),
			aud: self.aud.clone(),
			exp: self.exp,
			iat: self.iat,
			typ: "stepup".into(),
			intent_hash: self.intent_hash.clone(),
			method: self.method,
			origin: self.origin.clone(),
			jti: self.jti.clone(),
		}
	}

	fn token(&self) -> String {
		sign(&self.claims())
	}

	fn body(&self) -> Value {
		json!({ "proof": self.token() })
	}
}

fn cors_of(headers: &HeaderMap) -> (Option<&str>, Option<&str>) {
	let get = |name| headers.get(name).and_then(|v| v.to_str().ok());
	(get(header::ACCESS_CONTROL_ALLOW_ORIGIN), get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS))
}

const ROUTES: [(&str, &str); 3] = [
	("GET", "/api/wallet/csrf"),
	("POST", "/api/wallet/intents"),
	("POST", "/api/wallet/intents/00000000000000000000000000000000/confirm"),
];

fn preflight(uri: &str, origin: &str, method: &str) -> Request<Body> {
	Request::builder()
		.method("OPTIONS")
		.uri(uri)
		.header(header::ORIGIN, origin)
		.header(header::ACCESS_CONTROL_REQUEST_METHOD, method)
		.header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type,x-ev-csrf")
		.body(Body::empty())
		.expect("build the preflight")
}

// ── the tests ────────────────────────────────────────────────────────────────

/// With no host switched on nothing of this surface exists: every route, preflight included,
/// is the router's plain 404 with no CORS header.
#[tokio::test]
async fn nothing_is_served_when_no_host_is_switched_on() {
	let app = offline("");
	for (method, uri) in ROUTES {
		let answer = send(&app, Call::post(uri, json!({})).build(&IntentKeys::derive(KEY.as_bytes()))).await;
		assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method} {uri}");
		let answer = send(&app, preflight(uri, INVEST, method)).await;
		assert_eq!(answer.status, StatusCode::NOT_FOUND, "OPTIONS {uri}");
		assert_eq!(cors_of(&answer.headers), (None, None), "OPTIONS {uri}");
	}
}

/// A preflight is answered for an enabled host with exactly the spec's headers; anything
/// else gets a bare 403 the browser will not act on.
#[tokio::test]
async fn a_preflight_is_answered_for_an_enabled_host_only() {
	let app = offline(INVEST);
	for (method, uri) in ROUTES {
		let answer = send(&app, preflight(uri, INVEST, method)).await;
		assert_eq!(answer.status, StatusCode::NO_CONTENT, "{uri}");
		let get = |name| answer.headers.get(name).and_then(|v| v.to_str().ok());
		assert_eq!(get(header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(INVEST));
		assert_eq!(get(header::ACCESS_CONTROL_ALLOW_METHODS), Some(method));
		assert_eq!(get(header::ACCESS_CONTROL_ALLOW_HEADERS), Some("content-type, x-ev-csrf"));
		assert_eq!(get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS), Some("true"));
		assert_eq!(get(header::ACCESS_CONTROL_MAX_AGE), Some("60"));
		assert!(answer.headers.get_all(header::VARY).iter().any(|v| v == "origin"));

		for origin in [SA, "https://rogue.evinvest.ltd", "https://evinvest.ltd", "null"] {
			let answer = send(&app, preflight(uri, origin, method)).await;
			assert_eq!(answer.status, StatusCode::FORBIDDEN, "{uri} from {origin}");
			assert_eq!(cors_of(&answer.headers), (None, None), "{uri} from {origin}");
		}
		let other = if method == "GET" { "POST" } else { "GET" };
		assert_eq!(send(&app, preflight(uri, INVEST, other)).await.status, StatusCode::FORBIDDEN, "{uri} asked for {other}");
		let mut extra = preflight(uri, INVEST, method);
		extra
			.headers_mut()
			.insert(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type,x-ev-csrf,authorization".parse().unwrap());
		assert_eq!(send(&app, extra).await.status, StatusCode::FORBIDDEN, "{uri} asked for another header");
	}
}

/// Every page but an enabled host's exact origin is refused before the session is read,
/// and the refusal carries no CORS header.
#[tokio::test]
async fn a_foreign_page_is_refused_before_the_session() {
	let h = harness(INVEST, "move").await;
	let foreign = [
		Some("https://rogue.evinvest.ltd"),
		Some("https://evinvest.ltd.evil.example"),
		Some("null"),
		Some(""),
		Some("https://invest.evinvest.ltd:443"),
		Some("https://invest.evinvest.ltd/wallet"),
		Some("https://invest.evinvest.ltd/"),
		Some("http://invest.evinvest.ltd"),
		Some("https://INVEST.evinvest.ltd"),
		Some("https://evinvest.ltd"),
		Some(SA),
		None,
	];
	for (method, uri) in ROUTES {
		for origin in foreign {
			let call = if method == "GET" { Call::get(uri) } else { Call::post(uri, move_in("invest", "1")) };
			let answer = h.send(call.from(origin)).await;
			assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {uri} from {origin:?}: {}", answer.body);
			assert_eq!(answer.error(), "csrf", "{method} {uri} from {origin:?}");
			assert_eq!(cors_of(&answer.headers), (None, None), "{method} {uri} from {origin:?}");
		}
	}
	assert_eq!(h.acts(), 0);
}

/// `Sec-Fetch-Site` must be present and same-site; unlike the first-party routes a request
/// without it is refused.
#[tokio::test]
async fn fetch_metadata_must_say_same_site() {
	let h = harness(INVEST, "move").await;
	for site in [None, Some("cross-site"), Some("none"), Some("")] {
		let mut call = Call::post("/api/wallet/intents", move_in("invest", "1"));
		call.fetch_site = site;
		let answer = h.send(call).await;
		assert_eq!(answer.status, StatusCode::FORBIDDEN, "{site:?}");
		assert_eq!(answer.error(), "csrf", "{site:?}");
	}
	for site in ["same-origin", "same-site"] {
		let mut call = Call::post("/api/wallet/intents", move_in("invest", "1"));
		call.fetch_site = Some(site);
		call.session = None;
		assert_eq!(
			h.send(call).await.status,
			StatusCode::UNAUTHORIZED,
			"{site}: past the gate the next refusal is the missing session"
		);
	}
}

/// The CSRF route answers the account's own token for the page, uncacheable and readable by
/// that page only; a POST with any other token is refused.
#[tokio::test]
async fn the_csrf_token_is_this_accounts_on_this_page() {
	let h = harness(BOTH, "move").await;
	let answer = h.send(Call::get("/api/wallet/csrf")).await;
	assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
	assert_eq!(cors_of(&answer.headers), (Some(INVEST), Some("true")));
	assert_eq!(answer.headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some("no-store"));
	assert!(answer.headers.get_all(header::VARY).iter().any(|v| v == "origin"));
	let token = answer.body["csrf"].as_str().expect("a token").to_owned();
	assert!(h.keys.csrf_valid(&token, USER, INVEST, now()));

	let ok = h.send(Call::post("/api/wallet/intents", move_in("invest", "1")).csrf(&token)).await;
	assert_eq!(ok.status, StatusCode::OK, "the route's own token passes: {}", ok.body);

	let sa_token = h.keys.csrf_token(USER, SA, now()).0;
	let other_user = h.keys.csrf_token("user-2", INVEST, now()).0;
	let other_key = IntentKeys::derive(b"another-key-0123456789abcdef-0123").csrf_token(USER, INVEST, now()).0;
	let stale = h.keys.csrf_token(USER, INVEST, now() - 2 * keys::CSRF_WINDOW_SECS).0;
	for bad in ["", "garbage", "1.00", sa_token.as_str(), other_user.as_str(), other_key.as_str(), stale.as_str()] {
		let answer = h.send(Call::post("/api/wallet/intents", move_in("invest", "1")).csrf(bad)).await;
		assert_eq!(answer.status, StatusCode::FORBIDDEN, "{bad:?}");
		assert_eq!(answer.error(), "csrf", "{bad:?}");
	}
	assert_eq!(h.acts(), 0);
}

/// A body declared anything but `application/json` is 415, before it is read.
#[tokio::test]
async fn only_declared_json_is_read() {
	let h = harness(INVEST, "move").await;
	for content_type in [
		Some("text/plain"),
		Some("application/x-www-form-urlencoded"),
		Some("multipart/form-data; boundary=x"),
		Some("application/jsonp"),
		None,
	] {
		let mut call = Call::post("/api/wallet/intents", move_in("invest", "1"));
		call.content_type = content_type;
		let answer = h.send(call).await;
		assert_eq!(answer.status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{content_type:?}");
	}
	let malformed = Call {
		body: "{not json".into(),
		..Call::post("/api/wallet/intents", Value::Null)
	};
	assert_eq!(h.send(malformed).await.status, StatusCode::BAD_REQUEST);
}

/// A kind the env does not switch on, or the matrix does not give the host, is refused.
#[tokio::test]
async fn a_kind_outside_the_hosts_kinds_is_refused() {
	let h = harness(BOTH, "move").await;
	let withdraw = json!({"kind": "withdraw", "params": {"network": "bep20", "address": BEP20, "amount": "10"}});
	let pay = json!({"kind": "pay", "params": {"invoice_id": INVOICE, "pay_from": "main"}});
	let mandate = json!({"kind": "mandate", "params": {"request_id": MANDATE_REQUEST}});
	for body in [&withdraw, &pay, &mandate] {
		let answer = h.create(INVEST, body.clone()).await;
		assert_eq!((answer.status, answer.error()), (StatusCode::FORBIDDEN, "kind_not_enabled"), "{body}");
	}
	// The env switches withdrawals on, but the matrix never gives them to the SA panel.
	let h = harness(BOTH, "move,withdraw").await;
	let answer = h.create(SA, withdraw.clone()).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::FORBIDDEN, "kind_not_enabled"));
	assert_eq!(h.create(INVEST, withdraw).await.status, StatusCode::OK);
}

/// A small move runs on the confirm alone, with the intent as the hub's idempotency key and
/// the host's origin in the audit; a second confirm of the same intent is refused.
#[tokio::test]
async fn a_small_move_runs_without_a_proof_exactly_once() {
	let h = harness(INVEST, "move").await;
	let intent = h.intent(INVEST, move_in("invest", "50.50")).await;
	assert_eq!(intent["stepup"], json!({"required": false, "methods": []}));
	assert_eq!(intent["summary"]["amount"], "50.5");
	assert_eq!(intent["summary"]["scope"], "invest");
	assert_eq!(intent["summary"]["host_origin"], INVEST);
	assert_eq!(intent["expires_at"].as_u64().unwrap() - now(), intent::INTENT_TTL_SECS, "two minutes to confirm");

	let answer = h.confirm(INVEST, &intent, json!({})).await;
	assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
	assert_eq!(answer.body["status"], "executed");
	assert_eq!(cors_of(&answer.headers), (Some(INVEST), Some("true")));
	{
		let seen = h.hub.seen.lock().unwrap();
		let moved = &seen.moves[0];
		let id = intent["intent_id"].as_str().unwrap();
		assert_eq!((moved.from_scope.as_str(), moved.to_scope.as_str(), moved.amount.as_str()), ("main", "invest", "50.5"));
		assert_eq!(moved.idempotency_key, id);
		let audit = moved.audit.as_ref().expect("an audit block");
		assert_eq!((audit.origin.as_str(), audit.intent_id.as_str(), audit.stepup_method.as_str()), (INVEST, id, "none"));
	}

	let replay = h.confirm(INVEST, &intent, json!({})).await;
	assert_eq!((replay.status, replay.error()), (StatusCode::CONFLICT, "intent_not_pending"));
	assert_eq!(h.acts(), 1);
}

/// Above the line a move needs a proof, and only one bound to exactly this intent, account,
/// page and an allowed method; each other proof is refused and leaves the intent pending.
#[tokio::test]
async fn a_move_over_the_line_needs_a_proof_bound_to_it() {
	let h = harness(BOTH, "move").await;
	let small = h.intent(INVEST, move_in("invest", "150")).await;
	assert_eq!(small["stepup"], json!({"required": true, "methods": ["passkey", "email_code"]}));
	let large = h.intent(INVEST, move_in("invest", "300")).await;
	assert_eq!(large["stepup"], json!({"required": true, "methods": ["passkey"]}), "over the email cap");

	let answer = h.confirm(INVEST, &small, json!({})).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::FORBIDDEN, "proof_required"));

	let created_at = small["expires_at"].as_u64().unwrap() - intent::INTENT_TTL_SECS;
	let bent: Vec<(&str, Proof)> = vec![
		(
			"another intent",
			Proof {
				intent_hash: large["intent_hash"].as_str().unwrap().into(),
				..Proof::for_intent(&small)
			},
		),
		(
			"another account",
			Proof {
				sub: "user-2".into(),
				..Proof::for_intent(&small)
			},
		),
		(
			"another page",
			Proof {
				origin: SA.into(),
				..Proof::for_intent(&small)
			},
		),
		(
			"another audience",
			Proof {
				aud: "concierge".into(),
				..Proof::for_intent(&small)
			},
		),
		(
			"minted before the intent",
			Proof {
				iat: created_at - 1,
				exp: created_at + 119,
				..Proof::for_intent(&small)
			},
		),
		(
			"expired",
			Proof {
				iat: now() - 200,
				exp: now() - 80,
				..Proof::for_intent(&small)
			},
		),
	];
	for (why, proof) in &bent {
		let answer = h.confirm(INVEST, &small, proof.body()).await;
		assert_eq!((answer.status, answer.error()), (StatusCode::FORBIDDEN, "stepup_invalid"), "{why}: {}", answer.body);
	}
	let email = Proof {
		method: StepupMethod::EmailCode,
		..Proof::for_intent(&large)
	};
	let answer = h.confirm(INVEST, &large, email.body()).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::FORBIDDEN, "stepup_invalid"), "an emailed code above the cap");
	assert_eq!(h.acts(), 0, "no refused proof moved anything");

	let good = Proof::for_intent(&small);
	let answer = h.confirm(INVEST, &small, good.body()).await;
	assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
	assert_eq!(answer.body["stepup_method"], "passkey");
	assert_eq!(h.hub.seen.lock().unwrap().moves[0].audit.as_ref().unwrap().stepup_method, "passkey");

	let again = h.confirm(INVEST, &small, good.body()).await;
	assert_eq!((again.status, again.error()), (StatusCode::CONFLICT, "intent_not_pending"), "a proof authorises its intent once");
	assert_eq!(h.acts(), 1);
}

/// A proof whose `jti` another replica already spent is refused after the intent moved, so
/// the intent is spent too and nothing reaches the hub.
#[tokio::test]
async fn a_replayed_proof_jti_is_refused() {
	let h = harness(INVEST, "move").await;
	let intent = h.intent(INVEST, move_in("invest", "150")).await;
	let proof = Proof::for_intent(&intent);
	assert!(h.store.spend_jti(&proof.jti, 120).await.unwrap(), "another replica spent it first");
	let answer = h.confirm(INVEST, &intent, proof.body()).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::CONFLICT, "proof_replayed"));
	let fresh = Proof::for_intent(&intent);
	let answer = h.confirm(INVEST, &intent, fresh.body()).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::CONFLICT, "intent_not_pending"));
	assert_eq!(h.acts(), 0);
}

/// Store a sealed intent with chosen times, as `create` would have.
async fn plant(h: &Harness, created_at: u64, expires_at: u64) -> Value {
	let record = IntentRecord {
		intent_id: intent::new_intent_id().unwrap(),
		sub: USER.into(),
		origin: INVEST.into(),
		scope: "invest".into(),
		action: Action::Move(MoveParams {
			from_scope: "main".into(),
			to_scope: "invest".into(),
			amount: "1".into(),
		}),
		summary: Summary {
			amount: Some("1".into()),
			fee: Some("0".into()),
			net: Some("1".into()),
			recipient_full: "invest".into(),
			network: None,
			scope: "invest".into(),
			host_origin: INVEST.into(),
			mandate: None,
		},
		stepup: Stepup { required: false, methods: vec![] },
		created_at,
		expires_at,
		session: SessionBinding {
			token_version: 0,
			auth_time: Some(1_790_000_000),
		},
	};
	let bytes = record.to_bytes().unwrap();
	let hash = intent::intent_hash(&bytes);
	h.store.put_intent(&record.intent_id, &bytes, &h.keys.seal(&hash), 600).await.unwrap();
	json!({"intent_id": record.intent_id, "intent_hash": keys::hex(&hash)})
}

#[tokio::test]
async fn an_expired_intent_is_gone() {
	let h = harness(INVEST, "move").await;
	let intent = plant(&h, now() - 200, now() - 80).await;
	let answer = h.confirm(INVEST, &intent, json!({})).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::GONE, "intent_expired"));
	let id = intent["intent_id"].as_str().unwrap();
	assert_eq!(h.store.get_intent(id).await.unwrap().unwrap().status, IntentStatus::Expired);
	let again = h.confirm(INVEST, &intent, json!({})).await;
	assert_eq!(again.status, StatusCode::CONFLICT);
	assert_eq!(h.acts(), 0);
}

/// Whoever can write to the store cannot change a pending intent's terms: the seal fails and
/// the intent is treated as unknown.
#[tokio::test]
async fn a_record_edited_in_the_store_is_not_executed() {
	let h = harness(INVEST, "move").await;
	let intent = h.intent(INVEST, move_in("invest", "1")).await;
	let id = intent["intent_id"].as_str().unwrap();
	let stored = h.store.get_intent(id).await.unwrap().unwrap();
	let edited = String::from_utf8(stored.record).unwrap().replace("\"1\"", "\"99\"");
	h.store.put_intent(id, edited.as_bytes(), &stored.seal, 600).await.unwrap();
	let answer = h.confirm(INVEST, &intent, json!({})).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::NOT_FOUND, "intent_not_found"));
	assert_eq!(h.acts(), 0);
}

/// The confirm body carries the proof and nothing else: an attempt to restate the terms is
/// refused, and the confirmation that follows executes the stored terms.
#[tokio::test]
async fn the_confirm_body_cannot_change_the_terms() {
	let h = harness(INVEST, "move").await;
	let intent = h.intent(INVEST, move_in("invest", "50")).await;
	for body in [
		json!({"amount": "99"}),
		json!({"params": {"from_scope": "main", "to_scope": "invest", "amount": "99"}}),
		json!({"proof": null, "to_scope": "sa"}),
		json!({"scope": "sa"}),
	] {
		let answer = h.confirm(INVEST, &intent, body.clone()).await;
		assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{body}");
	}
	assert_eq!(h.acts(), 0);
	assert_eq!(h.confirm(INVEST, &intent, json!({"proof": null})).await.status, StatusCode::OK);
	assert_eq!(h.hub.seen.lock().unwrap().moves[0].amount, "50");
}

/// Only the account, page and session that created an intent may confirm it.
#[tokio::test]
async fn another_page_account_or_session_cannot_confirm() {
	let h = harness(BOTH, "move").await;
	let intent = h.intent(INVEST, move_in("invest", "1")).await;
	let uri = format!("/api/wallet/intents/{}/confirm", intent["intent_id"].as_str().unwrap());

	let from_sa = h.send(Call::post(&uri, json!({})).from(Some(SA))).await;
	assert_eq!((from_sa.status, from_sa.error()), (StatusCode::NOT_FOUND, "intent_not_found"), "another page");
	let other_user = h.send(Call::post(&uri, json!({})).signed_in_as("user-2", Some(1_790_000_000))).await;
	assert_eq!((other_user.status, other_user.error()), (StatusCode::NOT_FOUND, "intent_not_found"), "another account");
	let other_session = h.send(Call::post(&uri, json!({})).signed_in_as(USER, Some(1_790_000_001))).await;
	assert_eq!((other_session.status, other_session.error()), (StatusCode::FORBIDDEN, "session_changed"), "another sign-in");
	for bad_id in ["../../csrf", "ABCDEF", "00000000000000000000000000000000"] {
		let answer = h.send(Call::post(&format!("/api/wallet/intents/{bad_id}/confirm"), json!({}))).await;
		assert_eq!(answer.status, StatusCode::NOT_FOUND, "{bad_id}");
	}
	assert_eq!(h.acts(), 0);
	assert_eq!(h.confirm(INVEST, &intent, json!({})).await.status, StatusCode::OK, "the creator still can");
}

/// A host acts only within its own scope: it cannot move money into another host's scope,
/// pay another scope's invoice or grant a mandate another scope's service asked for.
#[tokio::test]
async fn a_host_cannot_act_on_another_scope() {
	let h = harness(BOTH, "move,pay,mandate").await;
	let confused = [
		move_in("invest", "1"),
		json!({"kind": "move", "params": {"from_scope": "invest", "to_scope": "main", "amount": "1"}}),
		json!({"kind": "move", "params": {"from_scope": "sa", "to_scope": "invest", "amount": "1"}}),
		json!({"kind": "pay", "params": {"invoice_id": INVOICE, "pay_from": "main"}}),
		json!({"kind": "pay", "params": {"invoice_id": INVOICE, "pay_from": "invest"}}),
		json!({"kind": "mandate", "params": {"request_id": MANDATE_REQUEST}}),
	];
	for body in &confused {
		let answer = h.create(SA, body.clone()).await;
		assert_eq!((answer.status, answer.error()), (StatusCode::FORBIDDEN, "scope_mismatch"), "{body}");
	}
	let scoped_body = json!({"kind": "move", "params": {"from_scope": "main", "to_scope": "invest", "amount": "1"}, "scope": "sa"});
	assert_eq!(h.create(SA, scoped_body).await.status, StatusCode::BAD_REQUEST, "a scope in the body is not read, it is refused");
	assert_eq!(h.create(SA, move_in("sa", "1")).await.status, StatusCode::OK);
	assert_eq!(h.create(INVEST, confused[3].clone()).await.status, StatusCode::OK, "invest pays its own invoice");
	assert_eq!(h.acts(), 0);
}

/// The 24 h line counts what was moved through the host; once it is reached the next move
/// needs a step-up, and of two intents created side by side under it only the first runs.
#[tokio::test]
async fn the_24h_budget_holds_across_intents() {
	let h = harness(INVEST, "move").await;
	for _ in 0..2 {
		let intent = h.intent(INVEST, move_in("invest", "100")).await;
		assert_eq!(h.confirm(INVEST, &intent, json!({})).await.status, StatusCode::OK);
	}
	let first = h.intent(INVEST, move_in("invest", "100")).await;
	let second = h.intent(INVEST, move_in("invest", "100")).await;
	assert_eq!(second["stepup"]["required"], false, "both were created under the line");
	assert_eq!(h.confirm(INVEST, &first, json!({})).await.status, StatusCode::OK);
	let answer = h.confirm(INVEST, &second, json!({})).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::CONFLICT, "limit_reached"));
	assert_eq!(h.acts(), 3);

	let next = h.intent(INVEST, move_in("invest", "1")).await;
	assert_eq!(next["stepup"], json!({"required": true, "methods": ["passkey", "email_code"]}));
}

/// A withdrawal shows the hub's fee and net, asks for a passkey and nothing else, and runs
/// with the host's origin and the passkey in the audit.
#[tokio::test]
async fn a_withdrawal_needs_a_passkey_and_shows_the_hubs_fee() {
	let h = harness(INVEST, "move,withdraw").await;
	let body = json!({"kind": "withdraw", "params": {"network": "bep20", "address": BEP20, "amount": "10"}});
	let intent = h.intent(INVEST, body).await;
	assert_eq!(intent["stepup"], json!({"required": true, "methods": ["passkey"]}));
	let summary = &intent["summary"];
	assert_eq!((summary["amount"].as_str(), summary["fee"].as_str(), summary["net"].as_str()), (Some("10"), Some("1"), Some("9")));
	assert_eq!((summary["recipient_full"].as_str(), summary["network"].as_str()), (Some(BEP20), Some("bep20")));
	assert_eq!((&intent["new_recipient"], &intent["release_at"]), (&Value::Null, &Value::Null), "the hub does not say yet");

	let email = Proof {
		method: StepupMethod::EmailCode,
		..Proof::for_intent(&intent)
	};
	assert_eq!(h.confirm(INVEST, &intent, email.body()).await.error(), "stepup_invalid");
	let answer = h.confirm(INVEST, &intent, Proof::for_intent(&intent).body()).await;
	assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
	{
		let seen = h.hub.seen.lock().unwrap();
		let withdrawal = &seen.withdrawals[0];
		assert_eq!(
			(withdrawal.address.as_str(), withdrawal.amount.as_str(), withdrawal.idempotency_key.as_str()),
			(BEP20, "10", intent["intent_id"].as_str().unwrap())
		);
		let audit = withdrawal.audit.as_ref().unwrap();
		assert_eq!((audit.origin.as_str(), audit.stepup_method.as_str()), (INVEST, "passkey"));
	}

	for (params, why) in [
		(json!({"network": "bep20", "address": "0xnope", "amount": "10"}), "invalid address"),
		(json!({"network": "eth", "address": BEP20, "amount": "10"}), "unknown network"),
		(json!({"network": "bep20", "address": BEP20, "amount": "1"}), "below_minimum"),
		(json!({"network": "bep20", "address": BEP20, "amount": "0"}), "invalid amount"),
	] {
		let answer = h.create(INVEST, json!({"kind": "withdraw", "params": params})).await;
		assert_eq!((answer.status, answer.error()), (StatusCode::BAD_REQUEST, why), "{params}");
	}
	assert_eq!(h.acts(), 1);
}

/// Paying steps up by the invoice's own amount, and a mandate always asks for a passkey; both
/// read their terms from the hub and run with the intent as the key.
#[tokio::test]
async fn pay_and_mandate_step_up_by_the_hubs_terms() {
	let h = harness(INVEST, "move,pay,mandate").await;
	let pay = json!({"kind": "pay", "params": {"invoice_id": INVOICE, "pay_from": "main"}});
	let small = h.intent(INVEST, pay.clone()).await;
	assert_eq!(small["stepup"], json!({"required": true, "methods": ["passkey", "email_code"]}));
	assert_eq!(
		(small["summary"]["amount"].as_str(), small["summary"]["recipient_full"].as_str()),
		(Some("100"), Some("EV Invest"))
	);
	h.hub.invoice.lock().unwrap().amount = "300".into();
	let large = h.intent(INVEST, pay).await;
	assert_eq!(large["stepup"], json!({"required": true, "methods": ["passkey"]}), "over the email cap");

	let email = Proof {
		method: StepupMethod::EmailCode,
		..Proof::for_intent(&large)
	};
	assert_eq!(h.confirm(INVEST, &large, email.body()).await.error(), "stepup_invalid");
	assert_eq!(h.confirm(INVEST, &large, Proof::for_intent(&large).body()).await.status, StatusCode::OK);
	assert_eq!(h.hub.seen.lock().unwrap().payments[0].idempotency_key, large["intent_id"].as_str().unwrap());

	let mandate = h.intent(INVEST, json!({"kind": "mandate", "params": {"request_id": MANDATE_REQUEST}})).await;
	assert_eq!(mandate["stepup"], json!({"required": true, "methods": ["passkey"]}));
	assert_eq!(mandate["summary"]["mandate"]["cap_24h"], "1000");
	assert_eq!(mandate["summary"]["mandate"]["operations"], json!(["MANDATE_OPERATION_RESERVE"]));
	let email = Proof {
		method: StepupMethod::EmailCode,
		..Proof::for_intent(&mandate)
	};
	assert_eq!(h.confirm(INVEST, &mandate, email.body()).await.error(), "stepup_invalid", "a mandate takes a passkey only");
	assert_eq!(h.confirm(INVEST, &mandate, Proof::for_intent(&mandate).body()).await.status, StatusCode::OK);
	let seen = h.hub.seen.lock().unwrap();
	assert_eq!((seen.grants.len(), seen.grants[0].request_id.as_str()), (1, MANDATE_REQUEST));
	assert_eq!(seen.grants[0].audit.as_ref().unwrap().intent_id, mandate["intent_id"].as_str().unwrap());
}

/// Terms the hub changed after the intent was shown are not executed: the intent is spent
/// and the person starts again with what is true now.
#[tokio::test]
async fn terms_that_changed_since_create_are_not_executed() {
	let h = harness(INVEST, "pay").await;
	let intent = h.intent(INVEST, json!({"kind": "pay", "params": {"invoice_id": INVOICE, "pay_from": "main"}})).await;
	h.hub.invoice.lock().unwrap().amount = "100.01".into();
	let answer = h.confirm(INVEST, &intent, Proof::for_intent(&intent).body()).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::CONFLICT, "terms_changed"));
	let answer = h.confirm(INVEST, &intent, Proof::for_intent(&intent).body()).await;
	assert_eq!(answer.error(), "intent_not_pending");
	assert_eq!(h.acts(), 0);
}

/// The account as concierge holds it now decides, not the access token: a suspended
/// account, or one whose sessions were revoked since the token was minted, is refused.
#[tokio::test]
async fn an_account_concierge_no_longer_holds_is_refused() {
	let h = harness(INVEST, "move").await;
	let intent = h.intent(INVEST, move_in("invest", "1")).await;
	*h.concierge.status.lock().unwrap() = "suspended";
	let answer = h.create(INVEST, move_in("invest", "1")).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::FORBIDDEN, "account_not_active"));
	let answer = h.confirm(INVEST, &intent, json!({})).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::FORBIDDEN, "account_not_active"));

	*h.concierge.status.lock().unwrap() = "active";
	h.concierge.token_version.store(1, Ordering::SeqCst);
	let answer = h.confirm(INVEST, &intent, json!({})).await;
	assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "sign out everywhere reaches a token minted before it");
	assert_eq!(h.acts(), 0);
}

/// A forged `kid` costs concierge at most one JWKS call per window, however often it is sent.
#[tokio::test]
async fn an_unknown_signing_key_refreshes_the_jwks_at_most_once_per_window() {
	let h = harness(INVEST, "move").await;
	let intent = h.intent(INVEST, move_in("invest", "150")).await;
	let before = h.concierge.jwks_calls.load(Ordering::SeqCst);
	for _ in 0..5 {
		let forged = sign_with_kid(&Proof::for_intent(&intent).claims(), "rotated-kid");
		let answer = h.confirm(INVEST, &intent, json!({ "proof": forged })).await;
		assert_eq!(answer.error(), "stepup_invalid");
	}
	assert_eq!(h.concierge.jwks_calls.load(Ordering::SeqCst) - before, 1, "the first miss fetches, the rest wait out the window");
	assert_eq!(h.acts(), 0);
}

async fn status_of(h: &Harness, intent: &Value) -> IntentStatus {
	let id = intent["intent_id"].as_str().expect("an intent id");
	h.store.get_intent(id).await.unwrap().expect("the intent is kept").status
}

/// The hub commits the move and the answer is lost. The page confirms the SAME intent again:
/// the act goes out under the same key, the hub opens no second move, and the page gets the
/// first result. Nothing is asked for or spent twice, and an executed intent stays closed.
#[tokio::test]
async fn a_lost_answer_is_resent_under_the_same_key_and_opens_one_act() {
	let h = harness(INVEST, "move").await;
	let intent = h.intent(INVEST, move_in("invest", "150")).await;
	let id = intent["intent_id"].as_str().unwrap().to_owned();
	let proof = Proof::for_intent(&intent);
	h.hub.lose_next_answer.store(true, Ordering::SeqCst);
	let lost = h.confirm(INVEST, &intent, proof.body()).await;
	assert_eq!((lost.status, lost.error()), (StatusCode::SERVICE_UNAVAILABLE, "intent_executing"));
	assert_eq!(cors_of(&lost.headers), (Some(INVEST), Some("true")), "the page can read that it may resend");
	assert_eq!(status_of(&h, &intent).await, IntentStatus::Executing);

	let resent = h.confirm(INVEST, &intent, json!({})).await;
	assert_eq!(resent.status, StatusCode::OK, "no proof is asked for again: {}", resent.body);
	assert_eq!((resent.body["status"].as_str(), resent.body["stepup_method"].as_str()), (Some("executed"), Some("passkey")));
	assert_eq!(resent.body["result"]["amount"], "150");
	{
		let seen = h.hub.seen.lock().unwrap();
		assert_eq!(seen.keys_sent, vec![id.clone(), id.clone()], "the same key both times");
		assert_eq!(seen.moves.len(), 1, "one move opened");
	}
	assert!(!h.store.spend_jti(&proof.jti, 60).await.unwrap(), "the proof's jti was spent on the first send");
	let moved = h.store.used_since(&store::move_budget(USER), now() - BUDGET_WINDOW_SECS).await.unwrap();
	assert_eq!(moved, 150_000_000, "the move is counted once");
	assert_eq!(status_of(&h, &intent).await, IntentStatus::Executed);

	let again = h.confirm(INVEST, &intent, json!({})).await;
	assert_eq!((again.status, again.error()), (StatusCode::CONFLICT, "intent_not_pending"), "an executed intent is answered once");
	assert_eq!(h.hub.seen.lock().unwrap().keys_sent.len(), 2);
}

/// The same for a withdrawal: money leaving the platform is sent once whatever happens to the
/// answer.
#[tokio::test]
async fn a_withdrawal_whose_answer_was_lost_is_not_opened_twice() {
	let h = harness(INVEST, "move,withdraw").await;
	let intent = h
		.intent(INVEST, json!({"kind": "withdraw", "params": {"network": "bep20", "address": BEP20, "amount": "900"}}))
		.await;
	h.hub.lose_next_answer.store(true, Ordering::SeqCst);
	let lost = h.confirm(INVEST, &intent, Proof::for_intent(&intent).body()).await;
	assert_eq!(lost.error(), "intent_executing");
	let resent = h.confirm(INVEST, &intent, Proof::for_intent(&intent).body()).await;
	assert_eq!(
		resent.status,
		StatusCode::OK,
		"a proof sent along on the resend is not needed and not in the way: {}",
		resent.body
	);
	let seen = h.hub.seen.lock().unwrap();
	assert_eq!((seen.keys_sent.len(), seen.withdrawals.len()), (2, 1));
	assert_eq!(seen.withdrawals[0].idempotency_key, intent["intent_id"].as_str().unwrap());
}

/// An intent whose act was sent before it expired can still be resent after: refusing would
/// leave a new intent, with a new key, as the only way on.
#[tokio::test]
async fn an_executing_intent_can_be_resent_after_its_expiry() {
	let h = harness(INVEST, "move").await;
	let intent = plant(&h, now() - 200, now() - 80).await;
	let id = intent["intent_id"].as_str().unwrap();
	assert!(h.store.transition(id, IntentStatus::Created, IntentStatus::Authorized).await.unwrap());
	assert!(h.store.begin_execution(id, "none", 3_600).await.unwrap());
	let answer = h.confirm(INVEST, &intent, json!({})).await;
	assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
	assert_eq!(answer.body["stepup_method"], "none", "the method the act was authorised with");
	assert_eq!(h.hub.seen.lock().unwrap().moves[0].audit.as_ref().unwrap().stepup_method, "none");
}

/// The expiry is checked again right before `created → authorized`: the reads of a confirm
/// can take seconds, and an intent that expired during them is not authorised.
#[tokio::test]
async fn an_intent_that_expires_during_its_confirm_is_not_authorised() {
	let h = harness(INVEST, "pay").await;
	let created = h.intent(INVEST, json!({"kind": "pay", "params": {"invoice_id": INVOICE, "pay_from": "main"}})).await;
	// The same intent, re-sealed to end two seconds from now.
	let id = created["intent_id"].as_str().unwrap();
	let stored = h.store.get_intent(id).await.unwrap().unwrap();
	let mut record: IntentRecord = serde_json::from_slice(&stored.record).unwrap();
	record.expires_at = now() + 2;
	let bytes = record.to_bytes().unwrap();
	let hash = intent::intent_hash(&bytes);
	h.store.put_intent(id, &bytes, &h.keys.seal(&hash), 600).await.unwrap();
	let intent = json!({"intent_id": id, "intent_hash": keys::hex(&hash)});

	*h.hub.invoice_read_delay.lock().unwrap() = Duration::from_secs(3);
	let reads_before = h.hub.seen.lock().unwrap().invoice_reads;
	let answer = h.confirm(INVEST, &intent, Proof::for_intent(&intent).body()).await;
	assert_eq!((answer.status, answer.error()), (StatusCode::GONE, "intent_expired"), "{}", answer.body);
	assert_eq!(
		h.hub.seen.lock().unwrap().invoice_reads,
		reads_before + 1,
		"it expired after the first check, during the terms re-read"
	);
	assert_eq!(status_of(&h, &intent).await, IntentStatus::Expired);
	assert_eq!(h.acts(), 0);
}

/// The 24 h line of moves without a step-up is one total per person across hosts: what was
/// moved through invest counts on the SA panel too.
#[tokio::test]
async fn the_move_budget_is_one_total_across_hosts() {
	let h = harness(BOTH, "move").await;
	for (origin, scope) in [(INVEST, "invest"), (INVEST, "invest"), (SA, "sa")] {
		let intent = h.intent(origin, move_in(scope, "100")).await;
		assert_eq!(intent["stepup"]["required"], false, "{origin}");
		assert_eq!(h.confirm(origin, &intent, json!({})).await.status, StatusCode::OK, "{origin}");
	}
	let next = h.intent(SA, move_in("sa", "1")).await;
	assert_eq!(next["stepup"]["required"], true, "300 already moved, through two hosts");
}

/// Emailed codes confirm at most 500 USDT a day per person: two 250 payments pass, a third
/// one created before them is refused at confirm, and the next one is offered a passkey only.
#[tokio::test]
async fn emailed_codes_stop_at_the_daily_total() {
	let h = harness(INVEST, "pay").await;
	h.hub.invoice.lock().unwrap().amount = "250".into();
	let pay = json!({"kind": "pay", "params": {"invoice_id": INVOICE, "pay_from": "main"}});
	let intents = [h.intent(INVEST, pay.clone()).await, h.intent(INVEST, pay.clone()).await, h.intent(INVEST, pay.clone()).await];
	for intent in &intents {
		assert_eq!(intent["stepup"]["methods"], json!(["passkey", "email_code"]), "nothing emailed yet when each was created");
	}
	let email = |intent: &Value| {
		Proof {
			method: StepupMethod::EmailCode,
			..Proof::for_intent(intent)
		}
		.body()
	};
	assert_eq!(h.confirm(INVEST, &intents[0], email(&intents[0])).await.status, StatusCode::OK);
	assert_eq!(h.confirm(INVEST, &intents[1], email(&intents[1])).await.status, StatusCode::OK);
	let third = h.confirm(INVEST, &intents[2], email(&intents[2])).await;
	assert_eq!((third.status, third.error()), (StatusCode::CONFLICT, "email_limit_reached"));
	assert_eq!(status_of(&h, &intents[2]).await, IntentStatus::Cancelled);
	assert_eq!(h.acts(), 2);

	let next = h.intent(INVEST, pay).await;
	assert_eq!(next["stepup"]["methods"], json!(["passkey"]), "the day's emailed total is used up");
	assert_eq!(
		h.confirm(INVEST, &next, Proof::for_intent(&next).body()).await.status,
		StatusCode::OK,
		"a passkey is not limited by it"
	);
}
