//! The wallet BFF — a standalone, stateless HTTP orchestration service.
//!
//! It is the wallet host's egress boundary: it proxies the browser's same-origin `/api/*`
//! JSON requests to two gRPC planes — **concierge** (identity: profile/directory) and
//! **piggybank** (money: wallet, consilium, health). Auth is SHELL-owned: the concierge
//! auth web surface (reached via the conductor's `/api/auth/*` on the shared origin)
//! signs users in and sets the zone-shared `ev_access` JWT cookie; the BFF verifies
//! that cookie locally against the concierge JWKS and, for money routes, mints the
//! SEPARATE banking (`aud=banking-core`) pair for the verified subject. The wallet
//! host runs no OAuth and holds no session.

// `ev::settings!` walks `AppConfig` one field and one attribute at a time; with the wallet
// fields the expansion passes the default limit of 128.
#![recursion_limit = "256"]

use std::sync::Arc;

use color_eyre::eyre::Context;
use ev::error_monitoring::{self, Config as SentryConfig};
use evconcierge_auth::{Verifier, VerifierConfig};

mod config;
mod cookies;
mod dto;
mod error;
mod governance;
mod routes;
mod session;
mod state;
mod util;
mod wallet_intents;

use config::AppConfig;
use cookies::CookieNames;
use routes::approval::AttemptLimiter;
use session::BankingTokens;
use state::{AppState, Grpc};

// Sentry must be initialised before the async runtime starts — no `#[tokio::main]`.
fn main() -> color_eyre::Result<()> {
	color_eyre::install()?;
	dotenvy::dotenv().ok();

	// The deploy contract, straight out of the image: the gitops preflight runs
	// this against the built image and diffs it with the cluster Secret's keys,
	// so a missing variable is caught before the rollout, not as a
	// CrashLoopBackOff after it.
	if let Some(profile) = print_required_vars_for() {
		for var in AppConfig::required_var_names(&profile) {
			println!("{var}");
		}
		return Ok(());
	}

	// One env read at boot; every missing/invalid variable fails HERE, in one
	// aggregate error, before anything binds — exiting 78 (EX_CONFIG), so "the
	// config is wrong" is distinguishable from a dependency blip.
	let config = ev::settings::or_exit(AppConfig::from_env());
	if let Err(reason) = config.check_script_origins().and_then(|()| config.check_wallet_actions()) {
		eprintln!("config: {reason}");
		// EX_CONFIG, as `or_exit` uses for every other configuration error.
		std::process::exit(78);
	}

	// Guard must stay alive for the duration of main — dropping it flushes events. A
	// `None` DSN makes `init` return `None`, so this binding is simply inert.
	let _sentry_guard = error_monitoring::init(&SentryConfig {
		dsn: config.sentry_dsn.clone(),
		environment: config.app_env.clone(),
		release: error_monitoring::release_name!().map(|r| r.into_owned()),
		// Several services share one Sentry project, so this is what tells them
		// apart in an issue list. Same name as OTEL_SERVICE_NAME, so a Sentry
		// issue, a trace and a log line agree.
		service: Some("cabinet-backend".to_string()),
		traces_sample_rate: SentryConfig::traces_sample_rate_for(&config.app_env),
	});

	// Held for the process lifetime — dropping flushes OTel logs/traces.
	let _otel_guard = init_tracing(&config.app_env);

	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.context("failed to build tokio runtime")?
		.block_on(run(config))
}

