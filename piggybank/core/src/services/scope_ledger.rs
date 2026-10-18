//! `ScopeLedgerService` (`scope/v1/ledger.proto`) — the service-facing money surface, on a
//! listener of its own.
//!
//! WHO CALLS. Another backend, never a browser: the listener speaks only HTTP/2 over mTLS
//! (no `accept_http1`, no gRPC-Web), every client certificate must chain to the configured
//! CA, and two layers stand in front of the service, the certificate outside the token:
//!   - [`ClientIdentityLayer`] refuses anything but `application/grpc` (a gRPC-Web request
//!     would otherwise be decoded as gRPC: tonic does not look at the content type) and maps
//!     the certificate the client presented to a [`ClientId`] by its fingerprint
//!     ([`ClientDirectory`]) — the ONLY source of the caller's identity.
//!   - the auth layer pinned to [`TokenClass::Service`] (`aud=banking-services`,
//!     `typ=service`), so a user's access token is refused at the verifier. The token is
//!     then bound to the certificate: its `sub` must be that client's id, so a service token
//!     stolen from one client is useless on another client's connection.
//!
//! ONE service sits outside the token layer, on purpose: `ScopeAuthService.MintServiceToken`,
//! where a client gets the token the rest demand. It is behind the certificate layer only, and
//! issues to the client that layer proved — see [`ScopeAuthSvc`].
//!
//! Served: `OpenAccount`, `GetBalances`, `Transfer`, the holds (`Reserve`, `Capture`,
//! `Void`, `GetHold` — a `Reserve` stands on the user's mandate for the client),
//! `Charge` (a one-off debit on the user's mandate), `Payout`, the requested
//! operations (`RequestPayout`, `RequestTransfer`, `GetPendingOperation`), `InvoiceService`,
//! `MandateRequestService` and `MoneyEvents.Pull`, behind the same two layers.
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
	error::DomainError,
	money::Usdt,
	scope_invoices::InvoiceState,
	scope_ledger::{ClientDirectory, ClientId, Grants, HoldState, IDEMPOTENCY_CONFLICT, MandateOperation, MoneyEventKind, SubjectRef, TransferReason},
	scope_mandates::MandateRequest,
	scope_operations::{OperationBody, OperationState, PayoutReason, RequestedLine},
	scopes::{ScopeAccountLabel, ScopeId},
};
use evbanking_auth::{Authenticate, Authorizer, MintService, TokenClass, TokenType, claims_of, grpc_auth_layer};
use evbanking_contracts::scope::v1::{
	self as pb,
	invoice_service_server::{InvoiceService, InvoiceServiceServer},
	mandate_request_service_server::{MandateRequestService, MandateRequestServiceServer},
	money_events_server::{MoneyEvents, MoneyEventsServer},
	scope_auth_service_server::{ScopeAuthService, ScopeAuthServiceServer},
	scope_ledger_service_server::{ScopeLedgerService, ScopeLedgerServiceServer},
};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tonic::{
	Code, Request, Response, Status,
	body::Body,
	codegen::http,
	transport::{
		Certificate, Identity, Server, ServerTlsConfig,
		server::{TcpConnectInfo, TlsConnectInfo},
	},
};
use tower::{Layer, Service};
use tower_http::trace::{DefaultOnFailure, TraceLayer};
use uuid::Uuid;

use crate::{
	application::{
		scope_charges::{self as charges_app, ChargeRequest},
		scope_invoices::{self as invoices_app, CreateInvoiceRequest, InvoicePorts},
		scope_ledger::{self as ledger_app, AccountView, CaptureRequest, ListHoldsRequest, ReserveRequest, ScopeLedgerPorts, TransferError, TransferRequest, VoidRequest},
		scope_mandates::{self as mandates_app, CreateRequest, RequestPorts},
		scope_operations::{self as operations_app, OperationPorts, PayoutRequest, RequestPayoutRequest, RequestTransferRequest},
	},
	config::ScopeLedgerSettings,
	ports::{
		compartments::CompartmentStore,
		ledger::Ledger,
		scope_charges::RecordedCharge,
		scope_invoices::{InvoiceStore, RecordedInvoice},
		scope_ledger::{CaptureTarget, EventAccount, HoldStore, MoneyEventRecord, MoneyEventStore, RecordedHold, RecordedTransfer, ScopeLedgerStore},
		scope_mandates::MandateRequestStore,
		scope_operations::{OperationStore, PayoutStore, RecordedOperation, RecordedPayout},
	},
	services::support::map_err,
};

/// The client the transport proved, put in the request's extensions by
/// [`ClientIdentityLayer`].
#[derive(Clone, Debug)]
struct CallerClient(ClientId);

/// Refuses anything but gRPC proper (INVALID_ARGUMENT), then maps the presented client
/// certificate to a [`ClientId`], or answers UNAUTHENTICATED.
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

/// Whether the request is gRPC over HTTP/2 proper: `application/grpc`, optionally with a
/// `+<codec>` suffix. `application/grpc-web…` is refused here, before anything else reads it.
fn is_grpc(content_type: Option<&http::HeaderValue>) -> bool {
	content_type
		.and_then(|value| value.to_str().ok())
		.is_some_and(|value| value == "application/grpc" || value.starts_with("application/grpc+"))
}

