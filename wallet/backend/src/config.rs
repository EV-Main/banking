use std::net::SocketAddr;

use crate::wallet_intents::{IntentKind, production_host_rule, trusted_host};

ev::settings! {
	/// Runtime configuration for the wallet BFF, read from the environment only
	/// (`dotenvy` loads `.env` first in dev; prod values ride the container env —
	/// the image's baked topology contract plus gitops' k8s Secret `envFrom`).
	/// Every missing/invalid variable is reported in one aggregate error before
	/// anything binds, and an empty string counts as unset.
	pub struct AppConfig {
		/// HTTP listener — the address the Next.js frontend's `/api/*` rewrite points at.
		/// Loopback by default: the BFF's request-auth is a bearer cookie, so it must sit
		/// behind the same-origin reverse proxy unless an operator opts into a wider bind
		/// (and pairs it with network segmentation — see `docs/ARCHITECTURE.md`).
		/// Named `CABINET_BACKEND_BIND` (not a bare `BIND`) so the deployment env stays
		/// unambiguous next to the other services' binds.
		#[env("CABINET_BACKEND_BIND")]
		bind: SocketAddr = "127.0.0.1:4000",
		/// The piggybank money plane (wallet/consilium/health), e.g. `http://127.0.0.1:50051`.
		piggybank_grpc_addr: String,
		/// The banking auth service (token issuance), e.g. `http://127.0.0.1:50052`. The BFF
		/// calls `IssueUserToken`/`Refresh` here to obtain the money-plane token pair.
		banking_auth_grpc_addr: String,
		/// The shared bearer the BFF presents on `IssueUserToken` (the concierge→banking seam).
		/// Must match `BANKING_ISSUANCE_TOKEN` on the banking auth side.
		#[secret]
		banking_issuance_token: IssuanceToken,
		/// The concierge identity plane (directory/profile), e.g. `http://127.0.0.1:50061`.
		/// Also serves the JWKS the access-JWT verifier caches (auth is shell-owned; the
		/// BFF only VERIFIES the shared `ev_access` cookie, it never runs OAuth).
		concierge_grpc_addr: String,
		/// Issuer + audience the verifier expects on the shared access JWT. Must match
		/// what the concierge plane stamps (its `AUTH_ISSUER` / `AUTH_CLIENT_AUDIENCE`).
		auth_issuer: String,
		auth_client_audience: String,
		/// Explicit override for cookie hardening; unset ⇒ infer from `app_env`
		/// (`__Host-`/Secure needs HTTPS, which dev lacks). See [`Self::cookie_secure`].
		auth_cookie_secure: Option<bool>,
		/// Cross-origin MFE bundle origins allowed beyond same-origin (relative) URLs.
		/// Mirrors the frontend CSP allow-list (`MFE_ALLOWED_ORIGINS`); in production it may
		/// name only the wallet host's own origin (see [`Self::check_script_origins`]).
		mfe_allowed_origins: Vec<String> = "",
		/// The browser origin of the wallet host's own pages (scheme://host[:port]): the only
		/// `Origin` the first-party gate of the mutating wallet routes admits
		/// (`routes::first_party_origin`). The gate fails CLOSED when it is unset, so a dev box
		/// that wants to move money sets it to its browser origin; production refuses to boot
		/// without it.
		#[required_in("production")]
		#[env("CABINET_WS_ORIGIN")]
		wallet_ws_origin: Option<String>,
		/// The scope hosts (`https://invest.evinvest.ltd`, …) that may READ the wallet
		/// cross-origin with the user's cookies — `GET /api/wallet`, `/api/wallet/compartments`,
		/// `/api/operations` and nothing else (`routes::cors`). Empty ⇒ no CORS at all. Not
		/// required in production: an empty list only means the scope hosts cannot show a
		/// balance, never that anyone can move money. Mirrors the origins in
		/// `wallet/frontend/shared/config/wallet-scopes.ts`.
		wallet_cors_origins: Vec<CorsOrigin> = "",
		/// Paying an invoice above this amount (decimal USDT) needs a sign-in no older than
		/// `STEP_UP_MAX_AGE_SECS`; at or below it a session is enough. Optional on purpose: the
		/// default is the product's line, and a malformed value fails the boot rather than
		/// silently lifting the check.
		pay_stepup_threshold_usdt: UsdtThreshold = "100",
		/// The host pages that may run a wallet action in place (`routes::intents`,
		/// `docs/WALLET-INTENTS.md`). Empty ⇒ the intent routes are not mounted at all. Each
		/// must be an entry of the in-code matrix (`wallet_intents::TRUSTED_HOSTS`): this
		/// list switches a host on and cannot add one. See [`Self::check_wallet_actions`].
		wallet_action_origins: Vec<CorsOrigin> = "",
		/// The kinds a host may ask for (`move`, `pay`, `mandate`, `withdraw`), narrowing each
		/// host's matrix entry. `move` alone by default: the spec switches the kinds on one by
		/// one, with observation after each.
		wallet_action_kinds: Vec<IntentKind> = "move",
		/// The BFF's wallet key: it signs the hosts' CSRF tokens and seals stored intents, each
		/// through its own derived subkey. Required once a host is switched on.
		#[secret]
		wallet_intent_key: Option<WalletIntentKey>,
		/// The store every replica shares for intents, spent step-up proofs and the budgets.
		/// Required in production once a host is switched on: with an in-process store a proof
		/// could be spent once per replica. A secret: in production it carries the Redis
		/// password (`redis://default:<password>@…`).
		#[secret]
		redis_url: Option<String>,
		/// A move through a host at or below this (decimal USDT) needs no step-up, while…
		wallet_move_no_stepup_per_op_usdt: UsdtThreshold = "100",
		/// …the user's moves through all hosts together in 24 h stay at or below this.
		wallet_move_no_stepup_24h_usdt: UsdtThreshold = "300",
		/// The largest pay or move an emailed code may confirm; above it a passkey only. The
		/// email method rests on a mailbox and on a summary the page writes itself.
		wallet_email_stepup_cap_usdt: UsdtThreshold = "250",
		/// The most one user may confirm by emailed code in 24 h, through all hosts together;
		/// past it a passkey only.
		wallet_email_stepup_24h_usdt: UsdtThreshold = "500",
		/// How many reverse proxies in front of the BFF append to `X-Forwarded-For` (Traefik
		/// alone is 1; Traefik plus the Next.js `/api` rewrite is 2). The audit trail takes the
		/// address that many entries from the RIGHT, because everything left of the trusted hops is
		/// whatever the browser chose to send. Audit only, never an authorization input.
		trusted_proxy_hops: u8 = "1",
		app_env: String,
		/// Unset ⇒ error monitoring is a silent no-op. Right locally; in
		/// production it means the BFF's errors reach nobody, so there the boot
		/// fails instead.
		#[required_in("production")]
		sentry_dsn: Option<String>,
		/// Deliberately NOT `required_in("production")`, unlike `sentry_dsn`: a
		/// missing error reporter hides failures, while missing product analytics
		/// only forgoes a metric. That must never be the reason a service refuses
		/// to boot — and the BFF builds an Analytics client it drops on the next
		/// line, so unset simply means no analytics.
		posthog_key: Option<String>,
		posthog_host: Option<String>,
	}
}

