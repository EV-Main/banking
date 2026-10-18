//! Token generation (signing).
//!
//! The host-only, auth-task-only minting side. A [`Signer`] holds the active
//! EdDSA private key and stamps the hub's claims; verification keys (the public
//! JWKS) are parsed separately by [`load_jwks`] into both a [`JwksCache`] (to
//! verify the hub's own tokens in-process) and the wire JWK list the `Jwks` RPC
//! publishes. Refresh tokens are NOT minted here — they are opaque handles owned
//! by [`management`](crate::management).

use std::{collections::HashMap, fmt};

use evbanking_contracts::banking::v1::Jwk;
use jsonwebtoken::{
	Algorithm, DecodingKey, EncodingKey, Header, encode, get_current_timestamp,
	jwk::{AlgorithmParameters, JwkSet},
};

use crate::{
	AuthError, Claims,
	claims::TokenType,
	config::{AuthConfig, SigningConfig},
	jwks::JwksCache,
};

/// Signs the hub's first-party access and service tokens with the active key.
///
/// The private key never appears in a `Debug` rendering: a leaked `Signer` in a log
/// or a panic message must not disclose key material.
pub struct Signer {
	encoding: EncodingKey,
	kid: String,
	issuer: String,
	client_audience: String,
	access_ttl_secs: u64,
	service_audience: String,
	service_ttl_secs: u64,
}
impl Signer {
	pub fn try_new(signing: &SigningConfig, config: &AuthConfig) -> Result<Self, AuthError> {
		let encoding = EncodingKey::from_ed_pem(signing.signing_key_pem.as_bytes()).map_err(|_| AuthError::NotConfigured)?;
		Ok(Self {
			encoding,
			kid: signing.kid.clone(),
			issuer: config.issuer.clone(),
			client_audience: config.client_audience.clone(),
			service_audience: config.service_audience.clone(),
			access_ttl_secs: config.access_ttl_secs,
			service_ttl_secs: config.service_ttl_secs,
		})
	}

	fn header(&self) -> Header {
		let mut header = Header::new(Algorithm::EdDSA);
		header.kid = Some(self.kid.clone());
		header
	}

	fn mint(&self, sub: &str, audience: &str, typ: TokenType, ttl_secs: u64, token_version: u64) -> Result<(String, u64), AuthError> {
		let now = get_current_timestamp();
		let exp = now + ttl_secs;
		let claims = Claims {
			sub: sub.to_owned(),
			iss: self.issuer.clone(),
			aud: audience.to_owned(),
			exp,
			iat: now,
			typ,
			jti: Some(uuid::Uuid::new_v4().to_string()),
			token_version,
		};
		let token = encode(&self.header(), &claims, &self.encoding).map_err(|_| AuthError::NotConfigured)?;
		Ok((token, exp))
	}

	/// Mint a client access token for a user. Returns `(token, exp_unix_secs)`.
	pub fn mint_access(&self, user_id: &str, token_version: u64) -> Result<(String, u64), AuthError> {
		self.mint(user_id, &self.client_audience, TokenType::Access, self.access_ttl_secs, token_version)
	}

	/// Mint an inter-service token for `service_name`. Returns `(token, exp_unix_secs)`.
	/// The one production caller is the scope listener's `ScopeAuthService.MintServiceToken`
	/// (via the auth task), which passes the client id its mTLS certificate maps to: `sub`
	/// is never a caller-supplied value, so keep it that way. No `token_version`: nothing
	/// stores a floor for a service; the short TTL and the certificate binding are the bound.
	pub fn mint_service(&self, service_name: &str) -> Result<(String, u64), AuthError> {
		self.mint(service_name, &self.service_audience, TokenType::Service, self.service_ttl_secs, 0)
	}
}

impl fmt::Debug for Signer {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Signer")
			.field("kid", &self.kid)
			.field("issuer", &self.issuer)
			.field("encoding", &"<redacted>")
			.finish_non_exhaustive()
	}
}

