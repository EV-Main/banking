//! Auth configuration — the issuing side ([`AuthConfig`], host-only) and the
//! downstream verifying side ([`VerifierConfig`]).
//!
//! Both read from the environment so the same binary runs unconfigured in local
//! dev and CI: with no signing key, the auth service mints nothing and authorizes
//! nothing (it answers [`AuthError::NotConfigured`](crate::AuthError)), exactly
//! like the rest of the scaffold's no-op-until-configured seams.

use std::env;

use color_eyre::eyre::{Context, ensure};

use crate::claims::TokenType;

/// This plane's identity token, asserted to appear in every configured issuer and
/// audience at boot. The two planes read the SAME `AUTH_*` env-var names and are kept
/// disjoint only by their default strings; if banking is ever launched from a shared
/// environment that overrides those defaults to concierge values, a concierge token
/// would be byte-for-byte valid here. This boot check refuses such a config so the
/// collision can never fail open. Defense-in-depth — the two planes MUST never share
/// an `AUTH_*` environment in the first place.
const PLANE: &str = "banking";

/// Configuration for the auth **service** (the token-issuing side). Construct with
/// [`AuthConfig::from_env`] in the composition root and hand to
/// [`AuthService::try_new`](crate::service::AuthService::try_new).
#[derive(Clone, Debug)]
pub struct AuthConfig {
	/// Issuer (`iss`) stamped on every minted token and required by verifiers.
	pub issuer: String,
	/// Audience for client access tokens (the hub data plane).
	pub client_audience: String,
	/// Audience of the hub's own inter-service bearer (the hub → signer token, minted offline
	/// by `examples/mint_service_token`). The hub never mints one in-process.
	pub service_audience: String,
	/// Audience of the tokens the scope listener issues to its mTLS clients. Distinct from
	/// [`service_audience`](Self::service_audience) on purpose: the signer accepts
	/// `banking-services` tokens, so a scope client's token must never carry it.
	pub scope_audience: String,
	/// Access-token TTL (seconds). Short by design (5–15 min).
	pub access_ttl_secs: u64,
	/// Refresh-token TTL (seconds) — the sliding window reset on each rotation.
	pub refresh_ttl_secs: u64,
	/// Absolute session lifetime (seconds): an immutable cap stamped at issue time,
	/// past which a family can no longer rotate regardless of how often it slides.
	pub max_session_secs: u64,
	/// Idle/inactivity timeout (seconds): a family is rejected once `now - last_seen`
	/// exceeds it. `0` disables the idle check (the absolute cap still applies).
	pub idle_timeout_secs: u64,
	/// TTL (seconds) of the hub's own service bearer. Only the offline example reads it.
	pub service_ttl_secs: u64,
	/// TTL (seconds) of the tokens the scope listener issues; [`SCOPE_TOKEN_TTL_BOUNDS`].
	pub scope_token_ttl_secs: u64,
	/// Signing/verification key material. `None` ⇒ auth disabled (dev/CI).
	pub signing: Option<SigningConfig>,
	/// Shared bearer the wallet BFF presents on `IssueUserToken` (the concierge→banking
	/// token-exchange seam). `None` ⇒ issuance is not configured and every call fails
	/// closed. Must match `BANKING_ISSUANCE_TOKEN` on the BFF side.
	pub issuance_token: Option<IssuanceToken>,
}
impl AuthConfig {
	pub fn from_env() -> color_eyre::Result<Self> {
		let signing = match (env::var("AUTH_SIGNING_KEY_PEM").ok(), env::var("AUTH_SIGNING_KID").ok(), env::var("AUTH_JWKS_JSON").ok()) {
			(Some(pem), Some(kid), Some(jwks)) if !pem.is_empty() && !kid.is_empty() && !jwks.is_empty() => Some(SigningConfig {
				signing_key_pem: pem,
				kid,
				jwks_json: jwks,
			}),
			_ => None,
		};
		let issuance_token = env::var("BANKING_ISSUANCE_TOKEN").ok().filter(|s| !s.is_empty()).map(IssuanceToken);
		let config = Self {
			issuer: env::var("AUTH_ISSUER").unwrap_or_else(|_| "https://auth.banking.ev".to_string()),
			client_audience: env::var("AUTH_CLIENT_AUDIENCE").unwrap_or_else(|_| "banking-core".to_string()),
			service_audience: env::var("AUTH_SERVICE_AUDIENCE").unwrap_or_else(|_| "banking-services".to_string()),
			scope_audience: env::var("AUTH_SCOPE_AUDIENCE").unwrap_or_else(|_| "banking-scope".to_string()),
			access_ttl_secs: parse_secs("AUTH_ACCESS_TTL_SECS", 900)?,
			refresh_ttl_secs: parse_secs("AUTH_REFRESH_TTL_SECS", 2_592_000)?,
			max_session_secs: parse_secs("AUTH_MAX_SESSION_SECS", 7_776_000)?,
			idle_timeout_secs: parse_secs("AUTH_IDLE_TIMEOUT_SECS", 0)?,
			service_ttl_secs: parse_secs("AUTH_SERVICE_TTL_SECS", 300)?,
			scope_token_ttl_secs: scope_token_ttl(env::var("AUTH_SCOPE_TOKEN_TTL_SECS").ok().as_deref())?,
			signing,
			issuance_token,
		};
		config.assert_plane()?;
		Ok(config)
	}

