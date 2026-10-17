use std::{env, net::SocketAddr};

use domain::{money::Network, users::KYC_LEVEL_VERIFIED};

ev::settings! {
	/// Application configuration, read from the environment only (`dotenvy`
	/// loads `.env` first in dev; prod values ride the container env — the
	/// image's baked topology contract plus gitops' k8s Secret `envFrom`).
	/// Every missing/invalid variable is reported in one aggregate error before
	/// anything binds, and an empty string counts as unset.
	///
	/// The on-chain rails (BSC/Polygon/TRON/TON + sweeps) live in [`Rails`], on their
	/// original env-based conditional construction; production boot-asserts the
	/// rail set in `main.rs` (at least one rail, TON mainnet keyed).
	pub struct AppConfig {
		database_url: String,
		/// Core tonic gRPC listener address (the hub's data-plane services). Loopback by
		/// default: an internal seam reached by the BFF (and same-host services). Opt into
		/// a wider bind (e.g. 0.0.0.0:50051) only behind network segmentation
		/// (see `docs/ARCHITECTURE.md`).
		grpc_addr: SocketAddr = "127.0.0.1:50051",
		/// Auth service gRPC listener address (token issuance routes for clients).
		auth_grpc_addr: SocketAddr = "127.0.0.1:50052",
		/// `None` ⇒ error monitoring is a silent no-op. Right for local and CI;
		/// on the money plane an unreported error is the one thing nobody can
		/// afford to miss, so in production the boot fails instead.
		#[required_in("production")]
		sentry_dsn: Option<String>,
		/// PostHog project key for native product-analytics capture. `None` disables
		/// capture (a silent no-op), so the same code runs unconfigured (local, CI).
		///
		/// Deliberately NOT `required_in("production")`, unlike `sentry_dsn`: a
		/// missing error reporter hides failures, while missing product analytics
		/// only forgoes a metric — a bad trade anywhere, and worst on the money
		/// plane. That must never be the reason a service refuses to boot.
		posthog_key: Option<String>,
		/// PostHog ingestion host; `None` falls back to the library default
		/// (`https://us.i.posthog.com`).
		posthog_host: Option<String>,
		app_env: String,
		/// TigerBeetle replica address (e.g. `"127.0.0.1:3033"` or a bare `"3033"`).
		tigerbeetle_address: String,
		/// TigerBeetle cluster id (a u128, so it rides as a string — the wire has no
		/// u128 lane).
		tigerbeetle_cluster_id: String,
		/// Endpoint of the separate-process signer (the key vault), for deposit-address
		/// provisioning over the `signer.v1` gRPC seam. The hub connects lazily, so this
		/// only needs to resolve by the time the first address is provisioned.
		signer_grpc_addr: String,
		/// Max connections for the request-serving Postgres pool (the core gRPC handlers).
		db_max_connections: u32 = "10",
		/// Max connections for the outbox relay's own dedicated Postgres pool, so request
		/// traffic and money dispatch can't exhaust each other. A small pool suffices since
		/// the relay is a single-worker drainer (one drain connection + the lock-holding
		/// connection).
		relay_db_max_connections: u32 = "3",
		/// The concierge plane's gRPC endpoint serving `UserEvents.PullUserLifecycle` —
		/// the cross-plane lifecycle bridge the consumer pulls from. In production it must
		/// be `https://` with the CA pinned by `BRIDGE_TLS_CA_PEM_FILE`, or loopback; see
		/// [`ensure_bridge_is_authenticated`].
		concierge_bridge_addr: String,
		/// The shared bridge service token (`authorization: Bearer …`), the same value
		/// concierge verifies the pull against.
		#[secret]
		bridge_service_token: String,
		/// Seconds between bridge pulls when the backlog is drained.
		bridge_poll_secs: u64 = "5",
		/// Base URL of the consilium approval page the emailed link points at
		/// (`<base>/<token>`). The page is served by the cabinet; the hub only mints the
		/// link. A wrong value here sends owners somewhere that cannot take their vote, so
		/// it is worth checking per environment.
		consilium_approval_url_base: String = "https://evinvest.ltd/cabinet/approve",
		/// Base URL of the payment CONSENT page the emailed link points at (`<base>/<token>`)
		/// — the investor-facing twin of the approval base, served by the cabinet on a
		/// public route for the same reason: the person clicking holds a token, not a
		/// session.
		payment_consent_url_base: String = "https://evinvest.ltd/cabinet/consent",
	}
}

/// The identity-verification gate on money movement, as a switch.
///
/// Enforced, it is the tier floor every money-crossing path shares: no deposit address is
/// provisioned, no withdrawal is admitted, and no queued withdrawal is dispatched below
/// [`KYC_LEVEL_VERIFIED`]. Lifted, it is exactly the pre-`v0.4.0` behaviour — address
/// issued, withdrawal accepted and paid out. There is no third position, and the switch is
/// read once at boot so a rail cannot change its mind mid-flight.
///
/// Deliberately NOT part of the boot-asserted [`AppConfig`], for the same reason [`Rails`]
/// is not: production does not set `KYC_GATE_ENABLED` today and must not have to start.
///
/// | `KYC_GATE_ENABLED` | gate |
/// | --- | --- |
/// | unset / empty / `true` / `1` | enforced |
/// | `false` / `0` | lifted |
/// | anything else | enforced, with a WARN naming the value |
///
/// Every uncertain reading resolves to *enforced*: this switch decides whether unverified
/// money may cross the platform boundary, so it opens only on an explicit, unambiguous
/// word and never on a typo. That also rules out [`bool_env`], which reads everything it
/// does not recognise as `false` — the safe default for an opt-in sweep, the wrong one
/// here. An unparseable value warns rather than refusing the boot on purpose: the reading
/// that cannot lose money is already available without stopping the hub.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KycGate {
	enforced: bool,
}

impl KycGate {
	/// Money movement requires a verified tier — the default, and what production runs.
	pub const ENFORCED: Self = Self { enforced: true };
	/// The gate is off: every tier deposits and withdraws.
	pub const LIFTED: Self = Self { enforced: false };

	pub fn from_env() -> Self {
		Self::read(env::var("KYC_GATE_ENABLED").ok().as_deref())
	}

	/// The parse, taken apart from the env read so the table above is a test rather than
	/// a promise.
	fn read(raw: Option<&str>) -> Self {
		match raw.map(str::trim) {
			None | Some("") => Self::ENFORCED,
			Some(value) if value.eq_ignore_ascii_case("false") || value == "0" => Self::LIFTED,
			Some(value) if value.eq_ignore_ascii_case("true") || value == "1" => Self::ENFORCED,
			Some(value) => {
				tracing::warn!("KYC_GATE_ENABLED={value:?} is not a boolean — the KYC gate stays enforced (only an explicit `false`/`0` lifts it)");
				Self::ENFORCED
			}
		}
	}

	/// Whether a caller has to clear the verification floor to move money.
	pub fn is_enforced(self) -> bool {
		self.enforced
	}

	/// Whether `kyc_level` may move money. The one place the floor is compared, so the
	/// three gates (address, admission, dispatch) cannot drift apart.
	pub fn admits(self, kyc_level: u32) -> bool {
		!self.enforced || kyc_level >= KYC_LEVEL_VERIFIED
	}
}

/// Which way a NEW payment order is decided: over concierge's approvals, or by the old
/// banking-run consilium and emailed consent.
///
/// `Consilium` is the old path, kept for the rollback and, for fund money, as the default
/// until the console can prove who asked (see [`PaymentApprovalModes`]). It changes how orders
/// OPENED from then on are seated and nothing else. An order already seated keeps the seat it
/// was opened with — the sweeper, the old consilium and consent surfaces and the concierge
/// executor all stay running in every mode — so flipping a switch strands nothing in flight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentApprovalMode {
	Concierge,
	Consilium,
}

impl PaymentApprovalMode {
	/// One switch's reading. Unset or empty is `default`; an unrecognised word is `default`
	/// too, with a WARN naming the variable — a typo never moves a payment path by itself.
	fn parse(variable: &str, raw: Option<&str>, default: Self) -> Self {
		match raw.map(str::trim) {
			None | Some("") => default,
			Some(value) if value.eq_ignore_ascii_case("concierge") => Self::Concierge,
			Some(value) if value.eq_ignore_ascii_case("consilium") => Self::Consilium,
			Some(value) => {
				tracing::warn!("{variable}={value:?} is neither `concierge` nor `consilium` — the default ({default:?}) applies");
				default
			}
		}
	}
}

/// The two switches, one per kind of source, read once at boot.
///
/// | variable | decides | default |
/// | --- | --- | --- |
/// | `PAYMENT_APPROVALS_FUND` | a payment out of fund money: `concierge` = a `bank:payment` approval of the platform owners, proposed in the initiator's name (the proof of who asked); `consilium` = banking's consilium | `consilium` |
/// | `PAYMENT_APPROVALS_SESSION` | a payment out of an investor's claim: `concierge` = the investor's own answer in the cabinet; `consilium` = the emailed token | `concierge` |
/// | `PAYMENT_APPROVALS` | the old single name: when set, the default of BOTH above; either specific variable overrides it | — |
///
/// Fund money defaults to the old path because the `concierge` path needs the initiator's
/// access token minted within the last 120 seconds, and the console cannot get one on demand
/// (the session renews its token only near expiry): opened under `concierge`, such a payment
/// would mostly be refused. An investor's payment needs no token of the initiator's — the
/// investor answers signed in — so it moves to the new path at once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaymentApprovalModes {
	pub fund: PaymentApprovalMode,
	pub session: PaymentApprovalMode,
}

impl PaymentApprovalModes {
	pub const ALL_CONCIERGE: Self = Self {
		fund: PaymentApprovalMode::Concierge,
		session: PaymentApprovalMode::Concierge,
	};
	pub const ALL_CONSILIUM: Self = Self {
		fund: PaymentApprovalMode::Consilium,
		session: PaymentApprovalMode::Consilium,
	};
	pub const DEFAULT: Self = Self {
		fund: PaymentApprovalMode::Consilium,
		session: PaymentApprovalMode::Concierge,
	};

	pub fn from_env() -> Self {
		let var = |name: &str| env::var(name).ok();
		Self::read(
			var("PAYMENT_APPROVALS").as_deref(),
			var("PAYMENT_APPROVALS_FUND").as_deref(),
			var("PAYMENT_APPROVALS_SESSION").as_deref(),
		)
	}

	fn read(legacy: Option<&str>, fund: Option<&str>, session: Option<&str>) -> Self {
		// A legacy value that is set (and valid) is the default of both sides; the specific
		// variables then override it.
		let legacy_set = legacy.is_some_and(|raw| !raw.trim().is_empty());
		let (fund_default, session_default) = if legacy_set {
			let both = PaymentApprovalMode::parse("PAYMENT_APPROVALS", legacy, PaymentApprovalMode::Concierge);
			let invalid = !matches!(legacy.map(|raw| raw.trim().to_ascii_lowercase()).as_deref(), Some("concierge" | "consilium"));
			if invalid { (Self::DEFAULT.fund, Self::DEFAULT.session) } else { (both, both) }
		} else {
			(Self::DEFAULT.fund, Self::DEFAULT.session)
		};
		Self {
			fund: PaymentApprovalMode::parse("PAYMENT_APPROVALS_FUND", fund, fund_default),
			session: PaymentApprovalMode::parse("PAYMENT_APPROVALS_SESSION", session, session_default),
		}
	}

	/// The mode that seats an order with this requirement.
	pub fn of(self, requirement: domain::payments::PaymentApproval) -> PaymentApprovalMode {
		match requirement {
			domain::payments::PaymentApproval::OwnerConsilium => self.fund,
			domain::payments::PaymentApproval::SubjectConsent(_) => self.session,
		}
	}
}

