//! Mint an out-of-band SERVICE_TOKEN (the hub→signer bearer) from the issuing
//! env (`AUTH_SIGNING_KEY_PEM`/`AUTH_SIGNING_KID`, issuer/audience/TTL via the usual
//! `AUTH_*` vars; the audience is `AUTH_SERVICE_AUDIENCE`, the TTL `AUTH_SERVICE_TTL_SECS`).
//! Out of band on purpose: the scope listener's `MintServiceToken` serves its mTLS clients
//! with a different audience (`AUTH_SCOPE_AUDIENCE`), not this bearer — see
//! `service_token.rs`. Prints the JWT to stdout, its expiry to stderr.

use evbanking_auth::{AuthConfig, TokenType, claims::Claims};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode, get_current_timestamp};

fn main() -> color_eyre::Result<()> {
	color_eyre::install()?;
	let config = AuthConfig::from_env()?;
	let signing = config.signing.as_ref().expect("AUTH_SIGNING_KEY_PEM/AUTH_SIGNING_KID/AUTH_JWKS_JSON must be set");
	let now = get_current_timestamp();
	let claims = Claims {
		sub: "piggybank-hub".to_owned(),
		iss: config.issuer.clone(),
		aud: config.service_audience.clone(),
		exp: now.saturating_add(config.service_ttl_secs),
		iat: now,
		typ: TokenType::Service,
		jti: None,
		token_version: 0,
	};
	let mut header = Header::new(Algorithm::EdDSA);
	header.kid = Some(signing.kid.clone());
	let token = encode(&header, &claims, &EncodingKey::from_ed_pem(signing.signing_key_pem.as_bytes())?)?;
	eprintln!("exp: {}", claims.exp);
	println!("{token}");
	Ok(())
}
