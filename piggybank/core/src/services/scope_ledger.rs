//! `ScopeLedgerService` (`scope/v1/ledger.proto`) — the service-facing money surface, on a
//! listener of its own.
//!
//! WHO CALLS. Another backend, never a browser: the listener speaks only HTTP/2 over mTLS
//! (no `accept_http1`, no gRPC-Web), every client certificate must chain to the configured
//! CA, and two layers stand in front of the service:
//!   - [`ClientIdentityLayer`] maps the certificate the client presented to a [`ClientId`]
//!     by its fingerprint ([`ClientDirectory`]) — the ONLY source of the caller's identity.
//!     Nothing in the request, the token's `sub` included, can name another client.
//!   - the auth layer pinned to [`TokenClass::Service`] (`aud=banking-services`,
//!     `typ=service`), so a user's access token is refused at the verifier.
//!
//! Part 1 serves `OpenAccount`, `GetBalances` and `Transfer`; every other RPC is
//! UNIMPLEMENTED with the reason, and a client must treat that as "not yet", not "never".
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large type we
//! don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use std::{
	future::Future,
	net::SocketAddr,
	pin::Pin,
	sync::Arc,
	task::{Context, Poll},
};

use domain::{
	money::Usdt,
	scope_ledger::{ClientDirectory, ClientId, Grants, SubjectRef, TransferReason},
	scopes::{ScopeAccountLabel, ScopeId},
};
use evbanking_auth::{Authenticate, Authorizer, TokenClass, TokenType, claims_of, grpc_auth_layer};
use evbanking_contracts::scope::v1::{
	self as pb,
	scope_ledger_service_server::{ScopeLedgerService, ScopeLedgerServiceServer},
};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tonic::{
	Request, Response, Status,
	body::Body,
	codegen::http,
	transport::{
		Certificate, Identity, Server, ServerTlsConfig,
		server::{TcpConnectInfo, TlsConnectInfo},
	},
};
use tower::{Layer, Service};
use tower_http::trace::{DefaultOnFailure, TraceLayer};

use crate::{
	application::scope_ledger::{self as ledger_app, AccountView, ScopeLedgerPorts, TransferError, TransferRequest},
	config::ScopeLedgerSettings,
	ports::{
		compartments::CompartmentStore,
		ledger::Ledger,
		scope_ledger::{RecordedTransfer, ScopeLedgerStore},
	},
	services::support::map_err,
};

/// The client the transport proved, put in the request's extensions by
/// [`ClientIdentityLayer`].
#[derive(Clone, Debug)]
struct CallerClient(ClientId);

/// Maps the presented client certificate to a [`ClientId`], or answers UNAUTHENTICATED.
#[derive(Clone)]
pub struct ClientIdentityLayer {
	clients: Arc<ClientDirectory>,
}

impl ClientIdentityLayer {
	pub fn new(clients: Arc<ClientDirectory>) -> Self {
		Self { clients }
	}
}

impl<S> Layer<S> for ClientIdentityLayer {
	type Service = ClientIdentity<S>;

	fn layer(&self, inner: S) -> Self::Service {
		ClientIdentity {
			inner,
			clients: self.clients.clone(),
		}
	}
}

/// The service produced by [`ClientIdentityLayer`].
#[derive(Clone)]
pub struct ClientIdentity<S> {
	inner: S,
	clients: Arc<ClientDirectory>,
}

/// The fingerprint a grants file pins: SHA-256 of the certificate's DER bytes, lowercase hex.
fn fingerprint(der: &[u8]) -> String {
	hex::encode(Sha256::digest(der))
}

