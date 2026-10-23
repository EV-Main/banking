//! Downstream token verifier — what *other* service repos use.
//!
//! Holds the hub's JWKS in a cache and verifies access/service tokens entirely
//! locally (no per-request round trip, no per-service token storage). The cache is
//! refreshed from the hub's `Jwks` gRPC RPC on construction and again on an
//! unknown-`kid` miss, so a key rotation heals without a restart. Plug it into
//! [`grpc_auth_layer`](crate::interceptor::grpc_auth_layer) to authorize inbound
//! gRPC, or call [`Verifier::verify`] directly.
//!
//! **Refresh is hardened against abuse.** The unknown-`kid` branch is reachable by
//! anyone with a syntactically valid header (no signature is checked first), so a
//! naive "refresh on every miss" would let forged tokens with random `kid`s
//! amplify into unbounded RPCs against the single central hub. Three guards prevent
//! that: a long-lived (lazy) channel rather than a dial per refresh; a minimum
//! interval between refreshes; and a single-flight lock so concurrent misses
//! collapse into one network call.

use std::{
	collections::HashMap,
	sync::Arc,
	time::{Duration, Instant},
};

use evbanking_contracts::banking::v1::{JwksRequest, auth_service_client::AuthServiceClient};
use jsonwebtoken::DecodingKey;
use tokio::sync::{Mutex, RwLock};
use tonic::transport::Channel;

use crate::{
	AuthError, Claims,
	config::VerifierConfig,
	interceptor::Authenticate,
	jwks::{JwksCache, VerifyPolicy, verify_token},
};

/// Don't hit the hub's `Jwks` RPC more than once per this window, so a flood of
/// forged unknown-`kid` tokens can't amplify into a DoS against the central hub.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Cap on establishing a TCP/TLS connection to the hub: a black-holed or half-open
/// hub must fail fast rather than wedge the awaiting verify task. Mirrors the
/// wallet BFF's upstream deadlines.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Per-RPC deadline on the `Jwks` refresh call. tonic has no default request
/// timeout, so without this a wedged hub would hold the single-flight lock — and
/// every verify queued behind it — indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A cheaply-cloneable handle for local token verification.
#[derive(Clone)]
pub struct Verifier {
	inner: Arc<Inner>,
}
impl Verifier {
	/// Build a verifier with an empty cache; the first verify (or any unknown-`kid`)
	/// fetches the JWKS. Fails only if the endpoint is not a valid URI.
	pub fn try_new(config: VerifierConfig) -> Result<Self, AuthError> {
		let channel = Channel::from_shared(config.jwks_grpc_endpoint.clone())
			.map_err(|e| AuthError::JwksFetch(format!("invalid jwks endpoint {}: {e}", config.jwks_grpc_endpoint)))?
			.connect_timeout(CONNECT_TIMEOUT)
			.timeout(REQUEST_TIMEOUT)
			.connect_lazy();
		Ok(Self {
			inner: Arc::new(Inner {
				cache: RwLock::new(JwksCache::new()),
				policy: VerifyPolicy {
					issuer: config.issuer,
					audiences: config.audiences,
					allowed_types: config.allowed_types,
				},
				client: AuthServiceClient::new(channel),
				last_refresh: Mutex::new(None),
			}),
		})
	}

	/// Build a verifier and warm its cache with an initial JWKS fetch, so the first
	/// real request does not pay the refresh latency.
	pub async fn connect(config: VerifierConfig) -> Result<Self, AuthError> {
		let verifier = Self::try_new(config)?;
		verifier.refresh().await?;
		Ok(verifier)
	}

	/// Verify a bearer token, refreshing the JWKS once if the `kid` is unknown
	/// (a key rotation) before giving up. The refresh is throttled + single-flighted,
	/// so a forged-`kid` flood costs at most one RPC per `MIN_REFRESH_INTERVAL`.
	pub async fn verify(&self, token: &str) -> Result<Claims, AuthError> {
		let first = {
			let cache = self.inner.cache.read().await;
			verify_token(token, &cache, &self.inner.policy)
		};
		match first {
			Err(AuthError::UnknownKid(_)) => {
				self.refresh().await?;
				let cache = self.inner.cache.read().await;
				verify_token(token, &cache, &self.inner.policy)
			}
			other => other,
		}
	}

	async fn refresh(&self) -> Result<(), AuthError> {
		// Single-flight: only one task refreshes at a time. Whoever waited then sees a
		// recent `last_refresh` and skips the redundant network call.
		let mut last_refresh = self.inner.last_refresh.lock().await;
		if let Some(at) = *last_refresh
			&& at.elapsed() < MIN_REFRESH_INTERVAL
		{
			return Ok(());
		}
		// Stamp the ATTEMPT before the RPC, so a FAILED refresh is throttled too. Otherwise
		// a flood of forged unknown-`kid` tokens would re-dial an already-degraded hub on
		// essentially every miss (the success-only throttle voids both guards during an
		// outage). A genuine key rotation heals at most `MIN_REFRESH_INTERVAL` later.
		*last_refresh = Some(Instant::now());

		let mut client = self.inner.client.clone();
		let response = client
			.jwks(JwksRequest {})
			.await
			.map_err(|e| AuthError::JwksFetch(format!("hub Jwks RPC failed: {e}")))?
			.into_inner();

		let mut keys = HashMap::new();
		for jwk in response.keys {
			if jwk.kty != "OKP" || jwk.crv != "Ed25519" {
				continue;
			}
			let key = DecodingKey::from_ed_components(&jwk.x).map_err(|e| AuthError::JwksFetch(format!("bad Ed25519 key {}: {e}", jwk.kid)))?;
			keys.insert(jwk.kid, key);
		}
		if keys.is_empty() {
			return Err(AuthError::JwksFetch("hub published no Ed25519 keys".into()));
		}
		self.inner.cache.write().await.replace(keys);
		Ok(())
	}
}

struct Inner {
	cache: RwLock<JwksCache>,
	policy: VerifyPolicy,
	/// A long-lived lazy channel to the hub auth service; reconnects under the hood.
	client: AuthServiceClient<Channel>,
	/// Single-flight + throttle guard: holds the last refresh ATTEMPT instant
	/// (`None` until the first). Held across the refresh so concurrent misses await
	/// one network call instead of each issuing their own.
	last_refresh: Mutex<Option<Instant>>,
}

impl Authenticate for Verifier {
	async fn authenticate(&self, token: String) -> Result<Claims, AuthError> {
		self.verify(&token).await
	}
}