fn status_response(status: Status) -> http::Response<Body> {
	let (parts, ()) = status.into_http::<()>().into_parts();
	http::Response::from_parts(parts, Body::empty())
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
		if !is_grpc(req.headers().get(http::header::CONTENT_TYPE)) {
			return Box::pin(async { Ok(status_response(Status::invalid_argument("only application/grpc is served here"))) });
		}
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
				None => Ok(status_response(Status::unauthenticated("client certificate is not a known client"))),
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
	pub holds: Arc<dyn HoldStore>,
	pub events: Arc<dyn MoneyEventStore>,
	pub invoices: Arc<dyn InvoiceStore>,
	pub mandate_requests: Arc<dyn MandateRequestStore>,
	/// The scope registry (which scopes exist, their state and policy).
	pub scopes: Arc<dyn CompartmentStore>,
	pub ledger: Arc<dyn Ledger>,
	/// Nudged after a transfer commits so the relay posts it at once.
	pub relay: Arc<Notify>,
	pub payouts: Arc<dyn PayoutStore>,
	pub operations: Arc<dyn OperationStore>,
	/// Nudged after an operation is stored so its approval is opened at once.
	pub executor: Arc<Notify>,
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
	/// The TLS material was parsed and matched when the settings were read
	/// ([`ScopeLedgerSettings::from_env`]), so what is handed to tonic here is known good.
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
/// against the hub's SERVICE token class and issuing service tokens through the same handle.
pub async fn serve(listener: ScopeLedgerListener, authorizer: &Authorizer, shutdown: impl Future<Output = ()>) -> Result<(), tonic::transport::Error> {
	serve_with(listener, authorizer.for_class(TokenClass::Service), authorizer.clone(), shutdown).await
}

/// [`serve`] over any authenticator and minter — the seam the listener's tests drive. Both
/// layers are mounted here and nowhere else, so what a test proves about this function is
/// what production runs.
pub async fn serve_with<A: Authenticate, M: MintService>(
	listener: ScopeLedgerListener,
	authenticator: A,
	minter: M,
	shutdown: impl Future<Output = ()>,
) -> Result<(), tonic::transport::Error> {
	let auth = grpc_auth_layer(authenticator);
	let identity = ClientIdentityLayer::new(listener.clients);
	let service = listener.service;
	// No `accept_http1` (ALPN offers only h2, so an HTTP/1.1 client fails the handshake) and
	// no `GrpcWebLayer`; a gRPC-Web request sent over h2 anyway is refused by the identity
	// layer's content-type gate, since tonic itself would decode it as gRPC. The certificate
	// layer is the outer one: a caller no certificate maps to learns nothing about its token.
	// Refusals are the protocol working (a client's `approval_required` is not an incident),
	// so the trace layer keeps them at DEBUG; the handlers log each one at INFO with the RPC
	// and the client, and what is unexpected at WARN (`refused`).
	Server::builder()
		.tls_config(listener.tls)?
		.layer(TraceLayer::new_for_grpc().on_failure(DefaultOnFailure::new().level(tracing::Level::DEBUG)))
		.add_service(identity.layer(auth.layer(ScopeLedgerServiceServer::new(service.clone()))))
		.add_service(identity.layer(auth.layer(InvoiceServiceServer::new(service.clone()))))
		.add_service(identity.layer(auth.layer(MandateRequestServiceServer::new(service.clone()))))
		.add_service(identity.layer(auth.layer(MoneyEventsServer::new(service))))
		// The only service without the token layer: a client with no token yet has to be able
		// to ask for one. The certificate layer still stands in front, so it is mTLS alone.
		.add_service(identity.layer(ScopeAuthServiceServer::new(ScopeAuthSvc { minter })))
		.serve_with_shutdown(listener.bind, shutdown)
		.await
}

/// `ScopeAuthService`: issues a service token to the client the certificate layer proved.
/// The subject comes from [`CallerClient`] and nowhere else — the request has no fields —
/// and the token is not logged (client and expiry are).
#[derive(Clone)]
struct ScopeAuthSvc<M> {
	minter: M,
}

#[tonic::async_trait]
impl<M: MintService> ScopeAuthService for ScopeAuthSvc<M> {
	async fn mint_service_token(&self, request: Request<pb::MintServiceTokenRequest>) -> Result<Response<pb::MintServiceTokenResponse>, Status> {
		let client = request
			.extensions()
			.get::<CallerClient>()
			.map(|caller| caller.0.clone())
			.ok_or_else(|| Status::unauthenticated("no client identity"))?;
		// The grants file refuses `banking` as a caller at load; this keeps the rule at the
		// point of issue, where a token for banking's own identity would be the worst one.
		if client.is_banking() {
			return Err(Status::permission_denied("banking is not a service client"));
		}
		let (token, exp) = self.minter.mint_service(client.as_str()).await.map_err(|err| {
			if err.is_unexpected() {
				tracing::warn!(rpc = "MintServiceToken", %client, "scope ledger: {err}");
			}
			Status::from(err)
		})?;
		tracing::info!(rpc = "MintServiceToken", %client, exp, "scope ledger: service token issued");
		Ok(Response::new(pb::MintServiceTokenResponse {
			token,
			expires_at: i64::try_from(exp).unwrap_or(i64::MAX),
		}))
	}
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
			holds: self.deps.holds.as_ref(),
			events: self.deps.events.as_ref(),
			scopes: self.deps.scopes.as_ref(),
			ledger: self.deps.ledger.as_ref(),
			relay: &self.deps.relay,
		}
	}

	fn operation_ports<'a>(&'a self, base: &'a ScopeLedgerPorts<'a>) -> OperationPorts<'a> {
		OperationPorts {
			base,
			payouts: self.deps.payouts.as_ref(),
			operations: self.deps.operations.as_ref(),
			executor: &self.deps.executor,
		}
	}

	fn invoice_ports(&self) -> InvoicePorts<'_> {
		InvoicePorts {
			grants: &self.grants,
			invoices: self.deps.invoices.as_ref(),
			scopes: self.deps.scopes.as_ref(),
		}
	}

	fn request_ports(&self) -> RequestPorts<'_> {
		RequestPorts {
			grants: &self.grants,
			requests: self.deps.mandate_requests.as_ref(),
			scopes: self.deps.scopes.as_ref(),
		}
	}
}