impl AppConfig {
	/// Whether cookies are `__Host-`-prefixed + `Secure` (production over HTTPS).
	pub fn cookie_secure(&self) -> bool {
		self.auth_cookie_secure.unwrap_or(self.app_env == "production")
	}

	/// In production, no script from another origin may run in the wallet host's page. The
	/// wallet's consent routes (GrantMandate above all) trust a request from the first-party
	/// origin with the user's cookies, and a microfrontend mounts in that page's light DOM,
	/// so a scope's bundle could grant itself a mandate with no one clicking. Until concierge
	/// stamps a login time the BFF can demand a fresh sign-in by, the only safe list is one
	/// naming nothing but the wallet host's own origin.
	pub fn check_script_origins(&self) -> Result<(), String> {
		if self.app_env != "production" {
			return Ok(());
		}
		let own = self.wallet_ws_origin.as_deref().unwrap_or_default();
		let foreign: Vec<&str> = self.mfe_allowed_origins.iter().map(String::as_str).filter(|origin| *origin != own).collect();
		if foreign.is_empty() {
			return Ok(());
		}
		Err(format!(
			"MFE_ALLOWED_ORIGINS names origins other than the wallet host's own ({own}): {}. Their scripts would run with the user's session on the wallet's consent routes",
			foreign.join(", ")
		))
	}

