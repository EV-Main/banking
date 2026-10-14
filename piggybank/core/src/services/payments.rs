//! `payments` context — the two gRPC surfaces of the payment order.
//!
//! [`PaymentsSvc`] sits **behind** the user-auth layer and is Admin/Owner-only: opening,
//! reading and withdrawing an order are things a signed-in operator does. What an order then
//! NEEDS — the owners' quorum, or the investor's consent — is seated by the application layer
//! from the order's source; there is no RPC that skips it.
//!
//! [`PaymentConsentSvc`] is mounted **outside** it, exactly as `ConsiliumApprovalService` is.
//! The emailed token IS the credential there, and the investor clicking a link in their
//! mailbox may well not be signed in. Everything that surface returns is deliberately narrow,
//! and every unusable token leaves by the same door.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large type we
//! don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use concierge_domain::authz::bank;
use domain::{
	balance::{Party, ServiceId},
	error::DomainError,
	money::{Network, Usdt, WalletAddress},
	payments::{PaymentDestination, PaymentId, PaymentReason, PaymentState, PaymentTerms},
	users::{ConciergeUserId, UserId, mask_email},
};
use evbanking_contracts::banking::v1::{self as pb, payment_consent_service_server::PaymentConsentService, payments_service_server::PaymentsService};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{
	AppState,
	application::payments as payments_app,
	ports::payments::{ConsentAudit, ConsentDecision, ConsentInvitation, EndDetail, PaymentFilter, PaymentView},
	services::support::{MAX_AUDIT_IP_BYTES, MAX_AUDIT_USER_AGENT_BYTES, caller_id, clamp, map_err, require_permission, unix_now},
};

/// The default page size for the payment history.
const DEFAULT_LIST_LIMIT: u32 = 50;
const MAX_LIST_LIMIT: u32 = 200;

#[derive(Clone)]
pub struct PaymentsSvc {
	pub state: AppState,
}

impl PaymentsSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}
}

#[derive(Clone)]
pub struct PaymentConsentSvc {
	pub state: AppState,
}

impl PaymentConsentSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}
}

impl AppState {
	fn payment_ports(&self) -> payments_app::PaymentPorts<'_> {
		payments_app::PaymentPorts {
			payments: self.payments.as_ref(),
			consilia: self.consilia.as_ref(),
			users: self.users.as_ref(),
			withdrawals: self.withdrawals.as_ref(),
			ledger: self.ledger.as_ref(),
			custody: self.custody.as_ref(),
			policy: self.outflow.as_ref(),
			allocations: self.allocations.as_ref(),
			relay: &self.relay_notify,
			configured: &self.configured_networks,
			kyc: self.kyc_gate,
			approval_url_base: &self.consilium_approval_url_base,
			consent_url_base: &self.payment_consent_url_base,
			governance_mail_wired: crate::infrastructure::governance_mail::is_wired(),
		}
	}

	/// One end of a payment as the wire names it, resolved into the money plane.
	///
	/// A `user` arrives under the CONCIERGE id the console carries (the identity plane's
	/// `ListUsers`), so it is resolved concierge-first through the bridge mirror, falling back
	/// to a banking id for a money-plane caller — the same two-step `GetUserBalance` takes. An
	/// id matching neither is NOT_FOUND, never a party that happens to parse.
	async fn parse_party(&self, party: Option<&pb::Party>) -> Result<Party, Status> {
		let party = party.ok_or_else(|| Status::invalid_argument("a party is required"))?;
		match party.kind.as_str() {
			// The retired kinds (#245) are refused by name rather than falling through to
			// "unknown": a console still sending them is told where the money went.
			"piggybank" | "revenue" => Err(Status::invalid_argument(format!(
				"party kind '{}' is retired: the fund's money is the fee and fund allocations — use {{kind: \"service\", id: \"fee\" | \"fund\"}}",
				party.kind
			))),
			"service" => ServiceId::parse(&party.id).map(Party::Service).map_err(map_err),
			// banking's own treasury (D17). The id is optional on the wire; there is one.
			"treasury" if party.id.is_empty() || party.id == Party::Treasury.id_str() => Ok(Party::Treasury),
			"treasury" => Err(Status::invalid_argument(format!("treasury party id must be empty or {}", Party::Treasury.id_str()))),
			"user" => {
				let raw = Uuid::parse_str(&party.id).map_err(|_| Status::invalid_argument("invalid user id"))?;
				let resolved = match self.users.resolve_issuance_by_concierge_id(ConciergeUserId::from_raw(raw)).await.map_err(map_err)? {
					Some(target) => target.user_id,
					None =>
						self.users
							.resolve_issuance_by_banking_id(UserId::from_raw(raw))
							.await
							.map_err(map_err)?
							.ok_or_else(|| Status::not_found("user"))?
							.user_id,
				};
				Ok(Party::User(resolved))
			}
			other => Err(Status::invalid_argument(format!("unknown party kind: {other}"))),
		}
	}

	async fn parse_destination(&self, destination: Option<&pb::PaymentDestination>) -> Result<PaymentDestination, Status> {
		let target = destination.and_then(|d| d.target.as_ref()).ok_or_else(|| Status::invalid_argument("a destination is required"))?;
		match target {
			pb::payment_destination::Target::Internal(party) => Ok(PaymentDestination::Internal(self.parse_party(Some(party)).await?)),
			pb::payment_destination::Target::External(external) => {
				let network = Network::parse(&external.network).map_err(map_err)?;
				let address = WalletAddress::parse(network, &external.address).map_err(map_err)?;
				Ok(PaymentDestination::External { network, address })
			}
		}
	}
}

