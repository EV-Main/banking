use std::{sync::Arc, time::Duration};

use evbanking_contracts::banking::v1 as bk;
use evconcierge_contracts::concierge::v1 as cc;
use tonic::{
	Request, Status,
	transport::{Channel, Endpoint},
};

use crate::{config::AppConfig, cookies::CookieNames, routes::approval::AttemptLimiter, session::BankingTokens, wallet_intents::WalletActions};

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
	/// The per-IP bound on the public approval vote — see [`crate::routes::approval`].
	/// Not a session and not a credential: the only state here that no cookie reaches.
	pub approvals: Arc<AttemptLimiter>,
	/// The per-user bound on the money mutations, keyed by the verified subject — see
	/// [`crate::routes::money_gate`].
	pub money_mutations: Arc<AttemptLimiter>,
	/// Local verifier for the shared concierge access JWT (JWKS-cached — no
	/// per-request round trip).
	pub verifier: evconcierge_auth::Verifier,
	/// The wallet actions a trusted host page may run in place; `None` (and the routes
	/// unmounted) unless `WALLET_ACTION_ORIGINS` names a host.
	pub wallet_actions: Option<Arc<WalletActions>>,
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

	fn funds(&self) -> bk::funds_service_client::FundsServiceClient<Channel> {
		bk::funds_service_client::FundsServiceClient::new(self.piggybank.clone())
	}

	fn fees(&self) -> bk::fees_service_client::FeesServiceClient<Channel> {
		bk::fees_service_client::FeesServiceClient::new(self.piggybank.clone())
	}

	fn allocations(&self) -> bk::allocations_service_client::AllocationsServiceClient<Channel> {
		bk::allocations_service_client::AllocationsServiceClient::new(self.piggybank.clone())
	}

	fn book(&self) -> bk::book_service_client::BookServiceClient<Channel> {
		bk::book_service_client::BookServiceClient::new(self.piggybank.clone())
	}

	fn health(&self) -> bk::health_service_client::HealthServiceClient<Channel> {
		bk::health_service_client::HealthServiceClient::new(self.piggybank.clone())
	}

	fn balance(&self) -> bk::balance_service_client::BalanceServiceClient<Channel> {
		bk::balance_service_client::BalanceServiceClient::new(self.piggybank.clone())
	}

	fn consilium(&self) -> bk::consilium_service_client::ConsiliumServiceClient<Channel> {
		bk::consilium_service_client::ConsiliumServiceClient::new(self.piggybank.clone())
	}

	fn consilium_approval(&self) -> bk::consilium_approval_service_client::ConsiliumApprovalServiceClient<Channel> {
		bk::consilium_approval_service_client::ConsiliumApprovalServiceClient::new(self.piggybank.clone())
	}

	fn payment_confirmations(&self) -> bk::payment_confirmation_service_client::PaymentConfirmationServiceClient<Channel> {
		bk::payment_confirmation_service_client::PaymentConfirmationServiceClient::new(self.piggybank.clone())
	}

	fn payment_consent(&self) -> bk::payment_consent_service_client::PaymentConsentServiceClient<Channel> {
		bk::payment_consent_service_client::PaymentConsentServiceClient::new(self.piggybank.clone())
	}

	fn platform(&self) -> cc::platform_service_client::PlatformServiceClient<Channel> {
		cc::platform_service_client::PlatformServiceClient::new(self.concierge.clone())
	}

	fn notifications(&self) -> cc::notification_service_client::NotificationServiceClient<Channel> {
		cc::notification_service_client::NotificationServiceClient::new(self.concierge.clone())
	}

	fn governance(&self) -> cc::governance_service_client::GovernanceServiceClient<Channel> {
		cc::governance_service_client::GovernanceServiceClient::new(self.concierge.clone())
	}

	fn payout_review(&self) -> bk::payout_review_service_client::PayoutReviewServiceClient<Channel> {
		bk::payout_review_service_client::PayoutReviewServiceClient::new(self.piggybank.clone())
	}

	fn approvals(&self) -> cc::approval_service_client::ApprovalServiceClient<Channel> {
		cc::approval_service_client::ApprovalServiceClient::new(self.concierge.clone())
	}

	fn removal_approval(&self) -> cc::owner_removal_approval_service_client::OwnerRemovalApprovalServiceClient<Channel> {
		cc::owner_removal_approval_service_client::OwnerRemovalApprovalServiceClient::new(self.concierge.clone())
	}

	// ── concierge identity plane ───────────────────────────────────────────────
	// The auth issuance RPCs (Exchange/Refresh/Logout/sessions) left with the OAuth
	// flow — the shell-owned auth surface calls them; the BFF only forwards the
	// verified user token to the directory.
	pub async fn get_me(&self, token: &str) -> Result<cc::UserProfile, Status> {
		Ok(self.directory().get_me(bearer(token, cc::GetMeRequest {})?).await?.into_inner())
	}

	pub async fn update_profile(&self, token: &str, req: cc::UpdateProfileRequest) -> Result<cc::UserProfile, Status> {
		Ok(self.directory().update_profile(bearer(token, req)?).await?.into_inner())
	}

	/// One concierge approval under the caller's own visibility (`ApprovalService.GetApproval`):
	/// an approval they may not see is the same `NOT_FOUND` as an unknown one.
	pub async fn get_approval(&self, token: &str, approval_id: &str) -> Result<cc::Approval, Status> {
		let req = cc::GetApprovalRequest {
			approval_id: approval_id.to_string(),
		};
		Ok(self.approvals().get_approval(bearer(token, req)?).await?.into_inner())
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

	pub async fn get_payout_approval(&self, token: &str, operation_id: &str) -> Result<bk::PayoutApprovalRef, Status> {
		let req = bk::GetPayoutApprovalRequest {
			operation_id: operation_id.to_string(),
		};
		Ok(self.payout_review().get_payout_approval(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_payout_lines(&self, token: &str, req: bk::ListPayoutLinesRequest) -> Result<bk::PayoutLinesPage, Status> {
		Ok(self.payout_review().list_payout_lines(bearer(token, req)?).await?.into_inner())
	}

	pub async fn subscribe(&self, token: &str, req: bk::SubscribeRequest) -> Result<bk::Subscription, Status> {
		Ok(self.funds().subscribe(bearer(token, req)?).await?.into_inner())
	}

	pub async fn redeem(&self, token: &str, req: bk::RedeemRequest) -> Result<bk::Redemption, Status> {
		Ok(self.funds().redeem(bearer(token, req)?).await?.into_inner())
	}

	pub async fn cancel_redemption(&self, token: &str, redemption_id: &str) -> Result<bk::Redemption, Status> {
		let req = bk::CancelRedemptionRequest {
			redemption_id: redemption_id.to_string(),
		};
		Ok(self.funds().cancel_redemption(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_positions(&self, token: &str) -> Result<bk::PositionList, Status> {
		Ok(self.funds().list_positions(bearer(token, bk::ListPositionsRequest {})?).await?.into_inner())
	}

	pub async fn list_redemptions(&self, token: &str) -> Result<bk::RedemptionList, Status> {
		Ok(self.funds().list_redemptions(bearer(token, bk::ListRedemptionsRequest {})?).await?.into_inner())
	}

	pub async fn list_allocations(&self, token: &str, include_unlisted: bool) -> Result<bk::AllocationList, Status> {
		let req = bk::ListAllocationsRequest { include_unlisted };
		Ok(self.allocations().list_allocations(bearer(token, req)?).await?.into_inner())
	}

	pub async fn get_allocation(&self, token: &str, service: &str) -> Result<bk::Allocation, Status> {
		let req = bk::GetAllocationRequest { service: service.to_string() };
		Ok(self.allocations().get_allocation(bearer(token, req)?).await?.into_inner())
	}

	pub async fn fund_nav(&self, token: &str, service: &str) -> Result<bk::FundNav, Status> {
		let req = bk::GetFundNavRequest { service: service.to_string() };
		Ok(self.funds().get_fund_nav(bearer(token, req)?).await?.into_inner())
	}

	/// The marks of one allocation over a window plus the caller's participation — the
	/// hub derives the second series from the caller's `sub`, so the token is the user's.
	pub async fn fund_nav_history(&self, token: &str, req: bk::ListFundNavHistoryRequest) -> Result<bk::FundNavHistory, Status> {
		Ok(self.funds().list_fund_nav_history(bearer(token, req)?).await?.into_inner())
	}

	/// A fund's fee terms. Readable by any authenticated caller — an investor is entitled
	/// to know what they are paying before they subscribe, not only after.
	pub async fn fee_policy(&self, token: &str, service: &str) -> Result<bk::FeePolicy, Status> {
		let req = bk::GetFeePolicyRequest { service: service.to_string() };
		Ok(self.fees().get_fee_policy(bearer(token, req)?).await?.into_inner())
	}

	/// What the caller's holding owes right now, without charging it — the figure a
	/// position's value has to be shown net of.
	pub async fn accrued_fees(&self, token: &str, service: &str) -> Result<bk::AccruedFees, Status> {
		let req = bk::GetAccruedFeesRequest { service: service.to_string() };
		Ok(self.fees().get_accrued_fees(bearer(token, req)?).await?.into_inner())
	}

	// ── piggybank book (the secondary market in an allocation's units) ─────────
	pub async fn place_order(&self, token: &str, req: bk::PlaceOrderRequest) -> Result<bk::Order, Status> {
		Ok(self.book().place_order(bearer(token, req)?).await?.into_inner())
	}

	pub async fn cancel_order(&self, token: &str, order_id: &str) -> Result<bk::Order, Status> {
		let req = bk::CancelOrderRequest { order_id: order_id.to_string() };
		Ok(self.book().cancel_order(bearer(token, req)?).await?.into_inner())
	}

	/// `service` empty = every allocation.
	pub async fn list_open_orders(&self, token: &str, service: &str) -> Result<bk::OrderList, Status> {
		let req = bk::ListOpenOrdersRequest { service: service.to_string() };
		Ok(self.book().list_open_orders(bearer(token, req)?).await?.into_inner())
	}

	/// `service` empty = every allocation; `limit` 0 = the hub's default page.
	pub async fn list_order_history(&self, token: &str, service: &str, limit: u32) -> Result<bk::OrderList, Status> {
		let req = bk::ListOrderHistoryRequest {
			service: service.to_string(),
			limit,
		};
		Ok(self.book().list_order_history(bearer(token, req)?).await?.into_inner())
	}

	/// `service` empty = every allocation; `limit` 0 = the hub's default page.
	pub async fn list_user_trades(&self, token: &str, service: &str, limit: u32) -> Result<bk::TradeList, Status> {
		let req = bk::ListUserTradesRequest {
			service: service.to_string(),
			limit,
		};
		Ok(self.book().list_user_trades(bearer(token, req)?).await?.into_inner())
	}

	/// `depth` 0 = the hub's default number of levels a side.
	pub async fn get_book(&self, token: &str, service: &str, depth: u32) -> Result<bk::BookSnapshot, Status> {
		let req = bk::GetBookRequest {
			service: service.to_string(),
			depth,
		};
		Ok(self.book().get_book(bearer(token, req)?).await?.into_inner())
	}

	/// The public tape; `limit` 0 = the hub's default page.
	pub async fn list_trades(&self, token: &str, service: &str, limit: u32) -> Result<bk::TradeList, Status> {
		let req = bk::ListTradesRequest {
			service: service.to_string(),
			limit,
		};
		Ok(self.book().list_trades(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_candles(&self, token: &str, req: bk::ListCandlesRequest) -> Result<bk::CandleList, Status> {
		Ok(self.book().list_candles(bearer(token, req)?).await?.into_inner())
	}

	/// The live book feed for one allocation: one frame now, then one per change. The
	/// per-RPC timeout bounds only the response headers, not the body, so the stream
	/// outlives [`REQUEST_TIMEOUT`] by design.
	pub async fn watch_book(&self, token: &str, service: &str, depth: u32) -> Result<tonic::Streaming<bk::BookEvent>, Status> {
		let req = bk::WatchBookRequest {
			service: service.to_string(),
			depth,
		};
		Ok(self.book().watch_book(bearer(token, req)?).await?.into_inner())
	}

	pub async fn get_book_policy(&self, token: &str, service: &str) -> Result<bk::BookPolicy, Status> {
		let req = bk::GetBookPolicyRequest { service: service.to_string() };
		Ok(self.book().get_book_policy(bearer(token, req)?).await?.into_inner())
	}

	pub async fn platform_config(&self, token: &str) -> Result<cc::PlatformConfig, Status> {
		Ok(self.platform().get_platform_config(bearer(token, cc::GetPlatformConfigRequest {})?).await?.into_inner())
	}

	// ── concierge notification plane ───────────────────────────────────────────
	// All self-service: concierge resolves the subscriber from the forwarded user
	// token, so the BFF never names a subscriber and cannot read another user's inbox.

	pub async fn list_notifications(&self, token: &str, req: cc::ListNotificationsRequest) -> Result<cc::ListNotificationsResponse, Status> {
		Ok(self.notifications().list_notifications(bearer(token, req)?).await?.into_inner())
	}

	pub async fn unread_notifications(&self, token: &str) -> Result<cc::GetUnreadCountResponse, Status> {
		Ok(self.notifications().get_unread_count(bearer(token, cc::GetUnreadCountRequest {})?).await?.into_inner())
	}

	pub async fn mark_notifications_read(&self, token: &str, req: cc::MarkReadRequest) -> Result<cc::MarkReadResponse, Status> {
		Ok(self.notifications().mark_read(bearer(token, req)?).await?.into_inner())
	}

	pub async fn notification_settings(&self, token: &str) -> Result<cc::NotificationSettings, Status> {
		Ok(self
			.notifications()
			.get_notification_settings(bearer(token, cc::GetNotificationSettingsRequest {})?)
			.await?
			.into_inner())
	}

	pub async fn set_notification_channel(&self, token: &str, req: cc::SetChannelEnabledRequest) -> Result<cc::NotificationSettings, Status> {
		Ok(self.notifications().set_channel_enabled(bearer(token, req)?).await?.into_inner())
	}

	pub async fn set_notification_topic(&self, token: &str, req: cc::SetTopicSubscriptionRequest) -> Result<cc::NotificationSettings, Status> {
		Ok(self.notifications().set_topic_subscription(bearer(token, req)?).await?.into_inner())
	}

	// ── concierge ownership plane (governance) ─────────────────────────────────
	// A seat is `users.role`, a concierge-owned fact, so only this plane may mutate it
	// and only this plane may authorize the mutation. Every call here forwards the
	// CONCIERGE token — the banking money token has a different issuer and audience, and
	// the separation is what stops one plane's credential authorizing the other's
	// decisions. The money plane runs its own consilium over its own mirrored roster.

	pub async fn list_owners(&self, token: &str) -> Result<cc::OwnerList, Status> {
		Ok(self.governance().list_owners(bearer(token, cc::ListOwnersRequest {})?).await?.into_inner())
	}

	pub async fn resign_ownership(&self, token: &str, confirm_email: &str) -> Result<cc::OwnerList, Status> {
		let req = cc::ResignOwnershipRequest {
			confirm_email: confirm_email.to_string(),
		};
		Ok(self.governance().resign_ownership(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_owner_removals(&self, token: &str, limit: u32) -> Result<cc::OwnerRemovalList, Status> {
		let req = cc::ListOwnerRemovalsRequest { limit };
		Ok(self.governance().list_owner_removals(bearer(token, req)?).await?.into_inner())
	}

	pub async fn open_owner_removal(&self, token: &str, target_user_id: &str, reason: &str) -> Result<cc::OwnerRemoval, Status> {
		let req = cc::OpenOwnerRemovalRequest {
			target_user_id: target_user_id.to_string(),
			reason: reason.to_string(),
		};
		Ok(self.governance().open_owner_removal(bearer(token, req)?).await?.into_inner())
	}

	pub async fn submit_peer_vote(&self, token: &str, removal_id: &str, vote: cc::RemovalVote) -> Result<cc::OwnerRemoval, Status> {
		let req = cc::SubmitPeerVoteRequest {
			removal_id: removal_id.to_string(),
			vote: vote as i32,
		};
		Ok(self.governance().submit_peer_vote(bearer(token, req)?).await?.into_inner())
	}

	pub async fn cancel_owner_removal(&self, token: &str, removal_id: &str) -> Result<cc::OwnerRemoval, Status> {
		let req = cc::CancelOwnerRemovalRequest { removal_id: removal_id.to_string() };
		Ok(self.governance().cancel_owner_removal(bearer(token, req)?).await?.into_inner())
	}

	// Admission is the ONLY way `Role::Owner` is granted — `SetRole` refuses it outside an
	// executed admission — and it passes only on unanimity of every other owner, so a
	// minority can never grow itself into a majority ahead of opening a payout.

	pub async fn list_owner_admissions(&self, token: &str, limit: u32) -> Result<cc::OwnerAdmissionList, Status> {
		let req = cc::ListOwnerAdmissionsRequest { limit };
		Ok(self.governance().list_owner_admissions(bearer(token, req)?).await?.into_inner())
	}

	pub async fn open_owner_admission(&self, token: &str, candidate_user_id: &str, reason: &str) -> Result<cc::OwnerAdmission, Status> {
		let req = cc::OpenOwnerAdmissionRequest {
			candidate_user_id: candidate_user_id.to_string(),
			reason: reason.to_string(),
		};
		Ok(self.governance().open_owner_admission(bearer(token, req)?).await?.into_inner())
	}

	pub async fn submit_admission_vote(&self, token: &str, admission_id: &str, vote: cc::AdmissionVote) -> Result<cc::OwnerAdmission, Status> {
		let req = cc::SubmitAdmissionVoteRequest {
			admission_id: admission_id.to_string(),
			vote: vote as i32,
		};
		Ok(self.governance().submit_admission_vote(bearer(token, req)?).await?.into_inner())
	}

	pub async fn cancel_owner_admission(&self, token: &str, admission_id: &str) -> Result<cc::OwnerAdmission, Status> {
		let req = cc::CancelOwnerAdmissionRequest {
			admission_id: admission_id.to_string(),
		};
		Ok(self.governance().cancel_owner_admission(bearer(token, req)?).await?.into_inner())
	}

	// The owners' verdict over one PERSON's standing — the other half of the split
	// blocking verb, plus the admin seat. Three kinds share one message because all three
	// ask the same question, and they pass on a MAJORITY of the snapshotted voters rather
	// than the unanimity the OWNER consilia demand: a hold lapses in 24h, so ratifying one
	// races a clock, and under unanimity one unreachable owner would not delay the verdict
	// — they would decide it by releasing a compromised account at the deadline.

	pub async fn list_user_proposals(&self, token: &str, limit: u32, kind: cc::UserProposalKind) -> Result<cc::UserProposalList, Status> {
		let req = cc::ListUserProposalsRequest { limit, kind: kind as i32 };
		Ok(self.governance().list_user_proposals(bearer(token, req)?).await?.into_inner())
	}

	/// Make a hold permanent. The reason is required upstream and here: it is what the
	/// other owners are voting on.
	pub async fn open_user_suspension(&self, token: &str, user_id: &str, reason: &str) -> Result<cc::UserProposal, Status> {
		let req = cc::OpenUserSuspensionRequest {
			user_id: user_id.to_string(),
			reason: reason.to_string(),
		};
		Ok(self.governance().open_user_suspension(bearer(token, req)?).await?.into_inner())
	}

	/// Lift a suspension THE OWNERS imposed. A hold needs nothing from here — one admin
	/// lifts it in a single act.
	pub async fn open_user_reinstatement(&self, token: &str, user_id: &str, reason: &str) -> Result<cc::UserProposal, Status> {
		let req = cc::OpenUserReinstatementRequest {
			user_id: user_id.to_string(),
			reason: reason.to_string(),
		};
		Ok(self.governance().open_user_reinstatement(bearer(token, req)?).await?.into_inner())
	}

	/// Grant `Role::Admin`. `SetRole` refuses that role in the GRANTING direction only:
	/// an operator who can appoint operators can appoint accomplices. Taking it away stays
	/// one act — containing a rogue operator must never be the slower path.
	pub async fn open_admin_admission(&self, token: &str, user_id: &str, reason: &str) -> Result<cc::UserProposal, Status> {
		let req = cc::OpenAdminAdmissionRequest {
			user_id: user_id.to_string(),
			reason: reason.to_string(),
		};
		Ok(self.governance().open_admin_admission(bearer(token, req)?).await?.into_inner())
	}

	pub async fn submit_user_proposal_vote(&self, token: &str, proposal_id: &str, vote: cc::ProposalVote) -> Result<cc::UserProposal, Status> {
		let req = cc::SubmitUserProposalVoteRequest {
			proposal_id: proposal_id.to_string(),
			vote: vote as i32,
		};
		Ok(self.governance().submit_user_proposal_vote(bearer(token, req)?).await?.into_inner())
	}

	pub async fn cancel_user_proposal(&self, token: &str, proposal_id: &str) -> Result<cc::UserProposal, Status> {
		let req = cc::CancelUserProposalRequest {
			proposal_id: proposal_id.to_string(),
		};
		Ok(self.governance().cancel_user_proposal(bearer(token, req)?).await?.into_inner())
	}

	/// The live ownership feed. ONE revision covers removals, admissions AND user
	/// proposals together, so a single subscription follows the whole governance surface.
	pub async fn watch_governance(&self, token: &str) -> Result<tonic::Streaming<cc::GovernanceTick>, Status> {
		Ok(self.governance().watch_governance(bearer(token, cc::WatchGovernanceRequest {})?).await?.into_inner())
	}

	// The two below carry NO bearer: the emailed token IS the credential and the target
	// clicking it may not be signed in.

	pub async fn removal_invitation(&self, token: &str) -> Result<cc::OwnerRemovalInvitation, Status> {
		let req = cc::GetRemovalInvitationRequest { token: token.to_string() };
		Ok(self.removal_approval().get_invitation(req).await?.into_inner())
	}

	pub async fn submit_self_decision(&self, req: cc::SubmitSelfDecisionRequest) -> Result<cc::SubmitSelfDecisionResponse, Status> {
		Ok(self.removal_approval().submit_self_decision(req).await?.into_inner())
	}

	// ── consilium: multi-owner authorization over the platform's own money ──────
	// The tally lives in the MONEY plane, against the owner roster this plane already
	// mirrors: `docs/ARCHITECTURE.md` rejects letting a concierge-signed artifact
	// authorize money movement, and a consilium verdict is exactly such an artifact.
	// So these carry the banking money token, never the concierge one.

	pub async fn list_consilia(&self, token: &str, limit: u32) -> Result<bk::ConsiliumList, Status> {
		let req = bk::ListConsiliaRequest { limit };
		Ok(self.consilium().list_consilia(bearer(token, req)?).await?.into_inner())
	}

	pub async fn get_consilium(&self, token: &str, consilium_id: &str) -> Result<bk::Consilium, Status> {
		let req = bk::GetConsiliumRequest {
			consilium_id: consilium_id.to_string(),
		};
		Ok(self.consilium().get_consilium(bearer(token, req)?).await?.into_inner())
	}

	/// Seat a new holder of a reserved allocation (#245): `units` of `fee` or `fund` for a
	/// person, minted once the owners' quorum carries. Same plane and same token as the
	/// other opens: the units are a share of the owners' money.
	pub async fn open_holder_grant(&self, token: &str, terms: bk::HolderGrantTerms) -> Result<bk::Consilium, Status> {
		let req = bk::OpenHolderGrantRequest { terms: Some(terms) };
		Ok(self.consilium().open_holder_grant(bearer(token, req)?).await?.into_inner())
	}

	pub async fn cancel_consilium(&self, token: &str, consilium_id: &str) -> Result<bk::Consilium, Status> {
		let req = bk::CancelConsiliumRequest {
			consilium_id: consilium_id.to_string(),
		};
		Ok(self.consilium().cancel_consilium(bearer(token, req)?).await?.into_inner())
	}

	// The two below carry NO bearer, deliberately: the emailed token IS the credential and
	// the owner clicking it may not be signed in. Attaching a user token here would invent
	// an authorization the flow does not have.

	/// The redacted invitation behind an emailed token. Side-effect free — mail scanners
	/// issue automatic GETs for every URL in a message.
	pub async fn consilium_invitation(&self, token: &str) -> Result<bk::ConsiliumInvitation, Status> {
		let req = bk::GetInvitationRequest { token: token.to_string() };
		Ok(self.consilium_approval().get_invitation(req).await?.into_inner())
	}

	/// Cast the emailed owner's vote. `client_ip`/`user_agent` on the request are AUDIT
	/// fields only; the plane never treats them as authorization.
	pub async fn submit_consilium_decision(&self, req: bk::SubmitDecisionRequest) -> Result<bk::SubmitDecisionResponse, Status> {
		Ok(self.consilium_approval().submit_decision(req).await?.into_inner())
	}

	// ── payments: the subject's consent to an order ─────────────────────────────
	// No bearer on the two below, for the same reason as the consilium approval pair: the
	// emailed token is the credential, and the investor clicking it may not be signed in.

	/// The redacted consent invitation behind an emailed token. Side-effect free.
	pub async fn payment_consent_invitation(&self, token: &str) -> Result<bk::PaymentConsentInvitation, Status> {
		let req = bk::GetConsentInvitationRequest { token: token.to_string() };
		Ok(self.payment_consent().get_consent_invitation(req).await?.into_inner())
	}

	/// Record the emailed investor's answer. `client_ip`/`user_agent` are AUDIT fields
	/// only; the plane never treats them as authorization.
	pub async fn submit_payment_consent(&self, req: bk::SubmitConsentRequest) -> Result<bk::SubmitConsentResponse, Status> {
		Ok(self.payment_consent().submit_consent(req).await?.into_inner())
	}

	pub async fn operations_mode(&self, token: &str) -> Result<bk::OperationsMode, Status> {
		Ok(self.balance().get_operations_mode(bearer(token, bk::GetOperationsModeRequest {})?).await?.into_inner())
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