	/// The boot check of the wallet-intent surface; nothing to check while it is off. Every
	/// listed origin must be a matrix entry and must not be the wallet host's own; production
	/// also holds each to [`production_host_rule`] and needs `REDIS_URL`. The key is needed
	/// everywhere: without it there is nothing to sign a CSRF token or an intent with.
	pub fn check_wallet_actions(&self) -> Result<(), String> {
		if self.wallet_action_origins.is_empty() {
			return Ok(());
		}
		let production = self.app_env == "production";
		let mut problems = Vec::new();
		for origin in self.wallet_action_origins.iter().map(CorsOrigin::as_str) {
			if trusted_host(origin).is_none() {
				problems.push(format!("{origin} is not a host of the in-code matrix (wallet_intents::TRUSTED_HOSTS)"));
			}
			if production && let Err(why) = production_host_rule(origin) {
				problems.push(format!("{origin} {why}"));
			}
			if self.wallet_ws_origin.as_deref() == Some(origin) {
				problems.push(format!("{origin} is the wallet host's own origin; its wallet routes are the first-party ones"));
			}
		}
		match &self.wallet_intent_key {
			None => problems.push("WALLET_INTENT_KEY is required once WALLET_ACTION_ORIGINS names a host".to_owned()),
			Some(key) if key.0.len() < MIN_WALLET_INTENT_KEY_BYTES => problems.push(format!("WALLET_INTENT_KEY is shorter than {MIN_WALLET_INTENT_KEY_BYTES} bytes")),
			Some(_) => {}
		}
		if production && self.redis_url.is_none() {
			problems.push("REDIS_URL is required in production once WALLET_ACTION_ORIGINS names a host".to_owned());
		}
		for (var, threshold) in [
			("WALLET_MOVE_NO_STEPUP_PER_OP_USDT", &self.wallet_move_no_stepup_per_op_usdt),
			("WALLET_MOVE_NO_STEPUP_24H_USDT", &self.wallet_move_no_stepup_24h_usdt),
			("WALLET_EMAIL_STEPUP_CAP_USDT", &self.wallet_email_stepup_cap_usdt),
			("WALLET_EMAIL_STEPUP_24H_USDT", &self.wallet_email_stepup_24h_usdt),
		] {
			if let Err(why) = crate::wallet_intents::usdt(threshold) {
				problems.push(format!("{var}: {why}"));
			}
		}
		if problems.is_empty() { Ok(()) } else { Err(problems.join("; ")) }
	}
}

/// The shortest wallet key the boot accepts: 32 bytes, a whole SHA-256 block's worth of the
/// HMAC's strength. `openssl rand -hex 32` gives 64.
pub const MIN_WALLET_INTENT_KEY_BYTES: usize = 32;

/// The wallet key, redacted in every `Debug` like [`IssuanceToken`].
#[derive(Clone, Default)]
pub struct WalletIntentKey(String);
impl WalletIntentKey {
	pub fn as_bytes(&self) -> &[u8] {
		self.0.as_bytes()
	}
}
impl std::fmt::Debug for WalletIntentKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("WalletIntentKey(<redacted>)")
	}
}
impl std::str::FromStr for WalletIntentKey {
	type Err = std::convert::Infallible;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Ok(Self(s.to_owned()))
	}
}
ev::settings_via_from_str!(WalletIntentKey);

