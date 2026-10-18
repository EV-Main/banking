//! The scope ledger's own listener, over a real mTLS handshake — the guard that the service
//! is never reachable without its two layers.
//!
//! Each test stands up [`scope_ledger::serve_with`] (or production's [`scope_ledger::serve`])
//! on an ephemeral loopback port under the fixtures in `fixtures/scope_tls/`, and dials it
//! the way a client service would. What is asserted is what reached the handler: a call
//! without a known certificate or without a service token must never get there, and one with
//! both must. The token side uses a stub authenticator for the classes a mounted layer can
//! hand the service (a service token, a user's access token) — the cryptographic class split
//! is the auth crate's and is tested there; this file proves the layer is mounted at all, and
//! the one test on `serve` proves production's function mounts the real one.
//!
//! The handler reads Postgres (the scope registry, the store), so the suite needs
//! `DATABASE_URL` and TigerBeetle like its siblings; it skips locally without them.
//! Concurrency is structured: the server and the client run as branches of one
//! `tokio::join!` under one cancellation token.

use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};

use domain::scopes::ScopeId;
use evbanking_auth::{AuthConfig, AuthError, AuthService, Authenticate, Claims, MintService, SigningConfig, TokenType, provisioner_channel};
use evbanking_contracts::scope::v1::{
	ChargeRequest, CreateMandateRequestRequest, GetBalancesRequest, GetBalancesResponse, GetInvoiceRequest, MandateOperation, MintServiceTokenRequest, MintServiceTokenResponse, PullRequest,
	invoice_service_client::InvoiceServiceClient, mandate_request_service_client::MandateRequestServiceClient, money_events_client::MoneyEventsClient,
	scope_auth_service_client::ScopeAuthServiceClient, scope_ledger_service_client::ScopeLedgerServiceClient,
};
use piggybank_core::{
	config::ScopeLedgerSettings,
	infrastructure::{compartments::PgCompartments, scope_ledger::PgScopeLedger},
	services::scope_ledger::{self, ScopeLedgerDeps, ScopeLedgerListener},
};
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;
use tonic::{
	Code, Request, Status,
	codegen::http,
	metadata::MetadataValue,
	transport::{Certificate, ClientTlsConfig, Endpoint, Identity},
};
use uuid::Uuid;

mod common;

fn fixture(name: &str) -> String {
	let path = format!("{}/tests/fixtures/scope_tls/{name}", env!("CARGO_MANIFEST_DIR"));
	std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("fixture {name} must be readable ({err}); regenerate with fixtures/scope_tls/regen.sh"))
}

/// The bearers a stub caller presents: `<class>:<sub>`, the class a real verifier would have
/// found in the token and the subject it was issued to.
const SERVICE_TOKEN: &str = "service:invest";
const PANEL_TOKEN: &str = "service:panel";
const ACCESS_TOKEN: &str = "access:invest";

/// Stands in for the hub's authorizer: answers a service token and a user's access token
/// with the claims the real one would, and refuses anything else as the real one does.
#[derive(Clone)]
struct StubAuthenticator;

impl Authenticate for StubAuthenticator {
	async fn authenticate(&self, token: String) -> Result<Claims, AuthError> {
		let (typ, sub) = match token.split_once(':') {
			Some(("service", sub)) => (TokenType::Service, sub),
			Some(("access", sub)) => (TokenType::Access, sub),
			_ => return Err(AuthError::InvalidToken),
		};
		Ok(Claims {
			sub: sub.into(),
			iss: "https://auth.test".into(),
			aud: "banking-services".into(),
			exp: u64::MAX,
			iat: 0,
			typ,
			jti: None,
			token_version: 0,
		})
	}
}

/// Stands in for the auth task's signer: issues the stub class's own bearer for the `sub` it
/// is asked for, so a minted token is accepted by [`StubAuthenticator`] exactly as far as the
/// `sub` is the client's.
#[derive(Clone)]
struct StubMinter;

impl MintService for StubMinter {
	async fn mint_service(&self, sub: &str) -> Result<(String, u64), AuthError> {
		Ok((format!("service:{sub}"), u64::MAX >> 1))
	}
}

struct Harness {
	scope: ScopeId,
	deps: ScopeLedgerDeps,
}

/// A scope of the test's own, granted to the `invest` fixture client.
async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "scope ledger listener tests").await?;
	let scope = ScopeId::parse(&format!("l_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	sqlx::query("INSERT INTO scopes (id, display_name) VALUES ($1, 'Listener scope')")
		.bind(scope.as_str())
		.execute(&pool)
		.await
		.unwrap();
	let store = Arc::new(PgScopeLedger::new(pool.clone()));
	Some(Harness {
		deps: ScopeLedgerDeps {
			store: store.clone(),
			holds: store.clone(),
			events: store.clone(),
			invoices: store.clone(),
			mandate_requests: store.clone(),
			scopes: Arc::new(PgCompartments::new(pool.clone())),
			ledger,
			relay: Arc::new(Notify::new()),
			payouts: store.clone(),
			operations: store,
			executor: Arc::new(Notify::new()),
		},
		scope,
	})
}

/// The settings production would load: the fixture server identity, the fixture CA, and a
/// grants file that knows `invest` and `panel` by their certificate fingerprints.
fn settings(bind: SocketAddr, scope: &ScopeId) -> ScopeLedgerSettings {
	settings_with(bind, scope, &[])
}

/// [`settings`] with `invest` carrying more fingerprints too (a certificate rotation in
/// progress). `panel` holds a grant elsewhere, so it is a client that may mint yet has none in
/// the harness's scope.
fn settings_with(bind: SocketAddr, scope: &ScopeId, invest_also: &[&str]) -> ScopeLedgerSettings {
	let invest_prints = std::iter::once(fixture("invest.sha256").trim().to_owned())
		.chain(invest_also.iter().map(|name| fixture(name).trim().to_owned()))
		.map(|print| format!("\"{print}\""))
		.collect::<Vec<_>>()
		.join(",");
	let grants = format!(
		r#"{{"clients":[{{"client_id":"invest","cert_sha256":[{invest_prints}]}},{{"client_id":"panel","cert_sha256":["{}"]}}],
		    "grants":[{{"client_id":"invest","scope":"{scope}","label_prefix":"alloc:"}},
		              {{"client_id":"panel","scope":"elsewhere","label_prefix":"panel:"}}]}}"#,
		fixture("panel.sha256").trim()
	);
	let (clients, grants) = ScopeLedgerSettings::parse_grants(&grants).expect("the test grants are valid");
	ScopeLedgerSettings {
		bind,
		cert_pem: fixture("server.pem"),
		key_pem: fixture("server.key"),
		client_ca_pem: fixture("ca.pem"),
		clients,
		grants,
	}
}