fn parse_payment_id(raw: &str) -> Result<PaymentId, Status> {
	Uuid::parse_str(raw).map(PaymentId::from_raw).map_err(|_| Status::invalid_argument("invalid payment id"))
}

fn state_to_proto(state: PaymentState) -> i32 {
	let mapped = match state {
		PaymentState::Pending => pb::PaymentState::Pending,
		PaymentState::Approved => pb::PaymentState::Approved,
		PaymentState::Executed => pb::PaymentState::Executed,
		PaymentState::ExecutionFailed => pb::PaymentState::ExecutionFailed,
		PaymentState::Rejected => pb::PaymentState::Rejected,
		PaymentState::Expired => pb::PaymentState::Expired,
		PaymentState::Cancelled => pb::PaymentState::Cancelled,
	};
	mapped as i32
}

/// `UNSPECIFIED` is "every state" on a filter, and nothing else is accepted.
fn state_from_proto(raw: i32) -> Result<Option<PaymentState>, Status> {
	match pb::PaymentState::try_from(raw) {
		Ok(pb::PaymentState::Unspecified) => Ok(None),
		Ok(pb::PaymentState::Pending) => Ok(Some(PaymentState::Pending)),
		Ok(pb::PaymentState::Approved) => Ok(Some(PaymentState::Approved)),
		Ok(pb::PaymentState::Executed) => Ok(Some(PaymentState::Executed)),
		Ok(pb::PaymentState::ExecutionFailed) => Ok(Some(PaymentState::ExecutionFailed)),
		Ok(pb::PaymentState::Rejected) => Ok(Some(PaymentState::Rejected)),
		Ok(pb::PaymentState::Expired) => Ok(Some(PaymentState::Expired)),
		Ok(pb::PaymentState::Cancelled) => Ok(Some(PaymentState::Cancelled)),
		Err(_) => Err(Status::invalid_argument("unknown payment state")),
	}
}

fn decision_to_proto(decision: ConsentDecision) -> i32 {
	let mapped = match decision {
		ConsentDecision::Pending => pb::ConsentDecision::Pending,
		ConsentDecision::Approve => pb::ConsentDecision::Approve,
		ConsentDecision::Reject => pb::ConsentDecision::Reject,
	};
	mapped as i32
}

/// `PENDING` is not an answer, so it is refused rather than silently recorded as one.
fn decision_from_proto(raw: i32) -> Result<ConsentDecision, Status> {
	match pb::ConsentDecision::try_from(raw) {
		Ok(pb::ConsentDecision::Approve) => Ok(ConsentDecision::Approve),
		Ok(pb::ConsentDecision::Reject) => Ok(ConsentDecision::Reject),
		_ => Err(Status::invalid_argument("decision must be approve or reject")),
	}
}

