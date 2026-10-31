//! The concierge half every route test stands up: a JWKS that publishes one throwaway key,
//! and a session cookie signed with it. Shared so a new concierge `AuthService` RPC is
//! stubbed once, not once per stub hub.

// `Status` is a large error type tonic mandates in handler signatures.
#![allow(clippy::result_large_err)]

use std::{collections::HashMap, sync::Arc};

use evconcierge_auth::{Claims, TokenType, Verifier, VerifierConfig};
use evconcierge_contracts::concierge::v1::{self as cc, auth_service_server::AuthService};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode, get_current_timestamp};
use tonic::{Request, Response, Status};

use crate::{
	config::AppConfig,
	cookies::CookieNames,
	session::BankingTokens,
	state::{AppState, Grpc},
	wallet_intents::{WalletActions, store::WalletStore},
};

pub const WALLET_KEY: &str = "test-wallet-intent-key-0123456789abcdef";

/// The env a route test boots from, with the hub at `hub` and concierge at `concierge`; a test
/// adds what it is about.
pub fn env(concierge: &str, hub: &str) -> HashMap<&'static str, String> {
	HashMap::from([
		("PIGGYBANK_GRPC_ADDR", hub.to_owned()),
		("BANKING_AUTH_GRPC_ADDR", hub.to_owned()),
		("CONCIERGE_GRPC_ADDR", concierge.to_owned()),
		("BANKING_ISSUANCE_TOKEN", "test-issuance".into()),
		("AUTH_ISSUER", ISSUER.into()),
		("AUTH_CLIENT_AUDIENCE", AUDIENCE.into()),
		("WALLET_INTENT_KEY", WALLET_KEY.into()),
		("APP_ENV", "development".into()),
	])
}

/// The state of a route test booted from `vars`, as `main` builds it.
pub fn state(vars: &HashMap<&'static str, String>, store: Arc<dyn WalletStore>) -> AppState {
	let config = AppConfig::from_source(|var| vars.get(var).cloned()).expect("the test env loads");
	config.check_wallet_actions().expect("the test env passes the boot check");
	let (concierge, hub) = (&config.concierge_grpc_addr, &config.piggybank_grpc_addr);
	let verifier = Verifier::try_new(VerifierConfig {
		issuer: ISSUER.into(),
		audiences: vec![AUDIENCE.into()],
		allowed_types: vec![TokenType::Access],
		jwks_grpc_endpoint: concierge.clone(),
	})
	.expect("build the verifier");
	AppState {
		cookies: Arc::new(CookieNames::new(config.cookie_secure())),
		banking: Arc::new(BankingTokens::new()),
		money_mutations: Arc::new(crate::routes::money_mutation_limiter()),
		verifier,
		grpc: Grpc::connect_lazy(hub, hub, concierge, Some("test-issuance".into())).expect("build the lazy channels"),
		wallet_actions: Arc::new(WalletActions::with_store(&config, store, concierge).expect("the wallet actions build")),
		config: Arc::new(config),
	}
}

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

/// [`access_token`] with a chosen `auth_time`.
pub fn access_token_at(auth_time: Option<u64>) -> String {
	access_token_for("user-1", auth_time)
}

/// [`access_token_at`] for another account.
pub fn access_token_for(sub: &str, auth_time: Option<u64>) -> String {
	let claims = Claims {
		sub: sub.into(),
		iss: ISSUER.into(),
		aud: AUDIENCE.into(),
		exp: get_current_timestamp() + 900,
		iat: get_current_timestamp(),
		typ: TokenType::Access,
		jti: None,
		token_version: 0,
		auth_time,
	};
	sign(&claims)
}

/// Any claims, signed with the key [`ConciergeJwks`] publishes — a step-up proof among them,
/// since concierge signs those with the access tokens' key.
pub fn sign<T: serde::Serialize>(claims: &T) -> String {
	sign_with_kid(claims, TEST_KID)
}

/// [`sign`] under a `kid` the JWKS may not publish.
pub fn sign_with_kid<T: serde::Serialize>(claims: &T, kid: &str) -> String {
	let mut header = Header::new(Algorithm::EdDSA);
	header.kid = Some(kid.into());
	encode(&header, claims, &EncodingKey::from_ed_pem(TEST_PEM.as_bytes()).unwrap()).expect("sign the test token")
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