	/// Refuse an issuing config whose issuer or either audience does not carry this
	/// plane's identity — so banking can never mint (or verify) under concierge's
	/// issuer/audience even if launched from a shared environment that overrides the
	/// per-binary defaults.
	pub fn assert_plane(&self) -> color_eyre::Result<()> {
		assert_plane("AUTH_ISSUER", &self.issuer)?;
		assert_plane("AUTH_CLIENT_AUDIENCE", &self.client_audience)?;
		assert_plane("AUTH_SERVICE_AUDIENCE", &self.service_audience)?;
		assert_plane("AUTH_SCOPE_AUDIENCE", &self.scope_audience)?;
		// The three audiences are three trust domains: a scope client's token must not verify
		// as the hub's bearer (the signer) nor as a user's access token, whatever the env says.
		ensure!(
			self.scope_audience != self.service_audience && self.scope_audience != self.client_audience,
			"AUTH_SCOPE_AUDIENCE {:?} must differ from AUTH_SERVICE_AUDIENCE and AUTH_CLIENT_AUDIENCE",
			self.scope_audience
		);
		Ok(())
	}
}

/// The hub's own signing key plus the public JWKS it publishes and verifies against.
///
/// `Debug` is hand-written to redact the private key: the derived impl would print
/// the PKCS#8 PEM, so any future `tracing::debug!(?config)` or panic message that
/// captured the config would leak the token-signing key into logs/Sentry.
#[derive(Clone)]
pub struct SigningConfig {
	/// Ed25519 private key, PKCS#8 PEM (`AUTH_SIGNING_KEY_PEM`).
	pub signing_key_pem: String,
	/// Key id stamped in the JWT header and matched on verify (`AUTH_SIGNING_KID`).
	pub kid: String,
	/// Public JWKS as a JSON `{"keys":[...]}` (`AUTH_JWKS_JSON`) — served by the
	/// `Jwks` RPC and parsed into the verification key ring. Includes the active
	/// `kid` and any retired-but-still-valid keys (make-before-break rotation).
	pub jwks_json: String,
}
/// The shared issuance bearer the BFF presents on `IssueUserToken`. A newtype with a
/// hand-written `Debug` so the secret is never printed by a derived `AuthConfig` debug
/// (same footgun as [`SigningConfig`]).
#[derive(Clone)]
pub struct IssuanceToken(pub String);
impl IssuanceToken {
	pub fn as_str(&self) -> &str {
		&self.0
	}
}
/// Configuration a **downstream service** uses to build a [`Verifier`](crate::verifier::Verifier).
#[derive(Clone, Debug)]
pub struct VerifierConfig {
	/// Expected issuer (`iss`).
	pub issuer: String,
	/// Accepted audiences — a SET, not a single value, so the hub's own in-process
	/// verifier can accept the variety of audiences the hub itself issues.
	pub audiences: Vec<String>,
	/// Accepted token types (e.g. `[Access]` for a user-facing service).
	pub allowed_types: Vec<TokenType>,
	/// gRPC address of the hub auth service, dialed to refresh the JWKS cache.
	pub jwks_grpc_endpoint: String,
}
impl VerifierConfig {
	pub fn from_env() -> color_eyre::Result<Self> {
		let config = Self {
			issuer: env::var("AUTH_ISSUER").unwrap_or_else(|_| "https://auth.banking.ev".to_string()),
			audiences: split_csv(&env::var("AUTH_CLIENT_AUDIENCE").unwrap_or_else(|_| "banking-core".to_string())),
			allowed_types: vec![TokenType::Access],
			jwks_grpc_endpoint: env::var("AUTH_JWKS_GRPC_ENDPOINT").context("AUTH_JWKS_GRPC_ENDPOINT must be set for a downstream verifier")?,
		};
		config.assert_plane()?;
		Ok(config)
	}

