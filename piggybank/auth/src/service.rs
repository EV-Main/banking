//! The auth service — a separate application run by `core`.
//!
//! Owns the signing keys, JWKS, and refresh store; serves the money-plane **issuance**
//! gRPC routes (`IssueUserToken`/`Refresh`/`Logout`/`ListSessions`/`RevokeSession`/
//! `Jwks`) on its own address; resolves the user it is minting for in-process over the
//! [`Provisioner`] channel; and answers `core`'s authorize requests over the
//! [`Authorizer`] channel. `core` builds it, takes
//! the `Authorizer`, hands it the `Provisioner`, and spawns [`AuthService::run`] in its
//! own task.
//!
//! This is the MONEY plane: it does NO third-party (Google) OAuth — sign-in lives wholly
//! in concierge, and users are mirrored here by the one-way bridge. `IssueUserToken` is the
//! concierge→banking seam, authenticated by a shared issuance token (not a user token).
//!
//! Unconfigured (no `AUTH_SIGNING_KEY_PEM`) it runs inert: issuance and authorize both
//! answer [`AuthError::NotConfigured`], so the scaffold still boots locally.

use std::{future::Future, net::SocketAddr, sync::Arc};

use color_eyre::eyre::{Context, eyre};
use evbanking_contracts::banking::v1::{
	IssueUserTokenRequest, JwksRequest, JwksResponse, ListSessionsRequest, ListSessionsResponse, LogoutRequest, LogoutResponse, RefreshRequest, RevokeSessionRequest, RevokeSessionResponse,
	Session, TokenResponse, UserSummary,
	auth_service_server::{AuthService as AuthServiceRpc, AuthServiceServer},
};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status, transport::Server};

use crate::{
	AuthError, Claims,
	authorizer::{AuthTask, Authorizer, TokenClass},
	claims::TokenType,
	config::AuthConfig,
	jwks::{JwksCache, VerifyPolicy, verify_token},
	management::{IssuedRefresh, RefreshInspect, RefreshStore, SessionBounds},
	provisioner::{ProvisionedUser, Provisioner},
	signer::{Signer, load_jwks},
};

/// The auth application: the authorize-channel receiver plus the issuance engine.
pub struct AuthService {
	authorize_rx: mpsc::Receiver<AuthTask>,
	grpc: AuthGrpc,
}

impl AuthService {
	/// Build the service from config and the [`Provisioner`] handle (core keeps the
	/// receiver). Returns the [`Authorizer`] handle for core to authorize requests.
	pub async fn try_new(config: AuthConfig, provisioner: Provisioner) -> color_eyre::Result<(Self, Authorizer)> {
		let (tx, rx) = mpsc::channel(1024);
		let grpc = AuthGrpc::build(config, provisioner).await?;
		Ok((Self { authorize_rx: rx, grpc }, Authorizer::new(tx)))
	}

	/// Run the auth task: serve issuance gRPC on `addr` and answer core's authorize
	/// requests over the in-process channel until `shutdown` fires (graceful: tonic
	/// drains in-flight issuance requests) or the channel closes.
	pub async fn run(self, addr: SocketAddr, shutdown: impl Future<Output = ()> + Send) -> color_eyre::Result<()> {
		let AuthService { mut authorize_rx, grpc } = self;
		let authorizer = grpc.clone();
		let issuance = Server::builder().add_service(AuthServiceServer::new(grpc)).serve_with_shutdown(addr, shutdown);
		let authorize = async move {
			while let Some(task) = authorize_rx.recv().await {
				// A dropped receiver means the caller went away; there is nobody to tell.
				match task {
					AuthTask::Authorize(request) => {
						let _ = request.respond_to.send(authorizer.authorize_token(&request.token, request.class));
					}
					AuthTask::MintService(request) => {
						let _ = request.respond_to.send(authorizer.mint_service_token(&request.sub));
					}
				}
			}
		};
		tokio::select! {
			result = issuance => result.context("auth issuance server error")?,
			() = authorize => {}
		}
		Ok(())
	}
}