/// The caller, as the two layers proved it: a service token issued to the very client the
/// certificate names. Either missing means the service was reached without its layers —
/// refused, never served as nobody. A token whose `sub` is another client is refused: a
/// service token copied off one client cannot ride another client's certificate.
fn caller<T>(request: &Request<T>) -> Result<ClientId, Status> {
	let claims = claims_of(request).ok_or_else(|| Status::unauthenticated("missing claims"))?;
	if claims.typ != TokenType::Service {
		return Err(Status::permission_denied("service token required"));
	}
	let client = request
		.extensions()
		.get::<CallerClient>()
		.map(|caller| caller.0.clone())
		.ok_or_else(|| Status::unauthenticated("no client identity"))?;
	if claims.sub != client.as_str() {
		return Err(Status::permission_denied("token is not this client's"));
	}
	Ok(client)
}

/// The refusal's reason is the whole message: clients branch on (code, message). Logged
/// here, once per refusal, with the RPC and the client: a refusal the protocol expects at
/// INFO, anything the caller could not have caused at WARN.
fn refused(rpc: &'static str, client: &ClientId, err: TransferError) -> Status {
	let status = match err {
		TransferError::Refused(refusal) if refusal.is_permission() => Status::permission_denied(refusal.reason()),
		TransferError::Refused(refusal) => Status::failed_precondition(refusal.reason()),
		TransferError::Internal(why) => {
			tracing::error!(rpc, %client, "scope ledger: {why}");
			return Status::internal("internal error");
		}
		// The whole message is the reason, like a refusal's: `map_err` would prefix it.
		TransferError::Domain(DomainError::Conflict(why)) if why == IDEMPOTENCY_CONFLICT => Status::already_exists(IDEMPOTENCY_CONFLICT),
		TransferError::Domain(err) => map_err(err),
		TransferError::Exhausted(why) => Status::resource_exhausted(why),
	};
	match status.code() {
		Code::Unavailable | Code::Internal | Code::Unknown => tracing::warn!(rpc, %client, code = ?status.code(), "scope ledger: {}", status.message()),
		_ => tracing::info!(rpc, %client, code = ?status.code(), reason = status.message(), "scope ledger: refused"),
	}
	status
}

fn scope_of(raw: &str) -> Result<ScopeId, Status> {
	ScopeId::parse(raw).map_err(map_err)
}

fn label_of(raw: &str) -> Result<ScopeAccountLabel, Status> {
	ScopeAccountLabel::parse(raw).map_err(map_err)
}

/// Most labels one `GetBalances` may name: one ledger read per label.
const MAX_BALANCE_LABELS: usize = 256;

fn subject_of(raw: &str) -> Result<SubjectRef, Status> {
	SubjectRef::parse(raw).map_err(map_err)
}

fn hold_id_of(raw: &str) -> Result<Uuid, Status> {
	// Lowercase only, as banking writes it: one hold, one spelling.
	Uuid::parse_str(raw)
		.ok()
		.filter(|id| id.to_string() == raw)
		.ok_or_else(|| Status::invalid_argument("hold_id must be a lowercase uuid"))
}

/// An optional amount: empty means "everything".
fn optional_amount(raw: &str) -> Result<Option<Usdt>, Status> {
	if raw.is_empty() {
		return Ok(None);
	}
	Usdt::parse_decimal(raw).map(Some).map_err(map_err)
}

/// A page token is the last hold id of the page before, as banking wrote it.
fn page_token_of(raw: &str) -> Result<Uuid, Status> {
	Uuid::parse_str(raw)
		.ok()
		.filter(|id| id.to_string() == raw)
		.ok_or_else(|| Status::invalid_argument("page_token is not one this service issued"))
}

fn operation_id_of(raw: &str) -> Result<Uuid, Status> {
	Uuid::parse_str(raw)
		.ok()
		.filter(|id| id.to_string() == raw)
		.ok_or_else(|| Status::invalid_argument("operation_id must be a lowercase uuid"))
}

fn lines_of(lines: Vec<pb::PayoutLine>) -> Result<Vec<RequestedLine>, Status> {
	lines
		.into_iter()
		.map(|line| {
			Ok(RequestedLine {
				amount: Usdt::parse_decimal(&line.amount).map_err(map_err)?,
				auth_subject: line.auth_subject,
			})
		})
		.collect()
}

