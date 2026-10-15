//! Composition root for the whole `piggybank` system.
//!
//! Loads config, opens the driven infrastructure (Postgres control plane,
//! TigerBeetle ledger), then runs two in-process tasks that talk over the
//! [`Authorizer`](evbanking_auth::Authorizer) channel:
//!   - the **auth service** ([`evbanking_auth::AuthService`]) — issuance gRPC routes
//!     + the authorize channel, on `auth_grpc_addr`;
//!   - the **core** gRPC services (health/users/balance/funds/wallet) on `grpc_addr`,
//!     authorizing each request via the `Authorizer` core got from auth.

use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use color_eyre::eyre::{Context, ensure, eyre};
use domain::{book::PriceTimeEngine, money::Network};
use ev::error_monitoring::{self, Config as SentryConfig};
use evbanking_auth::{AuthConfig, AuthService, ServiceTokenSource, provisioner_channel};
use evbanking_contracts::signer::v1::signer_service_client::SignerServiceClient;
use piggybank_core::{
	AppState,
	application::{
		auth_sync,
		book::BookFeed,
		scope_operations::{ApprovalExecutor, CatalogGate},
	},
	config::{self, Rails},
	infrastructure::{
		allocations::PgAllocations,
		approvals::ConciergeApprovals,
		book::PgBook,
		bridge::{
			BridgeConsumer,
			endpoint::{BridgeTlsFiles, bridge_endpoint},
		},
		compartments::PgCompartments,
		consilium::PgConsilia,
		consilium_mailer::{self, ConsiliumMailer},
		consilium_sweeper::ConsiliumSweeper,
		custody::{ChainCustody, MultiChainCustody, StubCustody},
		db,
		deposit_watcher::DepositWatcher,
		deposits::PgDeposits,
		dispatcher::Dispatcher,
		fee_policy_changes::PgFeePolicyChanges,
		fee_sweeper::FeeSweeper,
		fees::{PgFeeAssessments, PgFeePolicies, PgFeeSettlements, PgPositionAccruals},
		governance_mail,
		issuance::PgUnitIssuances,
		ledger::{self, TbLedger},
		nav::PgNav,
		operation_feed::PgOperationFeed,
		outflow::PgOutflowPolicy,
		payments::PgPayments,
		positions::PgFundPositions,
		reaper::Reaper,
		reconciliation::Reconciliation,
		redemptions::PgRedemptions,
		relay::Relay,
		scope_events::ScopeEventsConsumer,
		scope_ledger::PgScopeLedger,
		signer_addresses::SignerDepositAddresses,
		subscriptions::PgSubscriptions,
		sweep::Sweep,
		tigerbeetle::TigerBeetle,
		ton_custody::TonCustody,
		ton_deposit_watcher::TonDepositWatcher,
		ton_sweep::TonSweep,
		ton_withdrawal_watcher::TonWithdrawalWatcher,
		treasury_drift::TreasuryDrift,
		tron_custody::TronCustody,
		tron_deposit_watcher::TronDepositWatcher,
		tron_sweep::TronSweep,
		tron_withdrawal_watcher::TronWithdrawalWatcher,
		users::PgUsers,
		withdrawal_watcher::WithdrawalWatcher,
		withdrawals::PgWithdrawals,
	},
	ports::{
		AllocationRegistry, BookStore, CompartmentStore, ConsiliumRepository, Custody, DepositAddresses, Deposits, FeePorts, FundPositionReader, NavMarks, OperationFeed, OutflowPolicy,
		PaymentFeed, PaymentRepository, RedemptionRepository, SubscriptionRepository, UnitIssuanceRepository, UserRepository, WithdrawalRepository, ledger::Ledger,
	},
	services::{
		self,
		scope_ledger::{ScopeLedgerDeps, ScopeLedgerListener},
	},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

// Sentry must be initialised before the async runtime starts — no #[tokio::main].
fn main() -> color_eyre::Result<()> {
	color_eyre::install()?;
	// Before any TLS server or client is built — see `crypto.rs`.
	piggybank_core::crypto::install_crypto_provider();
	dotenvy::dotenv().ok();

	let command = Command::from_args(std::env::args().skip(1))?;
	// The deploy contract, straight out of the image: the gitops preflight runs
	// this against the built image and diffs it with the cluster Secret's keys,
	// so a missing variable is caught before the rollout, not as a
	// CrashLoopBackOff after it.
	if let Command::PrintRequiredVars(profile) = &command {
		for var in config::AppConfig::required_var_names(profile) {
			println!("{var}");
		}
		return Ok(());
	}

	// One env read at boot; every missing/invalid variable fails HERE, in one
	// aggregate error, before anything binds — exiting 78 (EX_CONFIG), so "the
	// config is wrong" is distinguishable from a dependency blip.
	let config = ev::settings::or_exit(config::AppConfig::from_env());

	// Guard must stay alive for the duration of main — dropping it flushes events.
	// `None` DSN → `init` returns `None`, so this binding is simply inert.
	let _sentry_guard = error_monitoring::init(&SentryConfig {
		dsn: config.sentry_dsn.clone(),
		environment: config.app_env.clone(),
		release: error_monitoring::release_name!().map(|r| r.into_owned()),
		// Several services share one Sentry project, so this is what tells them
		// apart in an issue list. Same name as OTEL_SERVICE_NAME, so a Sentry
		// issue, a trace and a log line agree.
		service: Some("piggybank-core".to_string()),
		traces_sample_rate: SentryConfig::traces_sample_rate_for(&config.app_env),
	});

	// Held for the process lifetime — dropping flushes OTel logs/traces.
	let _otel_guard = init_tracing(&config.app_env);

	let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("failed to build tokio runtime")?;
	match command {
		Command::Serve => runtime.block_on(run(config)),
		Command::PrintRequiredVars(_) => unreachable!("answered before the runtime was built"),
	}
}

async fn run(config: config::AppConfig) -> color_eyre::Result<()> {
	// The on-chain rails keep their env-based conditional construction — a rail
	// runs only when its endpoint var is set. Production asserts the rail set below.
	let rails = Rails::from_env().context("failed to load the on-chain rail configuration")?;
	// Claims are network-agnostic, so a testnet rail sharing this hub with a mainnet rail lets a
	// free testnet deposit be withdrawn as real mainnet USDT. Refuse such a mix in EVERY env (the
	// dangerous flip happens in staging), before any infra is dialed.
	rails.assert_single_realm()?;

	// The verification gate on money movement, read once and carried everywhere it is
	// applied. Announced at boot for the same reason the signer's spend policy is: a gate
	// that is off is visible in a pod's first log lines, not only in whoever's memory of
	// the environment.
	let kyc_gate = config::KycGate::from_env();
	if kyc_gate.is_enforced() {
		tracing::info!("kyc gate enforced — deposit addresses and withdrawals require a verified tier");
	} else {
		tracing::warn!("kyc gate LIFTED (KYC_GATE_ENABLED) — unverified users can be issued deposit addresses and can withdraw; unset the variable to restore the gate");
	}

	let auth_config = AuthConfig::from_env().context("failed to load auth configuration")?;
	// The auth crate's optional seams (absent ⇒ inert/fail-closed) are a dev/CI
	// affordance; production must never boot inert. Same for the refresh store's
	// in-process fallback — restart-lossy, dev-only. Asserted BEFORE any infra
	// dial so a misconfigured release dies instantly, not after a connect timeout.
	if config.app_env == "production" {
		ensure!(
			auth_config.signing.is_some(),
			"production requires the auth signing triple (AUTH_SIGNING_KEY_PEM/AUTH_SIGNING_KID/AUTH_JWKS_JSON)"
		);
		ensure!(
			auth_config.issuance_token.is_some(),
			"production requires BANKING_ISSUANCE_TOKEN (the BFF's IssueUserToken bearer) — without it every money route is NotConfigured"
		);
		ensure!(
			std::env::var("REDIS_URL").is_ok_and(|v| !v.is_empty()),
			"production requires REDIS_URL (refresh-token state must survive restarts)"
		);
		// Deposits are the product: a rail-less prod hub would boot healthy and
		// quietly serve no deposit path (the exact incident this guards against).
		ensure!(
			!rails.configured_networks().is_empty(),
			"production requires at least one on-chain deposit rail (set BSC_RPC_URL and/or TON_API_URL)"
		);
		// TRON stays out of this sweep while TRC20_FROZEN forces it off.
		for (gate, missing) in [
			("BSC_RPC_URL", rails.bsc.is_none()),
			("POLYGON_RPC_URL", rails.polygon.is_none()),
			("TON_API_URL", rails.ton.is_none()),
		] {
			if missing {
				tracing::error!("on-chain rail unconfigured in production: {gate} is unset — its deposits/withdrawals/custody are off");
			}
		}
		if let Some(ton) = &rails.ton {
			ensure!(
				ton.api_key.is_some() || ton.is_testnet,
				"production TON mainnet requires TON_API_KEY — the anonymous toncenter tier is rate-limited below the watcher's poll cadence"
			);
		}
		ensure!(
			std::env::var("SERVICE_TOKEN").is_ok_and(|v| !v.is_empty()),
			"production requires SERVICE_TOKEN (the hub→signer bearer) — without it deposit-address provisioning and custody fail Unauthenticated"
		);
	}
	// Production refuses to pull the lifecycle stream from a peer it cannot authenticate
	// (#199, phase 2) — asked here, with the other production guards and before any infra
	// dial, and before the endpoint exists, so the refusal reads as policy and not as a
	// transport error. The endpoint built next carries both cross-plane seams, the
	// lifecycle consumer and the governance-mail relay: one address, one trust
	// relationship, one place it is configured.
	// The scope ledger's listener, read and checked with the other boot guards: a grants file
	// that grants something unsafe, or a CA that is not one, stops the boot here.
	// EX_CONFIG (78), like every other configuration refusal: "the config is wrong" must not
	// read as a crash a restart could fix.
	let scope_ledger_settings = match config::ScopeLedgerSettings::from_env() {
		Ok(settings) => settings,
		Err(err) => {
			tracing::error!("scope ledger listener configuration refused: {err:#}");
			eprintln!("scope ledger listener configuration refused: {err:#}");
			std::process::exit(78);
		}
	};
	if scope_ledger_settings.is_none() {
		tracing::info!("scope ledger listener off (SCOPE_LEDGER_BIND unset) — no service can move money in a scope");
	}
	let bridge_tls = BridgeTlsFiles::from_env();
	config::ensure_bridge_is_authenticated(&config.app_env, &config.concierge_bridge_addr, bridge_tls.ca.as_deref())?;
	let concierge_endpoint = bridge_endpoint(&config.concierge_bridge_addr, &bridge_tls)?;

	// ── driven infrastructure ─────────────────────────────────────────────────
	// The hub applies pending control-plane migrations on boot (idempotent). New
	// migration FILES are authored with the sqlx CLI (`sqlx migrate add …`), never
	// hand-written.
	let pool = db::connect_sized(&config.database_url, config.db_max_connections)
		.await
		.context("failed to connect to the database")?;
	db::migrate(&pool).await.context("failed to apply database migrations")?;
	// Refuse to boot an EVM rail whose persisted state belongs to a different chain (an endpoint
	// re-pointed under a live rail — the Amoy→mainnet flip). Config-vs-persisted only, so a dead
	// node is still a bootable rail. Must run after `migrate` (the table) and before any watcher.
	for evm in [rails.bsc.as_ref(), rails.polygon.as_ref()].into_iter().flatten() {
		db::bind_chain_identity(&pool, evm.network, evm.chain_id).await?;
	}
	let tigerbeetle_cluster_id: u128 = config.tigerbeetle_cluster_id.parse().context("TIGERBEETLE_CLUSTER_ID must be an integer")?;
	let tigerbeetle = Arc::new(TigerBeetle::connect(tigerbeetle_cluster_id, &config.tigerbeetle_address).context("failed to connect to TigerBeetle")?);

	// The data-plane money gateway over TigerBeetle (it also holds the pool for the
	// `tb_accounts` id-map — its own non-transactional concern). Seed the fund's
	// singleton accounts (a custody wallet + capital claim per network, plus the bank
	// mock) at boot; idempotent.
	let ledger: Arc<dyn Ledger> = Arc::new(TbLedger::new(tigerbeetle, pool.clone()));
	ledger::seed_singletons(ledger.as_ref()).await.context("failed to seed the fund's ledger accounts")?;

	// Product-analytics capture (native PostHog). A `None` key makes capture a
	// silent no-op, so this is safe to construct unconfigured.
	let analytics = ev::analytics::Analytics::new(config.posthog_key.clone(), config.posthog_host.clone());

	let users: Arc<dyn UserRepository> = Arc::new(PgUsers::new(pool.clone()));
	let withdrawals: Arc<dyn WithdrawalRepository> = Arc::new(PgWithdrawals::new(pool.clone()));
	let consilia: Arc<dyn ConsiliumRepository> = Arc::new(PgConsilia::new(pool.clone()));
	// ONE adapter behind two ports: `PgPayments` implements the write repository and the
	// admin read model, and they are handed out separately so a surface can be granted the
	// history without the surface that opens orders.
	let pg_payments = Arc::new(PgPayments::new(pool.clone()));
	let payments: Arc<dyn PaymentRepository> = pg_payments.clone();
	let payment_feed: Arc<dyn PaymentFeed> = pg_payments;
	let allocations: Arc<dyn AllocationRegistry> = Arc::new(PgAllocations::new(pool.clone()));
	let subscriptions: Arc<dyn SubscriptionRepository> = Arc::new(PgSubscriptions::new(pool.clone()));
	let issuances: Arc<dyn UnitIssuanceRepository> = Arc::new(PgUnitIssuances::new(pool.clone()));
	let redemptions: Arc<dyn RedemptionRepository> = Arc::new(PgRedemptions::new(pool.clone()));
	// The allocation book: orders and fills in Postgres under one write lock per product,
	// escrow and settlement through the relay. Price-time priority is the one matching
	// rule today; it is a port so another can be wired here without touching the store.
	let book: Arc<dyn BookStore> = Arc::new(PgBook::new(pool.clone()));
	let book_feed = BookFeed::new();
	let deposits: Arc<dyn Deposits> = Arc::new(PgDeposits::new(pool.clone()));
	let nav: Arc<dyn NavMarks> = Arc::new(PgNav::new(pool.clone()));
	let positions: Arc<dyn FundPositionReader> = Arc::new(PgFundPositions::new(pool.clone()));
	// The fee plane. Charging moves units between holders on the Share ledger, so none of
	// these touch cash; only `settlements` ever crosses into it, once per period per fund.
	let fees = FeePorts {
		policies: Arc::new(PgFeePolicies::new(pool.clone())),
		changes: Arc::new(PgFeePolicyChanges::new(pool.clone())),
		accruals: Arc::new(PgPositionAccruals::new(pool.clone())),
		assessments: Arc::new(PgFeeAssessments::new(pool.clone())),
		settlements: Arc::new(PgFeeSettlements::new(pool.clone())),
	};
	let operations: Arc<dyn OperationFeed> = Arc::new(PgOperationFeed::new(pool.clone()));

	// Deposit addresses are provisioned by the separate-process signer (it mints + seals
	// the keypair and returns the address; the hub never holds the key). Connect lazily so
	// the hub boots even if the signer starts after it — the first provision call is when
	// the signer must be reachable. Cached addresses are served from Postgres without it.
	let mut signer_endpoint = Endpoint::from_shared(config.signer_grpc_addr.clone())
		.context("SIGNER_GRPC_ADDR must be a valid URL, e.g. http://127.0.0.1:50053")?
		// Explicit deadlines so a half-open/stalled signer surfaces as a bounded error
		// (mapped to a retryable provisioning failure) instead of hanging the request.
		.connect_timeout(Duration::from_secs(3))
		.timeout(Duration::from_secs(10));
	// An `https://` signer target is off-host (a distinct trust domain on the network):
	// require TLS on the seam so provisioning — and later, signing — never traverses it in
	// cleartext. Loopback `http://` is the single documented single-host exception.
	if config.signer_grpc_addr.starts_with("https://") {
		signer_endpoint = signer_endpoint.tls_config(signer_client_tls()?).context("failed to configure signer TLS")?;
	}
	let signer_channel = signer_endpoint.connect_lazy();
	// Authenticate the hub's onward calls to the signer with a service token (out-of-band
	// `SERVICE_TOKEN` until the `MintServiceToken` RPC lands). `None` in unconfigured dev/CI.
	let service_token = ServiceTokenSource::from_env();
	let deposit_addresses: Arc<dyn DepositAddresses> = Arc::new(SignerDepositAddresses::new(pool.clone(), SignerServiceClient::new(signer_channel.clone()), service_token));

	// The single-worker outbox relay moves money in TigerBeetle after each commit
	// (Write-Last); command handlers nudge it through `relay_notify` for low latency.
	// Custody is a separate trust domain — a stub stands in until the real signing
	// service exists; the relay broadcasts a withdrawal's on-chain leg through it.
	// It gets its own small pool so a burst of request traffic can't starve money
	// dispatch (and vice-versa) — the two planes no longer share the request pool.
	let relay_pool = db::connect_sized(&config.database_url, config.relay_db_max_connections)
		.await
		.context("failed to connect the relay's database pool")?;
	let relay_notify = Arc::new(Notify::new());
	// Per-rail custody registry: each configured chain registers its adapter, keyed by network;
	// the relay holds the one `MultiChainCustody` that fans out by `request.network`. An unwired
	// rail falls through to the no-op stub (an operator settles manually), so unconfigured dev/CI
	// is unaffected — same gate per chain as its deposit watcher.
	let mut custody_by_network: HashMap<Network, Arc<dyn Custody>> = HashMap::new();
	let bsc_custody: Option<Arc<ChainCustody>> = rails.bsc.as_ref().map(|bsc| {
		Arc::new(ChainCustody::new(
			pool.clone(),
			bsc,
			SignerServiceClient::new(signer_channel.clone()),
			ServiceTokenSource::from_env(),
		))
	});
	if let Some(bsc_custody) = &bsc_custody {
		custody_by_network.insert(Network::Bep20, bsc_custody.clone());
	}
	// Polygon (PoS) custody — the SECOND EVM rail, the same `ChainCustody` adapter as BSC,
	// distinguished by its `EvmConfig::network` (Polygon), which scopes its own nonce sequence + treasury key.
	let polygon_custody: Option<Arc<ChainCustody>> = rails.polygon.as_ref().map(|polygon| {
		Arc::new(ChainCustody::new(
			pool.clone(),
			polygon,
			SignerServiceClient::new(signer_channel.clone()),
			ServiceTokenSource::from_env(),
		))
	});
	if let Some(polygon_custody) = &polygon_custody {
		custody_by_network.insert(Network::Polygon, polygon_custody.clone());
	}
	let tron_custody: Option<Arc<TronCustody>> = rails
		.tron
		.as_ref()
		.map(|tron| Arc::new(TronCustody::new(pool.clone(), signer_channel.clone(), ServiceTokenSource::from_env(), tron)));
	if let Some(tron_custody) = &tron_custody {
		custody_by_network.insert(Network::Trc20, tron_custody.clone());
	}
	// --- TON custody (jetton USDT) ---
	// One shared `Arc<TonCustody>` — the registry broadcasts through it, and the withdrawal
	// watcher reuses it to re-sign a stuck expired send at the same seqno (only the key-holder
	// path can do that safely). Held so the watcher (built below) can clone it.
	let ton_custody: Option<Arc<TonCustody>> = rails
		.ton
		.as_ref()
		.map(|ton| Arc::new(TonCustody::new(pool.clone(), signer_channel.clone(), ServiceTokenSource::from_env(), ton)));
	if let Some(ton_custody) = &ton_custody {
		custody_by_network.insert(Network::Ton, ton_custody.clone());
	}
	let custody: Arc<dyn Custody> = Arc::new(MultiChainCustody::new(custody_by_network, Arc::new(StubCustody)));
	let relay = Relay::new(relay_pool.clone(), ledger.clone(), custody.clone(), relay_notify.clone());

	// Recovery jobs, on the relay's dedicated pool so their periodic scans don't compete
	// with request traffic. Reconciliation watches the PG-vs-TB invariants and surfaces any
	// parked outbox row (TB wins, alert-only); the reaper owns the timeout for abandoned
	// sagas (alert on stuck `processing` withdrawals; auto-resolve the safe `queued` ones);
	// the dispatcher drains the accept-and-queue backlog once a rail is topped up.
	let reconciliation = Reconciliation::new(relay_pool.clone(), ledger.clone(), allocations.clone());
	let reaper = Reaper::new(relay_pool.clone(), withdrawals.clone(), redemptions.clone(), relay_notify.clone());
	let dispatcher = Dispatcher::new(relay_pool, withdrawals.clone(), ledger.clone(), custody.clone(), relay_notify.clone(), kyc_gate);
	// The fee sweeper is the only job that *charges* rather than repairs: management fees
	// accrue with the clock, so something has to wake up and collect them. It shares the
	// recovery jobs' cadence and their per-item warn-and-continue discipline.
	let fee_sweeper = FeeSweeper::new(
		fees.policies.clone(),
		fees.changes.clone(),
		fees.accruals.clone(),
		fees.assessments.clone(),
		ledger.clone(),
		nav.clone(),
		relay_notify.clone(),
	);
	// The per-rail counterpart to `reconciliation`: that one relates two TigerBeetle accounts,
	// so it stays green when the LEDGER and the CHAIN disagree. This compares `wallet:<net>`
	// against the wallets it claims to describe, which is the only thing that notices USDT
	// arriving out of band. Alert-only, and a no-op on rails with no chain view.
	// One outflow-policy handle for every path money leaves through — the sweeper's chained
	// payment execution reads the same pause the RPC handlers and the dispatcher do.
	let outflow: Arc<dyn OutflowPolicy> = Arc::new(PgOutflowPolicy::new(pool.clone()));
	let treasury_drift = TreasuryDrift::new(ledger.clone(), custody.clone(), outflow.clone());

	// ── cross-plane lifecycle bridge consumer (one-way concierge → banking) ─────
	// Pull concierge `UserLifecycleEvent`s and mirror them onto the `users` control
	// plane so money ops can be gated (a SUSPENDED user is frozen). Always configured
	// (the addr + shared token are boot-asserted config). The consumer holds its own
	// pool clone so its polling reads don't compete with request traffic on the
	// relay pool.
	let bridge = Some(BridgeConsumer::new(
		pool.clone(),
		concierge_endpoint.connect_lazy(),
		config.bridge_service_token.clone(),
		Duration::from_secs(config.bridge_poll_secs),
	));
	// The scope registry follows concierge's scope stream over the same seam and token: a
	// scope concierge freezes or closes stops taking money into its compartments here.
	let scope_events = ScopeEventsConsumer::new(
		pool.clone(),
		concierge_endpoint.connect_lazy(),
		config.bridge_service_token.clone(),
		Duration::from_secs(config.bridge_poll_secs),
	);

	// ── auth service + user provisioning (in-process) ──────────────────────────
	// Auth owns the keys/JWKS and hands core an `Authorizer` (core → auth, verify);
	// core hands auth a `Provisioner` (auth → core, upsert users) and drains it.
	// ── on-chain deposit watcher (BEP20 USDT) ──────────────────────────────────
	// Poll BSC for confirmed USDT transfers to users' derived deposit addresses and record
	// each (idempotent by tx_ref); the relay then credits the user's claim. Runs only when
	// BSC_RPC_URL is set — unconfigured dev/CI doesn't watch. Its own pool clone keeps the
	// polling reads off the request path.
	// The custody adapter rides along so the scan also watches the rail's TREASURY: USDT the
	// operator sends there arrives with no ledger fact behind it, and the sweep's own arrivals
	// are filtered out by sender so consolidation is never counted as new capital.
	let deposit_watcher = rails
		.bsc
		.clone()
		.map(|watcher_config| DepositWatcher::new(pool.clone(), relay_notify.clone(), watcher_config, bsc_custody.clone()));

	// ── on-chain withdrawal confirmation watcher (BEP20 USDT) ──────────────────
	// Auto-settle a broadcast withdrawal once its transaction confirms — the positive chain
	// signal the reaper lacks (it can only alert on a stuck `processing` withdrawal). Same
	// BSC gate; its own pool clone keeps the receipt reads off the request path.
	let withdrawal_watcher = rails.bsc.as_ref().map(|bsc| WithdrawalWatcher::new(pool.clone(), withdrawals.clone(), relay_notify.clone(), bsc));

	// ── treasury sweep (BEP20 USDT consolidation) ──────────────────────────────
	// Move user deposit balances on-chain into the treasury (a gas station tops up gas
	// first). Opt-in (BSC configured AND SWEEP_ENABLED); its own pool clone + signer channel.
	let sweep = match (&rails.bsc, &rails.sweep) {
		(Some(bsc), Some(sweep_config)) => Some(Sweep::new(pool.clone(), signer_channel.clone(), ServiceTokenSource::from_env(), bsc, sweep_config.clone())),
		_ => None,
	};

	// ── on-chain Polygon (PoS USDT) watchers + sweep ───────────────────────────
	// The second EVM rail reuses the SAME `DepositWatcher` / `WithdrawalWatcher` / `Sweep` as BSC,
	// each keyed by `EvmConfig::network` (Polygon), so their cursors/nonces/sweep scans stay disjoint
	// from BSC's. Each runs only when POLYGON_RPC_URL is set; the sweep additionally needs its enable.
	let polygon_deposit_watcher = rails
		.polygon
		.clone()
		.map(|watcher_config| DepositWatcher::new(pool.clone(), relay_notify.clone(), watcher_config, polygon_custody.clone()));
	let polygon_withdrawal_watcher = rails
		.polygon
		.as_ref()
		.map(|polygon| WithdrawalWatcher::new(pool.clone(), withdrawals.clone(), relay_notify.clone(), polygon));
	let polygon_sweep = match (&rails.polygon, &rails.polygon_sweep) {
		(Some(polygon), Some(sweep_config)) => Some(Sweep::new(pool.clone(), signer_channel.clone(), ServiceTokenSource::from_env(), polygon, sweep_config.clone())),
		_ => None,
	};

	// ── on-chain Tron (TRC20 USDT) watchers + sweep ────────────────────────────
	// The Tron analogues of the BEP20 seams above: deposit watcher, withdrawal confirmation
	// watcher, and (opt-in) sweep. Each runs only when TRON_RPC_URL is set; their own pool clones
	// keep the polling reads off the request path.
	let tron_deposit_watcher = rails.tron.as_ref().map(|tron| TronDepositWatcher::new(pool.clone(), relay_notify.clone(), tron));
	// The confirmation watcher shares the custody adapter so it can re-drive a stuck (expired,
	// unlanded) send — TRON has no nonce, so nothing else could recover it. Wired like the TON one.
	let tron_withdrawal_watcher = match (&rails.tron, &tron_custody) {
		(Some(tron), Some(tron_custody)) => Some(TronWithdrawalWatcher::new(pool.clone(), tron_custody.clone(), withdrawals.clone(), relay_notify.clone(), tron)),
		_ => None,
	};
	let tron_sweep = match (&rails.tron, &rails.tron_sweep) {
		(Some(tron), Some(sweep_config)) => Some(TronSweep::new(pool.clone(), signer_channel.clone(), ServiceTokenSource::from_env(), tron, sweep_config.clone())),
		_ => None,
	};

	// --- TON on-chain tasks (jetton USDT) ---
	// Deposit watcher + withdrawal confirmation watcher run when TON_API_URL is set; the
	// sweep additionally needs SWEEP_ENABLED. Each holds its own pool clone / signer channel,
	// mirroring the BEP20 tasks; all are no-ops (idle branch) when unconfigured.
	let ton_deposit_watcher = rails.ton.clone().map(|ton| TonDepositWatcher::new(pool.clone(), relay_notify.clone(), ton, ton_custody.clone()));
	let ton_withdrawal_watcher = match (&rails.ton, &ton_custody) {
		(Some(ton), Some(ton_custody)) => Some(TonWithdrawalWatcher::new(pool.clone(), ton_custody.clone(), withdrawals.clone(), relay_notify.clone(), ton)),
		_ => None,
	};
	let ton_sweep = match (&rails.ton, &rails.ton_sweep) {
		(Some(ton), Some(sweep_config)) => Some(TonSweep::new(pool.clone(), signer_channel.clone(), ServiceTokenSource::from_env(), ton, sweep_config.clone())),
		_ => None,
	};

	// ── consilium (multi-owner payout authorization) ───────────────────────────
	// The sweeper owns the two things the governance aggregate cannot hear on its own: a
	// 72h window running out, and an approval whose payout never got created (a crash
	// between the verdict and the money). Both are idempotent.
	let consilium_sweeper = ConsiliumSweeper {
		pool: pool.clone(),
		consilia: consilia.clone(),
		withdrawals: withdrawals.clone(),
		payments: payments.clone(),
		users: users.clone(),
		ledger: ledger.clone(),
		custody: custody.clone(),
		policy: outflow.clone(),
		allocations: allocations.clone(),
		fee_changes: fees.changes.clone(),
		issuances: issuances.clone(),
		deposits: deposits.clone(),
		addresses: deposit_addresses.clone(),
		subscriptions: subscriptions.clone(),
		nav: nav.clone(),
		notify: relay_notify.clone(),
		configured: Arc::from(rails.configured_networks()),
		kyc: kyc_gate,
		approval_url_base: config.consilium_approval_url_base.clone(),
		consent_url_base: config.payment_consent_url_base.clone(),
	};
	// The mail worker exists only when the seam is compiled in — see
	// `infrastructure::governance_mail`. Unwired, the queue still fills (nothing is lost)
	// and the boot line below says so loudly, because an approval mail that never goes out
	// is a consilium that can never reach quorum.
	let consilium_mailer = governance_mail_adapter(&concierge_endpoint, &config).map(|mailer| ConsiliumMailer::new(pool.clone(), mailer));
	governance_mail::announce_wiring(consilium_mailer::pending_count(&pool).await.unwrap_or_default());

	let (provisioner, provision_rx) = provisioner_channel();
	let (auth_service, authorizer) = AuthService::try_new(auth_config, provisioner).await.context("failed to build the auth service")?;

	let compartments: Arc<dyn CompartmentStore> = Arc::new(PgCompartments::new(pool.clone()));
	let scope_ledger_store = Arc::new(PgScopeLedger::new(pool.clone()));
	let state = AppState::new(
		pool,
		ledger,
		authorizer,
		analytics,
		users.clone(),
		withdrawals.clone(),
		consilia.clone(),
		payments,
		payment_feed,
		allocations,
		subscriptions,
		issuances,
		redemptions,
		book,
		Arc::new(PriceTimeEngine),
		book_feed,
		deposits,
		nav,
		positions,
		fees,
		operations,
		deposit_addresses,
		custody,
		Arc::from(rails.configured_networks()),
		kyc_gate,
		relay_notify,
		config.consilium_approval_url_base.clone(),
		config.payment_consent_url_base.clone(),
		rails.ton.as_ref().is_some_and(|ton| ton.is_testnet),
		outflow,
		compartments,
		scope_ledger_store.clone(),
	)
	.with_scope_grants(scope_ledger_settings.as_ref().map(|settings| settings.grants.clone()).unwrap_or_default());

	// Operations are only ever requested over the scope ledger's listener, so their executor
	// runs exactly when the listener does: a hub without it neither opens nor pulls banking's
	// approvals.
	let executor_wake = Arc::new(Notify::new());
	let (scope_ledger, approval_executor) = match scope_ledger_settings {
		Some(settings) => {
			let store = scope_ledger_store;
			let executor = ApprovalExecutor {
				grants: Arc::new(settings.grants.clone()),
				operations: store.clone(),
				// The bridge's endpoint and token: banking↔concierge is one trust relationship.
				approvals: Arc::new(ConciergeApprovals::new(concierge_endpoint.connect_lazy(), config.bridge_service_token.clone())),
				ledger: state.ledger.clone(),
				relay: state.relay_notify.clone(),
				wake: executor_wake.clone(),
				catalog: CatalogGate::new(),
			};
			let listener = ScopeLedgerListener::new(
				settings,
				ScopeLedgerDeps {
					store: store.clone(),
					holds: store.clone(),
					events: store.clone(),
					invoices: store.clone(),
					scopes: state.compartments.clone(),
					ledger: state.ledger.clone(),
					relay: state.relay_notify.clone(),
					payouts: store.clone(),
					operations: store,
					executor: executor_wake,
				},
			);
			(Some(listener), Some(executor))
		}
		None => (None, None),
	};
	let scope_ledger_authorizer = state.authorizer.clone();
	tracing::info!(
		core = %config.grpc_addr,
		auth = %config.auth_grpc_addr,
		scope_ledger = ?scope_ledger.as_ref().map(|listener| listener.bind),
		"piggybank listening"
	);

	// Graceful shutdown, structured (no detached spawns): the core server, the auth task,
	// the provisioning loop, and the recovery jobs run as branches of one `join!` on this
	// task. ctrl_c / SIGTERM cancel a shared token; the gRPC servers `serve_with_shutdown`
	// on it (draining in-flight requests) and the relay finishes its current drain iteration
	// before exiting (already crash-safe between rows). Each branch *also* cancels the token
	// when it returns on its own (a signal, an error), and every branch exits once the token
	// is cancelled — so the first to finish triggers the rest to wind down on their own terms
	// rather than being aborted mid-work, and `join!` waits for them all.
	let shutdown = CancellationToken::new();
	let (
		signal,
		core,
		scope_ledger_done,
		auth,
		provisioner,
		relay_done,
		reconciliation_done,
		treasury_drift_done,
		reaper_done,
		dispatcher_done,
		fee_sweeper_done,
		bridge_done,
		scope_events_done,
		approval_executor_done,
		watcher_done,
		withdrawal_watcher_done,
		sweep_done,
		polygon_watcher_done,
		polygon_withdrawal_watcher_done,
		polygon_sweep_done,
		tron_watcher_done,
		tron_withdrawal_watcher_done,
		tron_sweep_done,
		ton_watcher_done,
		ton_withdrawal_watcher_done,
		ton_sweep_done,
		treasury_resolve_done,
		consilium_sweeper_done,
		consilium_mailer_done,
		settings_drift_done,
	) = tokio::join!(
		await_signal(shutdown.clone()),
		branch(&shutdown, "core gRPC server", services::serve(config.grpc_addr, state, shutdown.clone().cancelled_owned())),
		branch(&shutdown, "scope ledger gRPC server", async {
			match scope_ledger {
				Some(listener) => services::scope_ledger::serve(listener, &scope_ledger_authorizer, shutdown.clone().cancelled_owned()).await,
				None => {
					shutdown.cancelled().await;
					Ok(())
				}
			}
		}),
		branch(&shutdown, "auth service", auth_service.run(config.auth_grpc_addr, shutdown.clone().cancelled_owned())),
		branch(&shutdown, "provisioner", infallible(auth_sync::run_provisioner(provision_rx, users))),
		branch(&shutdown, "relay", infallible(relay.run(shutdown.clone()))),
		branch(&shutdown, "reconciliation", infallible(reconciliation.run(shutdown.clone()))),
		branch(&shutdown, "treasury drift watch", infallible(treasury_drift.run(shutdown.clone()))),
		branch(&shutdown, "reaper", infallible(reaper.run(shutdown.clone()))),
		branch(&shutdown, "dispatcher", infallible(dispatcher.run(shutdown.clone()))),
		branch(&shutdown, "fee sweeper", infallible(fee_sweeper.run(shutdown.clone()))),
		branch(
			&shutdown,
			"bridge",
			infallible(run_or_idle(shutdown.clone(), bridge.map(|consumer| consumer.run(shutdown.clone()))))
		),
		branch(&shutdown, "scope events", infallible(scope_events.run(shutdown.clone()))),
		branch(
			&shutdown,
			"approval executor",
			infallible(run_or_idle(shutdown.clone(), approval_executor.map(|executor| executor.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"deposit watcher",
			infallible(run_or_idle(shutdown.clone(), deposit_watcher.map(|watcher| watcher.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"withdrawal watcher",
			infallible(run_or_idle(shutdown.clone(), withdrawal_watcher.map(|watcher| watcher.run(shutdown.clone()))))
		),
		branch(&shutdown, "sweep", infallible(run_or_idle(shutdown.clone(), sweep.map(|sweep| sweep.run(shutdown.clone()))))),
		branch(
			&shutdown,
			"polygon deposit watcher",
			infallible(run_or_idle(shutdown.clone(), polygon_deposit_watcher.map(|watcher| watcher.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"polygon withdrawal watcher",
			infallible(run_or_idle(shutdown.clone(), polygon_withdrawal_watcher.map(|watcher| watcher.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"polygon sweep",
			infallible(run_or_idle(shutdown.clone(), polygon_sweep.map(|sweep| sweep.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"tron deposit watcher",
			infallible(run_or_idle(shutdown.clone(), tron_deposit_watcher.map(|watcher| watcher.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"tron withdrawal watcher",
			infallible(run_or_idle(shutdown.clone(), tron_withdrawal_watcher.map(|watcher| watcher.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"tron sweep",
			infallible(run_or_idle(shutdown.clone(), tron_sweep.map(|sweep| sweep.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"ton deposit watcher",
			infallible(run_or_idle(shutdown.clone(), ton_deposit_watcher.map(|watcher| watcher.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"ton withdrawal watcher",
			infallible(run_or_idle(shutdown.clone(), ton_withdrawal_watcher.map(|watcher| watcher.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"ton sweep",
			infallible(run_or_idle(shutdown.clone(), ton_sweep.map(|sweep| sweep.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"treasury resolve",
			infallible(resolve_treasuries(bsc_custody, polygon_custody, tron_custody, ton_custody, shutdown.clone()))
		),
		branch(&shutdown, "consilium sweeper", infallible(consilium_sweeper.run(shutdown.clone()))),
		branch(
			&shutdown,
			"consilium mailer",
			infallible(run_or_idle(shutdown.clone(), consilium_mailer.map(|mailer| mailer.run(shutdown.clone()))))
		),
		branch(
			&shutdown,
			"settings drift watch",
			infallible(async {
				tokio::select! {
					never = config::AppConfig::watch_drift() => match never {},
					() = shutdown.cancelled() => {}
				}
			})
		),
	);
	let () = signal;
	// The first error (if any) becomes the process result; a clean shutdown is `Ok`.
	core.and(scope_ledger_done)
		.and(auth)
		.and(provisioner)
		.and(relay_done)
		.and(reconciliation_done)
		.and(treasury_drift_done)
		.and(reaper_done)
		.and(dispatcher_done)
		.and(fee_sweeper_done)
		.and(bridge_done)
		.and(scope_events_done)
		.and(approval_executor_done)
		.and(watcher_done)
		.and(withdrawal_watcher_done)
		.and(sweep_done)
		.and(polygon_watcher_done)
		.and(polygon_withdrawal_watcher_done)
		.and(polygon_sweep_done)
		.and(tron_watcher_done)
		.and(tron_withdrawal_watcher_done)
		.and(tron_sweep_done)
		.and(ton_watcher_done)
		.and(ton_withdrawal_watcher_done)
		.and(ton_sweep_done)
		.and(treasury_resolve_done)
		.and(consilium_sweeper_done)
		.and(consilium_mailer_done)
		.and(settings_drift_done)
}

/// Build the governance-mail adapter over the concierge endpoint, or `None` when the seam
/// is not compiled in.
///
/// It rides the SAME endpoint and the SAME shared secret as the one-way lifecycle bridge:
/// banking↔concierge is one trust relationship, and giving the mail relay a second one would
/// mean a second thing to rotate and a second thing to get wrong. The endpoint is the one
/// already built (and boot-checked) for the bridge, not a second build of it: a second
/// build was how an `https://` bridge could have left the mail seam on a channel that
/// never negotiated TLS — and its failure was swallowed into "no mailer", which reads as a
/// build without the feature.
#[allow(unused_variables, reason = "both parameters are only read when the seam is compiled in")]
fn governance_mail_adapter(concierge: &Endpoint, config: &config::AppConfig) -> Option<Arc<dyn piggybank_core::ports::GovernanceMailer>> {
	#[cfg(feature = "concierge_governance_mail")]
	{
		Some(Arc::new(governance_mail::wired::ConciergeGovernanceMailer::new(
			concierge.connect_lazy(),
			config.bridge_service_token.clone(),
		)))
	}
	#[cfg(not(feature = "concierge_governance_mail"))]
	None
}

/// Resolve + log each configured rail's treasury hot wallet so the operator can fund it
/// (USDT for liquidity, native coin for gas) before any withdrawal settles. Runs as its own
/// `join!` branch because the signer verifies the hub's service token against the auth
/// listener — which only binds inside the `join!` — so an inline boot-time resolve always
/// failed `Unavailable` and the funding log line never printed. Retries with capped
/// exponential backoff; each adapter's `OnceCell` makes success sticky (repeat calls are
/// free), and once every configured rail is resolved the branch idles until shutdown.
async fn resolve_treasuries(bsc: Option<Arc<ChainCustody>>, polygon: Option<Arc<ChainCustody>>, tron: Option<Arc<TronCustody>>, ton: Option<Arc<TonCustody>>, shutdown: CancellationToken) {
	const MAX_BACKOFF: Duration = Duration::from_secs(30);
	let mut backoff = Duration::from_millis(500);
	loop {
		let mut unresolved = false;
		if let Some(bsc) = &bsc
			&& let Err(err) = bsc.treasury_address().await
		{
			tracing::warn!("could not resolve the BEP20 treasury address (retrying in {backoff:?}): {err}");
			unresolved = true;
		}
		if let Some(polygon) = &polygon
			&& let Err(err) = polygon.treasury_address().await
		{
			tracing::warn!("could not resolve the Polygon treasury address (retrying in {backoff:?}): {err}");
			unresolved = true;
		}
		if let Some(tron) = &tron
			&& let Err(err) = tron.treasury_address().await
		{
			tracing::warn!("could not resolve the Tron treasury address (retrying in {backoff:?}): {err}");
			unresolved = true;
		}
		if let Some(ton) = &ton
			&& let Err(err) = ton.treasury_address().await
		{
			tracing::warn!("could not resolve the TON treasury address (retrying in {backoff:?}): {err}");
			unresolved = true;
		}
		if !unresolved {
			shutdown.cancelled().await;
			return;
		}
		tokio::select! {
			() = shutdown.cancelled() => return,
			() = tokio::time::sleep(backoff) => {},
		}
		backoff = (backoff * 2).min(MAX_BACKOFF);
	}
}

/// Run one composition-root branch to completion, mapping any error to a `color_eyre` report,
/// then cancel the shared token so its peers start their graceful wind-down. It never returns
/// early on a peer's cancellation — the branch's own future owns its draining (the gRPC servers
/// via `serve_with_shutdown`, the loops via the token) — so `join!` over all branches drains them.
async fn branch<E: std::fmt::Display>(shutdown: &CancellationToken, name: &str, fut: impl Future<Output = Result<(), E>>) -> color_eyre::Result<()> {
	let result = fut.await.map_err(|err| eyre!("{name} error: {err}"));
	if let Err(err) = &result {
		tracing::error!("{err}");
	}
	shutdown.cancel();
	result
}

/// Adapt an infallible loop task (`-> ()`) to the `Result` shape [`branch`] expects.
async fn infallible(fut: impl Future<Output = ()>) -> Result<(), std::convert::Infallible> {
	fut.await;
	Ok(())
}

/// Run an optional rail task if it is configured, else idle until shutdown — so its `join!`
/// branch exists unconditionally (an unconfigured watcher/sweep/bridge is a no-op, not a
/// missing branch).
async fn run_or_idle(shutdown: CancellationToken, task: Option<impl Future<Output = ()>>) {
	match task {
		Some(task) => task.await,
		None => shutdown.cancelled().await,
	}
}

/// What the binary was asked to do. Hand-rolled: one flag is not worth an
/// argument-parser dependency.
enum Command {
	/// No arguments: serve.
	Serve,
	/// `--print-required-vars[=PROFILE]` (default `production`).
	PrintRequiredVars(String),
}

impl Command {
	fn from_args(mut args: impl Iterator<Item = String>) -> color_eyre::Result<Self> {
		const FLAG: &str = "--print-required-vars";
		// The one-off ownership data migration (#245) ran in production on 2026-09-27 and
		// was removed with the contract step (C-9). Refused by name rather than falling
		// through to `serve`: an operator repeating the runbook must not start a second
		// server inside the pod.
		const REMOVED: &str = "migrate-ownership";

		let Some(arg) = args.next() else {
			return Ok(Self::Serve);
		};
		match arg.split_once('=') {
			Some((FLAG, profile)) => return Ok(Self::PrintRequiredVars(profile.to_string())),
			Some(_) => return Ok(Self::Serve),
			None => {}
		}
		if arg == FLAG {
			return Ok(Self::PrintRequiredVars(args.next().unwrap_or_else(|| "production".to_string())));
		}
		ensure!(
			arg != REMOVED,
			"`{REMOVED}` was a one-off data migration; it ran on 2026-09-27 and has been removed (see docs/RUNBOOK-ownership-migration.md)"
		);
		Ok(Self::Serve)
	}
}

/// Resolve on the first of `SIGINT` (ctrl_c), `SIGTERM`, or a peer cancelling `shutdown`,
/// then cancel the token. Racing the token keeps this a structured `join!` branch — it never
/// outlives the others. On non-Unix only ctrl_c exists; a failed handler registration is
/// logged and this branch idles on the token (the process can still be force-killed) rather
/// than tearing down at boot.
async fn await_signal(shutdown: CancellationToken) {
	#[cfg(unix)]
	{
		use tokio::signal::unix::{SignalKind, signal};
		match signal(SignalKind::terminate()) {
			Ok(mut term) => {
				tokio::select! {
					biased;
					() = shutdown.cancelled() => return,
					result = tokio::signal::ctrl_c() => {
						if let Err(err) = result {
							tracing::error!("failed to listen for ctrl_c: {err}");
							shutdown.cancelled().await;
							return;
						}
					},
					_ = term.recv() => {},
				}
			}
			Err(err) => {
				tracing::error!("failed to install SIGTERM handler: {err}");
				shutdown.cancelled().await;
				return;
			}
		}
	}
	#[cfg(not(unix))]
	tokio::select! {
		biased;
		() = shutdown.cancelled() => return,
		result = tokio::signal::ctrl_c() => {
			if let Err(err) = result {
				tracing::error!("failed to listen for ctrl_c: {err}");
				shutdown.cancelled().await;
				return;
			}
		},
	}
	tracing::info!("shutdown signal received — draining");
	shutdown.cancel();
}

/// TLS for the hub's client side of an off-host signer seam. Trust anchors come from a
/// pinned CA (`SIGNER_TLS_CA_PEM_FILE`) when set — the private-CA / mTLS case the
/// architecture targets — else the public webpki roots. A client identity
/// (`SIGNER_TLS_CLIENT_CERT_PEM_FILE` + `…KEY…`) presents the hub's certificate for mTLS.
fn signer_client_tls() -> color_eyre::Result<ClientTlsConfig> {
	use std::env;

	let mut tls = match env::var("SIGNER_TLS_CA_PEM_FILE").ok().filter(|s| !s.is_empty()) {
		Some(ca_file) => {
			let ca = std::fs::read_to_string(&ca_file).with_context(|| format!("failed to read SIGNER_TLS_CA_PEM_FILE at {ca_file}"))?;
			ClientTlsConfig::new().ca_certificate(Certificate::from_pem(ca))
		}
		None => ClientTlsConfig::new().with_enabled_roots(),
	};
	if let (Some(cert_file), Some(key_file)) = (
		env::var("SIGNER_TLS_CLIENT_CERT_PEM_FILE").ok().filter(|s| !s.is_empty()),
		env::var("SIGNER_TLS_CLIENT_KEY_PEM_FILE").ok().filter(|s| !s.is_empty()),
	) {
		let cert = std::fs::read_to_string(&cert_file).with_context(|| format!("failed to read SIGNER_TLS_CLIENT_CERT_PEM_FILE at {cert_file}"))?;
		let key = std::fs::read_to_string(&key_file).with_context(|| format!("failed to read SIGNER_TLS_CLIENT_KEY_PEM_FILE at {key_file}"))?;
		tls = tls.identity(Identity::from_pem(cert, key));
	}
	Ok(tls)
}

// Returns the OTel guard (flushes/shuts down on drop); bind it in `main`. `None`
// when OTEL_EXPORTER_OTLP_ENDPOINT is unset — the layers are then inert.
fn init_tracing(environment: &str) -> Option<ev::otel::Telemetry> {
	use tracing_subscriber::{EnvFilter, fmt, prelude::*};

	let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,piggybank_core=debug,evbanking_auth=debug"));
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

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(args: &[&str]) -> color_eyre::Result<Command> {
		Command::from_args(args.iter().map(|a| (*a).to_string()))
	}

	#[test]
	fn the_command_line_is_serve_or_the_preflight_flag() {
		assert!(matches!(parse(&[]).unwrap(), Command::Serve));
		assert!(matches!(parse(&["--print-required-vars"]).unwrap(), Command::PrintRequiredVars(p) if p == "production"));
		assert!(matches!(parse(&["--print-required-vars=dev"]).unwrap(), Command::PrintRequiredVars(p) if p == "dev"));
		assert!(matches!(parse(&["--print-required-vars", "dev"]).unwrap(), Command::PrintRequiredVars(p) if p == "dev"));
		// The removed one-off command is refused, never served.
		assert!(parse(&["migrate-ownership", "--holders", "h.json"]).is_err());
	}
}