/// The issuance engine, shared cheaply (Arc) between the gRPC server and the
/// in-process authorize loop.
#[derive(Clone)]
pub struct AuthGrpc {
	engine: Arc<AuthEngine>,
}
impl AuthGrpc {
	async fn build(config: AuthConfig, provisioner: Provisioner) -> color_eyre::Result<Self> {
		let (signer, keyring, jwks) = match &config.signing {
			Some(signing) => {
				let signer = Signer::try_new(signing, &config).map_err(|e| eyre!("auth signer init failed: {e}"))?;
				let (keyring, jwks) = load_jwks(signing).map_err(|e| eyre!("auth jwks load failed: {e}"))?;
				(Some(signer), keyring, jwks)
			}
			None => (None, JwksCache::new(), Vec::new()),
		};
		let issuance_token = config.issuance_token.as_ref().map(|t| Arc::from(t.as_str()));
		let client_policy = VerifyPolicy {
			issuer: config.issuer.clone(),
			audiences: vec![config.client_audience.clone()],
			allowed_types: vec![TokenType::Access],
		};
		let scope_policy = VerifyPolicy {
			issuer: config.issuer.clone(),
			audiences: vec![config.scope_audience.clone()],
			allowed_types: vec![TokenType::Service],
		};
		Ok(Self {
			engine: Arc::new(AuthEngine {
				signer,
				keyring,
				client_policy,
				scope_policy,
				issuance_token,
				refresh: RefreshStore::from_env().await?,
				provisioner,
				jwks,
				session_bounds: SessionBounds {
					ttl_secs: config.refresh_ttl_secs,
					max_session_secs: config.max_session_secs,
					idle_timeout_secs: config.idle_timeout_secs,
				},
			}),
		})
	}

	/// Verify a data-plane token in-process (called from core's `Authorizer` loop)
	/// against the policy for the mounting layer's [`TokenClass`] — the same
	/// cryptographic `aud`+`typ` separation downstream services pin, so a service
	/// token is rejected at the verifier on user-facing services rather than relying
	/// on `caller_id`'s incidental UUID parse.
	fn authorize_token(&self, token: &str, class: TokenClass) -> Result<Claims, AuthError> {
		let engine = &self.engine;
		if engine.signer.is_none() || engine.keyring.is_empty() {
			return Err(AuthError::NotConfigured);
		}
		let policy = match class {
			TokenClass::Client => &engine.client_policy,
			TokenClass::Service => &engine.scope_policy,
		};
		verify_token(token, &engine.keyring, policy)
	}

	/// Issue a service token for `sub`, the client the scope listener's transport proved.
	/// Inert (`NotConfigured`) without a signing key, like authorize.
	fn mint_service_token(&self, sub: &str) -> Result<(String, u64), AuthError> {
		self.engine.signer.as_ref().ok_or(AuthError::NotConfigured)?.mint_scope(sub)
	}

	/// Authenticate the service-to-service caller of `IssueUserToken` against the shared
	/// issuance token. An unconfigured token fails closed (`UNAVAILABLE`); a wrong/absent
	/// bearer is rejected. This seam is authenticated by the shared secret, NOT a user
	/// access token — banking trusts its own credential, never concierge's issuer.
	fn authenticate_issuer<T>(&self, request: &Request<T>) -> Result<(), Status> {
		let Some(expected) = self.engine.issuance_token.as_deref() else {
			return Err(Status::unavailable("issuance not configured"));
		};
		match bearer_token(request) {
			Some(presented) if constant_time_eq(presented.as_bytes(), expected.as_bytes()) => Ok(()),
			_ => Err(Status::unauthenticated("invalid issuance token")),
		}
	}
}

fn bearer_token<T>(request: &Request<T>) -> Option<String> {
	let value = request.metadata().get("authorization")?.to_str().ok()?;
	value.strip_prefix("Bearer ").map(str::to_owned)
}