#[tonic::async_trait]
impl ScopeLedgerService for ScopeLedgerSvc {
	async fn open_account(&self, request: Request<pb::OpenAccountRequest>) -> Result<Response<pb::ScopeAccount>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let scope = scope_of(&req.scope)?;
		let label = label_of(&req.label)?;
		let view = ledger_app::open_account(&self.ports(), &client, &scope, &label, &req.idempotency_key)
			.await
			.map_err(|err| refused("OpenAccount", &client, err))?;
		Ok(Response::new(account_to_proto(&view)))
	}

	async fn reserve(&self, request: Request<pb::ReserveRequest>) -> Result<Response<pb::Hold>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let parsed = ReserveRequest {
			scope: scope_of(&req.scope)?,
			auth_subject: req.auth_subject,
			amount: Usdt::parse_decimal(&req.amount).map_err(map_err)?,
			ttl_seconds: req.ttl_seconds,
			subject_ref: subject_of(&req.subject_ref)?,
			idempotency_key: req.idempotency_key,
		};
		let recorded = ledger_app::reserve(&self.ports(), &client, parsed).await.map_err(|err| refused("Reserve", &client, err))?;
		Ok(Response::new(hold_to_proto(&recorded)))
	}

	async fn capture(&self, request: Request<pb::CaptureRequest>) -> Result<Response<pb::Hold>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let to = match req.to.and_then(|to| to.account) {
			Some(pb::account_ref::Account::ScopeLabel(label)) => CaptureTarget::Account(label_of(&label)?),
			Some(pb::account_ref::Account::AuthSubject(subject)) => CaptureTarget::User(subject),
			None => return Err(Status::invalid_argument("to is required")),
		};
		let parsed = CaptureRequest {
			scope: scope_of(&req.scope)?,
			hold_id: hold_id_of(&req.hold_id)?,
			to,
			amount: optional_amount(&req.amount)?,
			subject_ref: subject_of(&req.subject_ref)?,
			idempotency_key: req.idempotency_key,
		};
		let recorded = ledger_app::capture(&self.ports(), &client, parsed).await.map_err(|err| refused("Capture", &client, err))?;
		Ok(Response::new(hold_to_proto(&recorded)))
	}

	async fn void(&self, request: Request<pb::VoidRequest>) -> Result<Response<pb::Hold>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let parsed = VoidRequest {
			scope: scope_of(&req.scope)?,
			hold_id: hold_id_of(&req.hold_id)?,
			subject_ref: subject_of(&req.subject_ref)?,
			idempotency_key: req.idempotency_key,
		};
		let recorded = ledger_app::void(&self.ports(), &client, parsed).await.map_err(|err| refused("Void", &client, err))?;
		Ok(Response::new(hold_to_proto(&recorded)))
	}

	async fn charge(&self, request: Request<pb::ChargeRequest>) -> Result<Response<pb::ScopeCharge>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let parsed = ChargeRequest {
			scope: scope_of(&req.scope)?,
			auth_subject: req.auth_subject,
			amount: Usdt::parse_decimal(&req.amount).map_err(map_err)?,
			to: label_of(&req.to_label)?,
			memo: req.memo,
			subject_ref: subject_of(&req.subject_ref)?,
			idempotency_key: req.idempotency_key,
		};
		let recorded = charges_app::charge(&self.ports(), &client, parsed).await.map_err(|err| refused("Charge", &client, err))?;
		Ok(Response::new(charge_to_proto(&recorded)))
	}

	async fn payout(&self, request: Request<pb::PayoutRequest>) -> Result<Response<pb::ScopePayout>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let reason = match pb::PayoutReason::try_from(req.reason) {
			Ok(pb::PayoutReason::Redemption) => PayoutReason::Redemption,
			Ok(pb::PayoutReason::Book) => PayoutReason::Book,
			Ok(pb::PayoutReason::FeeRebate) => PayoutReason::FeeRebate,
			Ok(pb::PayoutReason::Unspecified) | Err(_) => return Err(Status::invalid_argument("reason is required")),
		};
		let parsed = PayoutRequest {
			scope: scope_of(&req.scope)?,
			from: label_of(&req.from_label)?,
			lines: lines_of(req.lines)?,
			reason,
			subject_ref: subject_of(&req.subject_ref)?,
			memo: req.memo,
			idempotency_key: req.idempotency_key,
		};
		let base = self.ports();
		let recorded = operations_app::payout(&self.operation_ports(&base), &client, parsed)
			.await
			.map_err(|err| refused("Payout", &client, err))?;
		Ok(Response::new(payout_to_proto(&recorded)))
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
		let recorded = ledger_app::transfer(&self.ports(), &client, parsed).await.map_err(|err| refused("Transfer", &client, err))?;
		Ok(Response::new(transfer_to_proto(&recorded)))
	}

	async fn request_payout(&self, request: Request<pb::RequestPayoutRequest>) -> Result<Response<pb::PendingOperation>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let parsed = RequestPayoutRequest {
			scope: scope_of(&req.scope)?,
			from: label_of(&req.from_label)?,
			lines: lines_of(req.lines)?,
			distribution: req.distribution,
			subject_ref: subject_of(&req.subject_ref)?,
			memo: req.memo,
			idempotency_key: req.idempotency_key,
		};
		let base = self.ports();
		let recorded = operations_app::request_payout(&self.operation_ports(&base), &client, parsed)
			.await
			.map_err(|err| refused("RequestPayout", &client, err))?;
		Ok(Response::new(operation_to_proto(&recorded)))
	}

	async fn request_transfer(&self, request: Request<pb::RequestTransferRequest>) -> Result<Response<pb::PendingOperation>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let parsed = RequestTransferRequest {
			scope: scope_of(&req.scope)?,
			from: label_of(&req.from_label)?,
			to_scope: if req.to_scope.is_empty() { None } else { Some(scope_of(&req.to_scope)?) },
			to: label_of(&req.to_label)?,
			amount: Usdt::parse_decimal(&req.amount).map_err(map_err)?,
			subject_ref: subject_of(&req.subject_ref)?,
			memo: req.memo,
			idempotency_key: req.idempotency_key,
		};
		let base = self.ports();
		let recorded = operations_app::request_transfer(&self.operation_ports(&base), &client, parsed)
			.await
			.map_err(|err| refused("RequestTransfer", &client, err))?;
		Ok(Response::new(operation_to_proto(&recorded)))
	}

	async fn get_pending_operation(&self, request: Request<pb::GetPendingOperationRequest>) -> Result<Response<pb::PendingOperation>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let scope = scope_of(&req.scope)?;
		let base = self.ports();
		let recorded = operations_app::get_pending_operation(&self.operation_ports(&base), &client, &scope, operation_id_of(&req.operation_id)?)
			.await
			.map_err(|err| refused("GetPendingOperation", &client, err))?;
		Ok(Response::new(operation_to_proto(&recorded)))
	}

	async fn get_balances(&self, request: Request<pb::GetBalancesRequest>) -> Result<Response<pb::GetBalancesResponse>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let scope = scope_of(&req.scope)?;
		if req.labels.len() > MAX_BALANCE_LABELS {
			return Err(Status::invalid_argument(format!("at most {MAX_BALANCE_LABELS} labels")));
		}
		let mut labels: Vec<ScopeAccountLabel> = Vec::with_capacity(req.labels.len());
		for raw in &req.labels {
			let label = label_of(raw)?;
			// A repeat asks nothing new; answering it once keeps the ledger read bounded.
			if !labels.contains(&label) {
				labels.push(label);
			}
		}
		let views = ledger_app::get_balances(&self.ports(), &client, &scope, &labels)
			.await
			.map_err(|err| refused("GetBalances", &client, err))?;
		Ok(Response::new(pb::GetBalancesResponse {
			accounts: views.iter().map(account_to_proto).collect(),
		}))
	}

	async fn get_hold(&self, request: Request<pb::GetHoldRequest>) -> Result<Response<pb::Hold>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let scope = scope_of(&req.scope)?;
		let recorded = ledger_app::get_hold(&self.ports(), &client, &scope, hold_id_of(&req.hold_id)?)
			.await
			.map_err(|err| refused("GetHold", &client, err))?;
		Ok(Response::new(hold_to_proto(&recorded)))
	}

	async fn list_holds(&self, request: Request<pb::ListHoldsRequest>) -> Result<Response<pb::ListHoldsResponse>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let parsed = ListHoldsRequest {
			scope: if req.scope.is_empty() { None } else { Some(scope_of(&req.scope)?) },
			subject_prefix: req.subject_ref_prefix,
			page_size: req.page_size,
			after: if req.page_token.is_empty() { None } else { Some(page_token_of(&req.page_token)?) },
		};
		let page = ledger_app::list_holds(&self.ports(), &client, parsed).await.map_err(|err| refused("ListHolds", &client, err))?;
		Ok(Response::new(pb::ListHoldsResponse {
			holds: page.holds.iter().map(hold_to_proto).collect(),
			next_page_token: page.next.map(|id| id.to_string()).unwrap_or_default(),
		}))
	}
}

