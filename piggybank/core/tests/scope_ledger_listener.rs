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
use evbanking_auth::{AuthConfig, AuthError, AuthService, Authenticate, Claims, TokenType, provisioner_channel};
use evbanking_contracts::scope::v1::{
	ChargeRequest, GetBalancesRequest, GetBalancesResponse, GetInvoiceRequest, PullRequest, invoice_service_client::InvoiceServiceClient, money_events_client::MoneyEventsClient,
	scope_ledger_service_client::ScopeLedgerServiceClient,
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
	let grants = format!(
		r#"{{"clients":[{{"client_id":"invest","cert_sha256":["{}"]}},{{"client_id":"panel","cert_sha256":["{}"]}}],
		    "grants":[{{"client_id":"invest","scope":"{scope}","label_prefix":"alloc:"}}]}}"#,
		fixture("invest.sha256").trim(),
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
	move |stop: CancellationToken| Box::pin(scope_ledger::serve_with(listener, StubAuthenticator, stop.cancelled_owned())) as std::pin::Pin<Box<dyn Future<Output = _>>>
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
	// A refused handshake surfaces as a transport failure, never as an answer of the service
	// or of either layer (those are UNAUTHENTICATED / PERMISSION_DENIED / OK).
	for (outcome, why) in [(anonymous, "no client certificate"), (foreign, "a certificate from another CA")] {
		let status = outcome.expect_err(why);
		assert!(
			!matches!(status.code(), Code::Ok | Code::Unauthenticated | Code::PermissionDenied | Code::NotFound),
			"{why}: the request must not have crossed the handshake: {status:?}"
		);
	}
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
		access_ttl_secs: 900,
		refresh_ttl_secs: 3600,
		max_session_secs: 7200,
		idle_timeout_secs: 0,
		service_ttl_secs: 300,
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
async fn an_rpc_not_served_yet_says_so() {
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
	let status = outcome.expect_err("Charge is not served yet");
	assert_eq!(status.code(), Code::Unimplemented);
	assert!(status.message().starts_with("Charge is not served yet"), "{}", status.message());
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
