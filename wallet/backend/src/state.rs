use std::{sync::Arc, time::Duration};

use evbanking_contracts::banking::v1 as bk;
use evconcierge_contracts::concierge::v1 as cc;
use tonic::{
	Request, Status,
	transport::{Channel, Endpoint},
};

use crate::{config::AppConfig, cookies::CookieNames, routes::AttemptLimiter, session::BankingTokens, wallet_intents::WalletActions};

/// Cap on establishing a TCP/TLS connection to an upstream plane: a black-holed or
/// half-open replica must fail fast rather than wedge the awaiting request task.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Per-RPC deadline applied to every upstream call. tonic has no default request
/// timeout, so without this a wedged plane keeps the browser-facing task (and the
/// transparent-refresh single-flight lock in [`session`](crate::session)) alive
/// indefinitely. The router's [`TimeoutLayer`](tower_http::timeout::TimeoutLayer) is a
/// slightly looser outer bound, so an upstream stall surfaces here first as an error.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared application state, cheaply cloneable per request.
#[derive(Clone)]
pub struct AppState {
	pub config: Arc<AppConfig>,
	pub grpc: Grpc,
	/// The per-user banking money-pair cache (the one server-side state left — auth
	/// is shell-owned and the request credential is the verified access-JWT cookie).
	pub banking: Arc<BankingTokens>,
	pub cookies: Arc<CookieNames>,
	/// The per-user bound on the money mutations, keyed by the verified subject — see
	/// [`crate::routes::money_gate`].
	pub money_mutations: Arc<AttemptLimiter>,
	/// Local verifier for the shared concierge access JWT (JWKS-cached — no
	/// per-request round trip).
	pub verifier: evconcierge_auth::Verifier,
	/// The wallet actions the intent pages run: the wallet's own and the trusted hosts'.
	pub wallet_actions: Arc<WalletActions>,
}

/// gRPC egress to both planes. Channels are lazily connected and cheap to clone, so a
/// fresh typed client is built per call from the shared channel.
#[derive(Clone)]
pub struct Grpc {
	piggybank: Channel,
	banking_auth: Channel,
	concierge: Channel,
	/// The shared bearer presented on the banking `IssueUserToken` seam. `None` ⇒ no
	/// money-plane token is minted (money routes surface `NotConfigured`).
	banking_issuance_token: Option<Arc<str>>,
}
impl Grpc {
	pub fn connect_lazy(piggybank_addr: &str, banking_auth_addr: &str, concierge_addr: &str, banking_issuance_token: Option<String>) -> color_eyre::Result<Self> {
		let piggybank = endpoint(piggybank_addr)?.connect_lazy();
		let banking_auth = endpoint(banking_auth_addr)?.connect_lazy();
		let concierge = endpoint(concierge_addr)?.connect_lazy();
		Ok(Self {
			piggybank,
			banking_auth,
			concierge,
			banking_issuance_token: banking_issuance_token.map(Arc::from),
		})
	}

	fn banking_auth(&self) -> bk::auth_service_client::AuthServiceClient<Channel> {
		bk::auth_service_client::AuthServiceClient::new(self.banking_auth.clone())
	}

	fn directory(&self) -> cc::user_directory_client::UserDirectoryClient<Channel> {
		cc::user_directory_client::UserDirectoryClient::new(self.concierge.clone())
	}

	fn wallet(&self) -> bk::wallet_service_client::WalletServiceClient<Channel> {
		bk::wallet_service_client::WalletServiceClient::new(self.piggybank.clone())
	}

	fn invoice_payments(&self) -> bk::invoice_payment_service_client::InvoicePaymentServiceClient<Channel> {
		bk::invoice_payment_service_client::InvoicePaymentServiceClient::new(self.piggybank.clone())
	}

	fn compartments(&self) -> bk::compartment_service_client::CompartmentServiceClient<Channel> {
		bk::compartment_service_client::CompartmentServiceClient::new(self.piggybank.clone())
	}

	fn operations(&self) -> bk::operations_service_client::OperationsServiceClient<Channel> {
		bk::operations_service_client::OperationsServiceClient::new(self.piggybank.clone())
	}

	fn health(&self) -> bk::health_service_client::HealthServiceClient<Channel> {
		bk::health_service_client::HealthServiceClient::new(self.piggybank.clone())
	}

	fn payment_confirmations(&self) -> bk::payment_confirmation_service_client::PaymentConfirmationServiceClient<Channel> {
		bk::payment_confirmation_service_client::PaymentConfirmationServiceClient::new(self.piggybank.clone())
	}