/// The on-chain rail configs, kept on their original env-based conditional
/// construction (a rail runs only when its endpoint var is set), so they are
/// NOT part of the boot-asserted [`AppConfig`] — production instead asserts
/// the rail set in `main.rs` (a rail-less prod boot is refused).
#[derive(Clone, Debug)]
pub struct Rails {
	/// The on-chain BSC config. `None` (no `BSC_RPC_URL`) leaves every on-chain seam — the
	/// deposit watcher, the withdrawal confirmation watcher, and real custody — un-run. See
	/// [`infrastructure::deposit_watcher`](crate::infrastructure::deposit_watcher) and
	/// [`infrastructure::withdrawal_watcher`](crate::infrastructure::withdrawal_watcher).
	pub bsc: Option<EvmConfig>,
	/// The treasury sweep's config. `Some` only when BSC is configured AND `SWEEP_ENABLED`
	/// is set — it moves user deposit balances on-chain into the treasury, so it is opt-in
	/// (merely configuring deposits/withdrawals does not start it). See
	/// [`infrastructure::sweep`](crate::infrastructure::sweep).
	pub sweep: Option<SweepConfig>,
	/// The on-chain Polygon (PoS) config — the second EVM rail. `None` (no `POLYGON_RPC_URL`) leaves
	/// every Polygon seam un-run, the same no-op-when-unconfigured stance as BSC. Shares [`EvmConfig`]
	/// and every EVM seam with BSC, distinguished only by `EvmConfig::network` (`Network::Polygon`).
	pub polygon: Option<EvmConfig>,
	/// The Polygon treasury sweep config. `Some` only when Polygon is configured AND its sweep is
	/// enabled (`SWEEP_ENABLED` or `POLYGON_SWEEP_ENABLED`) — the same opt-in gate as the BSC sweep.
	pub polygon_sweep: Option<SweepConfig>,
	/// The on-chain Tron (TRC20) config. `None` (no `TRON_RPC_URL`) leaves every Tron seam — the
	/// deposit watcher, the withdrawal confirmation watcher, and real custody — un-run, the same
	/// no-op-when-unconfigured stance as BSC. See [`infrastructure::tron_rpc`](crate::infrastructure::tron_rpc).
	pub tron: Option<TronConfig>,
	/// The Tron treasury sweep's config. `Some` only when Tron is configured AND `SWEEP_ENABLED`
	/// is set — the same opt-in gate as the BEP20 [`sweep`](Self::sweep).
	pub tron_sweep: Option<TronSweepConfig>,
	/// The on-chain TON config. `None` (no `TON_API_URL`) leaves every TON seam — the
	/// jetton deposit watcher, the withdrawal confirmation watcher, and real custody —
	/// un-run, the same no-op-when-unconfigured stance as BSC. Point `TON_API_URL` at
	/// `https://testnet.toncenter.com/api/v3` (`TON_IS_TESTNET=true`) for bring-up.
	pub ton: Option<TonConfig>,
	/// The TON treasury sweep's config. `Some` only when TON is configured AND
	/// `SWEEP_ENABLED` is set (the same opt-in gate as the BSC sweep).
	pub ton_sweep: Option<TonSweepConfig>,
}
impl Rails {
	pub fn from_env() -> color_eyre::Result<Self> {
		// The on-chain seams run only when BSC_RPC_URL is set (the endpoint must support
		// eth_getLogs for deposit scanning). Everything else has a sensible default —
		// mainnet USDT, 15 confs.
		let bsc = match env::var("BSC_RPC_URL").ok().filter(|s| !s.is_empty()) {
			Some(rpc_url) => Some(EvmConfig {
				network: Network::Bep20,
				rpc_url,
				usdt_contract: env::var("BSC_USDT_CONTRACT")
					.ok()
					.filter(|s| !s.is_empty())
					.unwrap_or_else(|| "0x55d398326f99059fF775485246999027B3197955".to_string()),
				confirmations: parse_opt("BSC_CONFIRMATIONS")?.unwrap_or(Network::Bep20.min_confirmations() as u64),
				poll_secs: parse_opt("BSC_POLL_SECS")?.unwrap_or(12),
				start_block: parse_opt("BSC_DEPOSIT_START_BLOCK")?,
				max_block_range: parse_opt("BSC_MAX_BLOCK_RANGE")?.unwrap_or(500),
				logs_rpc_url: env::var("BSC_LOGS_RPC_URL").ok().filter(|s| !s.is_empty()),
				chain_id: parse_opt("BSC_CHAIN_ID")?.unwrap_or(56),
				gas_limit: parse_opt("BSC_GAS_LIMIT")?.unwrap_or(100_000),
				max_gas_price_gwei: parse_opt("BSC_MAX_GAS_PRICE_GWEI")?.unwrap_or(100),
			}),
			None => None,
		};
		// Polygon PoS — the SECOND EVM rail, sharing every EVM seam with BSC (deposit watcher,
		// withdrawal watcher, custody, sweep) via `EvmConfig` + `Network::Polygon`. Runs only when
		// POLYGON_RPC_URL is set (an `eth_getLogs`-capable endpoint), the same no-op-when-unconfigured
		// stance as BSC. Defaults: mainnet native USDT (6-dp, unlike BSC 18-dp), chain-id 137, 128
		// confirmations (Polygon probabilistic-finality reorgs run deeper than BSC).
		let polygon = match env::var("POLYGON_RPC_URL").ok().filter(|s| !s.is_empty()) {
			Some(rpc_url) => Some(EvmConfig {
				network: Network::Polygon,
				rpc_url,
				usdt_contract: env::var("POLYGON_USDT_CONTRACT")
					.ok()
					.filter(|s| !s.is_empty())
					.unwrap_or_else(|| "0xc2132D05D31c914a87C6611C10748AEb04B58e8F".to_string()),
				confirmations: parse_opt("POLYGON_CONFIRMATIONS")?.unwrap_or(Network::Polygon.min_confirmations() as u64),
				poll_secs: parse_opt("POLYGON_POLL_SECS")?.unwrap_or(6),
				start_block: parse_opt("POLYGON_DEPOSIT_START_BLOCK")?,
				max_block_range: parse_opt("POLYGON_MAX_BLOCK_RANGE")?.unwrap_or(500),
				logs_rpc_url: env::var("POLYGON_LOGS_RPC_URL").ok().filter(|s| !s.is_empty()),
				chain_id: parse_opt("POLYGON_CHAIN_ID")?.unwrap_or(137),
				gas_limit: parse_opt("POLYGON_GAS_LIMIT")?.unwrap_or(100_000),
				max_gas_price_gwei: parse_opt("POLYGON_MAX_GAS_PRICE_GWEI")?.unwrap_or(5_000),
			}),
			None => None,
		};
		// TRC20 is FROZEN pending an energy-staking gas model — see EV-invest/banking#31. Burning
		// TRX per transfer (~15-30 TRX ≈ $2-5) loses money against the flat withdrawal fee, so the
		// whole rail is off: forcing `tron = None` here (regardless of TRON_RPC_URL) disables the
		// deposit/withdrawal watchers, the sweep, custody, the wallet's TRC20 deposit/withdraw UI,
		// and `configured_networks()` in one place. Re-enable = drop this `TRC20_FROZEN` gate once
		// #31 lands. The TRON seams below stay compiled (a byte-for-byte mirror of BEP20/TON), just
		// never constructed.
		const TRC20_FROZEN: bool = true;
		// Same no-op-when-unconfigured stance as BSC, gated on TRON_RPC_URL (TronGrid REST serves
		// both `/wallet/*` and the indexed `/v1/accounts/*`) — but short-circuited while frozen.
		let tron = match env::var("TRON_RPC_URL").ok().filter(|s| !s.is_empty()) {
			Some(rpc_url) if !TRC20_FROZEN => Some(TronConfig {
				rpc_url,
				usdt_contract: env::var("TRON_USDT_CONTRACT")
					.ok()
					.filter(|s| !s.is_empty())
					.unwrap_or_else(|| "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".to_string()),
				api_key: env::var("TRON_API_KEY").ok().filter(|s| !s.is_empty()),
				poll_secs: parse_opt("TRON_POLL_SECS")?.unwrap_or(12),
				start_timestamp: parse_opt("TRON_DEPOSIT_START_TS")?,
				fee_limit: parse_opt("TRON_FEE_LIMIT")?.unwrap_or(100_000_000),
				expiration_secs: parse_opt("TRON_EXPIRATION_SECS")?.unwrap_or(60),
				max_transfers_per_scan: parse_opt("TRON_MAX_TRANSFERS")?.unwrap_or(200),
			}),
			_ => None, // unconfigured OR frozen (TRC20_FROZEN) ⇒ rail off
		};
		// The on-chain TON seams run only when TON_API_URL is set (a toncenter v3 base URL).
		// Everything else defaults — mainnet USDT jetton master, 6s poll.
		let ton = match env::var("TON_API_URL").ok().filter(|s| !s.is_empty()) {
			Some(api_url) => {
				// Default the testnet flag from the URL (the testnet toncenter lives on a `testnet.`
				// host), so it moves together with the endpoint. Fail LOUD on EITHER dangerous
				// mismatch against the standard toncenter hosts, because the flag drives the
				// user-facing deposit-address tag and a wrong tag points the user at the wrong
				// network (a stranded, uncredited deposit):
				//   - a testnet URL with the flag off ⇒ testnet rail minting MAINNET-tagged addresses;
				//   - the mainnet host with the flag on ⇒ mainnet rail minting TESTNET-tagged addresses.
				// A CUSTOM endpoint (neither the mainnet nor the testnet toncenter host) is trusted to
				// the explicit TON_IS_TESTNET — that is the custom-testnet-proxy escape hatch.
				let url_is_testnet = api_url.contains("testnet");
				let url_is_mainnet_host = api_url.contains("toncenter.com") && !url_is_testnet;
				let is_testnet = env::var("TON_IS_TESTNET")
					.ok()
					.filter(|s| !s.is_empty())
					.map(|v| v == "true" || v == "1")
					.unwrap_or(url_is_testnet);
				if url_is_testnet && !is_testnet {
					color_eyre::eyre::bail!(
						"TON_API_URL ({api_url}) is a testnet endpoint but TON_IS_TESTNET is not true — user-facing TON deposit addresses would carry the mainnet tag for a testnet rail; set TON_IS_TESTNET=true"
					);
				}
				if url_is_mainnet_host && is_testnet {
					color_eyre::eyre::bail!(
						"TON_API_URL ({api_url}) is the mainnet toncenter host but TON_IS_TESTNET is true — user-facing TON deposit addresses would carry the testnet tag for a mainnet rail; unset TON_IS_TESTNET or point TON_API_URL at a testnet/custom endpoint"
					);
				}
				Some(TonConfig {
					api_url,
					api_key: env::var("TON_API_KEY").ok().filter(|s| !s.is_empty()),
					usdt_master: env::var("TON_USDT_MASTER")
						.ok()
						.filter(|s| !s.is_empty())
						.unwrap_or_else(|| "EQCxE6mUtQJKFnGfaROTKOt1lZbDiiX1kCixRv7Nw2Id_sDs".to_string()),
					poll_secs: parse_opt("TON_POLL_SECS")?.unwrap_or(6),
					start_cursor: parse_opt("TON_DEPOSIT_START_UTIME")?,
					is_testnet,
					wallet_version: env::var("TON_WALLET_VERSION").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "v4r2".to_string()),
					forward_ton_amount: parse_opt("TON_FORWARD_TON_AMOUNT")?.unwrap_or(50_000_000),
					msg_value: parse_opt("TON_MSG_VALUE")?.unwrap_or(100_000_000),
				})
			}
			None => None,
		};
		// The sweep moves user funds on-chain, so it runs only when explicitly enabled AND a chain
		// is configured — opt-in, never implied by the deposit/withdraw seams. `SWEEP_ENABLED` is
		// the global default; a per-rail `<RAIL>_SWEEP_ENABLED` overrides it so a freshly-configured
		// rail can run deposits+withdrawals WITHOUT arming its fund-moving sweep before its treasury
		// and gas station are funded — the sequenced bring-up BEP20 used. An unset per-rail var
		// inherits the global; an unconfigured rail simply has no sweep.
		let sweep_enabled = bool_env("SWEEP_ENABLED", false);
		let bsc_sweep_enabled = bool_env("BSC_SWEEP_ENABLED", sweep_enabled);
		let tron_sweep_enabled = bool_env("TRON_SWEEP_ENABLED", sweep_enabled);
		let ton_sweep_enabled = bool_env("TON_SWEEP_ENABLED", sweep_enabled);
		let polygon_sweep_enabled = bool_env("POLYGON_SWEEP_ENABLED", sweep_enabled);
		let sweep = if bsc_sweep_enabled && bsc.is_some() {
			Some(SweepConfig {
				min_usdt: parse_opt("SWEEP_MIN_USDT")?.unwrap_or(1_000_000_000_000_000_000),
				gas_drop_multiple: parse_opt("SWEEP_GAS_DROP_MULTIPLE")?.unwrap_or(3),
				min_gas_drop_wei: parse_opt("SWEEP_MIN_GAS_DROP_WEI")?.unwrap_or(300_000_000_000_000),
				topup_grace_secs: parse_opt("SWEEP_TOPUP_GRACE_SECS")?.unwrap_or(60),
				poll_secs: parse_opt("SWEEP_POLL_SECS")?.unwrap_or(30),
			})
		} else {
			None
		};
		// Polygon sweep reuses the EVM `SweepConfig` (same native-coin top-up gas model); only the
		// `min_usdt` floor defaults to the 6-dp scale (1 USDT = 1e6 on-chain, vs BSC 1e18 @ 18-dp).
		let polygon_sweep = if polygon_sweep_enabled && polygon.is_some() {
			Some(SweepConfig {
				min_usdt: parse_opt("POLYGON_SWEEP_MIN_USDT")?.unwrap_or(1_000_000),
				gas_drop_multiple: parse_opt("POLYGON_SWEEP_GAS_DROP_MULTIPLE")?.unwrap_or(3),
				min_gas_drop_wei: parse_opt("POLYGON_SWEEP_MIN_GAS_DROP_WEI")?.unwrap_or(300_000_000_000_000),
				topup_grace_secs: parse_opt("POLYGON_SWEEP_TOPUP_GRACE_SECS")?.unwrap_or(60),
				poll_secs: parse_opt("POLYGON_SWEEP_POLL_SECS")?.unwrap_or(30),
			})
		} else {
			None
		};
		let tron_sweep = if tron_sweep_enabled && tron.is_some() {
			Some(TronSweepConfig {
				min_usdt: parse_opt("TRON_SWEEP_MIN_USDT")?.unwrap_or(1_000_000),
				min_trx_drop_sun: parse_opt("TRON_SWEEP_MIN_TRX_DROP_SUN")?.unwrap_or(30_000_000),
				topup_grace_secs: parse_opt("TRON_SWEEP_TOPUP_GRACE_SECS")?.unwrap_or(60),
				poll_secs: parse_opt("TRON_SWEEP_POLL_SECS")?.unwrap_or(30),
			})
		} else {
			None
		};
		let ton_sweep = if ton_sweep_enabled && ton.is_some() {
			Some(TonSweepConfig {
				min_usdt: parse_opt("TON_SWEEP_MIN_USDT")?.unwrap_or(1_000_000),
				gas_topup_nano: parse_opt("TON_SWEEP_GAS_TOPUP_NANO")?.unwrap_or(100_000_000),
				topup_grace_secs: parse_opt("TON_SWEEP_TOPUP_GRACE_SECS")?.unwrap_or(60),
				poll_secs: parse_opt("TON_SWEEP_POLL_SECS")?.unwrap_or(30),
			})
		} else {
			None
		};
		Ok(Self {
			bsc,
			sweep,
			polygon,
			polygon_sweep,
			tron,
			tron_sweep,
			ton,
			ton_sweep,
		})
	}

	/// The rails whose chain config is present — exactly the rails whose deposit watcher
	/// runs (the same `BSC_RPC_URL`/`TRON_RPC_URL`/`TON_API_URL` gates as above). Only
	/// configured rails are provisioned/served by the wallet surface: an address minted
	/// for a rail no watcher scans would strand whatever is sent to it.
	pub fn configured_networks(&self) -> Vec<Network> {
		[
			self.bsc.as_ref().map(|_| Network::Bep20),
			self.polygon.as_ref().map(|_| Network::Polygon),
			self.tron.as_ref().map(|_| Network::Trc20),
			self.ton.as_ref().map(|_| Network::Ton),
		]
		.into_iter()
		.flatten()
		.collect()
	}

	/// Refuse to boot a hub whose configured rails mix a testnet and a mainnet chain.
	///
	/// USDT claims are network-agnostic — one fungible pool across every rail (migration
	/// `0004_unified_balance`), so a deposit on ANY rail mints a claim withdrawable on EVERY
	/// other rail. Put a testnet rail and a mainnet rail on one hub and a free testnet deposit
	/// becomes withdrawable as real mainnet USDT elsewhere; the `sum(custody)==sum(claims)`
	/// invariant stays clean because both legs inflate equally. `Unknown` chains (local dev)
	/// take no side. TRON is frozen off and never configured.
	pub fn assert_single_realm(&self) -> color_eyre::Result<()> {
		let realms = [
			self.bsc.as_ref().map(|c| ("BSC", c.realm())),
			self.polygon.as_ref().map(|c| ("Polygon", c.realm())),
			self.ton.as_ref().map(|c| ("TON", if c.is_testnet { Realm::Testnet } else { Realm::Mainnet })),
		];
		let named = |want: Realm| -> Vec<&'static str> { realms.iter().flatten().filter(|(_, r)| *r == want).map(|(n, _)| *n).collect() };
		let testnet = named(Realm::Testnet);
		let mainnet = named(Realm::Mainnet);
		color_eyre::eyre::ensure!(
			testnet.is_empty() || mainnet.is_empty(),
			"on-chain rails mix testnet and mainnet on one hub — testnet [{}] alongside mainnet [{}]. \
			 USDT claims are network-agnostic (one pool across rails), so a free testnet deposit would be \
			 withdrawable as real mainnet USDT on another rail. Run testnet rails on a separate hub + database.",
			testnet.join(", "),
			mainnet.join(", "),
		);
		Ok(())
	}
}

