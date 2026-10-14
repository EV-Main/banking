//! The bridge's TLS, exercised against a real handshake.
//!
//! `infrastructure::bridge::endpoint` decides what the channel proves about the peer — the
//! pinned CA, the name the certificate must carry, the client identity — and its unit
//! tests pin the wiring: which addresses read which files, and what stops the boot. None
//! of that reaches rustls. These tests do: a tonic server on a loopback socket, serving
//! a fake concierge `UserEvents` under the fixtures in `fixtures/tls/`, and the very
//! endpoint production builds dialling it. What is asserted is not only the client's
//! error but whether the server's handler ran at all — a handshake that fails never
//! reaches it, and that is the property a pinned CA exists to provide.
//!
//! The handshake tests need no database. The two consumer tests at the end run the real
//! [`BridgeConsumer`] over the TLS channel against Postgres, so "the events were applied"
//! and "the events were not applied" are stated against the `users` table and not against
//! a status code; they skip without `DATABASE_URL`, like the sibling suites.
//!
//! Concurrency is structured (no detached `tokio::spawn`): the server, the client and the
//! assertions run as branches of one `tokio::join!` under one cancellation token.

use std::{
	future::Future,
	net::SocketAddr,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use evconcierge_contracts::concierge::v1::{
	PullUserLifecycleRequest, PullUserLifecycleResponse, UserLifecycleEvent,
	user_events_client::UserEventsClient,
	user_events_server::{UserEvents, UserEventsServer},
	user_lifecycle_event::Kind,
};
use piggybank_core::infrastructure::bridge::{
	BridgeConsumer,
	endpoint::{BridgeTlsFiles, bridge_endpoint},
};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tonic::{
	Code, Request, Response, Status,
	metadata::MetadataValue,
	transport::{Certificate, Identity, Server, ServerTlsConfig},
};

mod common;

const BRIDGE_TOKEN: &str = "test-bridge-token";

/// A fixture's path, as `BridgeTlsFiles` wants it — the builder reads files, as production
/// reads a mounted Secret, so the tests hand it paths and not bytes.
fn fixture(name: &str) -> String {
	format!("{}/tests/fixtures/tls/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn fixture_bytes(name: &str) -> String {
	std::fs::read_to_string(fixture(name)).unwrap_or_else(|err| panic!("fixture {name} must be readable ({err}); regenerate with fixtures/tls/regen.sh"))
}

/// The CA every fixture leaf is signed by.
fn pinned_ca() -> BridgeTlsFiles {
	BridgeTlsFiles {
		ca: Some(fixture("ca.pem")),
		..BridgeTlsFiles::default()
	}
}

/// The pinned CA plus the hub's client identity, for the mTLS case.
fn pinned_ca_with_identity() -> BridgeTlsFiles {
	BridgeTlsFiles {
		client_cert: Some(fixture("client.pem")),
		client_key: Some(fixture("client.key")),
		..pinned_ca()
	}
}

/// Which certificate the fake concierge presents, and whether it demands one back.
struct ServerSide {
	cert: &'static str,
	key: &'static str,
	client_ca: Option<&'static str>,
}

const SERVER: ServerSide = ServerSide {
	cert: "server.pem",
	key: "server.key",
	client_ca: None,
};
/// A certificate for the production name only — no `localhost`, no `127.0.0.1`.
const SERVER_NAMED_CONCIERGE: ServerSide = ServerSide {
	cert: "server-concierge-only.pem",
	key: "server-concierge-only.key",
	client_ca: None,
};
const SERVER_DEMANDING_A_CLIENT_CERT: ServerSide = ServerSide {
	cert: "server.pem",
	key: "server.key",
	client_ca: Some("ca.pem"),
};

impl ServerSide {
	fn tls_config(&self) -> ServerTlsConfig {
		let config = ServerTlsConfig::new().identity(Identity::from_pem(fixture_bytes(self.cert), fixture_bytes(self.key)));
		match self.client_ca {
			Some(ca) => config.client_ca_root(Certificate::from_pem(fixture_bytes(ca))),
			None => config,
		}
	}
}

/// A fake concierge `UserEvents` that serves a fixed list and counts how many pulls reached
/// it — the count is what tells a handshake that failed from a request that was refused.
struct FakeUserEvents {
	events: Vec<UserLifecycleEvent>,
	pulls: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl UserEvents for FakeUserEvents {
	async fn pull_user_lifecycle(&self, request: Request<PullUserLifecycleRequest>) -> Result<Response<PullUserLifecycleResponse>, Status> {
		self.pulls.fetch_add(1, Ordering::SeqCst);
		match request.metadata().get("authorization").and_then(|v| v.to_str().ok()) {
			Some(value) if value == format!("Bearer {BRIDGE_TOKEN}") => {}
			_ => return Err(Status::unauthenticated("bad bridge token")),
		}
		let after = request.into_inner().after_position;
		let events: Vec<_> = self
			.events
			.iter()
			.enumerate()
			.filter(|(idx, _)| *idx as i64 + 1 > after)
			.map(|(_, event)| event.clone())
			.collect();
		let next_position = after + events.len() as i64;
		Ok(Response::new(PullUserLifecycleResponse { events, next_position }))
	}
}

fn created(subject: &str) -> UserLifecycleEvent {
	UserLifecycleEvent {
		user_id: uuid::Uuid::new_v4().to_string(),
		kind: Kind::Created as i32,
		kyc_level: 1,
		occurred_at: 0,
		event_id: uuid::Uuid::new_v4().to_string(),
		sequence: 1,
		auth_subject: subject.to_string(),
		email: "bridged@example.com".into(),
		email_verified: true,
		token_version: 0,
		role: String::new(),
		permissions: Vec::new(),
		seat_permissions: None,
	}
}

/// Run the fake concierge under `server`'s TLS on an ephemeral loopback port, concurrently
/// with `client`, which gets the port and the pull counter. Returns what the client
/// returned.
async fn with_server<T, F, Fut>(server: &ServerSide, events: Vec<UserLifecycleEvent>, client: F) -> T
where
	F: FnOnce(u16, Arc<AtomicUsize>) -> Fut,
	Fut: Future<Output = T>, {
	// Bind once to claim a free ephemeral port, then let tonic re-bind it — the same trick
	// as `bridge_consumer.rs`, avoiding a tokio-stream dependency for a pre-bound listener.
	let addr: SocketAddr = {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
		listener.local_addr().expect("the listener has an address")
	};
	let pulls = Arc::new(AtomicUsize::new(0));
	let stop = CancellationToken::new();
	// The provider production installs, by the function production calls (`crypto.rs`).
	piggybank_core::crypto::install_crypto_provider();

	let serve = {
		let stop = stop.clone();
		let pulls = Arc::clone(&pulls);
		async move {
			Server::builder()
				.tls_config(server.tls_config())
				.expect("the fixture identity is a usable server certificate")
				.add_service(UserEventsServer::new(FakeUserEvents { events, pulls }))
				.serve_with_shutdown(addr, stop.cancelled_owned())
				.await
				.expect("the fake concierge serves until cancelled");
		}
	};
	let drive = {
		let stop = stop.clone();
		let pulls = Arc::clone(&pulls);
		async move {
			// The server binds asynchronously; a lazy channel retries the connect, but the
			// first attempt against a not-yet-bound port costs a backoff worth skipping.
			tokio::time::sleep(Duration::from_millis(100)).await;
			let out = client(addr.port(), pulls).await;
			stop.cancel();
			out
		}
	};
	let ((), out) = tokio::join!(serve, drive);
	out
}

/// One authenticated pull over the endpoint production would build for `addr` — the
/// handshake, then the request.
async fn pull_once(addr: &str, tls: &BridgeTlsFiles) -> Result<PullUserLifecycleResponse, Status> {
	let endpoint = bridge_endpoint(addr, tls).expect("the endpoint builds; what is under test is the handshake");
	let mut client = UserEventsClient::new(endpoint.connect_lazy());
	let mut request = Request::new(PullUserLifecycleRequest { after_position: 0, limit: 10 });
	let bearer: MetadataValue<_> = format!("Bearer {BRIDGE_TOKEN}").parse().expect("a bearer header is ascii");
	request.metadata_mut().insert("authorization", bearer);
	client.pull_user_lifecycle(request).await.map(Response::into_inner)
}

/// A handshake that fails surfaces as a transport error with the server's handler never
/// having run. That no request crossed is the contract; `verdict` is rustls's own wording
/// for the client-side refusals (`UnknownIssuer`, `not valid for name`), pinned so the
/// failure is the one the test is about and not a port nobody was listening on. A server
/// that rejects the client's certificate just closes the connection, so the mTLS case has
/// no verdict to read on this side.
fn assert_never_reached_the_server(outcome: &Result<PullUserLifecycleResponse, Status>, pulls: &AtomicUsize, verdict: Option<&str>, why: &str) {
	let err = outcome.as_ref().expect_err(why);
	assert_ne!(
		err.code(),
		Code::Unauthenticated,
		"{why}: an UNAUTHENTICATED means the request reached the server's token check: {err}"
	);
	assert_eq!(pulls.load(Ordering::SeqCst), 0, "{why}: the server's handler must never have run: {err}");
	if let Some(verdict) = verdict {
		assert!(err.message().contains(verdict), "{why}: the client must have refused the certificate ({verdict}), got: {err}");
	}
}

#[tokio::test]
async fn a_pinned_ca_authenticates_the_server_and_the_pull_goes_through() {
	let subject = format!("itest-bridge-tls-{}", uuid::Uuid::new_v4());
	let response = with_server(&SERVER, vec![created(&subject)], |port, pulls| async move {
		let response = pull_once(&format!("https://localhost:{port}"), &pinned_ca())
			.await
			.expect("the handshake succeeds against the CA the server's certificate chains to");
		assert_eq!(pulls.load(Ordering::SeqCst), 1, "one pull reached the server");
		response
	})
	.await;
	assert_eq!(response.events.len(), 1, "the batch came back over the TLS channel");
	assert_eq!(response.events[0].auth_subject, subject);
}

#[tokio::test]
async fn a_certificate_from_another_ca_never_reaches_the_server() {
	let foreign = BridgeTlsFiles {
		ca: Some(fixture("other-ca.pem")),
		..BridgeTlsFiles::default()
	};
	with_server(&SERVER, vec![created("itest-bridge-tls-foreign")], |port, pulls| async move {
		let outcome = pull_once(&format!("https://localhost:{port}"), &foreign).await;
		assert_never_reached_the_server(
			&outcome,
			&pulls,
			Some("UnknownIssuer"),
			"a server whose certificate chains to a CA that is not the pinned one is not concierge",
		);
	})
	.await;
}

/// The name in the address is the name the certificate must carry, so `https://127.0.0.1`
/// is pinned to `127.0.0.1` and a certificate issued for `concierge` alone — the
/// production shape — is refused for it, while a certificate that does list the address
/// is accepted. Service discovery is never the proof; the certificate is.
#[tokio::test]
async fn the_server_name_is_pinned_to_the_host_of_the_address() {
	with_server(&SERVER_NAMED_CONCIERGE, vec![created("itest-bridge-tls-name")], |port, pulls| async move {
		let outcome = pull_once(&format!("https://127.0.0.1:{port}"), &pinned_ca()).await;
		assert_never_reached_the_server(
			&outcome,
			&pulls,
			Some("not valid for name \"127.0.0.1\""),
			"a certificate for `concierge` does not vouch for 127.0.0.1",
		);
	})
	.await;
	with_server(&SERVER_NAMED_CONCIERGE, vec![created("itest-bridge-tls-name")], |port, pulls| async move {
		let outcome = pull_once(&format!("https://localhost:{port}"), &pinned_ca()).await;
		assert_never_reached_the_server(
			&outcome,
			&pulls,
			Some("not valid for name \"localhost\""),
			"a certificate for `concierge` does not vouch for localhost either",
		);
	})
	.await;
	with_server(&SERVER, vec![created("itest-bridge-tls-name")], |port, pulls| async move {
		pull_once(&format!("https://127.0.0.1:{port}"), &pinned_ca())
			.await
			.expect("a certificate that lists 127.0.0.1 is accepted for it");
		assert_eq!(pulls.load(Ordering::SeqCst), 1);
	})
	.await;
}

/// mTLS is switched on by the server asking and the hub answering; nothing on this side
/// changes but two variables. Without the identity the handshake fails before any request;
/// with it the pull goes through.
#[tokio::test]
async fn a_server_demanding_a_client_certificate_gets_one_only_when_an_identity_is_configured() {
	with_server(&SERVER_DEMANDING_A_CLIENT_CERT, vec![created("itest-bridge-tls-mtls")], |port, pulls| async move {
		let outcome = pull_once(&format!("https://localhost:{port}"), &pinned_ca()).await;
		assert_never_reached_the_server(&outcome, &pulls, None, "no client identity, so the server's client-CA check fails the handshake");
	})
	.await;
	with_server(&SERVER_DEMANDING_A_CLIENT_CERT, vec![created("itest-bridge-tls-mtls")], |port, pulls| async move {
		pull_once(&format!("https://localhost:{port}"), &pinned_ca_with_identity())
			.await
			.expect("the hub's identity chains to the CA the server trusts");
		assert_eq!(pulls.load(Ordering::SeqCst), 1);
	})
	.await;
}

// ── the real consumer, over TLS, against Postgres ──────────────────────────────

/// The bridge cursor is one global row, so the two consumer tests take turns.
static CURSOR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn user_exists(pool: &PgPool, subject: &str) -> bool {
	sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM users WHERE auth_subject = $1)")
		.bind(subject)
		.fetch_one(pool)
		.await
		.expect("the users table is readable")
}

/// Run the real consumer over the endpoint built for `tls` against the fake concierge, give
/// it long enough to pull and apply, then report whether `subject` was mirrored.
async fn consumer_mirrors(pool: &PgPool, server: &ServerSide, tls: BridgeTlsFiles, subject: &str) -> bool {
	let _turn = CURSOR.lock().await;
	sqlx::query("UPDATE bridge_cursor SET position = 0 WHERE id = TRUE").execute(pool).await.expect("reset cursor");
	with_server(server, vec![created(subject)], |port, _| async move {
		let endpoint = bridge_endpoint(&format!("https://localhost:{port}"), &tls).expect("the endpoint builds");
		let stop = CancellationToken::new();
		let consumer = BridgeConsumer::new(pool.clone(), endpoint.connect_lazy(), BRIDGE_TOKEN.to_string(), Duration::from_millis(50)).run(stop.clone());
		let observe = async {
			tokio::time::sleep(Duration::from_millis(600)).await;
			let mirrored = user_exists(pool, subject).await;
			stop.cancel();
			mirrored
		};
		let ((), mirrored) = tokio::join!(consumer, observe);
		mirrored
	})
	.await
}

#[tokio::test]
async fn the_consumer_mirrors_events_pulled_over_a_pinned_tls_channel() {
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping real-DB test");
		return;
	};
	let subject = format!("itest-bridge-tls-{}", uuid::Uuid::new_v4());
	assert!(consumer_mirrors(&pool, &SERVER, pinned_ca(), &subject).await, "a CREATED pulled over TLS lands in `users`");
}

#[tokio::test]
async fn the_consumer_applies_nothing_from_a_server_the_pinned_ca_does_not_vouch_for() {
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping real-DB test");
		return;
	};
	let subject = format!("itest-bridge-tls-{}", uuid::Uuid::new_v4());
	let foreign = BridgeTlsFiles {
		ca: Some(fixture("other-ca.pem")),
		..BridgeTlsFiles::default()
	};
	assert!(
		!consumer_mirrors(&pool, &SERVER, foreign, &subject).await,
		"a server the pinned CA does not vouch for is not concierge, and nothing it says reaches the money plane"
	);
}
