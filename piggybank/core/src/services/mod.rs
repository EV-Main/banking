//! The hub's inbound **driving adapter** — its data-plane gRPC service surface
//! (tonic). A closed, internal surface consumed by other services and, via the
//! `cabinet/backend` BFF, by browsers. Named for the hexagonal role (the services it
//! exposes), not the wire protocol; gRPC/tonic-web is the implementation detail.
//!
//! `tonic-web` (`GrpcWebLayer` + `accept_http1`) lets browser/WASM clients reach
//! the services over gRPC-Web with no separate proxy. `TraceLayer` emits a span
//! per request through the same `tracing` subscriber (and Sentry integration).
//!
//! The auth *issuance* routes are NOT here — they are served by the auth task
//! (`evbanking_auth`) on its own address. Core authorizes each inbound request via
//! `state.authorizer` (the in-process channel to that task); the async auth layer
//! attaches at the marked point below.

use std::{future::Future, net::SocketAddr};

use evbanking_auth::{TokenClass, grpc_auth_layer};
use evbanking_contracts::banking::v1::{
	allocations_service_server::AllocationsServiceServer, balance_service_server::BalanceServiceServer, book_service_server::BookServiceServer,
	compartment_service_server::CompartmentServiceServer, consilium_approval_service_server::ConsiliumApprovalServiceServer, consilium_service_server::ConsiliumServiceServer,
	fees_service_server::FeesServiceServer, funds_service_server::FundsServiceServer, health_service_server::HealthServiceServer,
	invoice_payment_service_server::InvoicePaymentServiceServer, operations_service_server::OperationsServiceServer, payment_confirmation_service_server::PaymentConfirmationServiceServer,
	payment_consent_service_server::PaymentConsentServiceServer, payments_service_server::PaymentsServiceServer, payout_review_service_server::PayoutReviewServiceServer,
	users_service_server::UsersServiceServer, wallet_service_server::WalletServiceServer,
};
use tonic::transport::Server;
use tonic_web::GrpcWebLayer;
use tower::{Layer, ServiceBuilder};
use tower_http::trace::{DefaultOnFailure, TraceLayer};

use crate::{
	AppState,
	services::{
		allocations::AllocationsSvc,
		balance::BalanceSvc,
		book::BookSvc,
		compartments::CompartmentSvc,
		consilium::{ConsiliumApprovalSvc, ConsiliumSvc},
		fees::FeesSvc,
		funds::FundsSvc,
		health::Health,
		invoices::InvoicePaymentSvc,
		operations::OperationsSvc,
		payments::{PaymentConfirmationSvc, PaymentConsentSvc, PaymentsSvc},
		payout_review::PayoutReviewSvc,
		users::UsersSvc,
		wallet::WalletSvc,
	},
};

pub mod allocations;
pub mod balance;
pub mod book;
pub mod compartments;
pub mod consilium;
pub mod fees;
pub mod funds;
pub mod health;
pub mod invoices;
pub mod operations;
pub mod payments;
pub mod payout_review;
pub mod scope_ledger;
pub(crate) mod support;
pub mod users;
pub mod wallet;