/// Length-aware constant-time compare (length leak only, never a guessed value), so
/// issuance-token verification doesn't leak content via timing. Mirrors `management::ct_eq`.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
	a.len() == b.len() && a.ct_eq(b).into()
}

struct AuthEngine {
	signer: Option<Signer>,
	keyring: JwksCache,
	/// In-process authorize policy for user-facing data services: pins the client
	/// audience and `typ=access` only, so an inter-service token is rejected here.
	client_policy: VerifyPolicy,
	/// In-process authorize policy of [`TokenClass::Service`], i.e. the scope listener: pins
	/// the SCOPE audience (`banking-scope`) and `typ=service`. Deliberately not the hub's
	/// `banking-services`, which the signer accepts: a token minted for a scope client must
	/// not open the signer, and the hub's own bearer must not pass as a scope client's.
	scope_policy: VerifyPolicy,
	/// Shared bearer the BFF presents on `IssueUserToken` (the concierge→banking seam).
	/// `None` ⇒ issuance is not configured and every call fails closed.
	issuance_token: Option<Arc<str>>,
	refresh: RefreshStore,
	provisioner: Provisioner,
	jwks: Vec<evbanking_contracts::banking::v1::Jwk>,
	session_bounds: SessionBounds,
}

fn token_response(access_token: String, access_exp: u64, refresh: IssuedRefresh, summary: &ProvisionedUser) -> TokenResponse {
	TokenResponse {
		access_token,
		access_expires_at: access_exp as i64,
		refresh_token: refresh.token,
		refresh_expires_at: refresh.expires_at as i64,
		user: Some(UserSummary {
			user_id: summary.user_id.clone(),
			email: summary.email.clone(),
			status: summary.status.clone(),
			token_version: summary.token_version,
			// Empty on the money plane — the cabinet reads the role from concierge's login
			// summary; this field exists only for cross-plane wire parity.
			role: String::new(),
			// Same reason: the money plane has no emergency allowlist to report on.
			role_is_break_glass: false,
			// Parity only, like `role`.
			email_verified: false,
			username: String::new(),
		}),
	}
}

#[tonic::async_trait]
impl AuthServiceRpc for AuthGrpc {
	async fn issue_user_token(&self, request: Request<IssueUserTokenRequest>) -> Result<Response<TokenResponse>, Status> {
		// Service-to-service seam: authenticate the shared issuance token BEFORE anything
		// else, so an unauthenticated caller can neither mint nor probe internal state.
		self.authenticate_issuer(&request)?;
		let engine = &self.engine;
		let signer = engine.signer.as_ref().ok_or(AuthError::NotConfigured)?;
		let req = request.into_inner();

		// The user must already be mirrored locally by the one-way bridge (concierge owns
		// identity; banking never provisions on its own). A frozen/disabled user reads as
		// disabled and is refused a fresh money-plane token.
		let summary = engine.provisioner.resolve_for_issuance(req.concierge_user_id).await?;
		if summary.is_disabled() {
			return Err(Status::permission_denied("user is frozen or disabled"));
		}

		let (access_token, access_exp) = signer.mint_access(&summary.user_id, summary.token_version)?;
		let refresh = engine
			.refresh
			.issue(&summary.user_id, summary.token_version, engine.session_bounds, req.user_agent, req.ip)
			.await?;
		Ok(Response::new(token_response(access_token, access_exp, refresh, &summary)))
	}