	// ── concierge identity plane ───────────────────────────────────────────────
	// The auth issuance RPCs (Exchange/Refresh/Logout/sessions) left with the OAuth
	// flow — the shell-owned auth surface calls them; the BFF only forwards the
	// verified user token to the directory.
	pub async fn get_me(&self, token: &str) -> Result<cc::UserProfile, Status> {
		Ok(self.directory().get_me(bearer(token, cc::GetMeRequest {})?).await?.into_inner())
	}

	// ── banking auth plane (money-plane token issuance) ─────────────────────────
	/// Mint the money-plane token pair for the concierge-authenticated user — the
	/// concierge→banking exchange seam. Authenticated by the shared issuance token (NOT a
	/// user token); banking maps the concierge id to its bridge-mirrored row. Errors with
	/// `UNAVAILABLE` when issuance is not configured, so the caller leaves the pair empty.
	pub async fn issue_banking_token(&self, concierge_user_id: &str, user_agent: &str, ip: &str) -> Result<bk::TokenResponse, Status> {
		let token = self.banking_issuance_token.as_deref().ok_or_else(|| Status::unavailable("banking issuance not configured"))?;
		let req = bk::IssueUserTokenRequest {
			concierge_user_id: concierge_user_id.to_string(),
			user_agent: user_agent.to_string(),
			ip: ip.to_string(),
		};
		Ok(self.banking_auth().issue_user_token(bearer(token, req)?).await?.into_inner())
	}

	/// Rotate the money-plane refresh token (the banking-side family), independent of the
	/// concierge refresh. The public credential is the refresh token, not a user token.
	pub async fn refresh_banking_token(&self, refresh_token: &str) -> Result<bk::TokenResponse, Status> {
		let req = bk::RefreshRequest {
			refresh_token: refresh_token.to_string(),
		};
		Ok(self.banking_auth().refresh(req).await?.into_inner())
	}

	// ── piggybank money plane ──────────────────────────────────────────────────
	pub async fn check(&self) -> Result<bk::CheckResponse, Status> {
		Ok(self.health().check(bk::CheckRequest {}).await?.into_inner())
	}

	pub async fn get_wallet(&self, token: &str) -> Result<bk::Wallet, Status> {
		Ok(self.wallet().get_wallet(bearer(token, bk::GetWalletRequest {})?).await?.into_inner())
	}

	pub async fn deposit_address(&self, token: &str, network: &str) -> Result<bk::DepositAddress, Status> {
		let req = bk::GetDepositAddressRequest { network: network.to_string() };
		Ok(self.wallet().get_deposit_address(bearer(token, req)?).await?.into_inner())
	}

	pub async fn request_withdrawal(&self, token: &str, req: bk::RequestWithdrawalRequest) -> Result<bk::Withdrawal, Status> {
		Ok(self.wallet().request_withdrawal(bearer(token, req)?).await?.into_inner())
	}

	pub async fn cancel_withdrawal(&self, token: &str, withdrawal_id: &str) -> Result<bk::Withdrawal, Status> {
		let req = bk::CancelWithdrawalRequest {
			withdrawal_id: withdrawal_id.to_string(),
		};
		Ok(self.wallet().cancel_withdrawal(bearer(token, req)?).await?.into_inner())
	}

	pub async fn get_payable_invoice(&self, token: &str, invoice_id: &str) -> Result<bk::PayableInvoice, Status> {
		let req = bk::GetPayableInvoiceRequest { invoice_id: invoice_id.to_string() };
		Ok(self.invoice_payments().get_payable_invoice(bearer(token, req)?).await?.into_inner())
	}

