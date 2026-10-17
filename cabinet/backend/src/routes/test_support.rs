//! The concierge half every route test stands up: a JWKS that publishes one throwaway key,
//! and a session cookie signed with it. Shared so a new concierge `AuthService` RPC is
//! stubbed once, not once per stub hub.

// `Status` is a large error type tonic mandates in handler signatures.
#![allow(clippy::result_large_err)]

use evconcierge_auth::{Claims, TokenType};
use evconcierge_contracts::concierge::v1::{self as cc, auth_service_server::AuthService};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode, get_current_timestamp};
use tonic::{Request, Response, Status};

/// A throwaway Ed25519 keypair (`openssl genpkey -algorithm ed25519`) — the same one the
/// concierge verifier's own tests use. It signs nothing outside the tests.
const TEST_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIKolOSMXwE+tafZkX+jkKYJbmJ066f4E12wAwTIkKps6\n-----END PRIVATE KEY-----\n";
const TEST_JWK_X: &str = "Z6BCmq9-_wo9d7co5CDW84Wn0sAC3BA0XWK2AOstpV4";
const TEST_KID: &str = "test-kid";
pub const ISSUER: &str = "https://auth.test";
pub const AUDIENCE: &str = "concierge";

/// A valid `ev_access` cookie value for `user-1` — signed with the key [`ConciergeJwks`]
/// publishes.
pub fn access_token() -> String {
	access_token_at(Some(get_current_timestamp()))
}

/// [`access_token`] with a chosen `auth_time`, for the step-up gate.
pub fn access_token_at(auth_time: Option<u64>) -> String {
	let claims = Claims {
		sub: "user-1".into(),
		iss: ISSUER.into(),
		aud: AUDIENCE.into(),
		exp: get_current_timestamp() + 900,
		iat: get_current_timestamp(),
		typ: TokenType::Access,
		jti: None,
		token_version: 0,
		auth_time,
	};
	let mut header = Header::new(Algorithm::EdDSA);
	header.kid = Some(TEST_KID.into());
	encode(&header, &claims, &EncodingKey::from_ed_pem(TEST_PEM.as_bytes()).unwrap()).expect("sign the access token")
}

/// Concierge's `AuthService` as the BFF sees it: the only RPC it reaches is `Jwks` — the
/// verifier caches these keys and checks the `ev_access` cookie against them locally.
#[derive(Clone, Copy)]
pub struct ConciergeJwks;

fn not_reached<T>() -> Result<Response<T>, Status> {
	Err(Status::unimplemented("the BFF only reads the concierge JWKS"))
}

#[tonic::async_trait]
impl AuthService for ConciergeJwks {
	async fn jwks(&self, _: Request<cc::JwksRequest>) -> Result<Response<cc::JwksResponse>, Status> {
		Ok(Response::new(cc::JwksResponse {
			keys: vec![cc::Jwk {
				kid: TEST_KID.into(),
				kty: "OKP".into(),
				crv: "Ed25519".into(),
				x: TEST_JWK_X.into(),
				alg: "EdDSA".into(),
				r#use: "sig".into(),
			}],
		}))
	}

	async fn exchange_code(&self, _: Request<cc::ExchangeCodeRequest>) -> Result<Response<cc::ClientTokenResponse>, Status> {
		not_reached()
	}

	async fn refresh_client_token(&self, _: Request<cc::RefreshClientTokenRequest>) -> Result<Response<cc::ClientTokenResponse>, Status> {
		not_reached()
	}

	async fn refresh(&self, _: Request<cc::RefreshRequest>) -> Result<Response<cc::TokenResponse>, Status> {
		not_reached()
	}

	async fn logout(&self, _: Request<cc::LogoutRequest>) -> Result<Response<cc::LogoutResponse>, Status> {
		not_reached()
	}

	async fn list_sessions(&self, _: Request<cc::ListSessionsRequest>) -> Result<Response<cc::ListSessionsResponse>, Status> {
		not_reached()
	}

	async fn revoke_session(&self, _: Request<cc::RevokeSessionRequest>) -> Result<Response<cc::RevokeSessionResponse>, Status> {
		not_reached()
	}

	async fn publish_catalog(&self, _: Request<cc::PublishCatalogRequest>) -> Result<Response<cc::PublishCatalogResponse>, Status> {
		not_reached()
	}
}