impl<S, B> Service<http::Request<B>> for ClientIdentity<S>
where
	S: Service<http::Request<B>, Response = http::Response<Body>, Error = std::convert::Infallible> + Clone + Send + 'static,
	S::Future: Send + 'static,
	B: Send + 'static,
{
	type Error = std::convert::Infallible;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
	type Response = http::Response<Body>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
		// Ready-clone, as the auth layer does: call the instance that was polled ready.
		let clone = self.inner.clone();
		let mut inner = std::mem::replace(&mut self.inner, clone);
		// The leaf is the first certificate the client sent; the CA check already happened in
		// the handshake, so what is left is which of the trusted clients this one is.
		let client = req
			.extensions()
			.get::<TlsConnectInfo<TcpConnectInfo>>()
			.and_then(TlsConnectInfo::peer_certs)
			.and_then(|certs| certs.first().map(|leaf| fingerprint(leaf.as_ref())))
			.and_then(|print| self.clients.client_for(&print).cloned());
		Box::pin(async move {
			match client {
				Some(client) => {
					req.extensions_mut().insert(CallerClient(client));
					inner.call(req).await
				}
				None => {
					let (parts, ()) = Status::unauthenticated("client certificate is not a known client").into_http::<()>().into_parts();
					Ok(http::Response::from_parts(parts, Body::empty()))
				}
			}
		})
	}
}

// Preserve the wrapped service's gRPC name so the tonic router can dispatch to it.
impl<S: tonic::server::NamedService> tonic::server::NamedService for ClientIdentity<S> {
	const NAME: &'static str = S::NAME;
}

/// The driven ports the scope ledger serves from, wired by the composition root.
#[derive(Clone)]
pub struct ScopeLedgerDeps {
	pub store: Arc<dyn ScopeLedgerStore>,
	/// The scope registry (which scopes exist, their state and policy).
	pub scopes: Arc<dyn CompartmentStore>,
	pub ledger: Arc<dyn Ledger>,
	/// Nudged after a transfer commits so the relay posts it at once.
	pub relay: Arc<Notify>,
}

/// Everything the listener needs: the settings checked at boot ([`ScopeLedgerSettings`]) and
/// the ports the composition root wired.
pub struct ScopeLedgerListener {
	pub bind: SocketAddr,
	tls: ServerTlsConfig,
	clients: Arc<ClientDirectory>,
	service: ScopeLedgerSvc,
}

impl ScopeLedgerListener {
	pub fn new(settings: ScopeLedgerSettings, deps: ScopeLedgerDeps) -> Self {
		Self {
			bind: settings.bind,
			// A client certificate is required: `client_auth_optional` stays false.
			tls: ServerTlsConfig::new()
				.identity(Identity::from_pem(settings.cert_pem, settings.key_pem))
				.client_ca_root(Certificate::from_pem(settings.client_ca_pem)),
			clients: Arc::new(settings.clients),
			service: ScopeLedgerSvc {
				grants: Arc::new(settings.grants),
				deps,
			},
		}
	}
}

/// Serve the scope ledger on its own listener until `shutdown` fires, authorizing every call
/// against the hub's SERVICE token class.
pub async fn serve(listener: ScopeLedgerListener, authorizer: &Authorizer, shutdown: impl Future<Output = ()>) -> Result<(), tonic::transport::Error> {
	serve_with(listener, authorizer.for_class(TokenClass::Service), shutdown).await
}

/// [`serve`] over any authenticator — the seam the listener's tests drive. Both layers are
/// mounted here and nowhere else, so what a test proves about this function is what
/// production runs.
pub async fn serve_with<A: Authenticate>(listener: ScopeLedgerListener, authenticator: A, shutdown: impl Future<Output = ()>) -> Result<(), tonic::transport::Error> {
	let auth = grpc_auth_layer(authenticator);
	let identity = ClientIdentityLayer::new(listener.clients);
	let service = listener.service;
	// No `accept_http1` and no `GrpcWebLayer`: nothing a browser can speak reaches this port.
	Server::builder()
		.tls_config(listener.tls)?
		.layer(TraceLayer::new_for_grpc().on_failure(DefaultOnFailure::new().level(tracing::Level::WARN)))
		.add_service(auth.layer(identity.layer(ScopeLedgerServiceServer::new(service))))
		.serve_with_shutdown(listener.bind, shutdown)
		.await
}