#[tonic::async_trait]
impl MoneyEvents for ScopeLedgerSvc {
	async fn pull(&self, request: Request<pb::PullRequest>) -> Result<Response<pb::PullResponse>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let scope = if req.scope.is_empty() { None } else { Some(scope_of(&req.scope)?) };
		let pulled = ledger_app::pull(&self.ports(), &client, scope.as_ref(), req.after_position, req.limit)
			.await
			.map_err(|err| refused("Pull", &client, err))?;
		Ok(Response::new(pb::PullResponse {
			events: pulled.events.iter().map(event_to_proto).collect(),
			next_position: pulled.next_position,
		}))
	}
}

fn invoice_id_of(raw: &str) -> Result<Uuid, Status> {
	// Lowercase only, as banking writes it: one invoice, one spelling.
	Uuid::parse_str(raw)
		.ok()
		.filter(|id| id.to_string() == raw)
		.ok_or_else(|| Status::invalid_argument("invoice_id must be a lowercase uuid"))
}

#[tonic::async_trait]
impl InvoiceService for ScopeLedgerSvc {
	async fn create_invoice(&self, request: Request<pb::CreateInvoiceRequest>) -> Result<Response<pb::Invoice>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let parsed = CreateInvoiceRequest {
			scope: scope_of(&req.scope)?,
			to: label_of(&req.to_label)?,
			amount: Usdt::parse_decimal(&req.amount).map_err(map_err)?,
			memo: req.memo,
			expires_at: req.expires_at,
			payer_auth_subject: Some(req.payer_auth_subject).filter(|subject| !subject.is_empty()),
			subject_ref: subject_of(&req.subject_ref)?,
			idempotency_key: req.idempotency_key,
		};
		let recorded = invoices_app::create_invoice(&self.invoice_ports(), &client, parsed)
			.await
			.map_err(|err| refused("CreateInvoice", &client, err))?;
		Ok(Response::new(invoice_to_proto(&recorded)))
	}

	async fn cancel_invoice(&self, request: Request<pb::CancelInvoiceRequest>) -> Result<Response<pb::Invoice>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let recorded = invoices_app::cancel_invoice(&self.invoice_ports(), &client, &scope_of(&req.scope)?, invoice_id_of(&req.invoice_id)?)
			.await
			.map_err(|err| refused("CancelInvoice", &client, err))?;
		Ok(Response::new(invoice_to_proto(&recorded)))
	}

	async fn get_invoice(&self, request: Request<pb::GetInvoiceRequest>) -> Result<Response<pb::Invoice>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let recorded = invoices_app::get_invoice(&self.invoice_ports(), &client, &scope_of(&req.scope)?, invoice_id_of(&req.invoice_id)?)
			.await
			.map_err(|err| refused("GetInvoice", &client, err))?;
		Ok(Response::new(invoice_to_proto(&recorded)))
	}
}

