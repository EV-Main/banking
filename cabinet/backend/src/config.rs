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
		/// (`routes::first_party_origin`).
		#[required_in("production")]
		cabinet_ws_origin: Option<String>,
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

	/// The governance socket's cross-site guard must never be able to fail OPEN in
	/// production: a websocket handshake is exempt from CORS but still carries the
	/// `ev_access` cookie, so an unset origin there would let any page read the live
	/// consilium feed as the signed-in owner. Unset is a boot failure, not a default.
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