/// The on-chain EVM rail config (BEP20 / Polygon), shared by the deposit watcher, the withdrawal
/// confirmation watcher, the sweep, and real custody. Present only when the rail's RPC URL is set
/// (`BSC_RPC_URL` / `POLYGON_RPC_URL`); the endpoint MUST support `eth_getLogs` (for deposit
/// scanning). The two EVM rails are two instances of this one config — `network` is the only field
/// that structurally differs (it scopes the nonce sequence, keys, and deposit/withdrawal tag);
/// the rest are per-rail chain parameters.
#[derive(Clone, Debug)]
pub struct EvmConfig {
	/// Which EVM rail this config drives (`Network::Bep20` or `Network::Polygon`). Scopes the
	/// persisted nonce sequence, the signer's deposit-address keys, and the `network` tag stamped
	/// on every deposit/withdrawal — so the two EVM rails never share a nonce space or a key.
	pub network: Network,
	/// JSON-RPC endpoint (`BSC_RPC_URL` / `POLYGON_RPC_URL`). Switch this (+ the USDT contract)
	/// between testnet and mainnet — the watcher logic is network-agnostic.
	pub rpc_url: String,
	/// The USDT contract address to watch (`BSC_USDT_CONTRACT` / `POLYGON_USDT_CONTRACT`). Defaults
	/// to the rail's mainnet USDT (BSC `0x55d3…7955`, 18-dp; Polygon `0xc213…58e8F`, 6-dp); set it
	/// to the testnet token for a testnet run.
	pub usdt_contract: String,
	/// Confirmations to wait before crediting a deposit / settling a withdrawal
	/// (`BSC_CONFIRMATIONS` / `POLYGON_CONFIRMATIONS`); defaults to the domain's per-network value
	/// (BEP20 15, Polygon 128) — reorg safety.
	pub confirmations: u64,
	/// Seconds between polls, for both the deposit scan and the withdrawal-receipt check
	/// (`BSC_POLL_SECS` / `POLYGON_POLL_SECS`); defaults to 12 (BSC) / 6 (Polygon, ~2s blocks).
	pub poll_secs: u64,
	/// First block to scan on a fresh cursor (`BSC_DEPOSIT_START_BLOCK` / `POLYGON_DEPOSIT_START_BLOCK`).
	/// `None` ⇒ start at the current safe head (watch from now), ignoring pre-existing on-chain history.
	pub start_block: Option<u64>,
	/// Max blocks per `eth_getLogs` call (`BSC_MAX_BLOCK_RANGE` / `POLYGON_MAX_BLOCK_RANGE`);
	/// defaults to 500 to stay within common provider range limits.
	pub max_block_range: u64,
	/// Dedicated endpoint for the deposit scan (`BSC_LOGS_RPC_URL` / `POLYGON_LOGS_RPC_URL`), when
	/// the main `rpc_url` paywalls/throttles `eth_getLogs` but is otherwise a fine full node.
	/// `None` ⇒ the scan uses `rpc_url`.
	pub logs_rpc_url: Option<String>,
	/// Chain id for signing withdrawals (`BSC_CHAIN_ID` / `POLYGON_CHAIN_ID`); 56 = BSC mainnet,
	/// 97 = BSC testnet, 137 = Polygon mainnet, 80002 = Polygon Amoy testnet.
	pub chain_id: u64,
	/// Gas limit for an ERC-20 transfer withdrawal (`BSC_GAS_LIMIT` / `POLYGON_GAS_LIMIT`); defaults
	/// to 100_000 (a USDT transfer is ~50–65k — the headroom is safe, and unused gas is refunded).
	pub gas_limit: u64,
	/// The hub's own ceiling on the node's `eth_gasPrice` quote for a withdrawal, in gwei
	/// (`BSC_MAX_GAS_PRICE_GWEI` / `POLYGON_MAX_GAS_PRICE_GWEI`); defaults to 100 (BSC) / 5_000
	/// (Polygon) — the signer's own fee-budget defaults (`SIGNER_MAX_GAS_PRICE_GWEI_{BEP20,POLYGON}`).
	/// A quote above it is a *market* condition: custody reports it transient (retried on the
	/// next drain pass) instead of forwarding it to the signer, whose refusal is a *policy*
	/// verdict that parks the withdrawal until an operator unparks it. This value MUST be ≤ the
	/// signer's ceiling for the rail: set it above, and a spike between the two still reaches
	/// the signer and parks — the pre-check protects nothing.
	pub max_gas_price_gwei: u64,
}
impl EvmConfig {
	/// This rail's chain reality for the single-realm boot check. Only the four chain ids the
	/// project actually runs are classified; anything else (a local dev chain) is `Unknown` and
	/// never trips the guard on its own.
	fn realm(&self) -> Realm {
		match self.chain_id {
			56 | 137 => Realm::Mainnet,
			97 | 80002 => Realm::Testnet,
			_ => Realm::Unknown,
		}
	}
}