async fn ephemeral() -> SocketAddr {
	// Bind once to claim a free port, then let tonic re-bind it (as `bridge_tls.rs` does).
	TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap()
}

/// Until something accepts TCP on `port` — the listener binds asynchronously, and a client
/// dialling before it would read "connection refused" as the outcome under test.
async fn wait_listening(port: u16) {
	let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
	while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
		assert!(tokio::time::Instant::now() < deadline, "the listener never bound :{port}");
		tokio::time::sleep(Duration::from_millis(10)).await;
	}
}

/// Run `server` concurrently with `client`, once the server accepts on `port`; the server
/// stops when the client is done.
async fn with_server<T, S, F, Fut>(port: u16, server: impl FnOnce(CancellationToken) -> S, client: F) -> T
where
	S: Future<Output = Result<(), tonic::transport::Error>>,
	F: FnOnce() -> Fut,
	Fut: Future<Output = T>, {
	// The provider production installs, by the function production calls (`crypto.rs`).
	piggybank_core::crypto::install_crypto_provider();
	let stop = CancellationToken::new();
	let serve = server(stop.clone());
	let drive = async {
		wait_listening(port).await;
		let out = client().await;
		stop.cancel();
		out
	};
	let (served, out) = tokio::join!(serve, drive);
	served.expect("the listener serves until cancelled");
	out
}

/// One `GetBalances` over TLS, presenting `identity` (a fixture basename) if any.
async fn get_balances(port: u16, identity: Option<(&str, &str)>, bearer: Option<&str>, scope: &ScopeId) -> Result<GetBalancesResponse, Status> {
	let mut tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(fixture("ca.pem"))).domain_name("localhost");
	if let Some((cert, key)) = identity {
		tls = tls.identity(Identity::from_pem(fixture(cert), fixture(key)));
	}
	let endpoint = Endpoint::from_shared(format!("https://localhost:{port}"))
		.unwrap()
		.connect_timeout(Duration::from_secs(5))
		.tls_config(tls)
		.unwrap();
	let mut client = ScopeLedgerServiceClient::new(endpoint.connect_lazy());
	let mut request = Request::new(GetBalancesRequest {
		scope: scope.as_str().to_owned(),
		labels: Vec::new(),
	});
	if let Some(token) = bearer {
		let value: MetadataValue<_> = format!("Bearer {token}").parse().unwrap();
		request.metadata_mut().insert("authorization", value);
	}
	client.get_balances(request).await.map(tonic::Response::into_inner)
}

async fn serve_stub(h: &Harness, addr: SocketAddr) -> impl FnOnce(CancellationToken) -> std::pin::Pin<Box<dyn Future<Output = Result<(), tonic::transport::Error>>>> {
	let listener = ScopeLedgerListener::new(settings(addr, &h.scope), h.deps.clone());
	move |stop: CancellationToken| Box::pin(scope_ledger::serve_with(listener, StubAuthenticator, StubMinter, stop.cancelled_owned())) as std::pin::Pin<Box<dyn Future<Output = _>>>
}

#[tokio::test]
async fn a_known_client_with_a_service_token_reaches_the_service_in_its_scope() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let scope = h.scope.clone();
	let response = with_server(addr.port(), server, || async {
		get_balances(addr.port(), Some(("invest.pem", "invest.key")), Some(SERVICE_TOKEN), &scope).await
	})
	.await;
	assert!(response.expect("the handler answered").accounts.is_empty(), "invest owns nothing there yet");
}

#[tokio::test]
async fn the_client_id_is_the_certificates_whatever_the_token_says() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let scope = h.scope.clone();
	// `panel` is a known client with no grant in the scope: what it may do is decided by its
	// certificate, and invest's token does not make it invest.
	let (own, borrowed) = with_server(addr.port(), server, || async {
		(
			get_balances(addr.port(), Some(("panel.pem", "panel.key")), Some(PANEL_TOKEN), &scope).await,
			get_balances(addr.port(), Some(("panel.pem", "panel.key")), Some(SERVICE_TOKEN), &scope).await,
		)
	})
	.await;
	let own = own.expect_err("panel holds no grant in the scope");
	assert_eq!((own.code(), own.message()), (Code::PermissionDenied, "not_granted"));
	let borrowed = borrowed.expect_err("invest's token on panel's connection");
	assert_eq!((borrowed.code(), borrowed.message()), (Code::PermissionDenied, "token is not this client's"));
}