/// The issuance bearer as a newtype with a hand-written `Debug`, so the secret is never
/// printed by a derived debug (mirrors `evbanking_auth`'s `IssuanceToken`). The settings
/// `#[secret]` layer additionally redacts it in the generated `Debug`/error output.
#[derive(Clone, Default)]
pub struct IssuanceToken(pub String);
impl std::fmt::Debug for IssuanceToken {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("IssuanceToken(<redacted>)")
	}
}
impl std::str::FromStr for IssuanceToken {
	type Err = std::convert::Infallible;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Ok(Self(s.to_owned()))
	}
}
ev::settings_via_from_str!(IssuanceToken);

/// A non-negative decimal USDT amount (`100`, `99.5`), kept as the string the operator wrote
/// so the comparison against an invoice's decimal string stays digit-exact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsdtThreshold(String);

impl UsdtThreshold {
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl std::str::FromStr for UsdtThreshold {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
		let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
		if whole.is_empty() || !digits(whole) || !digits(frac) || s.ends_with('.') {
			return Err(format!("{s:?} is not a plain non-negative decimal amount"));
		}
		Ok(Self(s.to_owned()))
	}
}
ev::settings_via_from_str!(UsdtThreshold);

/// One exact `scheme://host[:port]` origin, compared byte for byte against the browser's
/// `Origin`. Parsed strictly so a typo fails the boot instead of silently never matching,
/// and so the two values that would hand the user's balance to anyone — `*` and the
/// `null` origin of a sandboxed frame — cannot be configured at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorsOrigin(String);