#[tonic::async_trait]
impl MandateRequestService for ScopeLedgerSvc {
	async fn create_mandate_request(&self, request: Request<pb::CreateMandateRequestRequest>) -> Result<Response<pb::MandateRequest>, Status> {
		let client = caller(&request)?;
		let req = request.into_inner();
		let optional_cap = |raw: &str| if raw.is_empty() { Ok(None) } else { Usdt::parse_decimal(raw).map(Some).map_err(map_err) };
		let parsed = CreateRequest {
			scope: scope_of(&req.scope)?,
			cap_per_op: Usdt::parse_decimal(&req.cap_per_op).map_err(map_err)?,
			cap_24h: Usdt::parse_decimal(&req.cap_24h).map_err(map_err)?,
			cap_outstanding: optional_cap(&req.cap_outstanding)?,
			cap_book_fill_24h: optional_cap(&req.cap_book_fill_24h)?,
			operations: req.operations.iter().map(|raw| mandate_operation_of(*raw)).collect::<Result<_, _>>()?,
			mandate_expires_at: match req.mandate_expires_at {
				0 => None,
				at if at < 0 => return Err(Status::invalid_argument("mandate_expires_at must be unix seconds, or 0")),
				at => Some(at),
			},
			idempotency_key: req.idempotency_key,
		};
		let created = mandates_app::create_request(&self.request_ports(), &client, parsed)
			.await
			.map_err(|err| refused("CreateMandateRequest", &client, err.into()))?;
		Ok(Response::new(mandate_request_to_proto(&created)))
	}
}

fn mandate_operation_of(raw: i32) -> Result<MandateOperation, Status> {
	match pb::MandateOperation::try_from(raw) {
		Ok(pb::MandateOperation::Reserve) => Ok(MandateOperation::Reserve),
		Ok(pb::MandateOperation::Charge) => Ok(MandateOperation::Charge),
		Ok(pb::MandateOperation::BookFill) => Ok(MandateOperation::BookFill),
		Ok(pb::MandateOperation::Unspecified) | Err(_) => Err(Status::invalid_argument("operations must name RESERVE, CHARGE or BOOK_FILL")),
	}
}

fn mandate_request_to_proto(request: &MandateRequest) -> pb::MandateRequest {
	let terms = &request.terms;
	pb::MandateRequest {
		request_id: request.id.to_string(),
		scope: terms.scope.as_str().to_owned(),
		operations: terms
			.operations
			.iter()
			.map(|operation| match operation {
				MandateOperation::Reserve => pb::MandateOperation::Reserve,
				MandateOperation::Charge => pb::MandateOperation::Charge,
				MandateOperation::BookFill => pb::MandateOperation::BookFill,
			} as i32)
			.collect(),
		cap_per_op: terms.caps.per_op.to_decimal_string(),
		cap_24h: terms.caps.per_24h.to_decimal_string(),
		cap_outstanding: terms.caps.outstanding.to_decimal_string(),
		cap_book_fill_24h: terms.caps.book_fill_24h.map(Usdt::to_decimal_string).unwrap_or_default(),
		mandate_expires_at: terms.mandate_expires_at.unwrap_or(0),
		expires_at: request.expires_at,
		created_at: request.created_at,
		grant_url: mandates_app::grant_url(request.id),
	}
}

fn invoice_to_proto(recorded: &RecordedInvoice) -> pb::Invoice {
	let invoice = &recorded.invoice;
	let terms = invoice.terms();
	// A payment the ledger has not taken is nobody's news yet: the invoice reads
	// processing (`state_at`) and names no payer until it posts.
	let payment = invoice.posted_payment();
	pb::Invoice {
		invoice_id: invoice.id().to_string(),
		scope: terms.scope.as_str().to_owned(),
		to_label: terms.to.as_str().to_owned(),
		amount: terms.amount.to_decimal_string(),
		memo: terms.memo.clone(),
		// An open invoice past its expiry reads expired before the reaper records it.
		state: match invoice.state_at(recorded.read_at) {
			InvoiceState::Open => pb::InvoiceState::Open,
			InvoiceState::Processing => pb::InvoiceState::Processing,
			InvoiceState::Paid => pb::InvoiceState::Paid,
			InvoiceState::Cancelled => pb::InvoiceState::Cancelled,
			InvoiceState::Expired => pb::InvoiceState::Expired,
		} as i32,
		created_at: recorded.created_at,
		expires_at: terms.expires_at,
		payer_auth_subject: terms.payer_auth_subject.clone().unwrap_or_default(),
		pay_url: invoices_app::pay_url(invoice.id()),
		subject_ref: terms.subject_ref.as_str().to_owned(),
		paid_by_auth_subject: payment.map(|paid| paid.payer_auth_subject.clone()).unwrap_or_default(),
		paid_at: payment.map_or(0, |paid| paid.paid_at),
		paid_from: payment.map(|paid| paid.from.as_str().to_owned()).unwrap_or_default(),
	}
}

