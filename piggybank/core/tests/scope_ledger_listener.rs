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
use evbanking_contracts::scope::v1::{GetBalancesRequest, GetBalancesResponse, ReserveRequest, scope_ledger_service_client::ScopeLedgerServiceClient};
use piggybank_core::{
	config::ScopeLedgerSettings,
	infrastructure::{compartments::PgCompartments, scope_ledger::PgScopeLedger},
	services::scope_ledger::{self, ScopeLedgerDeps, ScopeLedgerListener},
};
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;
use tonic::{
	Code, Request, Status,
	metadata::MetadataValue,
	transport::{Certificate, ClientTlsConfig, Endpoint, Identity},
};
use uuid::Uuid;

mod common;

fn fixture(name: &str) -> String {
	let path = format!("{}/tests/fixtures/scope_tls/{name}", env!("CARGO_MANIFEST_DIR"));
	std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("fixture {name} must be readable ({err}); regenerate with fixtures/scope_tls/regen.sh"))
}

/// The bearer a stub caller presents, by the class a real verifier would have found in it.
const SERVICE_TOKEN: &str = "a-service-token";
const ACCESS_TOKEN: &str = "a-users-access-token";

/// Stands in for the hub's authorizer: answers a service token and a user's access token
/// with the claims the real one would, and refuses anything else as the real one does.
#[derive(Clone)]
struct StubAuthenticator;

impl Authenticate for StubAuthenticator {
	async fn authenticate(&self, token: String) -> Result<Claims, AuthError> {
		let typ = match token.as_str() {
			SERVICE_TOKEN => TokenType::Service,
			ACCESS_TOKEN => TokenType::Access,
			_ => return Err(AuthError::InvalidToken),
		};
		Ok(Claims {
			sub: "whatever-the-token-says".into(),
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
	Some(Harness {
		deps: ScopeLedgerDeps {
			store: Arc::new(PgScopeLedger::new(pool.clone())),
			scopes: Arc::new(PgCompartments::new(pool.clone())),
			ledger,
			relay: Arc::new(Notify::new()),
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
	// tonic's server builder refuses to guess between the two crypto providers the binary
	// carries (see the workspace manifest); once per process, so later calls are told so.
	let _ = rustls::crypto::ring::default_provider().install_default();
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
	// `panel` is a known client with no grant in the scope; its token's `sub` is irrelevant.
	let outcome = with_server(addr.port(), server, || async {
		get_balances(addr.port(), Some(("panel.pem", "panel.key")), Some(SERVICE_TOKEN), &scope).await
	})
	.await;
	let status = outcome.expect_err("panel holds no grant in the scope");
	assert_eq!((status.code(), status.message()), (Code::PermissionDenied, "not_granted"));
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
	let outcome = with_server(addr.port(), server, || async {
		get_balances(addr.port(), Some(("stranger.pem", "stranger.key")), Some(SERVICE_TOKEN), &scope).await
	})
	.await;
	let status = outcome.expect_err("an unknown client");
	assert_eq!((status.code(), status.message()), (Code::Unauthenticated, "client certificate is not a known client"));
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
async fn an_rpc_part_one_does_not_serve_says_so() {
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
		let mut request = Request::new(ReserveRequest {
			scope: scope.as_str().to_owned(),
			..ReserveRequest::default()
		});
		request.metadata_mut().insert("authorization", format!("Bearer {SERVICE_TOKEN}").parse().unwrap());
		ScopeLedgerServiceClient::new(channel).reserve(request).await
	})
	.await;
	let status = outcome.expect_err("Reserve is not served in part 1");
	assert_eq!(status.code(), Code::Unimplemented);
	assert!(status.message().starts_with("Reserve is not served yet"), "{}", status.message());
}