#[tokio::test]
async fn no_bearer_or_a_users_access_token_never_reaches_the_service() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let scope = h.scope.clone();
	let (missing, access, garbage) = with_server(addr.port(), server, || async {
		let invest = Some(("invest.pem", "invest.key"));
		(
			get_balances(addr.port(), invest, None, &scope).await,
			get_balances(addr.port(), invest, Some(ACCESS_TOKEN), &scope).await,
			get_balances(addr.port(), invest, Some("forged"), &scope).await,
		)
	})
	.await;
	assert_eq!(missing.expect_err("no bearer").code(), Code::Unauthenticated);
	let access = access.expect_err("a user's token");
	assert_eq!((access.code(), access.message()), (Code::PermissionDenied, "service token required"));
	assert_eq!(garbage.expect_err("an unverifiable token").code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_certificate_the_ca_signed_but_no_client_maps_to_is_unauthenticated() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let scope = h.scope.clone();
	// The certificate is checked outside the token: with or without one, the answer is the
	// same and says nothing about the token.
	let (with_token, without) = with_server(addr.port(), server, || async {
		(
			get_balances(addr.port(), Some(("stranger.pem", "stranger.key")), Some(SERVICE_TOKEN), &scope).await,
			get_balances(addr.port(), Some(("stranger.pem", "stranger.key")), None, &scope).await,
		)
	})
	.await;
	for outcome in [with_token, without] {
		let status = outcome.expect_err("an unknown client");
		assert_eq!((status.code(), status.message()), (Code::Unauthenticated, "client certificate is not a known client"));
	}
}

#[tokio::test]
async fn no_certificate_or_one_from_another_ca_fails_the_handshake() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let scope = h.scope.clone();
	let (anonymous, foreign) = with_server(addr.port(), server, || async {
		(
			get_balances(addr.port(), None, Some(SERVICE_TOKEN), &scope).await,
			get_balances(addr.port(), Some(("foreign.pem", "foreign.key")), Some(SERVICE_TOKEN), &scope).await,
		)
	})
	.await;
	assert_handshake_refused(&anonymous.expect_err("no client certificate"), "CertificateRequired", "no client certificate");
	assert_handshake_refused(&foreign.expect_err("another CA"), "UnknownCA", "a certificate from another CA");
}

/// A refused handshake surfaces as a transport failure (code UNKNOWN, "transport error"),
/// never as an answer of the service or of either layer (those are UNAUTHENTICATED /
/// PERMISSION_DENIED / OK), and the cause is the TLS alert the server sent: `alert` names it,
/// so a listener that fails for some other reason does not pass for one that refused the
/// certificate.
fn assert_handshake_refused(status: &Status, alert: &str, why: &str) {
	assert_eq!(
		(status.code(), status.message()),
		(Code::Unknown, "transport error"),
		"{why}: must not have crossed the handshake: {status:?}"
	);
	assert!(format!("{status:?}").contains(alert), "{why}: expected the TLS alert {alert}, got {status:?}");
}

#[tokio::test]
async fn production_serve_mounts_the_hubs_authorizer() {
	let Some(h) = harness().await else { return };
	// An auth service with no signing key: its authorizer verifies nothing and answers every
	// token NotConfigured — which is exactly how a mounted layer shows itself.
	let config = AuthConfig {
		issuer: "https://auth.banking.test".into(),
		client_audience: "banking-core".into(),
		service_audience: "banking-services".into(),
		scope_audience: "banking-scope".into(),
		access_ttl_secs: 900,
		refresh_ttl_secs: 3600,
		max_session_secs: 7200,
		idle_timeout_secs: 0,
		service_ttl_secs: 300,
		scope_token_ttl_secs: 300,
		signing: None,
		issuance_token: None,
	};
	let (provisioner, _provision_rx) = provisioner_channel();
	let (auth_service, authorizer) = AuthService::try_new(config, provisioner).await.unwrap();
	let auth_addr = ephemeral().await;
	let addr = ephemeral().await;
	let listener = ScopeLedgerListener::new(settings(addr, &h.scope), h.deps.clone());
	let scope = h.scope.clone();
	let (missing, unverifiable) = with_server(
		addr.port(),
		move |stop: CancellationToken| async move {
			let (ledger, auth) = tokio::join!(
				scope_ledger::serve(listener, &authorizer, stop.clone().cancelled_owned()),
				auth_service.run(auth_addr, stop.cancelled_owned())
			);
			auth.expect("the auth task runs until cancelled");
			ledger
		},
		|| async {
			let invest = Some(("invest.pem", "invest.key"));
			(
				get_balances(addr.port(), invest, None, &scope).await,
				get_balances(addr.port(), invest, Some(SERVICE_TOKEN), &scope).await,
			)
		},
	)
	.await;
	assert_eq!(missing.expect_err("no bearer").code(), Code::Unauthenticated);
	let status = unverifiable.expect_err("nothing verifies without a signing key");
	assert_eq!((status.code(), status.message()), (Code::Unavailable, "auth not configured"));
}

#[tokio::test]
async fn charge_is_served_and_reads_its_request() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let scope = h.scope.clone();
	let outcome = with_server(addr.port(), server, || async {
		let tls = ClientTlsConfig::new()
			.ca_certificate(Certificate::from_pem(fixture("ca.pem")))
			.domain_name("localhost")
			.identity(Identity::from_pem(fixture("invest.pem"), fixture("invest.key")));
		let channel = Endpoint::from_shared(format!("https://localhost:{}", addr.port()))
			.unwrap()
			.tls_config(tls)
			.unwrap()
			.connect_lazy();
		let mut request = Request::new(ChargeRequest {
			scope: scope.as_str().to_owned(),
			..ChargeRequest::default()
		});
		request.metadata_mut().insert("authorization", format!("Bearer {SERVICE_TOKEN}").parse().unwrap());
		ScopeLedgerServiceClient::new(channel).charge(request).await
	})
	.await;
	// An empty request is read and refused by the handler itself (no amount), not answered as
	// "not served": the call got through both layers.
	let status = outcome.expect_err("an empty Charge names no amount");
	assert_eq!(status.code(), Code::InvalidArgument, "{}", status.message());
}