async fn run(config: AppConfig) -> color_eyre::Result<()> {
	// Product-analytics capture (native PostHog). A `None` key makes capture a silent
	// no-op, so this is safe to construct unconfigured.
	let _analytics = ev::analytics::Analytics::new(config.posthog_key.clone(), config.posthog_host.clone());

	let grpc = Grpc::connect_lazy(
		&config.piggybank_grpc_addr,
		&config.banking_auth_grpc_addr,
		&config.concierge_grpc_addr,
		Some(config.banking_issuance_token.0.clone()),
	)
	.context("invalid gRPC address (PIGGYBANK_GRPC_ADDR / BANKING_AUTH_GRPC_ADDR / CONCIERGE_GRPC_ADDR)")?;

	let bind_addr = config.bind;
	tracing::info!(
		bind = %bind_addr,
		piggybank = %config.piggybank_grpc_addr,
		concierge = %config.concierge_grpc_addr,
		"wallet BFF listening"
	);

	// Local verification of the shell-set access JWT: the JWKS is cached from the
	// concierge plane's public `Jwks` RPC (lazy — the first verify warms it), so no
	// per-request round trip. Fails closed until the plane publishes keys.
	let jwks_endpoint = std::env::var("AUTH_JWKS_GRPC_ENDPOINT").unwrap_or_else(|_| config.concierge_grpc_addr.clone());
	let verifier = Verifier::try_new(VerifierConfig {
		issuer: config.auth_issuer.clone(),
		audiences: vec![config.auth_client_audience.clone()],
		allowed_types: vec![evconcierge_auth::TokenType::Access],
		jwks_grpc_endpoint: jwks_endpoint.clone(),
	})
	.context("failed to build the access-token verifier")?;
	// Step-up proofs are signed with the access tokens' key, so they are checked against the
	// same JWKS endpoint.
	let wallet_actions = wallet_intents::WalletActions::connect(&config, &jwks_endpoint)
		.await
		.map_err(|reason| color_eyre::eyre::eyre!("wallet actions: {reason}"))?;

	let state = AppState {
		cookies: Arc::new(CookieNames::new(config.cookie_secure())),
		banking: Arc::new(BankingTokens::new()),
		approvals: Arc::new(AttemptLimiter::default()),
		money_mutations: Arc::new(crate::routes::money_mutation_limiter()),
		verifier,
		wallet_actions: wallet_actions.map(Arc::new),
		grpc,
		config: Arc::new(config),
	};

	let listener = tokio::net::TcpListener::bind(bind_addr).await.context("failed to bind HTTP listener")?;
	// `with_connect_info` so the public approval routes have a peer address to fall back
	// on when no proxy header is present — an audit field and a rate-limit key, never an
	// authorization input.
	//
	// Graceful on SIGTERM: the kubelet sends it on every rollout of this pod, and without
	// this the listener and every in-flight response died with the process — a read that
	// was a millisecond from its answer became a bodyless proxy error in the wallet host.
	// Now the listener closes, the in-flight requests finish (each already bounded by
	// the router's outer deadline), and only then does the process exit;
	// `terminationGracePeriodSeconds` remains the SIGKILL backstop.
	let serve = axum::serve(listener, routes::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(shutdown_signal());
	tokio::select! {
		never = AppConfig::watch_drift() => match never {},
		result = serve => result.context("wallet BFF HTTP server error"),
	}
}

/// Resolve on the first of `SIGINT` (ctrl_c) or, on Unix, `SIGTERM` — the same pair
/// piggybank drains on. A failed SIGTERM registration is logged and the wait falls back
/// to ctrl_c alone rather than refusing to boot: the process can still be force-killed.
async fn shutdown_signal() {
	#[cfg(unix)]
	{
		use tokio::signal::unix::{SignalKind, signal};
		match signal(SignalKind::terminate()) {
			Ok(mut term) => {
				tokio::select! {
					result = tokio::signal::ctrl_c() => {
						if let Err(err) = result {
							tracing::error!("failed to listen for ctrl_c: {err}");
							// ctrl_c is gone but SIGTERM still works; keep serving until it.
							term.recv().await;
						}
					},
					_ = term.recv() => {},
				}
			}
			Err(err) => {
				tracing::error!("failed to install SIGTERM handler: {err}");
				if let Err(err) = tokio::signal::ctrl_c().await {
					tracing::error!("failed to listen for ctrl_c: {err}");
					std::future::pending::<()>().await;
				}
			}
		}
	}
	#[cfg(not(unix))]
	if let Err(err) = tokio::signal::ctrl_c().await {
		tracing::error!("failed to listen for ctrl_c: {err}");
		std::future::pending::<()>().await;
	}
	tracing::info!("shutdown signal received — draining");
}

// Returns the OTel guard (flushes/shuts down on drop); bind it in `main`. `None`
// when OTEL_EXPORTER_OTLP_ENDPOINT is unset — the layers are then inert.
/// `--print-required-vars[=PROFILE]` (default `production`). Hand-rolled: this
/// binary has no other CLI surface, and a whole arg parser for one flag is not
/// worth the dependency.
fn print_required_vars_for() -> Option<String> {
	const FLAG: &str = "--print-required-vars";

	let mut args = std::env::args().skip(1);
	let arg = args.next()?;
	match arg.split_once('=') {
		Some((FLAG, profile)) => Some(profile.to_string()),
		Some(_) => None,
		None if arg == FLAG => Some(args.next().unwrap_or_else(|| "production".to_string())),
		None => None,
	}
}

fn init_tracing(environment: &str) -> Option<ev::otel::Telemetry> {
	use tracing_subscriber::{EnvFilter, fmt, prelude::*};

	let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,cabinet_backend=debug"));
	let (otel_guard, otel_layers) = ev::otel::telemetry(&ev::otel::Config {
		environment: environment.to_string(),
		traces_sample_rate: ev::otel::Config::traces_sample_rate_for(environment),
	})
	.unzip();
	tracing_subscriber::registry()
		.with(filter)
		.with(fmt::layer().json())
		.with(error_monitoring::tracing_layer())
		.with(otel_layers)
		.init();
	otel_guard
}