/// The recognisable detail, MASKED where it is somebody's address — every surface this
/// reaches is read by someone who is not that person.
fn detail_to_proto(detail: Option<&EndDetail>) -> String {
	match detail {
		Some(EndDetail::Mailbox(email)) => mask_email(email),
		Some(EndDetail::ProductTitle(title)) => title.clone(),
		None => String::new(),
	}
}

fn party_end(party: &Party, label: String, detail: String) -> pb::PaymentEnd {
	pb::PaymentEnd {
		label,
		kind: party.kind_str().to_owned(),
		id: party.id_str(),
		network: String::new(),
		address: String::new(),
		detail,
	}
}

fn source_to_proto(terms: &PaymentTerms) -> pb::PaymentEnd {
	party_end(terms.from(), terms.source_label(), String::new())
}

fn destination_to_proto(terms: &PaymentTerms, detail: Option<&EndDetail>) -> pb::PaymentEnd {
	match terms.to() {
		PaymentDestination::Internal(party) => party_end(party, terms.destination_label(), detail_to_proto(detail)),
		PaymentDestination::External { network, address } => pb::PaymentEnd {
			label: terms.destination_label(),
			kind: "external".to_owned(),
			id: String::new(),
			network: network.as_str().to_owned(),
			// In FULL, never truncated: a shortened address in an approval flow is an
			// invitation to approve the wrong wallet.
			address: address.as_str().to_owned(),
			detail: String::new(),
		},
	}
}

fn payment_to_proto(view: &PaymentView) -> pb::Payment {
	let order = &view.order;
	pb::Payment {
		id: order.id().to_string(),
		state: state_to_proto(order.state()),
		tier: order.tier().as_str().to_owned(),
		source: Some(source_to_proto(order.terms())),
		destination: Some(destination_to_proto(order.terms(), view.destination_detail.as_ref())),
		amount: order.terms().amount().to_decimal_string(),
		reason: order.terms().reason().as_str().to_owned(),
		requirement: order.requirement().as_str().to_owned(),
		payload_hash: order.payload_hash_hex(),
		initiator_email: view.initiator_email.clone(),
		consilium_id: view.consilium_id.map(|id| id.to_string()).unwrap_or_default(),
		consent: view.consent.as_ref().map(|consent| pb::PaymentConsent {
			subject_email: mask_email(&consent.email),
			decision: decision_to_proto(consent.decision),
			notified: consent.notified,
			attempts_remaining: consent.attempts_remaining,
			invalidated: consent.invalidated.is_some(),
			invalidation_reason: consent.invalidated.map(|cause| cause.to_string()).unwrap_or_default(),
		}),
		created_at: order.created_at(),
		expires_at: order.expires_at(),
		decided_at: order.decided_at().unwrap_or_default(),
		executed_withdrawal_id: order.executed_withdrawal_id().map(|id| id.to_string()).unwrap_or_default(),
		failure_reason: order.failure_reason().unwrap_or_default().to_owned(),
		version: order.version(),
	}
}

fn invitation_to_proto(view: &ConsentInvitation) -> pb::PaymentConsentInvitation {
	let terms = view.order.terms();
	pb::PaymentConsentInvitation {
		payment_id: view.payment_id.to_string(),
		state: state_to_proto(view.state),
		tier: terms.tier().as_str().to_owned(),
		source: Some(source_to_proto(terms)),
		destination: Some(destination_to_proto(terms, view.destination_detail.as_ref())),
		amount: terms.amount().to_decimal_string(),
		reason: terms.reason().as_str().to_owned(),
		payload_hash: view.payload_hash.clone(),
		initiator_email: mask_email(&view.initiator_email),
		subject_email: mask_email(&view.subject_email),
		expires_at: view.expires_at,
		decision: decision_to_proto(view.decision),
		attempts_remaining: view.attempts_remaining,
	}
}

/// The single answer every unusable token gets. Unknown, expired, spent, burned and closed
/// are one response with one message, so the surface cannot be probed for live tokens.
fn consent_err(err: DomainError) -> Status {
	match err {
		DomainError::NotFound { .. } => Status::not_found("invitation"),
		other => map_err(other),
	}
}

