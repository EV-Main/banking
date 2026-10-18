//! Service-token source for downstream onward calls.
//!
//! A downstream service that itself calls *into* the hub authenticates those gRPC
//! calls with a [`TokenType::Service`](crate::claims::TokenType) token whose `aud`
//! is the hub's service audience — distinct from a user's access token, so the two
//! can never be confused. For this slice the token is provided out-of-band (config
//! / env); a scope-listener client obtains it from `scope.v1.ScopeAuthService.MintServiceToken`
//! (mTLS only, on that listener).

use std::{env, sync::Arc};

use tonic::{Request, metadata::MetadataValue};

/// Holds the service token a downstream attaches to its onward requests.
#[derive(Clone)]
pub struct ServiceTokenSource {
	token: Arc<str>,
}

impl ServiceTokenSource {
	pub fn new(token: impl Into<String>) -> Self {
		Self { token: Arc::from(token.into()) }
	}

	/// Build from `SERVICE_TOKEN`, if set.
	pub fn from_env() -> Option<Self> {
		env::var("SERVICE_TOKEN").ok().filter(|s| !s.is_empty()).map(Self::new)
	}

	pub fn token(&self) -> &str {
		&self.token
	}

	/// Attach the service token as the `authorization: Bearer …` metadata on an
	/// outgoing tonic request. Returns the request so it composes in a builder
	/// chain. A malformed token (non-ASCII) is dropped rather than panicking.
	pub fn authorize<T>(&self, mut request: Request<T>) -> Request<T> {
		if let Ok(value) = MetadataValue::try_from(format!("Bearer {}", self.token)) {
			request.metadata_mut().insert("authorization", value);
		}
		request
	}
}
