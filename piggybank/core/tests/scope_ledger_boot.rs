//! The scope ledger listener booted the way `main` boots it — a test binary of its own, so
//! nothing else in the process installs a rustls crypto provider first.
//!
//! The hub once panicked on the listener's first handshake ("Could not automatically
//! determine the process-level CryptoProvider"): its graph carries both `ring` and
//! `aws-lc-rs`, a rustls server refuses to pick, and the suites that stood up TLS servers had
//! each installed a provider themselves — so they passed on a provider production never
//! installed. Here the only installation is `main`'s own call, and the server is production's
//! `serve` with the hub's authorizer. One test per binary on purpose: a second test sharing
//! the process would inherit the provider and prove nothing.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use domain::scopes::ScopeId;
use evbanking_auth::{AuthConfig, AuthService, provisioner_channel};
use evbanking_contracts::scope::v1::{GetBalancesRequest, scope_ledger_service_client::ScopeLedgerServiceClient};
use piggybank_core::{
	config::ScopeLedgerSettings,
	infrastructure::{compartments::PgCompartments, scope_ledger::PgScopeLedger},
	services::scope_ledger::{self, ScopeLedgerDeps, ScopeLedgerListener},
};
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;
use tonic::{
	Code, Request,
	transport::{Certificate, ClientTlsConfig, Endpoint, Identity},
};

mod common;

fn fixture(name: &str) -> String {
	let path = format!("{}/tests/fixtures/scope_tls/{name}", env!("CARGO_MANIFEST_DIR"));
	std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("fixture {name} must be readable ({err}); regenerate with fixtures/scope_tls/regen.sh"))
}

async fn ephemeral() -> SocketAddr {
	TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap()
}

#[tokio::test]
async fn the_listener_handshakes_with_the_provider_main_installs() {
	assert!(
		rustls::crypto::CryptoProvider::get_default().is_none(),
		"nothing in this binary may install a provider before main's call"
	);
	piggybank_core::crypto::install_crypto_provider();

	let Some(pool) = common::pool().await else { return };
	let Some(ledger) = common::seeded_ledger(&pool, "scope ledger boot test").await else {
		return;
	};
	let scope = ScopeId::parse("invest").unwrap();
	let grants = format!(
		r#"{{"clients":[{{"client_id":"invest","cert_sha256":["{}"]}}],"grants":[{{"client_id":"invest","scope":"invest","label_prefix":"alloc:"}}]}}"#,
		fixture("invest.sha256").trim()
	);
	let (clients, grants) = ScopeLedgerSettings::parse_grants(&grants).unwrap();
	let addr = ephemeral().await;
	let listener = ScopeLedgerListener::new(
		ScopeLedgerSettings {
			bind: addr,
			cert_pem: fixture("server.pem"),
			key_pem: fixture("server.key"),
			client_ca_pem: fixture("ca.pem"),
			clients,
			grants,
		},
		ScopeLedgerDeps {
			store: Arc::new(PgScopeLedger::new(pool.clone())),
			holds: Arc::new(PgScopeLedger::new(pool.clone())),
			events: Arc::new(PgScopeLedger::new(pool.clone())),
			invoices: Arc::new(PgScopeLedger::new(pool.clone())),
			scopes: Arc::new(PgCompartments::new(pool.clone())),
			ledger,
			relay: Arc::new(Notify::new()),
		},
	);
	// No signing key: the authorizer answers every token NotConfigured, which is enough to
	// show the request crossed the handshake and reached the auth layer.
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

	let stop = CancellationToken::new();
	let serve = async {
		let (served, auth) = tokio::join!(
			scope_ledger::serve(listener, &authorizer, stop.clone().cancelled_owned()),
			auth_service.run(auth_addr, stop.clone().cancelled_owned())
		);
		auth.expect("the auth task runs until cancelled");
		served.expect("the listener serves until cancelled");
	};
	let dial = async {
		let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
		while tokio::net::TcpStream::connect(addr).await.is_err() {
			assert!(tokio::time::Instant::now() < deadline, "the listener never bound");
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
		let tls = ClientTlsConfig::new()
			.ca_certificate(Certificate::from_pem(fixture("ca.pem")))
			.domain_name("localhost")
			.identity(Identity::from_pem(fixture("invest.pem"), fixture("invest.key")));
		let channel = Endpoint::from_shared(format!("https://localhost:{}", addr.port()))
			.unwrap()
			.tls_config(tls)
			.unwrap()
			.connect_lazy();
		let mut request = Request::new(GetBalancesRequest {
			scope: scope.as_str().to_owned(),
			labels: Vec::new(),
		});
		request.metadata_mut().insert("authorization", "Bearer any".parse().unwrap());
		let outcome = ScopeLedgerServiceClient::new(channel).get_balances(request).await;
		stop.cancel();
		outcome
	};
	let ((), outcome) = tokio::join!(serve, dial);
	let status = outcome.expect_err("nothing verifies without a signing key");
	assert_eq!(
		(status.code(), status.message()),
		(Code::Unavailable, "auth not configured"),
		"the handshake completed and the auth layer answered"
	);
}
