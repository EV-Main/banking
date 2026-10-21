//! Verification of concierge's step-up proofs (`docs/STEPUP-PROOF.md` in concierge).
//!
//! The proof is signed with the key of the access tokens, but the access-token
//! [`Verifier`](evconcierge_auth::Verifier) keeps its JWKS private, so this holds its own
//! copy, fetched from the same `Jwks` RPC with the same guards: at most one fetch per
//! [`MIN_REFRESH_INTERVAL`], single-flight, and the attempt (not the success) stamped. The
//! unknown-`kid` branch is reachable before any signature is checked, so without the guards
//! a flood of forged `kid`s would become a flood of RPCs against concierge.

use std::time::{Duration, Instant};

use evconcierge_auth::{
	JwksCache,
	stepup::{Expected, StepupClaims, StepupError, verify_stepup_proof},
};
use evconcierge_contracts::concierge::v1::{JwksRequest, auth_service_client::AuthServiceClient};
use tokio::sync::{Mutex, RwLock};
use tonic::transport::Channel;

const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

pub struct ProofVerifier {
	issuer: String,
	client: AuthServiceClient<Channel>,
	cache: RwLock<JwksCache>,
	last_refresh: Mutex<Option<Instant>>,
}

impl ProofVerifier {
	/// A verifier with an empty cache: the first proof fetches the JWKS.
	pub fn new(issuer: String, jwks_endpoint: &str) -> Result<Self, String> {
		let channel = Channel::from_shared(jwks_endpoint.to_owned())
			.map_err(|e| format!("invalid jwks endpoint {jwks_endpoint}: {e}"))?
			.connect_timeout(Duration::from_secs(3))
			.timeout(Duration::from_secs(10))
			.connect_lazy();
		Ok(Self {
			issuer,
			client: AuthServiceClient::new(channel),
			cache: RwLock::new(JwksCache::new()),
			last_refresh: Mutex::new(None),
		})
	}

	/// [`verify_stepup_proof`] against the cached JWKS, refreshing it once on an unknown
	/// `kid` (a key rotation) when the throttle allows.
	pub async fn verify(&self, proof: &str, expected: &Expected<'_>) -> Result<StepupClaims, StepupError> {
		let first = verify_stepup_proof(proof, &*self.cache.read().await, &self.issuer, expected);
		match first {
			Err(StepupError::UnknownKid(_)) => {
				self.refresh().await;
				verify_stepup_proof(proof, &*self.cache.read().await, &self.issuer, expected)
			}
			other => other,
		}
	}

	async fn refresh(&self) {
		let mut last_refresh = self.last_refresh.lock().await;
		if last_refresh.is_some_and(|at| at.elapsed() < MIN_REFRESH_INTERVAL) {
			return;
		}
		*last_refresh = Some(Instant::now());
		let response = match self.client.clone().jwks(JwksRequest {}).await {
			Ok(response) => response.into_inner(),
			Err(status) => {
				tracing::warn!(code = ?status.code(), detail = %status.message(), "step-up JWKS refresh failed");
				return;
			}
		};
		match JwksCache::from_wire(&response.keys) {
			Ok(cache) if !cache.is_empty() => *self.cache.write().await = cache,
			Ok(_) => tracing::warn!("concierge published no Ed25519 keys; keeping the step-up JWKS as it was"),
			Err(error) => tracing::warn!(%error, "concierge published a key that does not parse; keeping the step-up JWKS as it was"),
		}
	}
}
