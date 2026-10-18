//! In-process authorization channel.
//!
//! `core` and the auth service run as separate tasks in one process. Rather than
//! re-implementing verification or round-tripping over the network, the auth
//! service hands `core` an [`Authorizer`] — a cloneable handle backed by an
//! in-process channel to the auth task (which owns the signing keys / JWKS).
//! `core`'s gRPC interceptor calls [`Authorizer::authorize`] per request.

use std::future::Future;

use tokio::sync::{mpsc, oneshot};

use crate::{AuthError, Claims};

/// Which principal class a mounted layer accepts, so the auth task applies the
/// matching verify policy. The hub keeps the two classes cryptographically apart
/// at the verifier (distinct `aud` + `typ`), not by incidental downstream parsing:
/// user-facing data services accept only [`Client`](TokenClass::Client) tokens; a
/// scope-client surface (the scope listener) accepts only [`Service`](TokenClass::Service).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenClass {
	/// A client access token (`aud=banking-core`, `typ=access`) — the user data plane.
	Client,
	/// A scope-listener client token (`aud=banking-scope`, `typ=service`) — not the hub's own `banking-services` bearer.
	Service,
}

/// A verification request sent from core's interceptor to the auth task.
pub struct AuthorizeRequest {
	/// The bearer access token taken from the gRPC request metadata.
	pub token: String,
	/// The principal class the mounting layer accepts; selects the verify policy.
	pub class: TokenClass,
	/// Where the auth task returns the verified claims (or an error).
	pub respond_to: oneshot::Sender<Result<Claims, AuthError>>,
}

/// A request to issue a service token, sent from core's scope listener to the auth task,
/// which owns the signing key.
pub struct MintServiceRequest {
	/// The `sub` of the token: the client id the caller's mTLS certificate maps to. Taken
	/// by core from the transport, never from anything the caller sent.
	pub sub: String,
	/// Where the auth task returns `(token, exp_unix_secs)` (or an error).
	pub respond_to: oneshot::Sender<Result<(String, u64), AuthError>>,
}

/// What core's in-process handle can ask the auth task: verify a token, or issue a
/// service token. One channel, so one task owns the key for both.
pub enum AuthTask {
	Authorize(AuthorizeRequest),
	MintService(MintServiceRequest),
}

/// Something that can issue a service token to a client the transport has identified.
pub trait MintService: Clone + Send + Sync + 'static {
	/// Issue a `typ=service` token with `sub` as given. Returns `(token, exp_unix_secs)`.
	/// The caller is responsible for `sub` being proven (a certificate), not supplied.
	fn mint_service(&self, sub: &str) -> impl Future<Output = Result<(String, u64), AuthError>> + Send;
}

/// Cloneable handle `core` holds to authorize gRPC requests in-process — no
/// network hop, no per-service key material. Bound to one [`TokenClass`] so the
/// layer it backs accepts only that principal class. Obtained from
/// [`AuthService::try_new`] (the client-class handle); derive the service-class
/// handle with [`Authorizer::for_class`].
///
/// [`AuthService::try_new`]: crate::service::AuthService::try_new
#[derive(Clone)]
pub struct Authorizer {
	tx: mpsc::Sender<AuthTask>,
	class: TokenClass,
}

impl Authorizer {
	pub(crate) fn new(tx: mpsc::Sender<AuthTask>) -> Self {
		Self { tx, class: TokenClass::Client }
	}

	/// A handle to the same auth task that authorizes against `class`'s policy. Mount
	/// the [`TokenClass::Service`] handle on inter-service surfaces only.
	pub fn for_class(&self, class: TokenClass) -> Self {
		Self { tx: self.tx.clone(), class }
	}

	/// Verify a bearer token by asking the auth task over the channel and awaiting
	/// its verdict. The auth task runs a real [`verify_token`](crate::jwks::verify_token)
	/// against the policy for this handle's [`TokenClass`] when signing is configured,
	/// and only answers [`AuthError::NotConfigured`] when no signing key is set (dev/CI).
	pub async fn authorize(&self, token: &str) -> Result<Claims, AuthError> {
		let (respond_to, response) = oneshot::channel();
		// A closed channel (send) or a dropped responder (recv) means the auth task
		// is unreachable — that is `Unavailable`, never `NotConfigured`. Only the
		// task's own verdict (the inner `?`) may report the flow as unconfigured.
		self.tx
			.send(AuthTask::Authorize(AuthorizeRequest {
				token: token.to_owned(),
				class: self.class,
				respond_to,
			}))
			.await
			.map_err(|_| AuthError::Unavailable)?;
		response.await.map_err(|_| AuthError::Unavailable)?
	}
}

impl crate::interceptor::Authenticate for Authorizer {
	async fn authenticate(&self, token: String) -> Result<Claims, AuthError> {
		self.authorize(&token).await
	}
}

impl MintService for Authorizer {
	async fn mint_service(&self, sub: &str) -> Result<(String, u64), AuthError> {
		let (respond_to, response) = oneshot::channel();
		self.tx
			.send(AuthTask::MintService(MintServiceRequest { sub: sub.to_owned(), respond_to }))
			.await
			.map_err(|_| AuthError::Unavailable)?;
		response.await.map_err(|_| AuthError::Unavailable)?
	}
}