// MoneyEvents is a second service on the listener: it must sit behind both layers too.
#[tokio::test]
async fn money_events_are_served_behind_both_layers() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let scope = h.scope.clone();
	let (anonymous, served) = with_server(addr.port(), server, || async {
		let tls = ClientTlsConfig::new()
			.ca_certificate(Certificate::from_pem(fixture("ca.pem")))
			.domain_name("localhost")
			.identity(Identity::from_pem(fixture("invest.pem"), fixture("invest.key")));
		let channel = Endpoint::from_shared(format!("https://localhost:{}", addr.port()))
			.unwrap()
			.tls_config(tls)
			.unwrap()
			.connect_lazy();
		let pull = || PullRequest {
			scope: scope.as_str().to_owned(),
			after_position: 0,
			limit: 10,
		};
		let anonymous = MoneyEventsClient::new(channel.clone()).pull(Request::new(pull())).await;
		let mut request = Request::new(pull());
		request.metadata_mut().insert("authorization", format!("Bearer {SERVICE_TOKEN}").parse().unwrap());
		(anonymous, MoneyEventsClient::new(channel).pull(request).await)
	})
	.await;
	assert_eq!(anonymous.expect_err("no bearer, no events").code(), Code::Unauthenticated);
	let served = served.expect("a known client with its service token is served").into_inner();
	assert!(served.events.is_empty(), "a fresh scope has no events");
	assert_eq!(served.next_position, 0, "nothing scanned: the cursor stays put");
}

// InvoiceService is a third service on the listener: behind both layers like the others.
#[tokio::test]
async fn invoices_are_served_behind_both_layers() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let scope = h.scope.clone();
	let (anonymous, served) = with_server(addr.port(), server, || async {
		let tls = ClientTlsConfig::new()
			.ca_certificate(Certificate::from_pem(fixture("ca.pem")))
			.domain_name("localhost")
			.identity(Identity::from_pem(fixture("invest.pem"), fixture("invest.key")));
		let channel = Endpoint::from_shared(format!("https://localhost:{}", addr.port()))
			.unwrap()
			.tls_config(tls)
			.unwrap()
			.connect_lazy();
		let get = || GetInvoiceRequest {
			scope: scope.as_str().to_owned(),
			invoice_id: Uuid::new_v4().to_string(),
		};
		let anonymous = InvoiceServiceClient::new(channel.clone()).get_invoice(Request::new(get())).await;
		let mut request = Request::new(get());
		request.metadata_mut().insert("authorization", format!("Bearer {SERVICE_TOKEN}").parse().unwrap());
		(anonymous, InvoiceServiceClient::new(channel).get_invoice(request).await)
	})
	.await;
	assert_eq!(anonymous.expect_err("no bearer, no invoice").code(), Code::Unauthenticated);
	assert_eq!(
		served.expect_err("an invoice nobody issued").code(),
		Code::NotFound,
		"a known client with its service token reaches the handler"
	);
}

// MandateRequestService is a fourth service on the listener: behind both layers too.
#[tokio::test]
async fn mandate_requests_are_served_behind_both_layers() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let scope = h.scope.clone();
	let (anonymous, served) = with_server(addr.port(), server, || async {
		let tls = ClientTlsConfig::new()
			.ca_certificate(Certificate::from_pem(fixture("ca.pem")))
			.domain_name("localhost")
			.identity(Identity::from_pem(fixture("invest.pem"), fixture("invest.key")));
		let channel = Endpoint::from_shared(format!("https://localhost:{}", addr.port()))
			.unwrap()
			.tls_config(tls)
			.unwrap()
			.connect_lazy();
		let ask = || CreateMandateRequestRequest {
			scope: scope.as_str().to_owned(),
			operations: vec![MandateOperation::Reserve as i32],
			cap_per_op: "1".into(),
			cap_24h: "1".into(),
			idempotency_key: Uuid::new_v4().to_string(),
			..CreateMandateRequestRequest::default()
		};
		let anonymous = MandateRequestServiceClient::new(channel.clone()).create_mandate_request(Request::new(ask())).await;
		let mut request = Request::new(ask());
		request.metadata_mut().insert("authorization", format!("Bearer {SERVICE_TOKEN}").parse().unwrap());
		(anonymous, MandateRequestServiceClient::new(channel).create_mandate_request(request).await)
	})
	.await;
	assert_eq!(anonymous.expect_err("no bearer, no request").code(), Code::Unauthenticated);
	// The fixture grant lets `invest` open accounts in the scope but not draw on compartments,
	// so the handler itself refuses — not "unimplemented", which an unmounted service would be.
	assert_eq!(
		served.expect_err("a client without the compartments flag").code(),
		Code::InvalidArgument,
		"a known client with its service token reaches the handler"
	);
}

