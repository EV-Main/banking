//! FB-13: the hub↔signer seam is authenticated. The signer mounts the shared
//! `grpc_auth_layer` in front of `SignerService`, so it accepts ONLY the hub's service
//! token (`aud=banking-services`, `typ=service`) verified against the auth service's
//! JWKS. This proves the choke point end-to-end against the REAL stack (real auth layer,
//! real `Verifier`, real signer service over a real DB — no mocks):
//!   - no token            → UNAUTHENTICATED
//!   - a client-aud token  → UNAUTHENTICATED (wrong audience + typ)
//!   - a valid service tok → succeeds
//!
//! Runs when `SIGNER_DATABASE_URL`/`DATABASE_URL` is set; skips otherwise (the success
//! path provisions a real key, so it needs the signer's DB). A minimal in-process auth
//! server serves only the `Jwks` RPC (the verifier's sole dependency); tokens are minted
//! locally with a throwaway test key so the test is self-contained.

use crate::common;

use evbanking_auth::{Verifier, VerifierConfig, grpc_auth_layer};
use evbanking_contracts::{
	banking::v1::{
		Jwk, JwksResponse,
		auth_service_server::{AuthService, AuthServiceServer},
	},
	signer::v1::{ProvisionAddressRequest, signer_service_client::SignerServiceClient, signer_service_server::SignerServiceServer},
};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode, get_current_timestamp};
use piggybank_signer::{key_vault::Vault, secrets::WalletSecrets, service::Signer};
use serde::Serialize;
use tonic::{
	Request, Response, Status,
	transport::{Endpoint, Server},
};
use tower::Layer;
use uuid::Uuid;

// A throwaway Ed25519 keypair (the same public test vector the auth crate uses): a
// `openssl genpkey -algorithm ed25519` private key + its base64url public `x`. NOT a
// secret — it only signs tokens for this test's in-process auth server.
const TEST_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIKolOSMXwE+tafZkX+jkKYJbmJ066f4E12wAwTIkKps6\n-----END PRIVATE KEY-----\n";
const TEST_JWK_X: &str = "Z6BCmq9-_wo9d7co5CDW84Wn0sAC3BA0XWK2AOstpV4";
const TEST_KID: &str = "test-kid";
const ISSUER: &str = "https://auth.test";
const SERVICE_AUDIENCE: &str = "banking-services";
const CLIENT_AUDIENCE: &str = "banking-core";

#[derive(Serialize)]
struct TestClaims {
	sub: String,
	iss: String,
	aud: String,
	exp: u64,
	iat: u64,
	typ: String,
}

fn mint(audience: &str, typ: &str) -> String {
	let now = get_current_timestamp();
	let claims = TestClaims {
		sub: "test-caller".to_owned(),
		iss: ISSUER.to_owned(),
		aud: audience.to_owned(),
		exp: now + 300,
		iat: now,
		typ: typ.to_owned(),
	};
	let mut header = Header::new(Algorithm::EdDSA);
	header.kid = Some(TEST_KID.to_owned());
	let key = EncodingKey::from_ed_pem(TEST_PEM.as_bytes()).expect("test signing key");
	encode(&header, &claims, &key).expect("mint test token")
}

/// In-process auth server serving only the `Jwks` RPC the `Verifier` depends on; every
/// other route is unused on this path and answers `unimplemented`.
struct JwksOnlyAuth;

#[tonic::async_trait]
impl AuthService for JwksOnlyAuth {
	async fn jwks(&self, _request: Request<evbanking_contracts::banking::v1::JwksRequest>) -> Result<Response<JwksResponse>, Status> {
		Ok(Response::new(JwksResponse {
			keys: vec![Jwk {
				kid: TEST_KID.to_owned(),
				kty: "OKP".to_owned(),
				crv: "Ed25519".to_owned(),
				x: TEST_JWK_X.to_owned(),
				alg: "EdDSA".to_owned(),
				r#use: "sig".to_owned(),
			}],
		}))
	}

	async fn issue_user_token(&self, _r: Request<evbanking_contracts::banking::v1::IssueUserTokenRequest>) -> Result<Response<evbanking_contracts::banking::v1::TokenResponse>, Status> {
		Err(Status::unimplemented("jwks-only"))
	}