#[derive(Clone)]
struct ScopeLedgerSvc {
	grants: Arc<Grants>,
	deps: ScopeLedgerDeps,
}

impl ScopeLedgerSvc {
	fn ports(&self) -> ScopeLedgerPorts<'_> {
		ScopeLedgerPorts {
			grants: &self.grants,
			store: self.deps.store.as_ref(),
			scopes: self.deps.scopes.as_ref(),
			ledger: self.deps.ledger.as_ref(),
			relay: &self.deps.relay,
		}
	}
}

/// The caller, as the two layers proved it: a service token, and the client its certificate
/// names. Either missing means the service was reached without its layers — refused, never
/// served as nobody.
fn caller<T>(request: &Request<T>) -> Result<ClientId, Status> {
	let claims = claims_of(request).ok_or_else(|| Status::unauthenticated("missing claims"))?;
	if claims.typ != TokenType::Service {
		return Err(Status::permission_denied("service token required"));
	}
	request
		.extensions()
		.get::<CallerClient>()
		.map(|caller| caller.0.clone())
		.ok_or_else(|| Status::unauthenticated("no client identity"))
}

/// The refusal's reason is the whole message: clients branch on (code, message).
fn refuse(err: TransferError) -> Status {
	match err {
		TransferError::Refused(refusal) if refusal.is_permission() => Status::permission_denied(refusal.reason()),
		TransferError::Refused(refusal) => Status::failed_precondition(refusal.reason()),
		TransferError::Domain(err) => map_err(err),
	}
}

fn scope_of(raw: &str) -> Result<ScopeId, Status> {
	ScopeId::parse(raw).map_err(map_err)
}

fn label_of(raw: &str) -> Result<ScopeAccountLabel, Status> {
	ScopeAccountLabel::parse(raw).map_err(map_err)
}

/// What a part-1 server answers for an RPC it does not serve yet.
fn not_yet(rpc: &str, why: &str) -> Status {
	Status::unimplemented(format!("{rpc} is not served yet: {why}"))
}

const NEEDS_MANDATES: &str = "it draws on users' mandates, which are not granted yet";
const NEEDS_APPROVALS: &str = "banking does not open approvals for scope operations yet";