#[tonic::async_trait]
impl PaymentsService for PaymentsSvc {
	async fn open_payment(&self, request: Request<pb::OpenPaymentRequest>) -> Result<Response<pb::Payment>, Status> {
		require_permission(&self.state, &request, bank::Payment::Open).await?;
		let initiator = caller_id(&request)?;
		let req = request.into_inner();
		let source = self.state.parse_party(req.source.as_ref()).await?;
		let destination = self.state.parse_destination(req.destination.as_ref()).await?;
		let amount = Usdt::parse_decimal(&req.amount).map_err(map_err)?;
		let reason = PaymentReason::new(req.reason).map_err(map_err)?;
		let terms = PaymentTerms::new(source, destination, amount, reason).map_err(map_err)?;
		let view = payments_app::open(&self.state.payment_ports(), initiator, terms, unix_now()).await.map_err(map_err)?;
		// WARN on success on purpose: a proposal to move money between two named ends is
		// worth an audit line that stands out, the same way a payout proposal is.
		tracing::warn!(
			payment_id = %view.order.id(),
			initiator = %initiator,
			tier = view.order.tier().as_str(),
			source = %view.order.terms().source_label(),
			destination = %view.order.terms().destination_label(),
			amount = %view.order.terms().amount(),
			requirement = view.order.requirement().as_str(),
			"opened a payment order"
		);
		Ok(Response::new(payment_to_proto(&view)))
	}

	async fn list_payments(&self, request: Request<pb::ListPaymentsRequest>) -> Result<Response<pb::PaymentList>, Status> {
		require_permission(&self.state, &request, bank::Payment::Open).await?;
		let req = request.into_inner();
		let limit = if req.limit == 0 { DEFAULT_LIST_LIMIT } else { req.limit.min(MAX_LIST_LIMIT) };
		let party = match req.party.as_ref() {
			Some(party) => Some(self.state.parse_party(Some(party)).await?),
			None => None,
		};
		let filter = PaymentFilter {
			state: state_from_proto(req.state)?,
			party,
			fund_owned_source: req.fund_owned_only.then_some(true),
		};
		let views = payments_app::list(self.state.payment_feed.as_ref(), &filter, i64::from(limit)).await.map_err(map_err)?;
		Ok(Response::new(pb::PaymentList {
			items: views.iter().map(payment_to_proto).collect(),
		}))
	}

	async fn get_payment(&self, request: Request<pb::GetPaymentRequest>) -> Result<Response<pb::Payment>, Status> {
		require_permission(&self.state, &request, bank::Payment::Open).await?;
		let id = parse_payment_id(&request.get_ref().payment_id)?;
		let view = payments_app::find(self.state.payments.as_ref(), id).await.map_err(map_err)?;
		Ok(Response::new(payment_to_proto(&view)))
	}

	async fn cancel_payment(&self, request: Request<pb::CancelPaymentRequest>) -> Result<Response<pb::Payment>, Status> {
		require_permission(&self.state, &request, bank::Payment::Open).await?;
		let caller = caller_id(&request)?;
		let id = parse_payment_id(&request.get_ref().payment_id)?;
		let view = payments_app::cancel(&self.state.payment_ports(), id, caller, unix_now()).await.map_err(map_err)?;
		Ok(Response::new(payment_to_proto(&view)))
	}
}

#[tonic::async_trait]
impl PaymentConsentService for PaymentConsentSvc {
	async fn get_consent_invitation(&self, request: Request<pb::GetConsentInvitationRequest>) -> Result<Response<pb::PaymentConsentInvitation>, Status> {
		// STRICTLY side-effect free. Mail scanners fetch every URL in a message; if this
		// counted an attempt or spent the token, a corporate gateway would destroy an
		// investor's consent before they ever read the mail.
		let view = payments_app::invitation(self.state.payments.as_ref(), &request.get_ref().token, unix_now())
			.await
			.map_err(consent_err)?;
		Ok(Response::new(invitation_to_proto(&view)))
	}