	async fn refresh(&self, _r: Request<evbanking_contracts::banking::v1::RefreshRequest>) -> Result<Response<evbanking_contracts::banking::v1::TokenResponse>, Status> {
		Err(Status::unimplemented("jwks-only"))
	}

	async fn logout(&self, _r: Request<evbanking_contracts::banking::v1::LogoutRequest>) -> Result<Response<evbanking_contracts::banking::v1::LogoutResponse>, Status> {
		Err(Status::unimplemented("jwks-only"))
	}

	async fn list_sessions(&self, _r: Request<evbanking_contracts::banking::v1::ListSessionsRequest>) -> Result<Response<evbanking_contracts::banking::v1::ListSessionsResponse>, Status> {
		Err(Status::unimplemented("jwks-only"))
	}

	async fn revoke_session(&self, _r: Request<evbanking_contracts::banking::v1::RevokeSessionRequest>) -> Result<Response<evbanking_contracts::banking::v1::RevokeSessionResponse>, Status> {
		Err(Status::unimplemented("jwks-only"))
	}
}

fn ephemeral_addr() -> std::net::SocketAddr {
	std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port").local_addr().expect("local addr")
}

fn authorized(token: &str) -> Request<ProvisionAddressRequest> {
	let mut request = Request::new(ProvisionAddressRequest {
		user_id: Uuid::new_v4().to_string(),
		network: "ton".to_owned(),
	});
	request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().expect("ascii token"));
	request
}

#[tokio::test]
async fn seam_rejects_unauthenticated_and_foreign_aud_accepts_service_token() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping signer seam-auth test");
		return;
	};
	let pool = db.pool.clone();

	// Stand up the in-process auth server (Jwks) and the real auth-layered signer server,
	// then drive assertions — all as branches of one `select!` (no detached tasks). The
	// server futures never resolve; the test ends when the assertion branch returns.
	let auth_addr = ephemeral_addr();
	let signer_addr = ephemeral_addr();
	let auth_server = Server::builder().add_service(AuthServiceServer::new(JwksOnlyAuth)).serve(auth_addr);

	let vault = Vault::from_hex(&hex::encode([9u8; 32])).unwrap();
	let signer = Signer::new(vault, WalletSecrets::new(pool), piggybank_signer::policy::SignerPolicy::default());
	let verifier = Verifier::try_new(VerifierConfig {
		issuer: ISSUER.to_owned(),
		audiences: vec![SERVICE_AUDIENCE.to_owned()],
		allowed_types: vec![evbanking_auth::TokenType::Service],
		jwks_grpc_endpoint: format!("http://{auth_addr}"),
	})
	.expect("verifier");
	let auth = grpc_auth_layer(verifier);
	let signer_server = Server::builder().add_service(auth.layer(SignerServiceServer::new(signer))).serve(signer_addr);

	let channel = Endpoint::from_shared(format!("http://{signer_addr}")).expect("endpoint").connect_lazy();

	tokio::select! {
		result = auth_server => result.expect("serve auth"),
		result = signer_server => result.expect("serve signer"),
		() = assert_seam(SignerServiceClient::new(channel)) => {}
	}

	db.cleanup().await;
}

async fn assert_seam(mut client: SignerServiceClient<tonic::transport::Channel>) {
	// No token → UNAUTHENTICATED.
	let no_token = client
		.provision_address(ProvisionAddressRequest {
			user_id: Uuid::new_v4().to_string(),
			network: "ton".to_owned(),
		})
		.await;
	assert_eq!(no_token.unwrap_err().code(), tonic::Code::Unauthenticated, "an unauthenticated call must be rejected");

	// A client-audience access token → UNAUTHENTICATED (wrong aud + typ for this seam).
	let foreign = client.provision_address(authorized(&mint(CLIENT_AUDIENCE, "access"))).await;
	assert_eq!(foreign.unwrap_err().code(), tonic::Code::Unauthenticated, "a foreign-audience token must be rejected");

	// A valid service token → the call is authorized and provisions an address.
	let response = client
		.provision_address(authorized(&mint(SERVICE_AUDIENCE, "service")))
		.await
		.expect("service token authorizes the seam");
	assert!(!response.into_inner().address.is_empty(), "an authenticated service-token call succeeds");
}