/// Build the core tonic server and serve it on `addr` until `shutdown` fires.
///
/// `serve_with_shutdown` drains in-flight requests on the shutdown signal rather than
/// aborting them, so a deploy/restart returns them cleanly instead of erroring the BFF.
///
/// Authorization: each data service is wrapped in the async auth layer, which
/// authorizes the request in-process via `state.authorizer` (a channel round-trip
/// to the auth task — never the network) and injects the verified `Claims` into the
/// request extensions. These are user-facing services, so the layer is pinned to the
/// **client** token class — `aud=banking-core`, `typ=access` only — rejecting an
/// inter-service token at the verifier (a `TokenClass::Service` layer is reserved for
/// future inter-service surfaces). `HealthService` is left **unwrapped** so its liveness
/// (`Check`) and readiness (`Readiness`) probes stay public for the BFF and any
/// orchestrator/LB — they are infrastructure probes, not data services.
///
/// `scope.v1.ScopeLedgerService` is NOT served here: it is the service-facing money surface,
/// on a listener of its own that speaks only mTLS under the SERVICE token class
/// ([`scope_ledger::serve`]).
///
/// `ConsiliumApprovalService` is the one DATA service left unwrapped, for the same reason
/// the auth service's own routes are: the emailed approval token IS its credential. An owner
/// answering from their mailbox may not be signed in, and requiring a session would make the
/// mail useless. Its own surface is what carries the authorization — a single-use hashed
/// token with a 72h TTL, plus a secret code compared in constant time — and every unusable
/// token gets one identical `NOT_FOUND`. `ConsiliumService`, the owners' own surface, stays
/// behind the layer like every other data service. `PaymentConsentService` is the same shape
/// for the same reason — the emailed investor is its credential — and `PaymentsService`,
/// the operators' surface, stays behind the layer.
pub async fn serve(addr: SocketAddr, state: AppState, shutdown: impl Future<Output = ()>) -> Result<(), tonic::transport::Error> {
	let auth = grpc_auth_layer(state.authorizer.for_class(TokenClass::Client));
	let health = Health::new(state.pool.clone(), state.ledger.clone(), state.configured_networks.clone());
	// k8s gRPC probes speak only the standard grpc.health.v1.Health protocol —
	// serve it alongside the richer banking.v1.HealthService (deep PG/TB readiness
	// for the BFF). Process-level SERVING is exactly what the kubelet asks of it.
	let (health_reporter, std_health) = tonic_health::server::health_reporter();
	health_reporter.set_service_status("", tonic_health::ServingStatus::Serving).await;
	Server::builder()
		// grpc-web rides HTTP/1.1; required for the GrpcWebLayer to translate.
		.accept_http1(true)
		.layer(ServiceBuilder::new().layer(TraceLayer::new_for_grpc().on_failure(DefaultOnFailure::new().level(tracing::Level::WARN))).layer(GrpcWebLayer::new()).into_inner())
		.add_service(std_health)
		.add_service(HealthServiceServer::new(health))
		.add_service(auth.layer(UsersServiceServer::new(UsersSvc::new(state.clone()))))
		.add_service(auth.layer(BalanceServiceServer::new(BalanceSvc::new(state.clone()))))
		.add_service(auth.layer(FundsServiceServer::new(FundsSvc::new(state.clone()))))
		.add_service(auth.layer(AllocationsServiceServer::new(AllocationsSvc::new(state.clone()))))
			.add_service(auth.layer(BookServiceServer::new(BookSvc::new(state.clone()))))
		.add_service(auth.layer(FeesServiceServer::new(FeesSvc::new(state.clone()))))
		.add_service(auth.layer(OperationsServiceServer::new(OperationsSvc::new(state.clone()))))
		.add_service(auth.layer(ConsiliumServiceServer::new(ConsiliumSvc::new(state.clone()))))
		.add_service(ConsiliumApprovalServiceServer::new(ConsiliumApprovalSvc::new(state.clone())))
		.add_service(auth.layer(PaymentsServiceServer::new(PaymentsSvc::new(state.clone()))))
		.add_service(auth.layer(PaymentConfirmationServiceServer::new(PaymentConfirmationSvc::new(state.clone()))))
		.add_service(PaymentConsentServiceServer::new(PaymentConsentSvc::new(state.clone())))
		.add_service(auth.layer(PayoutReviewServiceServer::new(PayoutReviewSvc::new(state.clone()))))
		.add_service(auth.layer(CompartmentServiceServer::new(CompartmentSvc::new(state.clone()))))
		.add_service(auth.layer(InvoicePaymentServiceServer::new(InvoicePaymentSvc::new(state.clone()))))
		.add_service(auth.layer(WalletServiceServer::new(WalletSvc::new(state))))
		.serve_with_shutdown(addr, shutdown)
		.await
}