/// A configured rail's testnet/mainnet reality for [`Rails::assert_single_realm`]. `Unknown`
/// (an unrecognized chain id — e.g. a local dev chain) is deliberately neither: it takes no
/// side, so it can never pair with a real rail to trip the guard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Realm {
	Testnet,
	Mainnet,
	Unknown,
}
/// EVM treasury-sweep economics (BEP20 / Polygon). `Some` only when the rail is configured AND its
/// sweep is enabled (`SWEEP_ENABLED` or the per-rail override) — it moves user funds on-chain, so
/// opt-in. The chain params (rpc, USDT, chain id, the transfer gas limit) come from [`EvmConfig`];
/// these knobs tune *when* and *how much*. Reused by both EVM rails — only `min_usdt`'s default
/// differs by the rail's on-chain precision (18-dp on BEP20, 6-dp on Polygon).
#[derive(Clone, Debug)]
pub struct SweepConfig {
	/// Minimum USDT (in the rail's ON-CHAIN raw units) on a deposit address worth sweeping
	/// (`SWEEP_MIN_USDT` / `POLYGON_SWEEP_MIN_USDT`); defaults to 1 USDT (1e18 on BEP20 18-dp,
	/// 1e6 on Polygon 6-dp) — below this the gas isn't worth it.
	pub min_usdt: u128,
	/// A native-coin (BNB/POL) top-up sends `max(needed_gas × this, min_gas_drop_wei)`
	/// (`SWEEP_GAS_DROP_MULTIPLE`); defaults to 3, so one top-up covers several future sweeps.
	pub gas_drop_multiple: u128,
	/// Floor for a native-coin top-up, in wei (`SWEEP_MIN_GAS_DROP_WEI`); defaults to 3e14.
	pub min_gas_drop_wei: u128,
	/// Don't re-top-up the same address within this many seconds (`SWEEP_TOPUP_GRACE_SECS`);
	/// defaults to 60 — long enough for a top-up to confirm before we'd consider another.
	pub topup_grace_secs: u64,
	/// Seconds between sweep cycles (`SWEEP_POLL_SECS`); defaults to 30.
	pub poll_secs: u64,
}
/// The on-chain Tron (TRC20) config, shared by the deposit watcher, the withdrawal confirmation
/// watcher, the sweep, and real custody. Present only when `TRON_RPC_URL` is set.
#[derive(Clone, Debug)]
pub struct TronConfig {
	/// TronGrid base URL (`TRON_RPC_URL`). Switch between mainnet (`https://api.trongrid.io`) and
	/// the Nile testnet (`https://nile.trongrid.io`) here — the watcher logic is network-agnostic.
	pub rpc_url: String,
	/// The USDT (TRC20) contract, Base58Check `T…` (`TRON_USDT_CONTRACT`). Defaults to mainnet USDT
	/// (`TR7N…Lj6t`, 6-dp); set it to the Nile faucet token for a testnet run.
	pub usdt_contract: String,
	/// Optional TronGrid API key (`TRON_API_KEY`, sent as `TRON-PRO-API-KEY`). Lifts rate limits on
	/// mainnet; the testnets don't need one.
	pub api_key: Option<String>,
	/// Seconds between deposit/withdrawal-confirmation polls (`TRON_POLL_SECS`); defaults to 12.
	pub poll_secs: u64,
	/// First `block_timestamp` (unix ms) to scan on a fresh cursor (`TRON_DEPOSIT_START_TS`). `None`
	/// starts from the current head (watch from now), ignoring pre-existing on-chain history.
	pub start_timestamp: Option<i64>,
	/// `fee_limit` (SUN) cap for a TRC20 transfer (`TRON_FEE_LIMIT`); defaults to 100_000_000
	/// (100 TRX) — comfortably above a USDT transfer's energy burn, low enough to bound a misfire.
	pub fee_limit: i64,
	/// Seconds a signed transaction stays valid past the head block (`TRON_EXPIRATION_SECS`);
	/// defaults to 60. After this a not-yet-mined tx is provably dead and can be safely re-signed.
	pub expiration_secs: i64,
	/// Max TRC20 transfers fetched per address per deposit scan (`TRON_MAX_TRANSFERS`); defaults to
	/// 200 (the indexed-history page size), the analogue of BSC's `max_block_range`.
	pub max_transfers_per_scan: u32,
}
/// Tron treasury-sweep economics. `Some` only when Tron is configured AND `SWEEP_ENABLED` is set.
/// The chain params (rpc, USDT, fee limit) come from [`TronConfig`]; these tune *when*/*how much*.
#[derive(Clone, Debug)]
pub struct TronSweepConfig {
	/// Minimum USDT (6-dp base units) on a deposit address worth sweeping (`TRON_SWEEP_MIN_USDT`);
	/// defaults to 1_000_000 (1 USDT) — below this the fee isn't worth it.
	pub min_usdt: u128,
	/// TRX (SUN) to drop on an address short of fees (`TRON_SWEEP_MIN_TRX_DROP_SUN`); defaults to
	/// 30_000_000 (30 TRX) — covers a worst-case fresh-recipient USDT-transfer energy burn.
	pub min_trx_drop_sun: u128,
	/// Don't re-top-up the same address within this many seconds (`TRON_SWEEP_TOPUP_GRACE_SECS`);
	/// defaults to 60.
	pub topup_grace_secs: u64,
	/// Seconds between sweep cycles (`TRON_SWEEP_POLL_SECS`); defaults to 30.
	pub poll_secs: u64,
}
/// The on-chain TON config, shared by the jetton deposit watcher, the withdrawal
/// confirmation watcher, and real custody. Present only when `TON_API_URL` is set (a
/// toncenter v3 base URL). Switch it (+ `TON_USDT_MASTER`, `TON_IS_TESTNET`) between
/// mainnet and testnet — the watcher/custody logic is network-agnostic.
#[derive(Clone, Debug)]
pub struct TonConfig {
	/// toncenter v3 base URL (`TON_API_URL`), e.g. `https://toncenter.com/api/v3` or
	/// `https://testnet.toncenter.com/api/v3`.
	pub api_url: String,
	/// toncenter API key (`TON_API_KEY`), sent as `X-Api-Key`. `None` uses the anonymous
	/// (rate-limited) tier — fine for testnet bring-up.
	pub api_key: Option<String>,
	/// The USDT jetton master address (`TON_USDT_MASTER`). Defaults to mainnet USDT; set it
	/// to the testnet master (`kQD0GKBM…`) for a testnet run.
	pub usdt_master: String,
	/// Seconds between polls, for both the jetton-deposit scan and the withdrawal-seqno
	/// check (`TON_POLL_SECS`); defaults to 6 (TON finality is ~5s).
	pub poll_secs: u64,
	/// Start watermark for a fresh deposit cursor (`TON_DEPOSIT_START_UTIME`, unix seconds). `None` ⇒ start at
	/// the current time (watch from now), ignoring pre-existing on-chain history. NOTE: the
	/// watcher tracks the cursor as a unix-time watermark (a globally-comparable value), not
	/// a per-account logical time — see [`infrastructure::ton_deposit_watcher`].
	pub start_cursor: Option<u64>,
	/// Whether the rail is on TON testnet. Defaults from `TON_API_URL` (a `testnet.` host) and
	/// can be set explicitly with `TON_IS_TESTNET`; a testnet URL with the flag off is rejected
	/// (see [`AppConfig::from_env`]). Selects the user-facing address tag and is carried into the
	/// signer so signing stays consistent with the addresses the operator funds.
	pub is_testnet: bool,
	/// The wallet contract version (`TON_WALLET_VERSION`); only `v4r2` is supported today.
	pub wallet_version: String,
	/// Nanotons forwarded with a jetton transfer to deploy the recipient's jetton wallet (if
	/// absent) and trigger its notification (`TON_FORWARD_TON_AMOUNT`); defaults to 0.05 TON.
	pub forward_ton_amount: u64,
	/// Nanotons attached to the jetton-wallet internal message (the gas budget; excess
	/// returns to the response destination) (`TON_MSG_VALUE`); defaults to 0.1 TON.
	pub msg_value: u64,
}

/// TON treasury-sweep economics. `Some` only when TON is configured AND `SWEEP_ENABLED`
/// is set (it moves user funds on-chain — opt-in). The chain params (api, master, fee
/// budgets) come from [`TonConfig`]; these knobs tune *when* and *how much*.
#[derive(Clone, Debug)]
pub struct TonSweepConfig {
	/// Minimum USDT (6-dp base units) on a user's jetton wallet worth sweeping
	/// (`TON_SWEEP_MIN_USDT`); defaults to 1 USDT — below this the gas isn't worth it.
	pub min_usdt: u128,
	/// Nanotons the gas station tops a user wallet up with before sweeping its USDT
	/// (`TON_SWEEP_GAS_TOPUP_NANO`); defaults to 0.1 TON (covers the deploy + jetton send).
	pub gas_topup_nano: u64,
	/// Don't re-top-up the same address within this many seconds (`TON_SWEEP_TOPUP_GRACE_SECS`);
	/// defaults to 60 — long enough for a top-up to confirm before another is considered.
	pub topup_grace_secs: u64,
	/// Seconds between sweep cycles (`TON_SWEEP_POLL_SECS`); defaults to 30.
	pub poll_secs: u64,
}

/// What a `CONCIERGE_BRIDGE_ADDR` proves about the party on the other end.
///
/// The lifecycle bridge carries strictly more authority than any other seam this binary
/// dials: a `KYC_CHANGED` it applies is the single gate on both directions of money
/// movement, and a `ROLE_CHANGED` mirrors an operator role onto the money plane. The
/// shared `BRIDGE_SERVICE_TOKEN` only proves banking to concierge — it says nothing about
/// who answered — so the transport is the whole of the reverse proof (EV-invest/banking#199).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeTransport {
	/// `https://` — the channel authenticates the server against the CA pinned by
	/// `BRIDGE_TLS_CA_PEM_FILE`, or, outside production, against the public roots.
	Tls,
	/// Cleartext, but the peer is this host: the stream never reaches a network where
	/// another party could answer or read it. The documented single-host exception, the
	/// same one the signer seam makes.
	Loopback,
	/// Cleartext to a name or address off-host. Whatever answers to that name is believed.
	Cleartext,
}

/// The scheme (case-folded) and what follows `://` — or no scheme and the whole address.
fn bridge_scheme(addr: &str) -> (Option<String>, &str) {
	match addr.split_once("://") {
		Some((scheme, rest)) => (Some(scheme.to_ascii_lowercase()), rest),
		None => (None, addr),
	}
}

/// The host of a bridge address, read without a URL parser: the authority with userinfo
/// dropped, the port dropped and an IPv6 literal unwrapped from its brackets. Empty when
/// there is nothing that reads as a host.
///
/// One reader for two decisions. [`bridge_transport`] asks it whether the peer is this
/// host; the TLS builder pins it as the name the server's certificate must carry. Two
/// parsers would let those drift — the classifier exempting one spelling of loopback
/// while the pin verified another — and the brackets matter for the pin: rustls reads
/// `::1` as an address and `[::1]` as nothing at all.
pub fn bridge_host(addr: &str) -> &str {
	let (_, rest) = bridge_scheme(addr);
	let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
	// `user:pass@host` — the host is what follows the LAST `@`.
	let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
	if let Some(inside) = authority.strip_prefix('[') {
		// `[::1]:55670` — the brackets exist precisely because the address holds colons.
		inside.split(']').next().unwrap_or_default()
	} else {
		authority.split(':').next().unwrap_or_default()
	}
}

/// Classify a bridge address: scheme first, then the host from [`bridge_host`].
///
/// Anything that is not `https` and does not resolve *syntactically* to loopback is
/// [`BridgeTransport::Cleartext`] — an unparseable address included. Classification
/// decides whether production boots at all, so the uncertain reading is the refusing one.
pub fn bridge_transport(addr: &str) -> BridgeTransport {
	if bridge_scheme(addr).0.as_deref() == Some("https") {
		return BridgeTransport::Tls;
	}
	let host = bridge_host(addr).to_ascii_lowercase();
	let loopback = host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
	if loopback { BridgeTransport::Loopback } else { BridgeTransport::Cleartext }
}

/// Why a file pinned by `BRIDGE_TLS_CA_PEM_FILE` cannot serve as a trust anchor.
///
/// Every variant below ends the same way if it is let through: a root store holding fewer
/// anchors than the operator pinned — none at all, in each of these cases.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PinnedCaProblem {
	/// Not a PEM: an empty file, raw DER, a block whose `-----END …-----` never arrives, or
	/// a path left pointing at something that was never a certificate bundle.
	#[error("it holds no complete PEM block — an empty file, DER bytes, a truncated block, or a path pointing at something else")]
	NotPem,
	/// PEM, but no certificate among it. A private key handed to the CA slot is the shape
	/// this catches.
	#[error("it holds no CERTIFICATE block, only: {0}")]
	NoCertificate(String),
	/// Certificates, beside a block the PEM reader will skip. A `PRIVATE KEY` here means key
	/// material is sitting in the one file the deployment treats as a public trust anchor.
	#[error("it holds a {0} block beside the certificates; a trust anchor is certificates and nothing else")]
	ForeignBlock(String),
}

/// `-----BEGIN CERTIFICATE-----` ⇒ `CERTIFICATE`, given `marker = "-----BEGIN "`.
fn pem_label<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
	line.strip_prefix(marker)?.strip_suffix("-----").filter(|label| !label.is_empty())
}

/// The label of every complete PEM block in `raw`, in order: a `-----BEGIN <label>-----`,
/// at least one line of body, and the matching `-----END <label>-----`. Anything outside a
/// block — openssl's text dump above a certificate, a comment, trailing junk — is skipped,
/// and an unterminated block is not a block.
fn pem_block_labels(raw: &str) -> Vec<&str> {
	let mut labels = Vec::new();
	let mut open: Option<(&str, bool)> = None;
	for line in raw.lines() {
		let line = line.trim();
		let Some((label, has_body)) = open else {
			open = pem_label(line, "-----BEGIN ").map(|label| (label, false));
			continue;
		};
		if pem_label(line, "-----END ") == Some(label) {
			if has_body {
				labels.push(label);
			}
			open = None;
		} else if !line.is_empty() {
			open = Some((label, true));
		}
	}
	labels
}