/// One GetBalances request as raw gRPC framing over the fixture `invest` identity, with the
/// content type under test. The message is hand-encoded (`scope = 1`, a string): the bytes
/// stay ASCII for a scope id this short, so a `String` carries them as the request body.
async fn raw_get_balances(port: u16, content_type: &str, scope: &ScopeId) -> http::Response<tonic::body::Body> {
	use tower::ServiceExt;
	let tls = ClientTlsConfig::new()
		.ca_certificate(Certificate::from_pem(fixture("ca.pem")))
		.domain_name("localhost")
		.identity(Identity::from_pem(fixture("invest.pem"), fixture("invest.key")));
	let channel = Endpoint::from_shared(format!("https://localhost:{port}"))
		.unwrap()
		.tls_config(tls)
		.unwrap()
		.connect()
		.await
		.unwrap();
	let id = scope.as_str().as_bytes();
	assert!(id.len() < 120, "the hand framing assumes a one-byte length");
	let mut message = vec![0x0a, id.len() as u8];
	message.extend_from_slice(id);
	let mut frame = vec![0u8];
	frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
	frame.extend_from_slice(&message);
	let request = http::Request::builder()
		.method("POST")
		.uri(format!("https://localhost:{port}/scope.v1.ScopeLedgerService/GetBalances"))
		.header("content-type", content_type)
		.header("te", "trailers")
		.header("authorization", format!("Bearer {SERVICE_TOKEN}"))
		.body(tonic::body::Body::new(String::from_utf8(frame).expect("ASCII framing")))
		.unwrap();
	channel.oneshot(request).await.expect("the request crossed the connection")
}

#[tokio::test]
async fn grpc_web_never_reaches_the_service_even_over_http2() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let scope = h.scope.clone();
	let (grpc, web) = with_server(addr.port(), server, || async {
		(
			raw_get_balances(addr.port(), "application/grpc", &scope).await,
			raw_get_balances(addr.port(), "application/grpc-web+proto", &scope).await,
		)
	})
	.await;
	// The control: the same bytes as plain gRPC reach the handler (no status in the headers;
	// the answer comes in the trailers).
	assert!(grpc.headers().get("grpc-status").is_none(), "plain gRPC is served: {:?}", grpc.headers());
	// tonic does not look at the content type, so without the gate this request would be
	// decoded as gRPC and served; the gate answers in the headers instead.
	assert_eq!(web.headers().get("grpc-status").and_then(|v| v.to_str().ok()), Some("3"), "{:?}", web.headers());
	assert_eq!(
		web.headers().get("grpc-message").and_then(|v| v.to_str().ok()),
		Some("only%20application/grpc%20is%20served%20here")
	);
}

/// An HTTP/1.1 request over the listener's TLS, offering only `http/1.1` in ALPN — what a
/// gRPC-Web client in a browser sends. Synchronous rustls on a plain socket, run on the
/// blocking pool. Returns what came back, or the error that ended it.
fn http1_over_tls(port: u16) -> Result<Vec<u8>, String> {
	use std::io::{Read, Write};

	use rustls::{
		ClientConfig, ClientConnection, RootCertStore,
		pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
	};
	let mut roots = RootCertStore::empty();
	roots.add(CertificateDer::from_pem_slice(fixture("ca.pem").as_bytes()).unwrap()).unwrap();
	let chain = CertificateDer::pem_slice_iter(fixture("invest.pem").as_bytes()).collect::<Result<Vec<_>, _>>().unwrap();
	let key = PrivateKeyDer::from_pem_slice(fixture("invest.key").as_bytes()).unwrap();
	let mut config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
		.with_safe_default_protocol_versions()
		.unwrap()
		.with_root_certificates(roots)
		.with_client_auth_cert(chain, key)
		.unwrap();
	config.alpn_protocols = vec![b"http/1.1".to_vec()];
	let mut connection = ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap()).map_err(|e| e.to_string())?;
	let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
	socket.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
	let mut tls = rustls::Stream::new(&mut connection, &mut socket);
	let request = format!(
		"POST /scope.v1.ScopeLedgerService/GetBalances HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/grpc-web+proto\r\nAuthorization: Bearer {SERVICE_TOKEN}\r\nContent-Length: 0\r\n\r\n"
	);
	tls.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
	let mut answer = Vec::new();
	tls.read_to_end(&mut answer).map_err(|e| e.to_string())?;
	Ok(answer)
}

#[tokio::test]
async fn http1_never_reaches_the_service() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let outcome = with_server(addr.port(), server, || async {
		let port = addr.port();
		tokio::task::spawn_blocking(move || http1_over_tls(port)).await.expect("the blocking dial ran")
	})
	.await;
	// The listener offers only h2 in ALPN, and rustls refuses a client that offers only
	// http/1.1 in the handshake: no HTTP/1.1 response, of any status, ever comes back.
	let refused = outcome.expect_err("an http/1.1-only client must not complete a request");
	assert!(refused.to_lowercase().contains("application"), "refused for the protocol, not something else: {refused}");
}

// ---- ScopeAuthService.MintServiceToken: the one service outside the token layer ----

fn mint_channel(port: u16, identity: Option<(&str, &str)>) -> tonic::transport::Channel {
	let mut tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(fixture("ca.pem"))).domain_name("localhost");
	if let Some((cert, key)) = identity {
		tls = tls.identity(Identity::from_pem(fixture(cert), fixture(key)));
	}
	Endpoint::from_shared(format!("https://localhost:{port}"))
		.unwrap()
		.connect_timeout(Duration::from_secs(5))
		.tls_config(tls)
		.unwrap()
		.connect_lazy()
}

/// One `MintServiceToken` over TLS presenting `identity`, with `bearer` if any.
async fn mint(port: u16, identity: Option<(&str, &str)>, bearer: Option<&str>) -> Result<MintServiceTokenResponse, Status> {
	let mut request = Request::new(MintServiceTokenRequest {});
	if let Some(token) = bearer {
		request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
	}
	ScopeAuthServiceClient::new(mint_channel(port, identity))
		.mint_service_token(request)
		.await
		.map(tonic::Response::into_inner)
}

const INVEST: Option<(&str, &str)> = Some(("invest.pem", "invest.key"));
const PANEL: Option<(&str, &str)> = Some(("panel.pem", "panel.key"));

