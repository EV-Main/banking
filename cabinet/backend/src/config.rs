use std::net::SocketAddr;

ev::settings! {
	/// Runtime configuration for the cabinet BFF, read from the environment only
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
		/// The piggybank money plane (wallet/funds/health), e.g. `http://127.0.0.1:50051`.
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
		/// Path to the microfrontend registry served at `/api/mfe-registry`.
		mfe_registry_path: String,
		/// Cross-origin MFE bundle origins allowed beyond same-origin (relative) URLs.
		/// Mirrors the frontend CSP allow-list (`MFE_ALLOWED_ORIGINS`); the registry is
		/// validated against it before being served, so a poisoned entry never reaches the
		/// browser. Same-origin (relative) `scriptUrl`s need no entry.
		mfe_allowed_origins: Vec<String> = "",
		/// The browser origin allowed to open the governance websocket (scheme://host[:port]).
		/// A websocket handshake is exempt from CORS and carries cookies, so without this a
		/// cross-site page could open the live consilium feed as the signed-in user; the
		/// handshake rejects any other `Origin`. Unset ⇒ not enforced, which is right for
		/// local dev (the frontend runs on some `http://localhost:PORT`) and never right in
		/// production — where a missing value fails the boot rather than failing open. The
		/// same origin is the first-party gate of the mutating wallet routes
		/// (`routes::first_party_origin`), which, unlike the socket, fails CLOSED when it is
		/// unset: a dev box that wants to move money sets it to its browser origin.
		#[required_in("production")]
		cabinet_ws_origin: Option<String>,
		/// The scope hosts (`https://invest.evinvest.ltd`, …) that may READ the wallet
		/// cross-origin with the user's cookies — `GET /api/wallet`, `/api/wallet/compartments`,
		/// `/api/operations` and nothing else (`routes::cors`). Empty ⇒ no CORS at all. Not
		/// required in production: an empty list only means the scope hosts cannot show a
		/// balance, never that anyone can move money. Mirrors the origins in
		/// `cabinet/frontend/shared/config/wallet-scopes.ts`.
		wallet_cors_origins: Vec<CorsOrigin> = "",
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

	/// In production, no script from another origin may run in the cabinet's page. The
	/// wallet's consent routes (GrantMandate above all) trust a request from the first-party
	/// origin with the user's cookies, and a microfrontend mounts in that page's light DOM,
	/// so a scope's bundle could grant itself a mandate with no one clicking. Until concierge
	/// stamps a login time the BFF can demand a fresh sign-in by, the only safe list is one
	/// naming nothing but the cabinet's own origin.
	pub fn check_script_origins(&self) -> Result<(), String> {
		if self.app_env != "production" {
			return Ok(());
		}
		let own = self.cabinet_ws_origin.as_deref().unwrap_or_default();
		let foreign: Vec<&str> = self.mfe_allowed_origins.iter().map(String::as_str).filter(|origin| *origin != own).collect();
		if foreign.is_empty() {
			return Ok(());
		}
		Err(format!(
			"MFE_ALLOWED_ORIGINS names origins other than the cabinet's own ({own}): {}. Their scripts would run with the user's session on the wallet's consent routes",
			foreign.join(", ")
		))
	}
}

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

/// One exact `scheme://host[:port]` origin, compared byte for byte against the browser's
/// `Origin`. Parsed strictly so a typo fails the boot instead of silently never matching,
/// and so the two values that would hand the user's balance to anyone — `*` and the
/// `null` origin of a sandboxed frame — cannot be configured at all.
#[derive(Clone, Debug, PartialEq, Eq)]
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
			("MFE_REGISTRY_PATH", "/mfe-registry.json"),
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
				"MFE_REGISTRY_PATH",
				"MFE_ALLOWED_ORIGINS",
				"CABINET_WS_ORIGIN",
				"WALLET_CORS_ORIGINS",
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

	/// The governance socket's cross-site guard must never be able to fail OPEN in
	/// production: a websocket handshake is exempt from CORS but still carries the
	/// `ev_access` cookie, so an unset origin there would let any page read the live
	/// consilium feed as the signed-in owner. Unset is a boot failure, not a default.
	#[test]
	fn production_refuses_scripts_from_any_origin_but_the_cabinets() {
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

	#[test]
	fn the_websocket_origin_guard_is_mandatory_in_production() {
		let mut env = minimal_env();
		env.insert("APP_ENV".to_string(), "production".to_string());
		env.insert("SENTRY_DSN".to_string(), "https://k@sentry.invalid/1".to_string());
		let error = AppConfig::from_source(|var| env.get(var).cloned()).expect_err("production without a ws origin must not boot");
		let vars: Vec<&str> = error.errors.iter().map(|e| e.var.as_str()).collect();
		assert_eq!(vars, vec!["CABINET_WS_ORIGIN"]);
	}
}