fn hold_to_proto(recorded: &RecordedHold) -> pb::Hold {
	let hold = &recorded.hold;
	let terms = hold.terms();
	pb::Hold {
		hold_id: hold.id().to_string(),
		scope: terms.scope.as_str().to_owned(),
		auth_subject: recorded.auth_subject.clone(),
		amount: terms.amount.to_decimal_string(),
		captured: hold.progress().captured.to_decimal_string(),
		remaining: hold.remaining().to_decimal_string(),
		state: match hold.state() {
			HoldState::Active => pb::HoldState::Active,
			HoldState::Captured => pb::HoldState::Captured,
			HoldState::Voided => pb::HoldState::Voided,
			HoldState::Expired => pb::HoldState::Expired,
		} as i32,
		expires_at: terms.expires_at.unwrap_or(0),
		created_at: recorded.created_at,
		subject_ref: terms.subject_ref.as_str().to_owned(),
		mandate_id: terms.mandate_id.to_string(),
		placed_by_client: terms.client.as_str().to_owned(),
	}
}

fn account_ref(account: &EventAccount) -> pb::AccountRef {
	pb::AccountRef {
		account: Some(match account {
			EventAccount::Account(label) => pb::account_ref::Account::ScopeLabel(label.as_str().to_owned()),
			EventAccount::User(subject) => pb::account_ref::Account::AuthSubject(subject.clone()),
		}),
	}
}

fn event_to_proto(event: &MoneyEventRecord) -> pb::MoneyEvent {
	use pb::money_event::Kind;
	pb::MoneyEvent {
		event_id: event.event_id.to_string(),
		position: event.position,
		scope: event.scope.as_str().to_owned(),
		occurred_at: event.occurred_at,
		kind: match event.kind {
			MoneyEventKind::AccountOpened => Kind::AccountOpened,
			MoneyEventKind::HoldPlaced => Kind::HoldPlaced,
			MoneyEventKind::HoldCaptured => Kind::HoldCaptured,
			MoneyEventKind::HoldVoided => Kind::HoldVoided,
			MoneyEventKind::HoldExpired => Kind::HoldExpired,
			MoneyEventKind::Transferred => Kind::Transferred,
			MoneyEventKind::Charged => Kind::Charged,
			MoneyEventKind::PaidOut => Kind::PaidOut,
			MoneyEventKind::OperationRequested => Kind::OperationRequested,
			MoneyEventKind::OperationRejected => Kind::OperationRejected,
			MoneyEventKind::OperationFailed => Kind::OperationFailed,
			MoneyEventKind::InvoiceCreated => Kind::InvoiceCreated,
			MoneyEventKind::InvoicePaid => Kind::InvoicePaid,
			MoneyEventKind::InvoiceCancelled => Kind::InvoiceCancelled,
			MoneyEventKind::InvoiceExpired => Kind::InvoiceExpired,
			MoneyEventKind::MandateGranted => Kind::MandateGranted,
			MoneyEventKind::MandateRevoked => Kind::MandateRevoked,
		} as i32,
		ref_id: event.ref_id.clone(),
		aggregate_version: event.aggregate_version,
		auth_subject: event.auth_subject.clone(),
		amount: event.amount.map(|amount| amount.to_decimal_string()).unwrap_or_default(),
		from: event.from.as_ref().map(account_ref),
		to: event.to.as_ref().map(account_ref),
		subject_ref: event.subject_ref.clone(),
		initiator_client: event.initiator_client.clone(),
		balance_after: event
			.balances
			.iter()
			.map(|entry| pb::AccountBalance {
				account: Some(account_ref(&EventAccount::Account(entry.label.clone()))),
				balance: entry.balance.to_decimal_string(),
			})
			.collect(),
		hold_remaining: event.hold_remaining.map(|remaining| remaining.to_decimal_string()).unwrap_or_default(),
		paid_from: event.paid_from.clone(),
		counterparty_scope: event.counterparty_scope.clone(),
		failure_reason: event.failure_reason.clone(),
	}
}

fn charge_to_proto(recorded: &RecordedCharge) -> pb::ScopeCharge {
	let terms = recorded.charge.terms();
	pb::ScopeCharge {
		charge_id: recorded.charge.id().to_string(),
		scope: terms.scope.as_str().to_owned(),
		auth_subject: recorded.auth_subject.clone(),
		amount: terms.amount.to_decimal_string(),
		to_label: terms.to.as_str().to_owned(),
		created_at: recorded.created_at,
		subject_ref: terms.subject_ref.as_str().to_owned(),
		memo: terms.memo.clone(),
		mandate_id: terms.mandate_id.to_string(),
		charged_by_client: terms.client.as_str().to_owned(),
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
			// banking's route of the swept fees carries the sweep's reason (`ledger.proto`).
			TransferReason::FeeSweep | TransferReason::FeeRoute => pb::TransferReason::FeeSweep as i32,
			// An approved transfer has no automatic reason; its operation says why.
			TransferReason::Approved => pb::TransferReason::Unspecified as i32,
		},
		to_scope: recorded.to_scope.as_str().to_owned(),
		operation_id: transfer.operation_id().map(|id| id.to_string()).unwrap_or_default(),
	}
}

fn payout_lines_to_proto<'a>(lines: impl Iterator<Item = &'a domain::scope_operations::PayoutLine>) -> Vec<pb::PayoutLine> {
	lines
		.map(|line| pb::PayoutLine {
			auth_subject: line.auth_subject.clone(),
			amount: line.amount.to_decimal_string(),
		})
		.collect()
}