impl CorsOrigin {
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl std::str::FromStr for CorsOrigin {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let host = s
			.strip_prefix("https://")
			// Plain http only on loopback, for a dev scope host; anything else would let a
			// network attacker on the path read the balance the cookie unlocks.
			.or_else(|| s.strip_prefix("http://").filter(|h| ["localhost", "127.0.0.1"].contains(&h.split(':').next().unwrap_or_default())))
			.ok_or_else(|| format!("{s:?} is not an https:// origin"))?;
		let well_formed = !host.is_empty() && host.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | ':')) && !host.starts_with(['.', '-', ':']);
		if !well_formed {
			return Err(format!("{s:?} is not a bare lowercase origin (no path, wildcard or trailing slash)"));
		}
		Ok(Self(s.to_owned()))
	}
}
ev::settings_via_from_str!(CorsOrigin);

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::*;

	fn minimal_env() -> HashMap<String, String> {
		[
			("PIGGYBANK_GRPC_ADDR", "http://127.0.0.1:50051"),
			("BANKING_AUTH_GRPC_ADDR", "http://127.0.0.1:50052"),
			("BANKING_ISSUANCE_TOKEN", "test-issuance"),
			("CONCIERGE_GRPC_ADDR", "http://127.0.0.1:50061"),
			("AUTH_ISSUER", "https://auth.concierge.ev"),
			("AUTH_CLIENT_AUDIENCE", "concierge"),
			("APP_ENV", "development"),
		]
		.into_iter()
		.map(|(k, v)| (k.to_string(), v.to_string()))
		.collect()
	}

	#[test]
	fn default_bind_is_loopback() {
		let config = AppConfig::from_source(|var| minimal_env().get(var).cloned()).expect("minimal env loads");
		assert!(config.bind.ip().is_loopback(), "the BFF must default to loopback, not all interfaces");
	}

	/// The env surface IS the deploy contract (the flake image env + gitops
	/// manifests use exactly these names) — a rename here must be deliberate.
	#[test]
	fn env_surface_matches_the_deploy_contract() {
		assert_eq!(
			AppConfig::var_names(),
			vec![
				"CABINET_BACKEND_BIND",
				"PIGGYBANK_GRPC_ADDR",
				"BANKING_AUTH_GRPC_ADDR",
				"BANKING_ISSUANCE_TOKEN",
				"CONCIERGE_GRPC_ADDR",
				"AUTH_ISSUER",
				"AUTH_CLIENT_AUDIENCE",
				"AUTH_COOKIE_SECURE",
				"MFE_ALLOWED_ORIGINS",
				"CABINET_WS_ORIGIN",
				"WALLET_CORS_ORIGINS",
				"PAY_STEPUP_THRESHOLD_USDT",
				"WALLET_ACTION_ORIGINS",
				"WALLET_ACTION_KINDS",
				"WALLET_INTENT_KEY",
				"REDIS_URL",
				"WALLET_MOVE_NO_STEPUP_PER_OP_USDT",
				"WALLET_MOVE_NO_STEPUP_24H_USDT",
				"WALLET_EMAIL_STEPUP_CAP_USDT",
				"WALLET_EMAIL_STEPUP_24H_USDT",
				"TRUSTED_PROXY_HOPS",
				"APP_ENV",
				"SENTRY_DSN",
				"POSTHOG_KEY",
				"POSTHOG_HOST",
			]
		);
	}

	/// What a production deploy must provide. The gitops preflight diffs the
	/// cluster Secret against this list (`--print-required-vars`), so a change
	/// here is a change to the deploy contract.
	#[test]
	fn production_additionally_requires_the_silent_failure_surface() {
		let extra: Vec<String> = AppConfig::required_var_names("production")
			.into_iter()
			.filter(|var| !AppConfig::required_var_names("development").contains(var))
			.collect();
		// POSTHOG_KEY is read (it is in the env surface above) but never required:
		// analytics is a metric, not a safety net. See the field's comment.
		assert_eq!(extra, vec!["CABINET_WS_ORIGIN", "SENTRY_DSN"]);
	}

	#[test]
	fn a_production_deploy_without_monitoring_fails_to_boot() {
		let mut env = minimal_env();
		env.insert("APP_ENV".to_string(), "production".to_string());
		let error = AppConfig::from_source(|var| env.get(var).cloned()).expect_err("production without monitoring must not boot");

		let vars: Vec<&str> = error.errors.iter().map(|e| e.var.as_str()).collect();
		assert_eq!(vars, vec!["CABINET_WS_ORIGIN", "SENTRY_DSN"]);
	}

	#[test]
	fn the_pay_step_up_threshold_defaults_to_100_and_refuses_junk() {
		let config = AppConfig::from_source(|var| minimal_env().get(var).cloned()).expect("minimal env loads");
		assert_eq!(config.pay_stepup_threshold_usdt.as_str(), "100");
		for good in ["0", "250", "99.5"] {
			assert!(good.parse::<UsdtThreshold>().is_ok(), "{good}");
		}
		for bad in ["", "-1", "1e3", "1,5", "abc", "5.", ".5", "1 000"] {
			assert!(bad.parse::<UsdtThreshold>().is_err(), "{bad}");
		}
		let mut env = minimal_env();
		env.insert("PAY_STEPUP_THRESHOLD_USDT".into(), "lots".into());
		assert!(AppConfig::from_source(|var| env.get(var).cloned()).is_err(), "a malformed threshold must fail the boot");
	}

	#[test]
	fn wallet_cors_origins_parse_the_scope_hosts_and_default_to_none() {
		assert!(
			AppConfig::from_source(|var| minimal_env().get(var).cloned())
				.expect("minimal env loads")
				.wallet_cors_origins
				.is_empty()
		);
		let mut env = minimal_env();
		env.insert("WALLET_CORS_ORIGINS".into(), "https://invest.evinvest.ltd, https://sa.evinvest.ltd".into());
		let config = AppConfig::from_source(|var| env.get(var).cloned()).expect("two scope hosts load");
		let origins: Vec<&str> = config.wallet_cors_origins.iter().map(CorsOrigin::as_str).collect();
		assert_eq!(origins, ["https://invest.evinvest.ltd", "https://sa.evinvest.ltd"]);
	}

	/// A value that would widen the cross-origin read past named hosts must stop the boot.
	#[test]
	fn a_wildcard_or_malformed_cors_origin_is_refused() {
		for bad in [
			"*",
			"null",
			"https://*.evinvest.ltd",
			"https://sa.evinvest.ltd/",
			"https://SA.evinvest.ltd",
			"http://sa.evinvest.ltd",
			"http://localhost.evil.example",
			"sa.evinvest.ltd",
			"https://",
		] {
			assert!(bad.parse::<CorsOrigin>().is_err(), "{bad} must be refused");
		}
		for good in ["https://sa.evinvest.ltd", "https://invest.evinvest.ltd:8443", "http://localhost:3000", "http://127.0.0.1:3000"] {
			assert!(good.parse::<CorsOrigin>().is_ok(), "{good} must parse");
		}
	}

	#[test]
	fn production_refuses_scripts_from_any_origin_but_the_wallet_hosts() {
		let mut env = minimal_env();
		env.insert("APP_ENV".to_string(), "production".to_string());
		env.insert("SENTRY_DSN".to_string(), "https://k@sentry.invalid/1".to_string());
		env.insert("CABINET_WS_ORIGIN".to_string(), "https://evinvest.ltd".to_string());
		let load = |origins: &str| {
			let mut env = env.clone();
			env.insert("MFE_ALLOWED_ORIGINS".to_string(), origins.to_string());
			AppConfig::from_source(|var| env.get(var).cloned()).expect("production env loads")
		};
		assert!(load("").check_script_origins().is_ok());
		assert!(load("https://evinvest.ltd").check_script_origins().is_ok());
		let refused = load("https://evinvest.ltd,https://aquafix.example")
			.check_script_origins()
			.expect_err("a scope's origin must not boot");
		assert!(refused.contains("https://aquafix.example"), "{refused}");
		assert!(load("https://sub.evinvest.ltd").check_script_origins().is_err(), "a subdomain is another origin");

		let mut dev = minimal_env();
		dev.insert("MFE_ALLOWED_ORIGINS".to_string(), "http://localhost:3001".to_string());
		let dev = AppConfig::from_source(|var| dev.get(var).cloned()).expect("dev env loads");
		assert!(dev.check_script_origins().is_ok(), "local MFEs run on their own ports");
	}

	fn production_env() -> HashMap<String, String> {
		let mut env = minimal_env();
		env.insert("APP_ENV".to_string(), "production".to_string());
		env.insert("SENTRY_DSN".to_string(), "https://k@sentry.invalid/1".to_string());
		env.insert("CABINET_WS_ORIGIN".to_string(), "https://evinvest.ltd".to_string());
		env
	}

	fn with_wallet(mut env: HashMap<String, String>, pairs: &[(&str, &str)]) -> Result<AppConfig, String> {
		for (var, value) in pairs {
			env.insert((*var).to_string(), (*value).to_string());
		}
		let config = AppConfig::from_source(|var| env.get(var).cloned()).map_err(|e| format!("{e:?}"))?;
		config.check_wallet_actions().map(|()| config)
	}

	const KEY: &str = "0123456789abcdef0123456789abcdef";

	/// Off by default: no origin, no routes, and nothing else is demanded — not even in
	/// production, where the surface has to be switched on deliberately.
	#[test]
	fn wallet_actions_are_off_by_default_with_move_as_the_only_kind() {
		let config = with_wallet(production_env(), &[]).expect("production boots with the surface off");
		assert!(config.wallet_action_origins.is_empty());
		assert_eq!(config.wallet_action_kinds, vec![IntentKind::Move]);
		assert!(config.wallet_intent_key.is_none() && config.redis_url.is_none());
		let config = with_wallet(minimal_env(), &[("WALLET_ACTION_KINDS", "move, pay,mandate,withdraw")]).expect("every kind parses");
		assert_eq!(config.wallet_action_kinds, vec![IntentKind::Move, IntentKind::Pay, IntentKind::Mandate, IntentKind::Withdraw]);
		assert!(with_wallet(minimal_env(), &[("WALLET_ACTION_KINDS", "move,transfer")]).is_err(), "an unknown kind fails the boot");
	}

	/// Production takes only the matrix's exact hosts, with the key and the shared store.
	#[test]
	fn production_switches_on_matrix_hosts_only() {
		let good = [
			("WALLET_ACTION_ORIGINS", "https://invest.evinvest.ltd,https://sa.evinvest.ltd"),
			("WALLET_INTENT_KEY", KEY),
			("REDIS_URL", "redis://10.42.0.1:6379/2"),
		];
		with_wallet(production_env(), &good).expect("both hosts boot");

		for bad in [
			"https://rogue.evinvest.ltd",
			"https://evinvest.ltd.evil.example",
			"https://evinvest.ltd",
			"https://invest.evinvest.ltd:8443",
			"https://invest.evinvest.ltd/",
			"https://invest.evinvest.ltd/wallet",
			"http://invest.evinvest.ltd",
			"http://localhost:3000",
			"https://*.evinvest.ltd",
			"*",
			"null",
		] {
			let refused = with_wallet(production_env(), &[good[1], good[2], ("WALLET_ACTION_ORIGINS", bad)]);
			assert!(refused.is_err(), "{bad} must not boot in production");
		}
		let refused = with_wallet(production_env(), &[good[0], good[1]]).expect_err("no shared store");
		assert!(refused.contains("REDIS_URL"), "{refused}");
		let refused = with_wallet(production_env(), &[good[0], good[2]]).expect_err("no key");
		assert!(refused.contains("WALLET_INTENT_KEY"), "{refused}");
		let refused = with_wallet(production_env(), &[good[0], good[2], ("WALLET_INTENT_KEY", "short")]).expect_err("a short key");
		assert!(refused.contains("shorter than 32"), "{refused}");
	}

	/// A dev box may run on the in-process store, but never with a host the matrix lacks or
	/// without a key.
	#[test]
	fn development_still_needs_a_matrix_host_and_a_key() {
		with_wallet(minimal_env(), &[("WALLET_ACTION_ORIGINS", "https://invest.evinvest.ltd"), ("WALLET_INTENT_KEY", KEY)]).expect("no Redis is fine off production");
		assert!(with_wallet(minimal_env(), &[("WALLET_ACTION_ORIGINS", "http://localhost:3001"), ("WALLET_INTENT_KEY", KEY)]).is_err());
		assert!(with_wallet(minimal_env(), &[("WALLET_ACTION_ORIGINS", "https://invest.evinvest.ltd")]).is_err());
		let mut first_party = minimal_env();
		first_party.insert("CABINET_WS_ORIGIN".into(), "https://invest.evinvest.ltd".into());
		assert!(
			with_wallet(first_party, &[("WALLET_ACTION_ORIGINS", "https://invest.evinvest.ltd"), ("WALLET_INTENT_KEY", KEY)]).is_err(),
			"the wallet host's own origin"
		);
	}

	#[test]
	fn the_wallet_key_and_the_redis_password_never_print() {
		let url = "redis://default:hunter2-redis-password@10.42.0.1:6379/2";
		let config = with_wallet(
			minimal_env(),
			&[("WALLET_ACTION_ORIGINS", "https://invest.evinvest.ltd"), ("WALLET_INTENT_KEY", KEY), ("REDIS_URL", url)],
		)
		.unwrap();
		let printed = format!("{config:?}");
		assert!(!printed.contains(KEY) && !printed.contains("hunter2"), "{printed}");
		assert!(!format!("{:?}", config.wallet_intent_key).contains(KEY));
		let mut prod = production_env();
		prod.insert("REDIS_URL".into(), url.into());
		let refused = with_wallet(prod, &[("WALLET_ACTION_ORIGINS", "https://rogue.evinvest.ltd")]).expect_err("a host outside the matrix");
		assert!(!refused.contains("hunter2"), "{refused}");
	}

	/// The first-party origin must never be configured away by omission in production:
	/// unset is a boot failure, not a default.
	#[test]
	fn the_first_party_origin_is_mandatory_in_production() {
		let mut env = minimal_env();
		env.insert("APP_ENV".to_string(), "production".to_string());
		env.insert("SENTRY_DSN".to_string(), "https://k@sentry.invalid/1".to_string());
		let error = AppConfig::from_source(|var| env.get(var).cloned()).expect_err("production without a ws origin must not boot");
		let vars: Vec<&str> = error.errors.iter().map(|e| e.var.as_str()).collect();
		assert_eq!(vars, vec!["CABINET_WS_ORIGIN"]);
	}
}