/// Check that a pinned CA file is a trust anchor, before it is handed to a TLS builder.
///
/// The PEM reader underneath tonic's `ca_certificate` skips every block it cannot use and
/// reports nothing, so a file with no certificate in it builds an EMPTY root store: every
/// handshake fails while the hub stays live and ready, and the seam looks configured
/// everywhere an operator would look.
///
/// Searching for the BEGIN marker as a substring is not that check. It passes a key file
/// that carries a certificate too (the private key then sits in a file mounted as public
/// trust material), a marker quoted inside a comment, and a block that is never closed. So
/// the blocks are parsed and every label has to be `CERTIFICATE`.
///
/// What this still cannot prove is that the base64 body decodes to a certificate: that
/// parse happens inside rustls at connect time, and no crate here can reach it. The armor
/// is the line this gate holds.
pub fn check_pinned_ca_pem(raw: &str) -> Result<(), PinnedCaProblem> {
	let labels = pem_block_labels(raw);
	if labels.is_empty() {
		return Err(PinnedCaProblem::NotPem);
	}
	let (certificates, foreign): (Vec<&str>, Vec<&str>) = labels.into_iter().partition(|label| *label == "CERTIFICATE");
	match (certificates.is_empty(), foreign.first()) {
		(true, _) => Err(PinnedCaProblem::NoCertificate(foreign.join(", "))),
		(false, Some(label)) => Err(PinnedCaProblem::ForeignBlock((*label).to_string())),
		(false, None) => Ok(()),
	}
}

/// Why production refuses to pull the lifecycle stream over a given `CONCIERGE_BRIDGE_ADDR`.
///
/// The address is carried so the message names the value an operator has to change, not
/// only the variable; the variables to change it with are named in the text.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BridgeRefusal {
	/// Cleartext to a peer off-host: neither encrypted nor server-authenticated, so anything
	/// that can answer to that name could mirror a KYC tier or an operator role onto the
	/// money plane.
	#[error(
		"CONCIERGE_BRIDGE_ADDR {addr} is cleartext to a non-loopback peer, and production refuses to pull the lifecycle stream over it: the stream would be neither encrypted nor server-authenticated, so anything that can answer to that name could mirror a KYC tier or an operator role onto the money plane (EV-invest/banking#199). Set an https:// address and pin its CA with BRIDGE_TLS_CA_PEM_FILE."
	)]
	Cleartext { addr: String },
	/// `https://` with nothing pinned: the public roots vouch for no cluster-internal name,
	/// so an unpinned root store proves nothing about which concierge answered.
	#[error(
		"CONCIERGE_BRIDGE_ADDR {addr} is https:// but BRIDGE_TLS_CA_PEM_FILE is unset, and production refuses to trust the public roots for it: no public CA vouches for a cluster-internal name, so an unpinned root store is not proof of which concierge answered but a hope (EV-invest/banking#199). Pin concierge's CA with BRIDGE_TLS_CA_PEM_FILE."
	)]
	UnpinnedCa { addr: String },
}

/// The refusal due for this environment, address and pin, or `None` when the seam may be
/// dialled — outside production, or once it is https with a pinned CA, or loopback.
///
/// Split out of [`ensure_bridge_is_authenticated`] so the scoping is a value and not just
/// an early `return`: this is the whole of the boot gate phase 2 of #199 leaves behind, and
/// a guard that silently stopped matching (or started matching everywhere) is precisely
/// the regression to pin.
fn bridge_refusal(app_env: &str, addr: &str, ca_file: Option<&str>) -> Option<BridgeRefusal> {
	if app_env != "production" {
		return None;
	}
	match bridge_transport(addr) {
		BridgeTransport::Cleartext => Some(BridgeRefusal::Cleartext { addr: addr.to_string() }),
		BridgeTransport::Tls if ca_file.is_none_or(str::is_empty) => Some(BridgeRefusal::UnpinnedCa { addr: addr.to_string() }),
		BridgeTransport::Tls | BridgeTransport::Loopback => None,
	}
}

/// Refuse a production boot that would pull the lifecycle stream from a peer it cannot
/// authenticate.
///
/// A refusal and not a WARN, which is what phase 1 of #199 emitted while production ran
/// on `http://concierge:55670`: the one seam that can rewrite a KYC tier or an operator
/// role is held to the same rule as the signer seam (`piggybank/signer/src/config.rs`),
/// which refuses a non-loopback bind without TLS. A hub that refuses to start does take
/// the money plane down with it — deliberately: the deploy contract in `flake.nix` moves
/// to `https://concierge:55672` in the same release, so the only production that hits this
/// is one whose Secret does not carry the CA yet, and that is a rollout ordered wrong, not
/// a hub that should run anyway.
///
/// Two things are demanded of production, and only production. Cleartext is refused
/// unless the peer is loopback — the documented single-host exception. And `https://` is
/// refused without `BRIDGE_TLS_CA_PEM_FILE`: the public roots cannot vouch for the name
/// `concierge`, so `with_enabled_roots` against a cluster-internal name is a root store
/// that trusts nothing relevant and a handshake that can never succeed — or, worse, one
/// that succeeds against whichever public certificate a hijacked name presents. Outside
/// production the seam is whatever the address says: dev dials loopback in cleartext and
/// a staging https address may lean on the public roots.
pub fn ensure_bridge_is_authenticated(app_env: &str, addr: &str, ca_file: Option<&str>) -> Result<(), BridgeRefusal> {
	bridge_refusal(app_env, addr, ca_file).map_or(Ok(()), Err)
}