// Same throwaway Ed25519 keypair as the auth crate's tests.
const TEST_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIKolOSMXwE+tafZkX+jkKYJbmJ066f4E12wAwTIkKps6\n-----END PRIVATE KEY-----\n";
const TEST_JWK_X: &str = "Z6BCmq9-_wo9d7co5CDW84Wn0sAC3BA0XWK2AOstpV4";

/// Production's `serve` over a REAL signing auth task with the given service TTL, run for the
/// length of `client`.
async fn with_real_auth<T, F, Fut>(h: &Harness, scope_token_ttl_secs: u64, client: F) -> T
where
	F: FnOnce(u16) -> Fut,
	Fut: Future<Output = T>, {
	let config = AuthConfig {
		issuer: "https://auth.banking.test".into(),
		client_audience: "banking-core".into(),
		service_audience: "banking-services".into(),
		scope_audience: "banking-scope".into(),
		access_ttl_secs: 900,
		refresh_ttl_secs: 3600,
		max_session_secs: 7200,
		idle_timeout_secs: 0,
		service_ttl_secs: 300,
		scope_token_ttl_secs,
		signing: Some(SigningConfig {
			signing_key_pem: TEST_PEM.into(),
			kid: "test-kid".into(),
			jwks_json: format!(r#"{{"keys":[{{"kty":"OKP","crv":"Ed25519","x":"{TEST_JWK_X}","kid":"test-kid","alg":"EdDSA","use":"sig"}}]}}"#),
		}),
		issuance_token: None,
	};
	let (provisioner, _provision_rx) = provisioner_channel();
	let (auth_service, authorizer) = AuthService::try_new(config, provisioner).await.unwrap();
	let auth_addr = ephemeral().await;
	let addr = ephemeral().await;
	let listener = ScopeLedgerListener::new(settings(addr, &h.scope), h.deps.clone());
	with_server(
		addr.port(),
		move |stop: CancellationToken| async move {
			let (ledger, auth) = tokio::join!(
				scope_ledger::serve(listener, &authorizer, stop.clone().cancelled_owned()),
				auth_service.run(auth_addr, stop.cancelled_owned())
			);
			auth.expect("the auth task runs until cancelled");
			ledger
		},
		|| client(addr.port()),
	)
	.await
}

#[tokio::test]
async fn a_minted_token_passes_the_service_layer_for_the_client_that_minted_it() {
	let Some(h) = harness().await else { return };
	let scope = h.scope.clone();
	let (minted, served, borrowed) = with_real_auth(&h, 120, |port| async move {
		let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
		// No bearer on the mint: the certificate is all it asks for.
		let minted = mint(port, INVEST, None).await.expect("a known client mints without any token");
		let served = get_balances(port, INVEST, Some(&minted.token), &scope).await;
		// The same token on panel's connection: the binding to the certificate refuses it.
		let borrowed = get_balances(port, PANEL, Some(&minted.token), &scope).await;
		((before, minted), served, borrowed)
	})
	.await;
	let (before, minted) = minted;
	assert!(!minted.token.is_empty());
	assert!(
		(before + 120..=before + 125).contains(&minted.expires_at),
		"expires_at {} is now + the configured TTL",
		minted.expires_at
	);
	// `sub == invest`, signed by the hub, `typ=service`: the real verifier accepted it and the
	// handler found the caller's id in it.
	assert!(served.expect("the minted token is served").accounts.is_empty());
	let borrowed = borrowed.expect_err("invest's token on panel's connection");
	assert_eq!((borrowed.code(), borrowed.message()), (Code::PermissionDenied, "token is not this client's"));
}

#[tokio::test]
async fn each_client_is_minted_its_own_token_and_nothing_in_the_call_chooses_the_subject() {
	let Some(h) = harness().await else { return };
	let scope = h.scope.clone();
	let (panel_on_invest, panel_on_panel) = with_real_auth(&h, 300, |port| async move {
		let panel = mint(port, PANEL, None).await.expect("panel mints");
		let on_panel = get_balances(port, PANEL, Some(&panel.token), &scope).await;
		let on_invest = get_balances(port, INVEST, Some(&panel.token), &scope).await;
		(on_invest, on_panel)
	})
	.await;
	// panel's token is panel's: its own connection reaches the handler (panel holds no grant
	// there), invest's connection is refused on the binding.
	let panel_on_panel = panel_on_panel.expect_err("panel holds no grant in the scope");
	assert_eq!((panel_on_panel.code(), panel_on_panel.message()), (Code::PermissionDenied, "not_granted"));
	let panel_on_invest = panel_on_invest.expect_err("panel's token on invest's connection");
	assert_eq!((panel_on_invest.code(), panel_on_invest.message()), (Code::PermissionDenied, "token is not this client's"));
}

#[tokio::test]
async fn mint_needs_a_known_certificate_and_no_bearer_substitutes_for_it() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let (stranger, stranger_with_service, stranger_with_access, no_cert) = with_server(addr.port(), server, || async {
		let stranger = Some(("stranger.pem", "stranger.key"));
		(
			mint(addr.port(), stranger, None).await,
			mint(addr.port(), stranger, Some(SERVICE_TOKEN)).await,
			mint(addr.port(), stranger, Some(ACCESS_TOKEN)).await,
			mint(addr.port(), None, Some(ACCESS_TOKEN)).await,
		)
	})
	.await;
	for outcome in [stranger, stranger_with_service, stranger_with_access] {
		let status = outcome.expect_err("an unknown certificate");
		assert_eq!((status.code(), status.message()), (Code::Unauthenticated, "client certificate is not a known client"));
	}
	// No certificate at all never crosses the handshake, whatever bearer rides along.
	assert_handshake_refused(&no_cert.expect_err("no client certificate"), "CertificateRequired", "no certificate on the mint");
}