	async fn refresh(&self, request: Request<RefreshRequest>) -> Result<Response<TokenResponse>, Status> {
		let engine = &self.engine;
		let signer = engine.signer.as_ref().ok_or(AuthError::NotConfigured)?;
		let req = request.into_inner();

		// Classify the presented handle WITHOUT rotating it, and run the fallible user
		// lookup BEFORE the irreversible rotation. Rotating first would advance `prev`, so a
		// transient lookup failure would make the client's retry (with the same, now
		// rotated-out token) trip reuse detection and revoke the whole family. Reuse
		// detection is preserved: a replayed rotated-out secret is caught here as `Reuse`
		// and revokes the family, exactly as the destructive rotate would.
		let user_id = match engine.refresh.inspect(&req.refresh_token).await? {
			RefreshInspect::Current { user_id } => user_id,
			RefreshInspect::Reuse { user_id } => {
				engine.refresh.revoke_user(&user_id).await?;
				return Err(AuthError::InvalidToken.into());
			}
			RefreshInspect::Invalid => return Err(AuthError::InvalidToken.into()),
		};

		let summary = engine.provisioner.lookup(user_id).await?;
		if summary.is_disabled() {
			engine.refresh.revoke_user(&summary.user_id).await?;
			return Err(Status::permission_denied("user is disabled"));
		}

		// The fallible checks passed — commit the (irreversible) rotation now.
		let rotated = engine.refresh.rotate(&req.refresh_token, engine.session_bounds).await?;
		// A "revoke all" since this family was issued bumps the authoritative token_version
		// in Postgres; refuse to mint and drop the family. (A pure comparison, so running it
		// after the rotation is safe — the family is dropped on mismatch regardless.)
		if summary.token_version > rotated.token_version_snapshot {
			engine.refresh.revoke_user(&summary.user_id).await?;
			return Err(Status::unauthenticated("tokens revoked"));
		}

		let (access_token, access_exp) = signer.mint_access(&summary.user_id, summary.token_version)?;
		Ok(Response::new(token_response(access_token, access_exp, rotated.refresh, &summary)))
	}

	async fn logout(&self, request: Request<LogoutRequest>) -> Result<Response<LogoutResponse>, Status> {
		let engine = &self.engine;
		let req = request.into_inner();
		// Authorize on the full credential (the secret), not the family-id prefix, so a
		// leaked/rotated-out token cannot force-logout a victim.
		let RefreshInspect::Current { user_id, .. } = engine.refresh.inspect(&req.refresh_token).await? else {
			return Err(AuthError::InvalidToken.into());
		};
		if req.revoke_all {
			// Durable half: bump the authoritative token_version in the control plane.
			// Best-effort — dropping the refresh families below already ends every
			// session and access tokens expire within the short TTL, so a transient
			// control-plane blip must not fail the logout.
			if let Err(err) = engine.provisioner.revoke_all(user_id.clone()).await {
				crate::telemetry::report(&err);
			}
			engine.refresh.revoke_user(&user_id).await?;
		} else {
			engine.refresh.revoke(&req.refresh_token).await?;
		}
		Ok(Response::new(LogoutResponse {}))
	}

	async fn list_sessions(&self, request: Request<ListSessionsRequest>) -> Result<Response<ListSessionsResponse>, Status> {
		let engine = &self.engine;
		let req = request.into_inner();
		// Authorize on the secret, not the family-id prefix — else a leaked handle would
		// disclose every session's device/IP metadata for the family.
		let RefreshInspect::Current { user_id, .. } = engine.refresh.inspect(&req.refresh_token).await? else {
			return Err(AuthError::InvalidToken.into());
		};
		let current_id = engine.refresh.family_id_of(&req.refresh_token).await?;
		let sessions = engine
			.refresh
			.list_for_user(&user_id)
			.await?
			.into_iter()
			.map(|s| Session {
				current: current_id.as_deref() == Some(s.id.as_str()),
				id: s.id,
				user_agent: s.user_agent,
				ip: s.ip,
				created_at: s.created_at as i64,
				last_seen: s.last_seen as i64,
			})
			.collect();
		Ok(Response::new(ListSessionsResponse { sessions }))
	}