/// The scope ledger's own listener (`scope.v1.ScopeLedgerService`) — the service-facing money
/// surface, kept off the core listener because what authenticates a caller here is different:
/// mTLS, with the client's identity taken from its certificate and nothing it says.
///
/// | Variable | |
/// | --- | --- |
/// | `SCOPE_LEDGER_BIND` | listen address, e.g. `0.0.0.0:50054`; unset/empty ⇒ the listener is off |
/// | `SCOPE_LEDGER_TLS_CERT_PEM_FILE` | the listener's certificate |
/// | `SCOPE_LEDGER_TLS_KEY_PEM_FILE` | its private key |
/// | `SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE` | the CA every client certificate must chain to |
/// | `SCOPE_LEDGER_GRANTS_FILE` | JSON: clients by certificate fingerprint, and their grants |
///
/// With `SCOPE_LEDGER_BIND` set every file is required and checked at boot: a listener that
/// cannot prove who calls it, or that grants something unsafe, never starts. The grants
/// file:
///
/// ```json
/// {
///   "clients": [{ "client_id": "invest", "cert_sha256": ["<64 hex>"] }],
///   "grants": [{ "client_id": "invest", "scope": "invest", "label_prefix": "alloc:", "compartments": false }]
/// }
/// ```
///
/// `cert_sha256` is the SHA-256 of the client certificate's DER bytes, lowercase hex
/// (`openssl x509 -in client.pem -outform der | openssl dgst -sha256`); list two to rotate.
pub struct ScopeLedgerSettings {
	pub bind: SocketAddr,
	pub cert_pem: String,
	pub key_pem: String,
	pub client_ca_pem: String,
	pub clients: domain::scope_ledger::ClientDirectory,
	pub grants: domain::scope_ledger::Grants,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantsFile {
	clients: Vec<ClientEntry>,
	grants: Vec<GrantEntry>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientEntry {
	client_id: String,
	cert_sha256: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantEntry {
	client_id: String,
	scope: String,
	label_prefix: String,
	#[serde(default)]
	compartments: bool,
}

/// Build the rustls server config the listener will run: the certificate chain, its private
/// key (and that they belong together), and a client-certificate verifier over the CA.
fn check_server_tls(cert_pem: &str, key_pem: &str, client_ca_pem: &str) -> color_eyre::Result<()> {
	use std::sync::Arc;

	use color_eyre::eyre::eyre;
	use rustls::{
		RootCertStore, ServerConfig,
		pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
		server::WebPkiClientVerifier,
	};
	let chain = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| eyre!("SCOPE_LEDGER_TLS_CERT_PEM_FILE: {e}"))?;
	if chain.is_empty() {
		return Err(eyre!("SCOPE_LEDGER_TLS_CERT_PEM_FILE holds no certificate"));
	}
	let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).map_err(|e| eyre!("SCOPE_LEDGER_TLS_KEY_PEM_FILE: {e}"))?;
	let mut roots = RootCertStore::empty();
	for ca in CertificateDer::pem_slice_iter(client_ca_pem.as_bytes()) {
		let ca = ca.map_err(|e| eyre!("SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE: {e}"))?;
		// webpki takes any certificate as a trust anchor, a leaf included; with a leaf here the
		// hub boots healthy and then refuses every client in the handshake. Only a CA may stand.
		if !der_is_ca(ca.as_ref()) {
			return Err(eyre!(
				"SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE holds a certificate that is not a CA (basicConstraints CA:TRUE missing)"
			));
		}
		roots.add(ca).map_err(|e| eyre!("SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE: {e}"))?;
	}
	// The provider is named rather than taken from the process default, so this check means
	// the same thing whether or not `main` has installed one yet.
	let provider = Arc::new(rustls::crypto::ring::default_provider());
	let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
		.build()
		.map_err(|e| eyre!("SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE: {e}"))?;
	ServerConfig::builder_with_provider(provider)
		.with_safe_default_protocol_versions()
		.map_err(|e| eyre!("scope ledger TLS: {e}"))?
		.with_client_cert_verifier(verifier)
		.with_single_cert(chain, key)
		.map_err(|e| eyre!("SCOPE_LEDGER_TLS_CERT_PEM_FILE / SCOPE_LEDGER_TLS_KEY_PEM_FILE: {e}"))?;
	Ok(())
}

/// One DER TLV at the head of `der`: `(tag, contents, rest)`. Definite lengths only, which is
/// all DER allows; `None` on anything malformed.
fn der_tlv(der: &[u8]) -> Option<(u8, &[u8], &[u8])> {
	let (&tag, rest) = der.split_first()?;
	let (&first, rest) = rest.split_first()?;
	let (len, rest) = if first < 0x80 {
		(usize::from(first), rest)
	} else {
		let count = usize::from(first & 0x7f);
		if count == 0 || count > 4 || rest.len() < count {
			return None;
		}
		let (bytes, rest) = rest.split_at(count);
		(bytes.iter().fold(0usize, |len, b| (len << 8) | usize::from(*b)), rest)
	};
	(rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// Whether an X.509 certificate's basicConstraints says `cA = TRUE`. Walks Certificate →
/// tbsCertificate → extensions `[3]` → the extension with OID 2.5.29.19 → its value. No
/// extension, or `cA` absent (DEFAULT FALSE), is not a CA. Hand-walked so the check needs no
/// X.509 crate for one boolean.
fn der_is_ca(certificate: &[u8]) -> bool {
	const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
	let Some((0x30, cert, _)) = der_tlv(certificate) else { return false };
	let Some((0x30, mut tbs, _)) = der_tlv(cert) else { return false };
	while let Some((tag, contents, rest)) = der_tlv(tbs) {
		tbs = rest;
		if tag != 0xa3 {
			continue;
		}
		let Some((0x30, mut extensions, _)) = der_tlv(contents) else { return false };
		while let Some((0x30, extension, rest)) = der_tlv(extensions) {
			extensions = rest;
			let Some((0x06, oid, mut fields)) = der_tlv(extension) else { return false };
			if oid != BASIC_CONSTRAINTS {
				continue;
			}
			// `critical BOOLEAN DEFAULT FALSE` may precede the value.
			if let Some((0x01, _, rest)) = der_tlv(fields) {
				fields = rest;
			}
			let Some((0x04, value, _)) = der_tlv(fields) else { return false };
			let Some((0x30, constraints, _)) = der_tlv(value) else { return false };
			return matches!(der_tlv(constraints), Some((0x01, [flag], _)) if *flag != 0);
		}
		return false;
	}
	false
}

impl ScopeLedgerSettings {
	pub fn from_env() -> color_eyre::Result<Option<Self>> {
		Self::read(|key| env::var(key).ok().filter(|value| !value.trim().is_empty()))
	}

	fn read(var: impl Fn(&str) -> Option<String>) -> color_eyre::Result<Option<Self>> {
		use color_eyre::eyre::{WrapErr, eyre};
		let Some(bind) = var("SCOPE_LEDGER_BIND") else {
			return Ok(None);
		};
		let bind: SocketAddr = bind.trim().parse().map_err(|e| eyre!("SCOPE_LEDGER_BIND must be a socket address such as 0.0.0.0:50054: {e}"))?;
		let file = |key: &str| -> color_eyre::Result<String> {
			let path = var(key).ok_or_else(|| eyre!("SCOPE_LEDGER_BIND is set, so {key} is required: the scope ledger serves only mTLS"))?;
			std::fs::read_to_string(&path).wrap_err_with(|| format!("{key}: cannot read {path}"))
		};
		let cert_pem = file("SCOPE_LEDGER_TLS_CERT_PEM_FILE")?;
		let key_pem = file("SCOPE_LEDGER_TLS_KEY_PEM_FILE")?;
		let client_ca_pem = file("SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE")?;
		// The CA is what every client identity hangs on; an unusable one would build an empty
		// root store and refuse every client while the hub looks healthy.
		check_pinned_ca_pem(&client_ca_pem).map_err(|problem| eyre!("SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE is not a trust anchor: {problem}"))?;
		// tonic parses the PEMs only when it starts serving — after the hub said it is
		// listening, and as a failure of one branch, not of the boot. Build the same rustls
		// server config here so a broken key or a certificate that is not the key's stops the
		// boot with the variable's name.
		check_server_tls(&cert_pem, &key_pem, &client_ca_pem)?;
		let (clients, grants) = Self::parse_grants(&file("SCOPE_LEDGER_GRANTS_FILE")?).wrap_err("SCOPE_LEDGER_GRANTS_FILE")?;
		Ok(Some(Self {
			bind,
			cert_pem,
			key_pem,
			client_ca_pem,
			clients,
			grants,
		}))
	}

	/// Parse and check the grants file: every rule of [`ClientDirectory::new`] and
	/// [`Grants::new`], and every grant naming a client the file identifies — a grant to a
	/// client no certificate maps to is a typo that would otherwise grant nothing, silently.
	///
	/// [`ClientDirectory::new`]: domain::scope_ledger::ClientDirectory::new
	/// [`Grants::new`]: domain::scope_ledger::Grants::new
	pub fn parse_grants(json: &str) -> color_eyre::Result<(domain::scope_ledger::ClientDirectory, domain::scope_ledger::Grants)> {
		use color_eyre::eyre::eyre;
		use domain::{
			scope_ledger::{ClientDirectory, ClientId, Grant, Grants},
			scopes::ScopeId,
		};
		let file: GrantsFile = serde_json::from_str(json).map_err(|e| eyre!("not the grants shape: {e}"))?;
		let clients = file
			.clients
			.into_iter()
			.map(|entry| Ok((ClientId::parse(&entry.client_id)?, entry.cert_sha256)))
			.collect::<Result<Vec<_>, domain::error::DomainError>>()
			.map_err(|e| eyre!("{e}"))?;
		let directory = ClientDirectory::new(clients).map_err(|e| eyre!("{e}"))?;
		let grants = file
			.grants
			.into_iter()
			.map(|entry| {
				Ok(Grant {
					client: ClientId::parse(&entry.client_id)?,
					scope: ScopeId::parse(&entry.scope)?,
					label_prefix: entry.label_prefix,
					compartments: entry.compartments,
				})
			})
			.collect::<Result<Vec<_>, domain::error::DomainError>>()
			.map_err(|e| eyre!("{e}"))?;
		if let Some(orphan) = grants.iter().find(|grant| !grant.client.is_banking() && !directory.knows(&grant.client)) {
			return Err(eyre!("grant to {} in {}: no client entry identifies {}", orphan.client, orphan.scope, orphan.client));
		}
		let grants = Grants::new(grants).map_err(|e| eyre!("{e}"))?;
		Ok((directory, grants))
	}
}

/// A boolean env var: `true`/`1` ⇒ true, anything else ⇒ false, unset/empty ⇒ `default`.
fn bool_env(key: &str, default: bool) -> bool {
	match env::var(key).ok().filter(|s| !s.is_empty()) {
		Some(v) => v == "true" || v == "1",
		None => default,
	}
}

/// Parse an optional env var that, when present and non-empty, must be a valid `T`.
fn parse_opt<T: std::str::FromStr>(key: &str) -> color_eyre::Result<Option<T>>
where
	T::Err: std::fmt::Display, {
	match env::var(key).ok().filter(|s| !s.is_empty()) {
		Some(raw) => raw.parse::<T>().map(Some).map_err(|e| color_eyre::eyre::eyre!("{key} must be a valid value: {e}")),
		None => Ok(None),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const FINGERPRINT: &str = "32f91a20f9908f7c21a964ae0eb55394b90ecb7f9b92472d8349eb1f1e4ea675";

	fn scope_fixture(name: &str) -> String {
		format!("{}/tests/fixtures/scope_tls/{name}", env!("CARGO_MANIFEST_DIR"))
	}

	fn grants_json(clients: &str, grants: &str) -> String {
		format!(r#"{{"clients":[{clients}],"grants":[{grants}]}}"#)
	}

	fn invest_client() -> String {
		format!(r#"{{"client_id":"invest","cert_sha256":["{FINGERPRINT}"]}}"#)
	}

	#[test]
	fn only_a_certificate_with_ca_true_counts_as_a_ca() {
		use rustls::pki_types::{CertificateDer, pem::PemObject};
		let der = |name: &str| CertificateDer::from_pem_file(scope_fixture(name)).unwrap();
		assert!(der_is_ca(der("ca.pem").as_ref()));
		assert!(der_is_ca(der("foreign-ca.pem").as_ref()));
		for leaf in ["invest.pem", "server.pem", "stranger.pem"] {
			assert!(!der_is_ca(der(leaf).as_ref()), "{leaf} is CA:FALSE");
		}
		assert!(!der_is_ca(b""));
		assert!(!der_is_ca(&[0x30, 0x82, 0xff]), "a truncated certificate");
	}

	#[test]
	fn the_grants_file_refuses_what_would_grant_too_much_or_nothing() {
		let ok = grants_json(&invest_client(), r#"{"client_id":"invest","scope":"invest","label_prefix":"alloc:","compartments":true}"#);
		let (clients, grants) = ScopeLedgerSettings::parse_grants(&ok).unwrap();
		assert_eq!(clients.client_for(FINGERPRINT).map(|c| c.as_str()), Some("invest"));
		assert!(!grants.is_empty());
		for (json, needle) in [
			(grants_json(&invest_client(), r#"{"client_id":"banking","scope":"invest","label_prefix":"alloc:"}"#), "banking"),
			(grants_json(&format!(r#"{{"client_id":"banking","cert_sha256":["{FINGERPRINT}"]}}"#), ""), "banking"),
			(grants_json(&invest_client(), r#"{"client_id":"invest","scope":"main","label_prefix":"alloc:"}"#), "scope main"),
			(grants_json(&invest_client(), r#"{"client_id":"invest","scope":"invest","label_prefix":""}"#), "label_prefix"),
			(
				grants_json(
					&format!(r#"{},{{"client_id":"panel","cert_sha256":["{}"]}}"#, invest_client(), "b".repeat(64)),
					r#"{"client_id":"invest","scope":"sa","label_prefix":"alloc:"},{"client_id":"panel","scope":"sa","label_prefix":"alloc:panel"}"#,
				),
				"overlap",
			),
			(grants_json(&invest_client(), r#"{"client_id":"panel","scope":"sa","label_prefix":"panel:"}"#), "no client entry"),
			(
				grants_json(&invest_client(), r#"{"client_id":"invest","scope":"sa","label_prefix":"x:","admin":true}"#),
				"unknown field",
			),
			(grants_json(r#"{"client_id":"invest","cert_sha256":["ABC"]}"#, ""), "cert_sha256"),
		] {
			let err = format!("{:#}", ScopeLedgerSettings::parse_grants(&json).expect_err(needle));
			assert!(err.contains(needle), "expected {needle:?} in: {err}");
		}
	}

	#[test]
	fn the_scope_ledger_listener_is_off_until_bound_and_then_needs_every_file() {
		let off = ScopeLedgerSettings::read(|_| None).unwrap();
		assert!(off.is_none());

		let grants = std::env::temp_dir().join(format!("scope-ledger-grants-{}.json", std::process::id()));
		std::fs::write(&grants, grants_json(&invest_client(), r#"{"client_id":"invest","scope":"invest","label_prefix":"alloc:"}"#)).unwrap();
		let files = |ca: &'static str| {
			let grants = grants.to_string_lossy().into_owned();
			move |key: &str| -> Option<String> {
				Some(match key {
					"SCOPE_LEDGER_BIND" => "127.0.0.1:50054".to_owned(),
					"SCOPE_LEDGER_TLS_CERT_PEM_FILE" => scope_fixture("server.pem"),
					"SCOPE_LEDGER_TLS_KEY_PEM_FILE" => scope_fixture("server.key"),
					"SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE" => scope_fixture(ca),
					"SCOPE_LEDGER_GRANTS_FILE" => grants.clone(),
					_ => return None,
				})
			}
		};
		let on = ScopeLedgerSettings::read(files("ca.pem")).unwrap().expect("bound");
		assert_eq!(on.bind, "127.0.0.1:50054".parse::<SocketAddr>().unwrap());

		let err = format!("{:#}", ScopeLedgerSettings::read(files("server.key")).err().expect("a key is not a CA"));
		assert!(err.contains("SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE"), "{err}");

		let err = format!("{:#}", ScopeLedgerSettings::read(files("invest.pem")).err().expect("a leaf is not a CA"));
		assert!(err.contains("not a CA"), "{err}");

		let wrong_key = |key: &str| {
			if key == "SCOPE_LEDGER_TLS_KEY_PEM_FILE" {
				Some(scope_fixture("invest.key"))
			} else {
				files("ca.pem")(key)
			}
		};
		let err = format!("{:#}", ScopeLedgerSettings::read(wrong_key).err().expect("a key that is not the certificate's"));
		assert!(err.contains("SCOPE_LEDGER_TLS_KEY_PEM_FILE"), "{err}");
		let garbage_key = |key: &str| {
			if key == "SCOPE_LEDGER_TLS_KEY_PEM_FILE" {
				Some(scope_fixture("README.md"))
			} else {
				files("ca.pem")(key)
			}
		};
		let err = format!("{:#}", ScopeLedgerSettings::read(garbage_key).err().expect("not a key at all"));
		assert!(err.contains("SCOPE_LEDGER_TLS_KEY_PEM_FILE"), "{err}");

		let without_ca = |key: &str| (key != "SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE").then(|| files("ca.pem")(key)).flatten();
		let err = format!("{:#}", ScopeLedgerSettings::read(without_ca).err().expect("mTLS needs the client CA"));
		assert!(err.contains("SCOPE_LEDGER_TLS_CLIENT_CA_PEM_FILE is required"), "{err}");
		std::fs::remove_file(&grants).unwrap();
	}

	/// The money-plane switch opens only on an explicit, unambiguous word. A typo, a
	/// half-edited value, an empty string and an unset variable all leave the gate shut —
	/// this is what keeps the default from drifting.
	#[test]
	fn the_kyc_gate_is_lifted_only_by_an_explicit_false() {
		for raw in [
			None,
			Some(""),
			Some("  "),
			Some("true"),
			Some("TRUE"),
			Some("1"),
			Some("no"),
			Some("off"),
			Some("disabled"),
			Some("fals"),
		] {
			assert_eq!(KycGate::read(raw), KycGate::ENFORCED, "{raw:?} must leave the gate enforced");
		}
		for raw in [Some("false"), Some("False"), Some("0"), Some(" false ")] {
			assert_eq!(KycGate::read(raw), KycGate::LIFTED, "{raw:?} must lift the gate");
		}
	}

	/// Each switch resolves to its default on every reading but the two explicit words, and a
	/// typo never moves a payment path by itself.
	#[test]
	fn the_payment_switches_read_their_words_and_default_on_anything_else() {
		use PaymentApprovalMode::{Concierge, Consilium};
		let read = |legacy, fund, session| PaymentApprovalModes::read(legacy, fund, session);
		// Fund money stays on the old path and investors' on the new, with nothing set.
		assert_eq!(
			read(None, None, None),
			PaymentApprovalModes {
				fund: Consilium,
				session: Concierge
			}
		);
		assert_eq!(read(Some(""), Some("  "), Some("")), PaymentApprovalModes::DEFAULT);
		// Each side has its own word, case and spaces aside.
		assert_eq!(read(None, Some("concierge"), None).fund, Concierge);
		assert_eq!(read(None, Some(" CONSILIUM "), None).fund, Consilium);
		assert_eq!(read(None, None, Some("Consilium")).session, Consilium);
		// A typo is the side's default, not the other word.
		for typo in ["consilum", "old", "true"] {
			assert_eq!(read(None, Some(typo), Some(typo)), PaymentApprovalModes::DEFAULT, "{typo}");
		}
		// The old single name sets both when it is set; the specific names override it.
		assert_eq!(read(Some("concierge"), None, None), PaymentApprovalModes::ALL_CONCIERGE);
		assert_eq!(read(Some("consilium"), None, None), PaymentApprovalModes::ALL_CONSILIUM);
		assert_eq!(
			read(Some("concierge"), Some("consilium"), None),
			PaymentApprovalModes {
				fund: Consilium,
				session: Concierge
			}
		);
		assert_eq!(
			read(Some("consilium"), None, Some("concierge")),
			PaymentApprovalModes {
				fund: Consilium,
				session: Concierge
			}
		);
		// A bad old name is the defaults, not a half-reading.
		assert_eq!(read(Some("nonsense"), None, None), PaymentApprovalModes::DEFAULT);
	}

	/// The requirement picks the switch: fund money by `FUND`, an investor's by `SESSION`.
	#[test]
	fn the_requirement_picks_its_switch() {
		use domain::{payments::PaymentApproval, users::UserId};
		let modes = PaymentApprovalModes::DEFAULT;
		assert_eq!(modes.of(PaymentApproval::OwnerConsilium), PaymentApprovalMode::Consilium);
		assert_eq!(modes.of(PaymentApproval::SubjectConsent(UserId::new())), PaymentApprovalMode::Concierge);
	}

	/// The classification decides whether production says anything at all about the seam
	/// that can rewrite a KYC tier, so each family is pinned: `https` is the only scheme
	/// that proves the peer, loopback is the only cleartext exemption, and everything else
	/// — a bare host, a cluster DNS name, a public address, a value nobody can parse —
	/// lands on the loud side.
	#[test]
	fn only_https_or_loopback_makes_the_bridge_seam_something_other_than_cleartext() {
		for addr in ["https://concierge:55670", "HTTPS://concierge", "https://concierge.apps.svc.cluster.local/"] {
			assert_eq!(bridge_transport(addr), BridgeTransport::Tls, "{addr} is TLS");
		}
		for addr in [
			"http://127.0.0.1:55670",
			"http://localhost:55670",
			"http://LOCALHOST",
			"http://[::1]:55670",
			"http://127.9.9.9:55670",
			"http://user:pass@localhost:55670",
			"localhost:55670",
		] {
			assert_eq!(bridge_transport(addr), BridgeTransport::Loopback, "{addr} never leaves the host");
		}
		for addr in [
			"http://concierge:55670",
			"http://concierge.apps.svc.cluster.local:55670",
			"http://10.42.0.7:55670",
			"http://[2001:db8::1]:55670",
			"http://user@concierge:55670",
			"",
			"not a url",
		] {
			assert_eq!(bridge_transport(addr), BridgeTransport::Cleartext, "{addr} is believed on the strength of a name");
		}
	}

	/// A throwaway self-signed certificate (`openssl req -x509 -newkey rsa:2048 -nodes`,
	/// valid to 2126), here so the gate is exercised against a real certificate and not only
	/// against shapes of armor. No private key was kept and nothing trusts it.
	const TEST_CA_PEM: &str = r"-----BEGIN CERTIFICATE-----
MIIDGzCCAgOgAwIBAgIUNQD11Yz00cFGinwEzH2XA+Owz9swDQYJKoZIhvcNAQEL
BQAwHDEaMBgGA1UEAwwRRVYgdGVzdCBicmlkZ2UgQ0EwIBcNMjYwOTE1MTg0MzQw
WhgPMjEyNjA4MjIxODQzNDBaMBwxGjAYBgNVBAMMEUVWIHRlc3QgYnJpZGdlIENB
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEApmWEKLoU5f5XZwYIHZyi
ivR2+9nizSTgbr5EEx8ifjYhU0V+h+gRzZrWfZUNMR3OPmNYC3/3VxzwCQ1lHg4r
hgkiDXqUaI9KUMZZo/HfSn2V6h4O/iYl3Q2jVcmtKCwKC7A0eDH+4NvaOY40wmsn
3b9kV9AlVDsUx4cBaIGNgBdEq4vN5DYjY/ZhPOoKfAiDiM4zQupW69XauEzhclFY
qjzG/bnr11uWB4L+o7lZFu5gNN8bbmtL9jnxEYfl3KKnk4GpoAVDKHU9l5S/6oDm
9o5/KMgZgtQCOU0h8NSTHT2s3UkLCCeSufF2Tx/Um7C9V899yryuyt34Kpy6odSQ
cQIDAQABo1MwUTAdBgNVHQ4EFgQUlqaFS6fHa5yah2xLuk8CfaCWM9QwHwYDVR0j
BBgwFoAUlqaFS6fHa5yah2xLuk8CfaCWM9QwDwYDVR0TAQH/BAUwAwEB/zANBgkq
hkiG9w0BAQsFAAOCAQEAlU+v3Iui7pujKrW1fciQmJWwxiFLxuE8QFv4oAM+lzFx
TKqj42bAOApMNXCw5flGp/Z9LhFPPoIThgxiRbYc7AyxhHXyuwfkAF22qrtB88r+
njxEK4PFw7JoieY19UKhMqZ82gbesvW7T9Rd7deBXCksRNObzKc9LSl9O5UO+9Ks
kNsL7Jype//j0lo3pVQcvbdq/fNno1YWnF72Y2w/mCOBV1Mc8fRX76z7sF3cHhkU
10NvzdF+DGKDJJaUe89qas9TZrAdjCN6j9XXwrzG9Mc4Z9H9TCJ35tOJGM6GO5Qb
gRI8JvM30gtx/NBsGEV927PGd7imCZpKdAlR1pYzGA==
-----END CERTIFICATE-----
";

	/// PEM armor around a base64 sentence. The gate reads labels, never bodies, so no key
	/// material has to exist for the "this is a key file" case to be real.
	const TEST_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\nYSB0ZXN0IHZhbHVlLCBub3QgYSBrZXk=\n-----END PRIVATE KEY-----\n";

	/// A pinned CA decides which concierge the hub believes about KYC tiers and operator
	/// roles, and the reader beneath it skips in silence what it cannot use. So each way a
	/// file can fail to be a trust anchor is pinned here: every one of them would otherwise
	/// boot a hub that is live, ready, TLS-configured — and trusting nothing, so the
	/// lifecycle stream simply stops.
	#[test]
	fn a_pinned_ca_is_accepted_only_when_it_is_certificates_and_nothing_else() {
		assert_eq!(check_pinned_ca_pem(TEST_CA_PEM), Ok(()), "a real certificate is the whole point of the variable");
		assert_eq!(
			check_pinned_ca_pem(&format!("{TEST_CA_PEM}{TEST_CA_PEM}")),
			Ok(()),
			"a private CA usually ships a bundle, not one anchor"
		);
		assert_eq!(
			check_pinned_ca_pem(&format!("subject=CN = EV test bridge CA\n\n{TEST_CA_PEM}trailing notes\n")),
			Ok(()),
			"openssl's text dump around the block is not a block"
		);

		for (raw, why) in [
			("", "an empty file — an unset secret that mounted as a file anyway"),
			("\u{0}\u{1}\u{2}0\u{82}\u{3}\u{1}", "raw DER, the other way a certificate is spelled"),
			("-----BEGIN CERTIFICATE-----\nMIIDGzCCAgOgAwIBAgIUNQ\n", "a block whose END never arrives is a truncated copy"),
			(
				"a note about -----BEGIN CERTIFICATE----- and how to make one\n",
				"the marker quoted in prose is not a certificate",
			),
		] {
			assert_eq!(check_pinned_ca_pem(raw), Err(PinnedCaProblem::NotPem), "{why}");
		}

		assert_eq!(
			check_pinned_ca_pem(TEST_KEY_PEM),
			Err(PinnedCaProblem::NoCertificate("PRIVATE KEY".to_string())),
			"a key file in the CA slot is the mix-up this gate exists for"
		);
		assert_eq!(
			check_pinned_ca_pem(&format!("{TEST_KEY_PEM}{TEST_CA_PEM}")),
			Err(PinnedCaProblem::ForeignBlock("PRIVATE KEY".to_string())),
			"a substring check passes this one, and the key then rides along in a file mounted as public trust material"
		);
	}

	/// The host is what the TLS pin verifies the certificate against, so every shape the
	/// classifier already reads has to yield the same host here — and the IPv6 literal has
	/// to come out of its brackets, because `[::1]` is not a name rustls can pin.
	#[test]
	fn the_bridge_host_is_the_authority_without_userinfo_port_or_brackets() {
		for (addr, host) in [
			("https://concierge:55672", "concierge"),
			("https://concierge", "concierge"),
			("https://concierge.apps.svc.cluster.local/", "concierge.apps.svc.cluster.local"),
			("http://127.0.0.1:55670", "127.0.0.1"),
			("http://[::1]:55670", "::1"),
			("http://user:pass@localhost:55670/path?q#f", "localhost"),
			("localhost:55670", "localhost"),
			("https://", ""),
			("", ""),
		] {
			assert_eq!(bridge_host(addr), host, "{addr}");
		}
	}

	/// The boot gate is the whole of what phase 2 of #199 leaves behind on this side, so
	/// the matrix is pinned cell by cell: production refuses cleartext to anything but
	/// loopback and refuses https without a pinned CA; everything else — every other
	/// environment, loopback, https with a pin — boots. The refusal names the address and
	/// the variable that ends it, because the operator reading it is looking at a Secret.
	#[test]
	fn production_refuses_an_unauthenticated_bridge_and_nothing_else_is_refused() {
		let pinned = Some("/etc/settings/BRIDGE_TLS_CA_PEM");
		for (addr, why) in [
			("http://concierge:55670", "production on h2c to a cluster name is what phase 1 warned about and phase 2 refuses"),
			("http://10.42.0.7:55670", "a pod address is not this host"),
			("not a url", "an address nobody can parse is not a reason to boot"),
		] {
			for ca in [None, pinned] {
				assert_eq!(
					bridge_refusal("production", addr, ca),
					Some(BridgeRefusal::Cleartext { addr: addr.to_string() }),
					"{why} (ca {ca:?})"
				);
			}
		}
		for addr in ["https://concierge:55672", "HTTPS://concierge:55672", "https://concierge.apps.svc.cluster.local/"] {
			for ca in [None, Some("")] {
				assert_eq!(
					bridge_refusal("production", addr, ca),
					Some(BridgeRefusal::UnpinnedCa { addr: addr.to_string() }),
					"{addr} without a pin trusts the public roots for a cluster name (ca {ca:?})"
				);
			}
			assert_eq!(bridge_refusal("production", addr, pinned), None, "{addr} with a pinned CA is the production shape");
		}
		for addr in ["http://127.0.0.1:55670", "http://localhost:55670", "http://[::1]:55670"] {
			assert_eq!(bridge_refusal("production", addr, None), None, "{addr} never reaches a network");
		}
		for env in ["development", "staging", ""] {
			for addr in ["http://concierge:55670", "https://concierge:55672", "not a url"] {
				assert_eq!(bridge_refusal(env, addr, None), None, "{env} is not production, so {addr} is whatever the operator says");
			}
		}

		let Err(refusal) = ensure_bridge_is_authenticated("production", "http://concierge:55670", None) else {
			panic!("the refusal must reach the caller as an error, not a log line");
		};
		let text = refusal.to_string();
		for needle in ["http://concierge:55670", "https://", "BRIDGE_TLS_CA_PEM_FILE", "#199"] {
			assert!(text.contains(needle), "the cleartext refusal must name {needle}: {text}");
		}
		let text = ensure_bridge_is_authenticated("production", "https://concierge:55672", None)
			.expect_err("unpinned https is refused")
			.to_string();
		for needle in ["https://concierge:55672", "BRIDGE_TLS_CA_PEM_FILE", "#199"] {
			assert!(text.contains(needle), "the unpinned refusal must name {needle}: {text}");
		}
		assert_eq!(ensure_bridge_is_authenticated("production", "https://concierge:55672", pinned), Ok(()));
		assert_eq!(ensure_bridge_is_authenticated("development", "http://127.0.0.1:55670", None), Ok(()));
	}

	#[test]
	fn a_lifted_gate_admits_every_tier_and_an_enforced_one_only_the_verified() {
		assert!(!KycGate::ENFORCED.admits(0));
		assert!(KycGate::ENFORCED.admits(KYC_LEVEL_VERIFIED));
		assert!(KycGate::LIFTED.admits(0));
		assert!(KycGate::ENFORCED.is_enforced());
		assert!(!KycGate::LIFTED.is_enforced());
	}

	#[test]
	fn default_grpc_binds_are_loopback() {
		let minimal: std::collections::HashMap<&str, &str> = [
			("DATABASE_URL", "postgres://localhost/banking"),
			("APP_ENV", "development"),
			("TIGERBEETLE_ADDRESS", "3033"),
			("TIGERBEETLE_CLUSTER_ID", "0"),
			("SIGNER_GRPC_ADDR", "http://127.0.0.1:50053"),
			("CONCIERGE_BRIDGE_ADDR", "http://127.0.0.1:55670"),
			("BRIDGE_SERVICE_TOKEN", "test-bridge"),
		]
		.into_iter()
		.collect();
		let config = AppConfig::from_source(|var| minimal.get(var).map(|v| v.to_string())).expect("minimal env loads");
		for addr in [config.grpc_addr, config.auth_grpc_addr] {
			assert!(addr.ip().is_loopback(), "{addr} must default to loopback, not all interfaces");
		}
	}

	/// The env surface IS the deploy contract (the flake image env + gitops
	/// manifests use exactly these names) — a rename here must be deliberate.
	#[test]
	fn env_surface_matches_the_deploy_contract() {
		assert_eq!(
			AppConfig::var_names(),
			vec![
				"DATABASE_URL",
				"GRPC_ADDR",
				"AUTH_GRPC_ADDR",
				"SENTRY_DSN",
				"POSTHOG_KEY",
				"POSTHOG_HOST",
				"APP_ENV",
				"TIGERBEETLE_ADDRESS",
				"TIGERBEETLE_CLUSTER_ID",
				"SIGNER_GRPC_ADDR",
				"DB_MAX_CONNECTIONS",
				"RELAY_DB_MAX_CONNECTIONS",
				"CONCIERGE_BRIDGE_ADDR",
				"BRIDGE_SERVICE_TOKEN",
				"BRIDGE_POLL_SECS",
				"CONSILIUM_APPROVAL_URL_BASE",
				"PAYMENT_CONSENT_URL_BASE",
			]
		);
	}

	/// THE APPROVAL LINK MUST LAND ON A PUBLIC ROUTE.
	///
	/// An owner opens their approval mail on whatever device the mail is on — a phone with no
	/// session, someone else's laptop — holding a single-use token with a 72h life. The
	/// cabinet's `/cabinet/consilium` is the OWNER-ONLY console and is not in the frontend's
	/// public-route list, so a signed-out owner following a link there is bounced to
	/// `/login`; by the time they have signed in, the deep link is gone and so, often, is the
	/// token. `/cabinet/approve/{token}` is the public page built for exactly this.
	///
	/// This default was `/cabinet/consilium` and would have shipped a governance mechanism
	/// whose every invitation dead-ended at a login wall.
	#[test]
	fn the_default_approval_url_points_at_the_public_approval_route() {
		let minimal: std::collections::HashMap<&str, &str> = [
			("DATABASE_URL", "postgres://localhost/banking"),
			("APP_ENV", "development"),
			("TIGERBEETLE_ADDRESS", "3033"),
			("TIGERBEETLE_CLUSTER_ID", "0"),
			("SIGNER_GRPC_ADDR", "http://127.0.0.1:50053"),
			("CONCIERGE_BRIDGE_ADDR", "http://127.0.0.1:55670"),
			("BRIDGE_SERVICE_TOKEN", "test-bridge"),
		]
		.into_iter()
		.collect();
		let base = AppConfig::from_source(|var| minimal.get(var).map(|v| v.to_string()))
			.expect("minimal env loads")
			.consilium_approval_url_base;
		assert!(base.ends_with("/cabinet/approve"), "the approval base must be the public approval route, got {base}");
		assert!(
			!base.contains("/cabinet/consilium"),
			"/cabinet/consilium is the owner-only console and is not a public route — a signed-out owner would be bounced to /login and lose the token"
		);
		assert!(
			base.starts_with("https://"),
			"an approval link carries a bearer token in its path and must never be sent over http, got {base}"
		);
	}

	/// The consent link is held to the same rule: a public route, over https, and never the
	/// owner-only console — the investor answering it is by definition not an owner.
	#[test]
	fn the_default_consent_url_points_at_a_public_route() {
		let minimal: std::collections::HashMap<&str, &str> = [
			("DATABASE_URL", "postgres://localhost/banking"),
			("APP_ENV", "development"),
			("TIGERBEETLE_ADDRESS", "3033"),
			("TIGERBEETLE_CLUSTER_ID", "0"),
			("SIGNER_GRPC_ADDR", "http://127.0.0.1:50053"),
			("CONCIERGE_BRIDGE_ADDR", "http://127.0.0.1:55670"),
			("BRIDGE_SERVICE_TOKEN", "test-bridge"),
		]
		.into_iter()
		.collect();
		let base = AppConfig::from_source(|var| minimal.get(var).map(|v| v.to_string()))
			.expect("minimal env loads")
			.payment_consent_url_base;
		assert!(base.ends_with("/cabinet/consent"), "the consent base must be the public consent route, got {base}");
		assert!(!base.contains("/cabinet/consilium"), "/cabinet/consilium is the owner-only console");
		assert!(
			base.starts_with("https://"),
			"a consent link carries a bearer token in its path and must never be sent over http, got {base}"
		);
	}

	/// What a production deploy must provide beyond the always-required set.
	/// The gitops preflight diffs the cluster Secret against this list
	/// (`--print-required-vars`), so a change here changes the deploy contract.
	#[test]
	fn production_additionally_requires_the_silent_failure_surface() {
		let extra: Vec<String> = AppConfig::required_var_names("production")
			.into_iter()
			.filter(|var| !AppConfig::required_var_names("development").contains(var))
			.collect();
		// POSTHOG_KEY is read (it is in the env surface above) but never required:
		// analytics is a metric, not a safety net. See the field's comment.
		assert_eq!(extra, vec!["SENTRY_DSN"]);
	}

	#[test]
	fn a_production_deploy_without_monitoring_fails_to_boot() {
		let minimal: std::collections::HashMap<&str, &str> = [
			("DATABASE_URL", "postgres://localhost/banking"),
			("APP_ENV", "production"),
			("TIGERBEETLE_ADDRESS", "127.0.0.1:3033"),
			("TIGERBEETLE_CLUSTER_ID", "0"),
			("SIGNER_GRPC_ADDR", "http://127.0.0.1:50053"),
			("CONCIERGE_BRIDGE_ADDR", "http://127.0.0.1:55670"),
			("BRIDGE_SERVICE_TOKEN", "test-bridge"),
		]
		.into_iter()
		.collect();
		let error = AppConfig::from_source(|var| minimal.get(var).map(|v| v.to_string())).expect_err("the money plane must not run unmonitored");

		let vars: Vec<&str> = error.errors.iter().map(|e| e.var.as_str()).collect();
		assert_eq!(vars, vec!["SENTRY_DSN"]);
	}

	/// A minimal rails config with the given chain Options set — only the fields
	/// `configured_networks` reads matter; the rest are inert placeholders.
	fn config(bsc: bool, polygon: bool, tron: bool, ton: bool) -> Rails {
		let evm = |network| EvmConfig {
			network,
			rpc_url: String::new(),
			usdt_contract: String::new(),
			confirmations: 1,
			poll_secs: 1,
			start_block: None,
			max_block_range: 1,
			logs_rpc_url: None,
			chain_id: 1,
			gas_limit: 1,
			max_gas_price_gwei: 1,
		};
		Rails {
			bsc: bsc.then(|| evm(Network::Bep20)),
			sweep: None,
			polygon: polygon.then(|| evm(Network::Polygon)),
			polygon_sweep: None,
			tron: tron.then(|| TronConfig {
				rpc_url: String::new(),
				usdt_contract: String::new(),
				api_key: None,
				poll_secs: 1,
				start_timestamp: None,
				fee_limit: 1,
				expiration_secs: 1,
				max_transfers_per_scan: 1,
			}),
			tron_sweep: None,
			ton: ton.then(|| TonConfig {
				api_url: String::new(),
				api_key: None,
				usdt_master: String::new(),
				poll_secs: 1,
				start_cursor: None,
				is_testnet: false,
				wallet_version: String::new(),
				forward_ton_amount: 1,
				msg_value: 1,
			}),
			ton_sweep: None,
		}
	}

	#[test]
	fn configured_networks_mirror_the_chain_config_options() {
		assert!(config(false, false, false, false).configured_networks().is_empty());
		assert_eq!(config(true, false, false, false).configured_networks(), [Network::Bep20]);
		assert_eq!(config(false, true, false, false).configured_networks(), [Network::Polygon]);
		assert_eq!(config(false, false, true, false).configured_networks(), [Network::Trc20]);
		assert_eq!(config(false, false, false, true).configured_networks(), [Network::Ton]);
		assert_eq!(
			config(true, true, true, true).configured_networks(),
			[Network::Bep20, Network::Polygon, Network::Trc20, Network::Ton]
		);
	}

	/// Build a rails config with explicit per-rail chain reality; `None` leaves a rail off.
	fn realmed(bsc_chain: Option<u64>, polygon_chain: Option<u64>, ton: Option<bool>) -> Rails {
		let evm = |network, chain_id| EvmConfig {
			network,
			rpc_url: String::new(),
			usdt_contract: String::new(),
			confirmations: 1,
			poll_secs: 1,
			start_block: None,
			max_block_range: 1,
			logs_rpc_url: None,
			chain_id,
			gas_limit: 1,
			max_gas_price_gwei: 1,
		};
		let mut r = config(false, false, false, false);
		r.bsc = bsc_chain.map(|c| evm(Network::Bep20, c));
		r.polygon = polygon_chain.map(|c| evm(Network::Polygon, c));
		r.ton = ton.map(|is_testnet| TonConfig {
			api_url: String::new(),
			api_key: None,
			usdt_master: String::new(),
			poll_secs: 1,
			start_cursor: None,
			is_testnet,
			wallet_version: String::new(),
			forward_ton_amount: 1,
			msg_value: 1,
		});
		r
	}

	#[test]
	fn single_realm_accepts_uniform_and_unconfigured_hubs() {
		// All mainnet, all testnet, and no rails at all are each a coherent hub.
		realmed(Some(56), Some(137), Some(false)).assert_single_realm().expect("all mainnet");
		realmed(Some(97), Some(80002), Some(true)).assert_single_realm().expect("all testnet");
		realmed(None, None, None).assert_single_realm().expect("no rails");
		realmed(Some(56), None, None).assert_single_realm().expect("single mainnet rail");
	}

	#[test]
	fn single_realm_rejects_a_testnet_rail_beside_a_mainnet_rail() {
		// Polygon Amoy (80002) beside live BSC (56) — the exact runbook flip hazard.
		let err = realmed(Some(56), Some(80002), None).assert_single_realm().unwrap_err().to_string();
		assert!(err.contains("BSC"), "names the mainnet rail: {err}");
		assert!(err.contains("Polygon"), "names the testnet rail: {err}");
		// The cross-family case: TON testnet beside a BSC mainnet rail.
		assert!(
			realmed(Some(56), None, Some(true)).assert_single_realm().is_err(),
			"testnet TON beside mainnet BSC must be refused"
		);
	}

	#[test]
	fn single_realm_ignores_unknown_chains() {
		// A local dev chain (unrecognized id) takes no side, so it can't trip the guard even
		// next to a real testnet rail.
		realmed(Some(31337), None, Some(true)).assert_single_realm().expect("unknown + testnet");
		realmed(Some(31337), Some(137), None).assert_single_realm().expect("unknown + mainnet");
	}
}