fn payout_to_proto(recorded: &RecordedPayout) -> pb::ScopePayout {
	let payout = &recorded.payout;
	let terms = payout.terms();
	pb::ScopePayout {
		payout_id: payout.id().to_string(),
		scope: terms.scope.as_str().to_owned(),
		from_label: terms.from.as_str().to_owned(),
		lines: payout_lines_to_proto(terms.lines.iter()),
		total: payout.total().to_decimal_string(),
		created_at: recorded.created_at,
		subject_ref: terms.subject_ref.as_str().to_owned(),
		memo: terms.memo.clone(),
		reason: match terms.reason {
			PayoutReason::Redemption => pb::PayoutReason::Redemption,
			PayoutReason::Book => pb::PayoutReason::Book,
			PayoutReason::FeeRebate => pb::PayoutReason::FeeRebate,
			// An approved payout has no automatic reason; its operation says why.
			PayoutReason::Approved => pb::PayoutReason::Unspecified,
		} as i32,
		operation_id: terms.operation_id.map(|id| id.to_string()).unwrap_or_default(),
	}
}

fn operation_to_proto(recorded: &RecordedOperation) -> pb::PendingOperation {
	let operation = &recorded.operation;
	let terms = operation.terms();
	let progress = operation.progress();
	let (to_scope, to_label, lines, distribution) = match &terms.body {
		OperationBody::Payout { lines, distribution, .. } => (String::new(), String::new(), lines.as_slice(), *distribution),
		OperationBody::Transfer { to_scope, to, .. } => (to_scope.as_str().to_owned(), to.as_str().to_owned(), [].as_slice(), false),
	};
	pb::PendingOperation {
		operation_id: operation.id().to_string(),
		scope: terms.scope.as_str().to_owned(),
		state: match progress.state {
			OperationState::AwaitingApproval => pb::PendingOperationState::AwaitingApproval,
			OperationState::Executed => pb::PendingOperationState::Executed,
			OperationState::Rejected => pb::PendingOperationState::Rejected,
			OperationState::Failed => pb::PendingOperationState::Failed,
		} as i32,
		subject_ref: terms.subject_ref.as_str().to_owned(),
		approval_id: progress.approval_id.clone().unwrap_or_default(),
		kind: operation.kind().as_str().to_owned(),
		from_label: terms.from.as_str().to_owned(),
		to_scope,
		to_label,
		recipients: u32::try_from(lines.len()).unwrap_or(u32::MAX),
		lines: payout_lines_to_proto(lines.iter()),
		lines_digest: recorded.lines_digest.clone().unwrap_or_default(),
		distribution,
		total: operation.total().to_decimal_string(),
		memo: terms.memo.clone(),
		created_at: recorded.created_at,
		decided_at: recorded.decided_at.unwrap_or(0),
		result_id: progress.result_id.clone().unwrap_or_default(),
		failure_reason: progress.failure_reason.clone().unwrap_or_default(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_reused_key_is_already_exists_with_the_reason_alone_as_the_message() {
		let client = ClientId::parse("invest").unwrap();
		let status = refused("Reserve", &client, TransferError::Domain(DomainError::Conflict(IDEMPOTENCY_CONFLICT.into())));
		assert_eq!((status.code(), status.message()), (Code::AlreadyExists, "idempotency_conflict"));
		// The other ALREADY_EXISTS (a label another client owns) keeps its own words.
		let taken = refused("OpenAccount", &client, TransferError::Domain(DomainError::Conflict("label taken".into())));
		assert_eq!(taken.code(), Code::AlreadyExists);
		assert_ne!(taken.message(), "idempotency_conflict");
	}

	use evbanking_auth::AuthError;

	/// Records the `sub` it was asked to issue for, and issues a recognisable token.
	#[derive(Clone, Default)]
	struct RecordingMinter(Arc<std::sync::Mutex<Vec<String>>>);

	impl MintService for RecordingMinter {
		async fn mint_service(&self, sub: &str) -> Result<(String, u64), AuthError> {
			self.0.lock().unwrap().push(sub.to_owned());
			Ok((format!("token-for-{sub}"), 1_000))
		}
	}

	fn mint_request(client: Option<ClientId>) -> Request<pb::MintServiceTokenRequest> {
		let mut request = Request::new(pb::MintServiceTokenRequest {});
		if let Some(client) = client {
			request.extensions_mut().insert(CallerClient(client));
		}
		request
	}

	#[tokio::test]
	async fn mint_issues_to_the_certificates_client_and_to_nobody_else() {
		let minter = RecordingMinter::default();
		let svc = ScopeAuthSvc { minter: minter.clone() };
		let response = svc.mint_service_token(mint_request(Some(ClientId::parse("invest").unwrap()))).await.unwrap().into_inner();
		assert_eq!((response.token.as_str(), response.expires_at), ("token-for-invest", 1_000));
		assert_eq!(*minter.0.lock().unwrap(), ["invest"], "the subject is the client the certificate layer proved");
	}

	#[tokio::test]
	async fn mint_refuses_banking_and_a_call_that_skipped_the_certificate_layer() {
		let minter = RecordingMinter::default();
		let svc = ScopeAuthSvc { minter: minter.clone() };
		let banking = svc.mint_service_token(mint_request(Some(ClientId::banking()))).await.unwrap_err();
		assert_eq!(banking.code(), Code::PermissionDenied);
		let anonymous = svc.mint_service_token(mint_request(None)).await.unwrap_err();
		assert_eq!(anonymous.code(), Code::Unauthenticated);
		assert!(minter.0.lock().unwrap().is_empty(), "neither reached the signer");
	}
}