	pub async fn pay_invoice(&self, token: &str, req: bk::PayInvoiceRequest) -> Result<bk::PayableInvoice, Status> {
		Ok(self.invoice_payments().pay_invoice(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_compartments(&self, token: &str) -> Result<bk::CompartmentList, Status> {
		Ok(self.compartments().list_compartments(bearer(token, bk::ListCompartmentsRequest {})?).await?.into_inner())
	}

	pub async fn get_compartment(&self, token: &str, scope: &str) -> Result<bk::Compartment, Status> {
		let req = bk::GetCompartmentRequest { scope: scope.to_string() };
		Ok(self.compartments().get_compartment(bearer(token, req)?).await?.into_inner())
	}

	pub async fn move_funds(&self, token: &str, req: bk::MoveFundsRequest) -> Result<bk::CompartmentMove, Status> {
		Ok(self.compartments().move_funds(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_payment_confirmations(&self, token: &str) -> Result<bk::PaymentConfirmationList, Status> {
		Ok(self
			.payment_confirmations()
			.list_payment_confirmations(bearer(token, bk::ListPaymentConfirmationsRequest {})?)
			.await?
			.into_inner())
	}

	pub async fn get_payment_confirmation(&self, token: &str, payment_id: &str) -> Result<bk::PaymentConfirmation, Status> {
		let req = bk::GetPaymentConfirmationRequest { payment_id: payment_id.to_string() };
		Ok(self.payment_confirmations().get_payment_confirmation(bearer(token, req)?).await?.into_inner())
	}

	pub async fn answer_payment_confirmation(&self, token: &str, req: bk::AnswerPaymentConfirmationRequest) -> Result<bk::PaymentConfirmation, Status> {
		Ok(self.payment_confirmations().answer_payment_confirmation(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_mandates(&self, token: &str) -> Result<bk::MandateList, Status> {
		Ok(self.compartments().list_mandates(bearer(token, bk::ListMandatesRequest {})?).await?.into_inner())
	}

	pub async fn get_mandate_request(&self, token: &str, request_id: &str) -> Result<bk::MandateRequest, Status> {
		let req = bk::GetMandateRequestRequest { request_id: request_id.to_string() };
		Ok(self.compartments().get_mandate_request(bearer(token, req)?).await?.into_inner())
	}

	pub async fn grant_mandate(&self, token: &str, req: bk::GrantMandateRequest) -> Result<bk::Mandate, Status> {
		Ok(self.compartments().grant_mandate(bearer(token, req)?).await?.into_inner())
	}

	pub async fn revoke_mandate(&self, token: &str, mandate_id: &str, audit: bk::AuditContext) -> Result<bk::Mandate, Status> {
		let req = bk::RevokeMandateRequest {
			mandate_id: mandate_id.to_string(),
			audit: Some(audit),
		};
		Ok(self.compartments().revoke_mandate(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_withdrawals(&self, token: &str) -> Result<bk::WithdrawalList, Status> {
		Ok(self.wallet().list_withdrawals(bearer(token, bk::ListWithdrawalsRequest {})?).await?.into_inner())
	}

	pub async fn list_deposits(&self, token: &str) -> Result<bk::DepositList, Status> {
		Ok(self.wallet().list_deposits(bearer(token, bk::ListDepositsRequest {})?).await?.into_inner())
	}

	pub async fn list_operations(&self, token: &str, limit: u32) -> Result<bk::OperationList, Status> {
		let req = bk::ListOperationsRequest { limit };
		Ok(self.operations().list_operations(bearer(token, req)?).await?.into_inner())
	}
}

/// A lazily-connected upstream `Endpoint` with explicit connect + per-RPC deadlines, so a
/// degraded plane fails fast instead of stalling the awaiting task indefinitely.
fn endpoint(addr: &str) -> color_eyre::Result<Endpoint> {
	Ok(Endpoint::from_shared(addr.to_string())?.connect_timeout(CONNECT_TIMEOUT).timeout(REQUEST_TIMEOUT))
}

/// Attach the user's access token as `authorization: Bearer …` metadata — the inbound
/// auth layer on each plane reads exactly that.
// `tonic::Status` is a large error type; boxing it at every call site buys nothing here.
#[allow(clippy::result_large_err)]
fn bearer<T>(token: &str, msg: T) -> Result<Request<T>, Status> {
	let mut req = Request::new(msg);
	let value = format!("Bearer {token}").parse().map_err(|_| Status::unauthenticated("invalid bearer token"))?;
	req.metadata_mut().insert("authorization", value);
	Ok(req)
}

#[cfg(test)]
mod tests {
	use std::{net::TcpListener, time::Instant};

	use super::*;

	/// A half-open upstream — a port whose TCP connection completes but never speaks HTTP/2
	/// — is the worst case for the missing timeout: the connect succeeds, then the RPC awaits
	/// a response that never comes. A bound listener that we never `accept()` is exactly that:
	/// the OS backlog completes the handshake, but no gRPC frame is ever served. With the
	/// per-RPC `timeout` on the `Endpoint`, the call must return an error well inside the
	/// layer's outer deadline rather than hanging; an outer guard fails loudly on regression.
	#[tokio::test]
	async fn upstream_rpc_fails_fast_against_a_half_open_plane() {
		let listener = TcpListener::bind("127.0.0.1:0").expect("bind black-hole listener");
		let addr = listener.local_addr().unwrap();

		let grpc = Grpc::connect_lazy(&format!("http://{addr}"), &format!("http://{addr}"), &format!("http://{addr}"), None).expect("build lazy channels");

		let started = Instant::now();
		let guard = tokio::time::timeout(REQUEST_TIMEOUT + Duration::from_secs(5), grpc.check()).await;

		let result = guard.expect("the call must return within the deadline, not hang");
		assert!(result.is_err(), "a half-open plane must surface an error, not a hung await");
		assert!(started.elapsed() < REQUEST_TIMEOUT + Duration::from_secs(2), "the per-RPC timeout must bound the call");

		drop(listener);
	}
}