	async fn submit_consent(&self, request: Request<pb::SubmitConsentRequest>) -> Result<Response<pb::SubmitConsentResponse>, Status> {
		let req = request.into_inner();
		let decision = decision_from_proto(req.decision)?;
		let audit = ConsentAudit {
			client_ip: clamp(req.client_ip, MAX_AUDIT_IP_BYTES),
			user_agent: clamp(req.user_agent, MAX_AUDIT_USER_AGENT_BYTES),
		};
		let outcome = payments_app::submit_consent(&self.state.payment_ports(), &req.token, &req.code, decision, &audit, unix_now())
			.await
			.map_err(consent_err)?;
		let payment = &outcome.payment;
		let terms = payment.order.terms();
		// The answered seat, rendered as the invitation the same page showed before — with
		// the state the answer left the order in.
		let invitation = pb::PaymentConsentInvitation {
			payment_id: payment.order.id().to_string(),
			state: state_to_proto(payment.order.state()),
			tier: terms.tier().as_str().to_owned(),
			source: Some(source_to_proto(terms)),
			destination: Some(destination_to_proto(terms, payment.destination_detail.as_ref())),
			amount: terms.amount().to_decimal_string(),
			reason: terms.reason().as_str().to_owned(),
			payload_hash: payment.order.payload_hash_hex(),
			initiator_email: mask_email(&payment.initiator_email),
			subject_email: payment.consent.as_ref().map(|consent| mask_email(&consent.email)).unwrap_or_default(),
			expires_at: payment.order.expires_at(),
			decision: payment.consent.as_ref().map(|consent| decision_to_proto(consent.decision)).unwrap_or_default(),
			attempts_remaining: payment.consent.as_ref().map(|consent| consent.attempts_remaining).unwrap_or_default(),
		};
		Ok(Response::new(pb::SubmitConsentResponse {
			invitation: Some(invitation),
			decided: outcome.decided,
		}))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pending_is_not_an_acceptable_consent() {
		assert!(decision_from_proto(pb::ConsentDecision::Approve as i32).is_ok());
		assert!(decision_from_proto(pb::ConsentDecision::Reject as i32).is_ok());
		assert!(decision_from_proto(pb::ConsentDecision::Pending as i32).is_err());
		assert!(decision_from_proto(pb::ConsentDecision::Unspecified as i32).is_err());
		assert!(decision_from_proto(99).is_err());
	}

	#[test]
	fn an_unspecified_state_filters_nothing_and_junk_is_refused() {
		assert_eq!(state_from_proto(pb::PaymentState::Unspecified as i32).unwrap(), None);
		assert_eq!(state_from_proto(pb::PaymentState::Approved as i32).unwrap(), Some(PaymentState::Approved));
		assert!(state_from_proto(99).is_err());
	}

	#[test]
	fn every_unusable_token_produces_one_identical_answer() {
		let one = consent_err(crate::ports::payments::consent_not_found());
		let two = consent_err(DomainError::NotFound {
			entity: "consent",
			id: "something else".to_owned(),
		});
		assert_eq!(one.code(), tonic::Code::NotFound);
		assert_eq!(one.message(), two.message());
	}

	#[test]
	fn audit_strings_are_clamped_on_a_character_boundary() {
		assert_eq!(clamp("203.0.113.7".to_owned(), MAX_AUDIT_IP_BYTES), "203.0.113.7");
		assert_eq!(clamp("x".repeat(1000), MAX_AUDIT_USER_AGENT_BYTES).len(), MAX_AUDIT_USER_AGENT_BYTES);
		// A 4-byte code point straddling the cut is dropped whole rather than split.
		let clamped = clamp(format!("{}😀", "a".repeat(510)), MAX_AUDIT_USER_AGENT_BYTES);
		assert_eq!(clamped.len(), 510);
	}

	#[test]
	fn a_receiving_mailbox_is_masked_and_a_title_is_not() {
		assert_eq!(detail_to_proto(Some(&EndDetail::Mailbox("bob@example.com".into()))), "b***@example.com");
		assert_eq!(detail_to_proto(Some(&EndDetail::ProductTitle("Alpha".into()))), "Alpha");
		assert_eq!(detail_to_proto(None), "");
	}
}