#[tokio::test]
async fn a_users_access_token_neither_replaces_the_certificate_nor_changes_the_subject() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let minted = with_server(addr.port(), server, || async { mint(addr.port(), INVEST, Some(ACCESS_TOKEN)).await }).await;
	// The bearer is not read on this service: the subject is the certificate's.
	assert_eq!(minted.expect("a known client mints").token, "service:invest");
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let other = with_server(addr.port(), server, || async { mint(addr.port(), PANEL, Some(SERVICE_TOKEN)).await }).await;
	assert_eq!(other.expect("a known client mints").token, "service:panel", "invest's bearer does not make panel invest");
}

/// A raw gRPC call to `path` over the fixture `invest` identity, no bearer, with the content
/// type under test; what the listener answered, in the headers (a trailers-only answer).
async fn raw_call(port: u16, path: &str, content_type: &str) -> http::Response<tonic::body::Body> {
	use tower::ServiceExt;
	let request = http::Request::builder()
		.method("POST")
		.uri(format!("https://localhost:{port}{path}"))
		.header("content-type", content_type)
		.header("te", "trailers")
		.body(tonic::body::Body::empty())
		.unwrap();
	mint_channel(port, INVEST).oneshot(request).await.expect("the request crossed the connection")
}

fn grpc_status(response: &http::Response<tonic::body::Body>) -> Option<&str> {
	response.headers().get("grpc-status").and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn grpc_web_never_reaches_the_mint_either() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let web = with_server(addr.port(), server, || async {
		raw_call(addr.port(), "/scope.v1.ScopeAuthService/MintServiceToken", "application/grpc-web+proto").await
	})
	.await;
	assert_eq!(grpc_status(&web), Some("3"), "{:?}", web.headers());
	assert_eq!(
		web.headers().get("grpc-message").and_then(|v| v.to_str().ok()),
		Some("only%20application/grpc%20is%20served%20here")
	);
}

fn scope_protos() -> Vec<(String, String)> {
	let dir = format!("{}/../../contracts/proto/scope/v1", env!("CARGO_MANIFEST_DIR"));
	std::fs::read_dir(&dir)
		.unwrap_or_else(|err| panic!("{dir}: {err}"))
		.map(|entry| {
			let path = entry.unwrap().path();
			(path.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read_to_string(&path).unwrap())
		})
		.collect()
}

/// The service name on a `service Name {` line, however it is indented or spaced
/// (`service Name{` too); `None` for anything else, comments included.
fn service_name(line: &str) -> Option<&str> {
	let rest = line.trim_start().strip_prefix("service")?;
	// `servicefoo` is not a declaration; the keyword is followed by whitespace.
	let rest = rest.strip_prefix(char::is_whitespace)?;
	let name = rest.trim_start().split(|c: char| c.is_whitespace() || c == '{').next()?;
	(!name.is_empty()).then_some(name)
}

/// Every `service` the scope.v1 protos declare, read from the files so a service added later
/// is covered without anyone remembering this test.
fn declared_services() -> Vec<String> {
	let mut services: Vec<String> = scope_protos()
		.iter()
		.flat_map(|(_, text)| text.lines().filter_map(service_name).map(str::to_owned).collect::<Vec<_>>())
		.collect();
	services.sort();
	services
}

#[test]
fn the_service_scan_reads_declarations_however_they_are_spaced() {
	assert_eq!(service_name("service Foo {"), Some("Foo"));
	assert_eq!(service_name("service Foo{"), Some("Foo"));
	assert_eq!(service_name("\t  service   Foo  {"), Some("Foo"));
	assert_eq!(service_name("// service Foo {"), None);
	assert_eq!(service_name("servicefoo {"), None);
	assert_eq!(service_name("  rpc Pull(PullRequest) returns (PullResponse);"), None);
}

// The mint file carries exactly one RPC and it is the mint: whatever else is added to this
// service would also be served with no token.
#[test]
fn the_service_outside_the_token_layer_declares_exactly_one_rpc_the_mint() {
	let protos = scope_protos();
	let (_, text) = protos.iter().find(|(name, _)| name == "service_auth.proto").expect("service_auth.proto");
	let rpcs: Vec<&str> = text.lines().map(str::trim_start).filter(|line| line.starts_with("rpc ")).collect();
	assert_eq!(rpcs.len(), 1, "{rpcs:?}");
	assert!(rpcs[0].starts_with("rpc MintServiceToken("), "{rpcs:?}");
	let services: Vec<&str> = text.lines().filter_map(service_name).collect();
	assert_eq!(services, ["ScopeAuthService"], "no other service shares the file");
}

// The guard that the token layer cannot be dropped from a service by accident: with a known
// certificate and NO bearer, every service of the listener answers UNAUTHENTICATED from the
// token layer, except the mint, which is the one that reaches its own router (an unknown
// method there is UNIMPLEMENTED). A new service that is mounted bare, or not mounted, fails
// this list.
#[tokio::test]
async fn the_mint_is_the_only_service_outside_the_token_layer() {
	let Some(h) = harness().await else { return };
	let services = declared_services();
	assert!(services.len() >= 5 && services.contains(&"ScopeAuthService".to_owned()), "the proto scan found {services:?}");
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let answers = with_server(addr.port(), server, || async {
		let mut answers = Vec::new();
		for service in &services {
			let response = raw_call(addr.port(), &format!("/scope.v1.{service}/NoSuchMethod"), "application/grpc").await;
			answers.push((service.clone(), grpc_status(&response).map(str::to_owned)));
		}
		answers
	})
	.await;
	for (service, status) in answers {
		let expected = if service == "ScopeAuthService" { "12" } else { "16" };
		assert_eq!(status.as_deref(), Some(expected), "{service} without a bearer");
	}
}