/// Parse the configured public JWKS into (a) a verification [`JwksCache`] and (b)
/// the wire JWK list served by the `Jwks` RPC. Only Ed25519 (OKP) keys are used.
pub fn load_jwks(signing: &SigningConfig) -> Result<(JwksCache, Vec<Jwk>), AuthError> {
	let set: JwkSet = serde_json::from_str(&signing.jwks_json).map_err(|e| AuthError::JwksFetch(format!("invalid AUTH_JWKS_JSON: {e}")))?;

	let mut keys = HashMap::new();
	let mut wire = Vec::new();
	for jwk in &set.keys {
		let kid = jwk.common.key_id.clone().ok_or_else(|| AuthError::JwksFetch("JWKS entry missing kid".into()))?;
		let AlgorithmParameters::OctetKeyPair(okp) = &jwk.algorithm else {
			continue; // only Ed25519 OKP keys are signing keys for this hub
		};
		let decoding = DecodingKey::from_ed_components(&okp.x).map_err(|e| AuthError::JwksFetch(format!("bad Ed25519 key {kid}: {e}")))?;
		keys.insert(kid.clone(), decoding);
		wire.push(Jwk {
			kid,
			kty: "OKP".to_string(),
			crv: "Ed25519".to_string(),
			x: okp.x.clone(),
			alg: "EdDSA".to_string(),
			r#use: "sig".to_string(),
		});
	}
	if keys.is_empty() {
		return Err(AuthError::JwksFetch("no Ed25519 keys in AUTH_JWKS_JSON".into()));
	}
	let mut cache = JwksCache::new();
	cache.replace(keys);
	Ok((cache, wire))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::jwks::{VerifyPolicy, verify_token};

	// A throwaway Ed25519 keypair, generated with `openssl genpkey -algorithm ed25519`.
	const TEST_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIKolOSMXwE+tafZkX+jkKYJbmJ066f4E12wAwTIkKps6\n-----END PRIVATE KEY-----\n";
	const TEST_JWK_X: &str = "Z6BCmq9-_wo9d7co5CDW84Wn0sAC3BA0XWK2AOstpV4";

	fn config() -> AuthConfig {
		AuthConfig {
			issuer: "https://auth.test".into(),
			client_audience: "banking-core".into(),
			service_audience: "banking-services".into(),
			access_ttl_secs: 900,
			refresh_ttl_secs: 3600,
			max_session_secs: 7_776_000,
			idle_timeout_secs: 0,
			service_ttl_secs: 300,
			signing: Some(SigningConfig {
				signing_key_pem: TEST_PEM.into(),
				kid: "test-kid".into(),
				jwks_json: format!(r#"{{"keys":[{{"kty":"OKP","crv":"Ed25519","x":"{TEST_JWK_X}","kid":"test-kid","alg":"EdDSA","use":"sig"}}]}}"#),
			}),
			issuance_token: None,
		}
	}

	fn policy(audience: &str, types: Vec<TokenType>) -> VerifyPolicy {
		VerifyPolicy {
			issuer: "https://auth.test".into(),
			audiences: vec![audience.into()],
			allowed_types: types,
		}
	}

	#[test]
	fn access_token_round_trips_and_enforces_aud_and_typ() {
		let cfg = config();
		let signing = cfg.signing.clone().unwrap();
		let signer = Signer::try_new(&signing, &cfg).unwrap();
		let (cache, wire) = load_jwks(&signing).unwrap();
		assert_eq!(wire.len(), 1);
		assert_eq!(wire[0].kid, "test-kid");

		let (token, exp) = signer.mint_access("user-123", 7).unwrap();
		assert!(exp > 0);

		let claims = verify_token(&token, &cache, &policy("banking-core", vec![TokenType::Access])).unwrap();
		assert_eq!(claims.sub, "user-123");
		assert_eq!(claims.token_version, 7);
		assert_eq!(claims.typ, TokenType::Access);

		// Wrong audience is rejected.
		assert!(verify_token(&token, &cache, &policy("someone-else", vec![TokenType::Access])).is_err());
		// A service-only policy rejects an access token (the typ separation).
		assert!(verify_token(&token, &cache, &policy("banking-core", vec![TokenType::Service])).is_err());
	}

	// FB-22 / BANK-ARCH-01, CROSS-1, BANK-COMM-5: the money plane must reject a token minted
	// by the concierge identity plane — distinct issuer AND audience — so the cabinet BFF
	// cannot move money with an identity token. The BFF holds two token pairs precisely
	// because this verification fails; the money-plane trust direction is exchange-based
	// (a banking-aud token), never "trust concierge's issuer". See docs/ARCHITECTURE.md.
	#[test]
	fn concierge_issued_token_is_rejected_by_the_money_plane() {
		let mut cfg = config();
		cfg.issuer = "https://auth.concierge.ev".into();
		cfg.client_audience = "concierge".into();
		let signing = cfg.signing.clone().unwrap();
		let concierge_signer = Signer::try_new(&signing, &cfg).unwrap();
		let (cache, _) = load_jwks(&signing).unwrap();

		let (concierge_token, _) = concierge_signer.mint_access("user-123", 1).unwrap();

		// The money plane verifies under the banking issuer + banking-core audience.
		let money_policy = VerifyPolicy {
			issuer: "https://auth.banking.ev".into(),
			audiences: vec!["banking-core".into(), "banking-services".into()],
			allowed_types: vec![TokenType::Access, TokenType::Service],
		};
		assert!(
			verify_token(&concierge_token, &cache, &money_policy).is_err(),
			"a concierge-issued identity token must NOT authorize the money plane"
		);

		// And the same token verifies fine under its OWN (concierge) policy — proving the
		// rejection above is the cross-plane boundary, not a malformed token.
		let concierge_policy = VerifyPolicy {
			issuer: "https://auth.concierge.ev".into(),
			audiences: vec!["concierge".into()],
			allowed_types: vec![TokenType::Access],
		};
		assert!(verify_token(&concierge_token, &cache, &concierge_policy).is_ok());
	}

	#[test]
	fn service_token_is_distinct_from_an_access_token() {
		let cfg = config();
		let signing = cfg.signing.clone().unwrap();
		let signer = Signer::try_new(&signing, &cfg).unwrap();
		let (cache, _) = load_jwks(&signing).unwrap();

		let (token, _) = signer.mint_service("allocations").unwrap();
		let claims = verify_token(&token, &cache, &policy("banking-services", vec![TokenType::Service])).unwrap();
		assert_eq!(claims.typ, TokenType::Service);
		assert_eq!(claims.sub, "allocations");

		// An access-only client policy must reject the service token.
		assert!(verify_token(&token, &cache, &policy("banking-services", vec![TokenType::Access])).is_err());
	}

	#[test]
	fn debug_redacts_the_private_key() {
		let cfg = config();
		let signing = cfg.signing.clone().unwrap();
		let signer = Signer::try_new(&signing, &cfg).unwrap();
		let rendered = format!("{signer:?}");
		assert!(rendered.contains("<redacted>"));
		assert!(!rendered.contains("PRIVATE KEY"));
	}
}