	/// Refuse a verifier whose expected issuer or any accepted audience does not carry
	/// this plane's identity — so a downstream banking service can never be pointed at
	/// concierge's issuer/audience and accept concierge tokens. Callers that build a
	/// [`VerifierConfig`] by hand (not via [`from_env`](Self::from_env)) MUST invoke this
	/// at boot to get the same guard.
	pub fn assert_plane(&self) -> color_eyre::Result<()> {
		assert_plane("issuer", &self.issuer)?;
		for audience in &self.audiences {
			assert_plane("audience", audience)?;
		}
		Ok(())
	}
}

fn assert_plane(field: &str, value: &str) -> color_eyre::Result<()> {
	ensure!(
		value.contains(PLANE),
		"auth config {field} {value:?} does not carry this plane's identity ({PLANE:?}) — refusing to start with a cross-plane (or shared-environment) auth config"
	);
	Ok(())
}

impl std::fmt::Debug for SigningConfig {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("SigningConfig")
			.field("signing_key_pem", &"<redacted>")
			.field("kid", &self.kid)
			.field("jwks_json", &self.jwks_json)
			.finish()
	}
}

impl std::fmt::Debug for IssuanceToken {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("IssuanceToken(<redacted>)")
	}
}

/// Bounds, in seconds, of `AUTH_SCOPE_TOKEN_TTL_SECS`: long enough that a client is not
/// minting every few seconds, short enough that a leaked token is not a standing credential.
pub const SCOPE_TOKEN_TTL_BOUNDS: std::ops::RangeInclusive<u64> = 30..=900;

/// `AUTH_SCOPE_TOKEN_TTL_SECS`: unset or empty is 300; anything else must be an integer
/// inside [`SCOPE_TOKEN_TTL_BOUNDS`], so a typo (`0`, `3000000`) stops the boot instead of
/// minting tokens that are dead on arrival or live for weeks.
fn scope_token_ttl(raw: Option<&str>) -> color_eyre::Result<u64> {
	let Some(raw) = raw.filter(|v| !v.is_empty()) else {
		return Ok(300);
	};
	let ttl: u64 = raw
		.parse()
		.with_context(|| format!("AUTH_SCOPE_TOKEN_TTL_SECS must be an integer number of seconds, got {raw:?}"))?;
	ensure!(
		SCOPE_TOKEN_TTL_BOUNDS.contains(&ttl),
		"AUTH_SCOPE_TOKEN_TTL_SECS must be within {}..={} seconds, got {ttl}",
		SCOPE_TOKEN_TTL_BOUNDS.start(),
		SCOPE_TOKEN_TTL_BOUNDS.end()
	);
	Ok(ttl)
}

fn parse_secs(key: &str, default: u64) -> color_eyre::Result<u64> {
	match env::var(key) {
		Ok(v) if !v.is_empty() => v.parse().with_context(|| format!("{key} must be a positive integer (seconds)")),
		_ => Ok(default),
	}
}