// The listener serves the scope services and nothing else: the hub's health probe and
// server reflection live on the other listeners and must not be reachable here (reflection
// would list every method to any certificate holder).
#[tokio::test]
async fn health_and_reflection_are_not_served_on_this_listener() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let answers = with_server(addr.port(), server, || async {
		let mut answers = Vec::new();
		for path in [
			"/grpc.health.v1.Health/Check",
			"/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
			"/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo",
		] {
			let response = raw_call(addr.port(), path, "application/grpc").await;
			answers.push((path, grpc_status(&response).map(str::to_owned)));
		}
		answers
	})
	.await;
	for (path, status) in answers {
		assert_eq!(status.as_deref(), Some("12"), "{path} must be UNIMPLEMENTED here");
	}
}

// A certificate rotation lists two fingerprints for one client: either certificate is that
// client, so a token minted over one is the same `sub` as over the other.
#[tokio::test]
async fn two_fingerprints_of_one_client_mint_the_same_subject() {
	let Some(h) = harness().await else { return };
	let scope = h.scope.clone();
	let second = Some(("stranger.pem", "stranger.key"));
	let mut minted = Vec::new();
	for (via, also) in [(INVEST, "stranger.sha256"), (second, "stranger.sha256")] {
		let addr = ephemeral().await;
		let listener = ScopeLedgerListener::new(settings_with(addr, &h.scope, &[also]), h.deps.clone());
		let server =
			move |stop: CancellationToken| Box::pin(scope_ledger::serve_with(listener, StubAuthenticator, StubMinter, stop.cancelled_owned())) as std::pin::Pin<Box<dyn Future<Output = _>>>;
		let (token, on_the_other) = with_server(addr.port(), server, || async {
			let token = mint(addr.port(), via, None).await.expect("both certificates are the client");
			// The token minted over one certificate is accepted on the other's connection.
			let other = if via == INVEST { second } else { INVEST };
			(token.token.clone(), get_balances(addr.port(), other, Some(&token.token), &scope).await)
		})
		.await;
		on_the_other.expect("the same client, so the same sub");
		minted.push(token);
	}
	assert_eq!(minted, ["service:invest", "service:invest"]);
}

// The rate limit is per client, not per certificate: a second fingerprint is not a second
// allowance.
#[tokio::test]
async fn minting_again_inside_the_interval_is_resource_exhausted_whatever_the_certificate() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let listener = ScopeLedgerListener::new(settings_with(addr, &h.scope, &["stranger.sha256"]), h.deps.clone());
	let server =
		move |stop: CancellationToken| Box::pin(scope_ledger::serve_with(listener, StubAuthenticator, StubMinter, stop.cancelled_owned())) as std::pin::Pin<Box<dyn Future<Output = _>>>;
	let (first, again, other_cert, panel) = with_server(addr.port(), server, || async {
		(
			mint(addr.port(), INVEST, None).await,
			mint(addr.port(), INVEST, None).await,
			mint(addr.port(), Some(("stranger.pem", "stranger.key")), None).await,
			mint(addr.port(), PANEL, None).await,
		)
	})
	.await;
	first.expect("the first mint is served");
	for refused in [again, other_cert] {
		let status = refused.expect_err("a second mint inside the interval");
		assert_eq!(status.code(), Code::ResourceExhausted);
		assert!(status.message().contains("reuse the token until expires_at"), "{}", status.message());
	}
	panel.expect("another client has its own allowance");
}

// A certificate the CA signed and the directory knows, but whose client holds no grant, gets
// no token. (`panel` holds one elsewhere, which is why it can mint in the other tests.)
#[tokio::test]
async fn a_client_with_no_grants_at_all_gets_no_token() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let grants = format!(
		r#"{{"clients":[{{"client_id":"invest","cert_sha256":["{}"]}},{{"client_id":"panel","cert_sha256":["{}"]}}],
		    "grants":[{{"client_id":"invest","scope":"{}","label_prefix":"alloc:"}}]}}"#,
		fixture("invest.sha256").trim(),
		fixture("panel.sha256").trim(),
		h.scope
	);
	let (clients, grants) = ScopeLedgerSettings::parse_grants(&grants).expect("valid");
	let mut config = settings(addr, &h.scope);
	config.clients = clients;
	config.grants = grants;
	let listener = ScopeLedgerListener::new(config, h.deps.clone());
	let server =
		move |stop: CancellationToken| Box::pin(scope_ledger::serve_with(listener, StubAuthenticator, StubMinter, stop.cancelled_owned())) as std::pin::Pin<Box<dyn Future<Output = _>>>;
	let status = with_server(addr.port(), server, || async { mint(addr.port(), PANEL, None).await })
		.await
		.expect_err("panel holds no grant");
	assert_eq!((status.code(), status.message()), (Code::PermissionDenied, "not_granted"));
}

// A certificate from a CA nothing here trusts, even one that names itself `invest`, never
// completes the handshake, so there is nothing to mint for.
#[tokio::test]
async fn a_foreign_ca_certificate_named_invest_mints_nothing() {
	let Some(h) = harness().await else { return };
	let addr = ephemeral().await;
	let server = serve_stub(&h, addr).await;
	let status = with_server(addr.port(), server, || async { mint(addr.port(), Some(("foreign.pem", "foreign.key")), None).await })
		.await
		.expect_err("the foreign certificate must not be accepted");
	assert_handshake_refused(&status, "UnknownCA", "a certificate from another CA, CN=invest");
}