	async fn revoke_session(&self, request: Request<RevokeSessionRequest>) -> Result<Response<RevokeSessionResponse>, Status> {
		let engine = &self.engine;
		let req = request.into_inner();
		// Authorize on the secret, not the family-id prefix — else a leaked handle could
		// revoke any of the victim's sessions (targeted DoS).
		let RefreshInspect::Current { user_id, .. } = engine.refresh.inspect(&req.refresh_token).await? else {
			return Err(AuthError::InvalidToken.into());
		};
		engine.refresh.revoke_by_id(&user_id, &req.session_id).await?;
		Ok(Response::new(RevokeSessionResponse {}))
	}

	async fn jwks(&self, _request: Request<JwksRequest>) -> Result<Response<JwksResponse>, Status> {
		Ok(Response::new(JwksResponse { keys: self.engine.jwks.clone() }))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{config::SigningConfig, provisioner::provisioner_channel};

	// Same throwaway Ed25519 keypair as the signer tests.
	const TEST_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIKolOSMXwE+tafZkX+jkKYJbmJ066f4E12wAwTIkKps6\n-----END PRIVATE KEY-----\n";
	const TEST_JWK_X: &str = "Z6BCmq9-_wo9d7co5CDW84Wn0sAC3BA0XWK2AOstpV4";

	async fn configured_grpc() -> AuthGrpc {
		grpc_with_issuance(None).await
	}

	async fn grpc_with_issuance(issuance_token: Option<&str>) -> AuthGrpc {
		grpc_with(issuance_token, 300, true).await
	}

	async fn grpc_with(issuance_token: Option<&str>, scope_token_ttl_secs: u64, signing: bool) -> AuthGrpc {
		let signing = signing.then(|| SigningConfig {
			signing_key_pem: TEST_PEM.into(),
			kid: "test-kid".into(),
			jwks_json: format!(r#"{{"keys":[{{"kty":"OKP","crv":"Ed25519","x":"{TEST_JWK_X}","kid":"test-kid","alg":"EdDSA","use":"sig"}}]}}"#),
		});
		let config = AuthConfig {
			issuer: "https://auth.test".into(),
			client_audience: "banking-core".into(),
			service_audience: "banking-services".into(),
			scope_audience: "banking-scope".into(),
			access_ttl_secs: 900,
			refresh_ttl_secs: 3600,
			max_session_secs: 7_776_000,
			idle_timeout_secs: 0,
			service_ttl_secs: 300,
			scope_token_ttl_secs,
			signing,
			issuance_token: issuance_token.map(|t| crate::config::IssuanceToken(t.to_string())),
		};
		let (provisioner, _rx) = provisioner_channel();
		AuthGrpc::build(config, provisioner).await.unwrap()
	}

	fn request_with_bearer(token: Option<&str>) -> Request<IssueUserTokenRequest> {
		let mut request = Request::new(IssueUserTokenRequest::default());
		if let Some(token) = token {
			request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
		}
		request
	}

	// The issuance seam is gated by the shared bearer: unconfigured fails closed
	// (UNAVAILABLE), a wrong/absent token is UNAUTHENTICATED, and the exact token passes
	// the auth check (reaching the resolver, which is dropped here → not Unauthenticated).
	#[tokio::test]
	async fn issue_user_token_requires_the_shared_issuance_token() {
		use tonic::Code;

		let unconfigured = grpc_with_issuance(None).await;
		assert_eq!(unconfigured.authenticate_issuer(&request_with_bearer(Some("anything"))).unwrap_err().code(), Code::Unavailable);

		let configured = grpc_with_issuance(Some("s3cret-issuance")).await;
		assert_eq!(configured.authenticate_issuer(&request_with_bearer(None)).unwrap_err().code(), Code::Unauthenticated);
		assert_eq!(configured.authenticate_issuer(&request_with_bearer(Some("wrong"))).unwrap_err().code(), Code::Unauthenticated);
		assert!(configured.authenticate_issuer(&request_with_bearer(Some("s3cret-issuance"))).is_ok());
	}

	// The hub's mounted authorize path — not just the verify policy in isolation — keeps the
	// principal classes apart in both directions: a client access token is rejected on the
	// service (scope) layer, a scope token on the client layer, and the hub's own
	// `banking-services` bearer (what the signer accepts) is no scope client's token, nor a
	// scope client's token the signer's.
	#[tokio::test]
	async fn authorize_token_separates_client_and_service_classes() {
		let grpc = configured_grpc().await;
		let signer = grpc.engine.signer.as_ref().unwrap();

		let (access, _) = signer.mint_access("00000000-0000-0000-0000-000000000001", 0).unwrap();
		let (hub, _) = signer.mint_hub_service("allocations").unwrap();
		let (scope, _) = grpc.mint_service_token("invest").unwrap();

		let client_claims = grpc.authorize_token(&access, TokenClass::Client).unwrap();
		assert!(client_claims.is_access());
		assert!(grpc.authorize_token(&hub, TokenClass::Client).is_err());
		assert!(grpc.authorize_token(&scope, TokenClass::Client).is_err());

		let scope_claims = grpc.authorize_token(&scope, TokenClass::Service).unwrap();
		assert_eq!(scope_claims.typ, TokenType::Service);
		assert!(grpc.authorize_token(&access, TokenClass::Service).is_err());
		assert!(grpc.authorize_token(&hub, TokenClass::Service).is_err(), "the hub's bearer is not a scope client's token");

		// And the signer's side of the same wall, with the policy `piggybank/signer` builds
		// from `AUTH_SERVICE_AUDIENCE`.
		let signer_policy = VerifyPolicy {
			issuer: "https://auth.test".into(),
			audiences: vec!["banking-services".into()],
			allowed_types: vec![TokenType::Service],
		};
		assert!(verify_token(&scope, &grpc.engine.keyring, &signer_policy).is_err(), "a scope token must not open the signer");
		assert!(verify_token(&hub, &grpc.engine.keyring, &signer_policy).is_ok());
	}

	// The scope listener's mint path: the token verifies on the SERVICE class only, carries
	// the `sub` it was asked for, and lives the configured TTL.
	#[tokio::test]
	async fn minted_service_token_verifies_on_the_service_class_with_the_configured_ttl() {
		let grpc = grpc_with(None, 120, true).await;
		let before = jsonwebtoken::get_current_timestamp();
		let (token, exp) = grpc.mint_service_token("invest").unwrap();
		assert!((before + 120..=before + 122).contains(&exp), "exp {exp} is now + AUTH_SCOPE_TOKEN_TTL_SECS");

		let claims = grpc.authorize_token(&token, TokenClass::Service).unwrap();
		assert_eq!((claims.sub.as_str(), claims.typ, claims.exp), ("invest", TokenType::Service, exp));
		assert!(grpc.authorize_token(&token, TokenClass::Client).is_err(), "a service token is no user token");
	}

	// Expiry is enforced on the service class too: a token past its `exp` (beyond the
	// verifier's leeway) is refused, however it was signed.
	#[tokio::test]
	async fn an_expired_service_token_is_refused() {
		use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

		let grpc = configured_grpc().await;
		let now = jsonwebtoken::get_current_timestamp();
		let claims = Claims {
			sub: "invest".into(),
			iss: "https://auth.test".into(),
			aud: "banking-services".into(),
			exp: now - 3600,
			iat: now - 3900,
			typ: TokenType::Service,
			jti: None,
			token_version: 0,
		};
		let mut header = Header::new(Algorithm::EdDSA);
		header.kid = Some("test-kid".into());
		let expired = encode(&header, &claims, &EncodingKey::from_ed_pem(TEST_PEM.as_bytes()).unwrap()).unwrap();
		assert!(grpc.authorize_token(&expired, TokenClass::Service).is_err());
	}

	// Without a signing key nothing is issued (dev/CI): the same inert answer as authorize.
	#[tokio::test]
	async fn minting_without_a_signing_key_is_not_configured() {
		let grpc = grpc_with(None, 300, false).await;
		assert!(matches!(grpc.mint_service_token("invest"), Err(AuthError::NotConfigured)));
	}
}