fn split_csv(raw: &str) -> Vec<String> {
	raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use super::*;

	#[test]
	fn signing_config_debug_redacts_private_key() {
		let cfg = SigningConfig {
			signing_key_pem: "-----BEGIN PRIVATE KEY-----\nSUPERSECRETKEYMATERIAL\n-----END PRIVATE KEY-----".to_string(),
			kid: "kid-1".to_string(),
			jwks_json: r#"{"keys":[]}"#.to_string(),
		};
		let rendered = format!("{cfg:?}");
		assert!(!rendered.contains("SUPERSECRETKEYMATERIAL"), "Debug leaked the signing key: {rendered}");
		assert!(rendered.contains("<redacted>"));
		assert!(rendered.contains("kid-1"));
	}

	#[test]
	fn issuance_token_debug_redacts_the_secret() {
		let cfg = AuthConfig {
			issuance_token: Some(IssuanceToken("super-secret-bridge-issuance".to_string())),
			..issuing("https://auth.banking.ev", "banking-core", "banking-services")
		};
		let rendered = format!("{cfg:?}");
		assert!(!rendered.contains("super-secret-bridge-issuance"), "Debug leaked the issuance token: {rendered}");
		assert!(rendered.contains("<redacted>"));
	}

	fn issuing(issuer: &str, client_audience: &str, service_audience: &str) -> AuthConfig {
		AuthConfig {
			issuer: issuer.into(),
			client_audience: client_audience.into(),
			service_audience: service_audience.into(),
			scope_audience: "banking-scope".into(),
			access_ttl_secs: 900,
			refresh_ttl_secs: 2_592_000,
			max_session_secs: 7_776_000,
			idle_timeout_secs: 0,
			service_ttl_secs: 300,
			scope_token_ttl_secs: 300,
			signing: None,
			issuance_token: None,
		}
	}

	#[test]
	fn default_banking_config_passes_plane_check() {
		issuing("https://auth.banking.ev", "banking-core", "banking-services")
			.assert_plane()
			.expect("banking defaults carry the plane identity");
	}

	#[test]
	fn concierge_issuer_is_rejected() {
		let err = issuing("https://auth.concierge.ev", "banking-core", "banking-services")
			.assert_plane()
			.expect_err("a concierge issuer must be refused");
		assert!(err.to_string().contains("AUTH_ISSUER"), "error should name the offending field: {err}");
	}

	#[test]
	fn concierge_audience_is_rejected() {
		assert!(
			issuing("https://auth.banking.ev", "concierge", "banking-services").assert_plane().is_err(),
			"a concierge client audience must be refused"
		);
		assert!(
			issuing("https://auth.banking.ev", "banking-core", "concierge-services").assert_plane().is_err(),
			"a concierge service audience must be refused"
		);
	}

	#[test]
	fn the_scope_audience_carries_the_plane_and_differs_from_the_other_two() {
		let with = |scope: &str| AuthConfig {
			scope_audience: scope.into(),
			..issuing("https://auth.banking.ev", "banking-core", "banking-services")
		};
		with("banking-scope").assert_plane().expect("the default");
		assert!(with("concierge-scope").assert_plane().is_err(), "a concierge scope audience must be refused");
		assert!(with("banking-services").assert_plane().is_err(), "the signer's audience is not a scope client's");
		assert!(with("banking-core").assert_plane().is_err(), "nor is the user data plane's");
	}

	#[test]
	fn the_scope_token_ttl_is_bounded_and_defaults_to_300() {
		assert_eq!(scope_token_ttl(None).unwrap(), 300);
		assert_eq!(scope_token_ttl(Some("")).unwrap(), 300);
		assert_eq!((scope_token_ttl(Some("30")).unwrap(), scope_token_ttl(Some("900")).unwrap()), (30, 900));
		for refused in ["0", "29", "901", "3000000", "18446744073709551615", "18446744073709551616", "-1", "5m", " 300"] {
			assert!(scope_token_ttl(Some(refused)).is_err(), "{refused:?} must stop the boot");
		}
	}

	#[test]
	fn verifier_rejects_cross_plane_audience() {
		let cfg = VerifierConfig {
			issuer: "https://auth.banking.ev".into(),
			audiences: vec!["concierge".into()],
			allowed_types: vec![TokenType::Access],
			jwks_grpc_endpoint: "http://127.0.0.1:50052".into(),
		};
		assert!(cfg.assert_plane().is_err(), "a verifier pointed at a concierge audience must be refused");
	}

	// Drives the real boot path: a concierge-prefixed issuer in the environment must fail
	// `from_env` at startup. Serialized via a mutex + env restore because it mutates the
	// process environment (unsafe in edition 2024) shared by all tests in this binary.
	#[test]
	fn from_env_rejects_concierge_issuer_at_boot() {
		static ENV_GUARD: Mutex<()> = Mutex::new(());
		let _guard = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
		let prior = env::var("AUTH_ISSUER").ok();
		unsafe { env::set_var("AUTH_ISSUER", "https://auth.concierge.ev") };
		let result = AuthConfig::from_env();
		match prior {
			Some(v) => unsafe { env::set_var("AUTH_ISSUER", v) },
			None => unsafe { env::remove_var("AUTH_ISSUER") },
		}
		assert!(result.is_err(), "from_env must reject a concierge issuer for the banking plane");
	}
}
