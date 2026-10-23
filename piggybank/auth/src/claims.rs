use serde::{Deserialize, Serialize};

/// The kind of first-party **JWT**, carried in the `typ` claim.
///
/// This keeps the two signed-token directions apart: a human-user `Access` token
/// can never stand in for an inter-service `Service` token (or vice versa) — even
/// before `aud` is checked. A verifier states the `typ`s it accepts in its
/// [`VerifyPolicy`](crate::jwks::VerifyPolicy). (Refresh tokens are **not** JWTs —
/// they are opaque, rotated, server-side handles owned by the `management`
/// module — so they have no `typ`.)
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenType {
	/// A short-TTL access token for an http client (the wallet host's user), scoped to
	/// the hub's data-plane audience.
	Access,
	/// An inter-service token: another backend authenticating its onward gRPC calls
	/// into the hub, scoped to the service audience.
	Service,
}

impl TokenType {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Access => "access",
			Self::Service => "service",
		}
	}
}

/// First-party token claims minted by the central auth service and verified
/// locally by every service against the published JWKS.
///
/// `sub` is the hub's canonical principal id — a user UUID for [`TokenType::Access`]
/// (never the upstream identity provider's `sub`), or a service name for [`TokenType::Service`]. It is a
/// plain `String` so this crate never needs to depend on `domain`; the hub parses
/// it into a typed id at the edge.
///
/// `token_version` lets the central service invalidate all of a principal's tokens
/// (a "revoke all" bumps the stored version). It is checked where the authoritative
/// value is reachable — at refresh time by the auth service — not by stateless
/// downstream verifiers, which rely on the short access-token TTL instead.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Claims {
	/// Subject — the principal id (user UUID or service name).
	pub sub: String,
	/// Issuer — the central auth service.
	pub iss: String,
	/// Audience — the service/group the token is scoped to.
	pub aud: String,
	/// Expiry (unix seconds). TTL is short (5–15 min for access tokens).
	pub exp: u64,
	/// Issued-at (unix seconds).
	#[serde(default)]
	pub iat: u64,
	/// Token kind — the access/refresh/service discriminator.
	pub typ: TokenType,
	/// Unique token id, for optional `jti` revocation and tracing.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub jti: Option<String>,
	/// Per-principal token version for coarse "revoke all" semantics.
	#[serde(default)]
	pub token_version: u64,
}

impl Claims {
	/// The subject parsed as a `Uuid`-shaped string is left to the caller; this is a
	/// convenience for the common "is this an access token for a user" guard.
	pub fn is_access(&self) -> bool {
		matches!(self.typ, TokenType::Access)
	}
}

// CROSS-PLANE PARITY CONTRACT — keep this block byte-identical in
// evbanking_auth and evconcierge_auth.
//
// The two planes are intentionally independent (separate repos, no shared crate),
// so the wire shape of `Claims`, `TokenType`, and `VerifyPolicy` is kept in lockstep
// by this assertion rather than by a shared type. A synthetic divergence — dropping,
// renaming, or adding a serialized field (the historic `jti` drift), or changing the
// `typ` discriminant strings — fails this test in whichever plane drifts, before two
// planes ever exchange a token.
#[cfg(test)]
mod parity {
	use super::*;

	fn full_claims() -> Claims {
		Claims {
			sub: "user-123".into(),
			iss: "https://auth.test".into(),
			aud: "plane".into(),
			exp: 1,
			iat: 1,
			typ: TokenType::Access,
			jti: Some("00000000-0000-0000-0000-000000000000".into()),
			token_version: 0,
		}
	}

	fn keys(value: &serde_json::Value) -> Vec<String> {
		let mut k: Vec<String> = value.as_object().expect("claims serialize to a JSON object").keys().cloned().collect();
		k.sort();
		k
	}

	#[test]
	fn claims_wire_field_set_is_canonical() {
		let value = serde_json::to_value(full_claims()).unwrap();
		assert_eq!(keys(&value), ["aud", "exp", "iat", "iss", "jti", "sub", "token_version", "typ"]);
	}

	#[test]
	fn jti_is_omitted_when_absent() {
		let mut claims = full_claims();
		claims.jti = None;
		let value = serde_json::to_value(&claims).unwrap();
		assert!(value.get("jti").is_none(), "jti must be skipped when None to stay byte-compatible");
		assert_eq!(keys(&value), ["aud", "exp", "iat", "iss", "sub", "token_version", "typ"]);
	}

	#[test]
	fn token_type_discriminants_are_stable() {
		assert_eq!(serde_json::to_value(TokenType::Access).unwrap(), serde_json::json!("access"));
		assert_eq!(serde_json::to_value(TokenType::Service).unwrap(), serde_json::json!("service"));
		assert_eq!(TokenType::Access.as_str(), "access");
		assert_eq!(TokenType::Service.as_str(), "service");
	}

	#[test]
	fn verify_policy_carries_issuer_audiences_and_allowed_types() {
		let policy = crate::jwks::VerifyPolicy {
			issuer: "https://auth.test".into(),
			audiences: vec!["plane".into()],
			allowed_types: vec![TokenType::Access, TokenType::Service],
		};
		assert_eq!(policy.issuer, "https://auth.test");
		assert_eq!(policy.audiences, ["plane"]);
		assert_eq!(policy.allowed_types, [TokenType::Access, TokenType::Service]);
	}
}