#[tonic::async_trait]
impl ScopeLedgerService for ScopeLedgerSvc {
	async fn open_account(&self, request: Request<pb::OpenAccountRequest>) -> Result<Response<pb::ScopeAccount>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let scope = scope_of(&req.scope)?;
		let label = label_of(&req.label)?;
		let view = ledger_app::open_account(&self.ports(), &client, &scope, &label, &req.idempotency_key).await.map_err(refuse)?;
		Ok(Response::new(account_to_proto(&view)))
	}

	async fn reserve(&self, request: Request<pb::ReserveRequest>) -> Result<Response<pb::Hold>, Status> {
		caller(&request)?;
		Err(not_yet("Reserve", NEEDS_MANDATES))
	}

	async fn capture(&self, request: Request<pb::CaptureRequest>) -> Result<Response<pb::Hold>, Status> {
		caller(&request)?;
		Err(not_yet("Capture", "there are no holds yet"))
	}

	async fn void(&self, request: Request<pb::VoidRequest>) -> Result<Response<pb::Hold>, Status> {
		caller(&request)?;
		Err(not_yet("Void", "there are no holds yet"))
	}

	async fn charge(&self, request: Request<pb::ChargeRequest>) -> Result<Response<pb::ScopeCharge>, Status> {
		caller(&request)?;
		Err(not_yet("Charge", NEEDS_MANDATES))
	}

	async fn payout(&self, request: Request<pb::PayoutRequest>) -> Result<Response<pb::ScopePayout>, Status> {
		caller(&request)?;
		Err(not_yet("Payout", "payouts into users' compartments come with the compartments grant"))
	}

	async fn transfer(&self, request: Request<pb::TransferRequest>) -> Result<Response<pb::ScopeTransfer>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let reason = match pb::TransferReason::try_from(req.reason) {
			Ok(pb::TransferReason::FeeSweep) => TransferReason::FeeSweep,
			Ok(pb::TransferReason::Unspecified) | Err(_) => return Err(Status::invalid_argument("reason is required")),
		};
		let parsed = TransferRequest {
			scope: scope_of(&req.scope)?,
			from: label_of(&req.from_label)?,
			to: label_of(&req.to_label)?,
			amount: Usdt::parse_decimal(&req.amount).map_err(map_err)?,
			reason,
			subject_ref: SubjectRef::parse(&req.subject_ref).map_err(map_err)?,
			memo: req.memo,
			idempotency_key: req.idempotency_key,
		};
		let recorded = ledger_app::transfer(&self.ports(), &client, parsed).await.map_err(refuse)?;
		Ok(Response::new(transfer_to_proto(&recorded)))
	}

	async fn request_payout(&self, request: Request<pb::RequestPayoutRequest>) -> Result<Response<pb::PendingOperation>, Status> {
		caller(&request)?;
		Err(not_yet("RequestPayout", NEEDS_APPROVALS))
	}

	async fn request_transfer(&self, request: Request<pb::RequestTransferRequest>) -> Result<Response<pb::PendingOperation>, Status> {
		caller(&request)?;
		Err(not_yet("RequestTransfer", NEEDS_APPROVALS))
	}

	async fn get_pending_operation(&self, request: Request<pb::GetPendingOperationRequest>) -> Result<Response<pb::PendingOperation>, Status> {
		caller(&request)?;
		Err(not_yet("GetPendingOperation", NEEDS_APPROVALS))
	}

	async fn get_balances(&self, request: Request<pb::GetBalancesRequest>) -> Result<Response<pb::GetBalancesResponse>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let scope = scope_of(&req.scope)?;
		let labels = req.labels.iter().map(|raw| label_of(raw)).collect::<Result<Vec<_>, _>>()?;
		let views = ledger_app::get_balances(&self.ports(), &client, &scope, &labels).await.map_err(refuse)?;
		Ok(Response::new(pb::GetBalancesResponse {
			accounts: views.iter().map(account_to_proto).collect(),
		}))
	}

	async fn get_hold(&self, request: Request<pb::GetHoldRequest>) -> Result<Response<pb::Hold>, Status> {
		caller(&request)?;
		Err(not_yet("GetHold", "there are no holds yet"))
	}
}

fn account_to_proto(view: &AccountView) -> pb::ScopeAccount {
	pb::ScopeAccount {
		scope: view.account.scope.as_str().to_owned(),
		label: view.account.label.as_str().to_owned(),
		balance: view.balance.to_decimal_string(),
		opened_at: view.account.opened_at,
		owner_client: view.account.owner.as_str().to_owned(),
	}
}

fn transfer_to_proto(recorded: &RecordedTransfer) -> pb::ScopeTransfer {
	let transfer = &recorded.transfer;
	pb::ScopeTransfer {
		transfer_id: transfer.id().to_string(),
		scope: transfer.scope().as_str().to_owned(),
		from_label: transfer.from().as_str().to_owned(),
		to_label: transfer.to().as_str().to_owned(),
		amount: transfer.amount().to_decimal_string(),
		created_at: recorded.created_at,
		subject_ref: transfer.subject_ref().as_str().to_owned(),
		memo: transfer.memo().to_owned(),
		reason: match transfer.reason() {
			TransferReason::FeeSweep => pb::TransferReason::FeeSweep as i32,
		},
		to_scope: recorded.to_scope.as_str().to_owned(),
		operation_id: String::new(),
	}
}
