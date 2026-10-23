//! `consilium` context — the two gRPC surfaces of multi-owner authorization.
//!
//! [`ConsiliumSvc`] sits **behind** the user-auth layer and is Owner-only: opening, reading
//! and withdrawing a request are things a signed-in owner does.
//!
//! [`ConsiliumApprovalSvc`] is mounted **outside** it, exactly as the auth service's own
//! routes are. The emailed token IS the credential there, and the owner clicking a link in
//! their mailbox may well not be signed in — requiring a session would make the mail
//! useless. Everything that surface returns is deliberately narrow, and every unusable token
//! leaves by the same door.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large type we
//! don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use concierge_domain::authz::bank;
use domain::{
	balance::ServiceId,
	consilium::{ConsiliumState, ConsiliumTerms, HolderGrantTerms, ValuationOverrideTerms, VoteDecision},
	error::DomainError,
	fees::FeePolicy,
	money::{Shares, Usdt},
	users::mask_email,
};
use evbanking_contracts::banking::v1::{self as pb, consilium_approval_service_server::ConsiliumApprovalService, consilium_service_server::ConsiliumService};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{
	AppState,
	application::consilium as consilium_app,
	ports::consilium::{ConsiliumView, FeePolicyDetail, InvitationView, VoteAudit},
	services::support::{MAX_AUDIT_IP_BYTES, MAX_AUDIT_USER_AGENT_BYTES, caller_id, clamp, map_err, require_permission, resolve_target_user, unix_now},
};

/// The default page size for the governance history.
const DEFAULT_LIST_LIMIT: u32 = 50;
const MAX_LIST_LIMIT: u32 = 200;

#[derive(Clone)]
pub struct ConsiliumSvc {
	pub state: AppState,
}

impl ConsiliumSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}
}

#[derive(Clone)]
pub struct ConsiliumApprovalSvc {
	pub state: AppState,
}

impl ConsiliumApprovalSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}
}

impl AppState {
	/// The consilium write-path's ports over the hub's state. `pub(crate)` because the
	/// balance surface opens a seed consilium through the same ports (`SeedCapital`).
	pub(crate) fn consilium_ports(&self) -> consilium_app::ConsiliumPorts<'_> {
		consilium_app::ConsiliumPorts {
			consilia: self.consilia.as_ref(),
			withdrawals: self.withdrawals.as_ref(),
			payments: self.payments.as_ref(),
			users: self.users.as_ref(),
			ledger: self.ledger.as_ref(),
			custody: self.custody.as_ref(),
			policy: self.outflow.as_ref(),
			allocations: self.allocations.as_ref(),
			nav: self.nav.as_ref(),
			fee_changes: self.fees.changes.as_ref(),
			issuances: self.issuances.as_ref(),
			deposits: self.deposits.as_ref(),
			addresses: self.deposit_addresses.as_ref(),
			subscriptions: self.subscriptions.as_ref(),
			relay: &self.relay_notify,
			configured: &self.configured_networks,
			kyc: self.kyc_gate,
			approval_url_base: &self.consilium_approval_url_base,
			consent_url_base: &self.payment_consent_url_base,
			governance_mail_wired: crate::infrastructure::governance_mail::is_wired(),
		}
	}
}

fn parse_consilium_id(raw: &str) -> Result<domain::consilium::ConsiliumId, Status> {
	Uuid::parse_str(raw)
		.map(domain::consilium::ConsiliumId::from_raw)
		.map_err(|_| Status::invalid_argument("invalid consilium id"))
}

/// The valuation-override terms, validated into their domain form. The fund's existence
/// and its unit supply are the application's gates; this is only the wire's shape.
fn parse_valuation_override_terms(terms: Option<pb::ValuationOverrideTerms>) -> Result<ValuationOverrideTerms, Status> {
	let terms = terms.ok_or_else(|| Status::invalid_argument("terms are required"))?;
	Ok(ValuationOverrideTerms {
		service: ServiceId::parse(&terms.service).map_err(map_err)?,
		aum: Usdt::parse_decimal(&terms.aum).map_err(map_err)?,
	})
}

fn state_to_proto(state: ConsiliumState) -> i32 {
	let mapped = match state {
		ConsiliumState::Open => pb::ConsiliumState::Open,
		ConsiliumState::Approved => pb::ConsiliumState::Approved,
		ConsiliumState::Rejected => pb::ConsiliumState::Rejected,
		ConsiliumState::Expired => pb::ConsiliumState::Expired,
		ConsiliumState::Cancelled => pb::ConsiliumState::Cancelled,
		ConsiliumState::Executed => pb::ConsiliumState::Executed,
		ConsiliumState::ExecutionFailed => pb::ConsiliumState::ExecutionFailed,
	};
	mapped as i32
}

fn decision_to_proto(decision: VoteDecision) -> i32 {
	let mapped = match decision {
		VoteDecision::Pending => pb::VoteDecision::Pending,
		VoteDecision::Approve => pb::VoteDecision::Approve,
		VoteDecision::Reject => pb::VoteDecision::Reject,
	};
	mapped as i32
}

/// `PENDING` is not an answer, so it is refused rather than silently recorded as one.
fn decision_from_proto(raw: i32) -> Result<VoteDecision, Status> {
	match pb::VoteDecision::try_from(raw) {
		Ok(pb::VoteDecision::Approve) => Ok(VoteDecision::Approve),
		Ok(pb::VoteDecision::Reject) => Ok(VoteDecision::Reject),
		_ => Err(Status::invalid_argument("decision must be approve or reject")),
	}
}

/// The valuation-override terms as the wire carries them — the third of the "exactly one
/// terms field is set" siblings. `None` for the other kinds, and NEVER an empty message:
/// the wallet frontend treats an empty object as unrenderable, and `Option` is what the generated
/// DTO folds to `null`.
fn valuation_override_terms_to_proto(terms: &ConsiliumTerms) -> Option<pb::ValuationOverrideTerms> {
	match terms {
		ConsiliumTerms::ValuationOverride(terms) => Some(pb::ValuationOverrideTerms {
			service: terms.service.to_string(),
			aum: terms.aum.to_decimal_string(),
		}),
		ConsiliumTerms::Payment(_) | ConsiliumTerms::FeePolicy(_) | ConsiliumTerms::HolderGrant(_) | ConsiliumTerms::SeedCapital(_) => None,
	}
}

/// The payment terms as the wire carries them — one more of "exactly one of the terms
/// fields is set".
///
/// The two ends are LABELS, taken from the same [`domain::payments::PaymentTerms`] the
/// consent mail and the payments screen describe, so the three surfaces cannot come to
/// disagree about what an owner approved.
fn payment_terms_to_proto(terms: &ConsiliumTerms) -> Option<pb::ConsiliumPaymentTerms> {
	match terms {
		ConsiliumTerms::ValuationOverride(_) | ConsiliumTerms::FeePolicy(_) | ConsiliumTerms::HolderGrant(_) | ConsiliumTerms::SeedCapital(_) => None,
		ConsiliumTerms::Payment(subject) => Some(pb::ConsiliumPaymentTerms {
			payment_id: subject.payment_id.to_string(),
			tier: subject.terms.tier().as_str().to_owned(),
			source: subject.terms.source_label(),
			destination: subject.terms.destination_label(),
			amount: subject.terms.amount().to_decimal_string(),
			reason: subject.terms.reason().as_str().to_owned(),
		}),
	}
}

/// The holder-grant terms as the wire carries them — the fifth terms sibling (#245). The
/// user crosses as the money-plane id the terms stored, which is what the hash binds.
fn holder_grant_terms_to_proto(terms: &ConsiliumTerms) -> Option<pb::HolderGrantTerms> {
	match terms {
		ConsiliumTerms::HolderGrant(terms) => Some(pb::HolderGrantTerms {
			allocation: terms.allocation.to_string(),
			user_id: terms.user.to_string(),
			units: terms.units.to_decimal_string(),
		}),
		ConsiliumTerms::Payment(_) | ConsiliumTerms::ValuationOverride(_) | ConsiliumTerms::FeePolicy(_) | ConsiliumTerms::SeedCapital(_) => None,
	}
}

/// The seed-capital terms as the wire carries them — the sixth terms sibling (#245). The
/// reference is rendered in FULL, as the address of a payout is: the owners are approving
/// that THIS transfer is this person's.
fn seed_capital_terms_to_proto(terms: &ConsiliumTerms) -> Option<pb::SeedCapitalTerms> {
	match terms {
		ConsiliumTerms::SeedCapital(terms) => Some(pb::SeedCapitalTerms {
			tx_ref: terms.tx_ref.as_str().to_owned(),
			network: terms.network.as_str().to_owned(),
			amount: terms.amount.to_decimal_string(),
			depositor_user_id: terms.depositor.to_string(),
		}),
		ConsiliumTerms::Payment(_) | ConsiliumTerms::ValuationOverride(_) | ConsiliumTerms::FeePolicy(_) | ConsiliumTerms::HolderGrant(_) => None,
	}
}

/// The fee-policy terms as the wire carries them: the hashed subject plus the live
/// presentation (product title, holder count) the repository reads beside it.
fn fee_policy_terms_to_proto(terms: &ConsiliumTerms, detail: Option<&FeePolicyDetail>) -> Option<pb::ConsiliumFeePolicyTerms> {
	match terms {
		ConsiliumTerms::Payment(_) | ConsiliumTerms::ValuationOverride(_) | ConsiliumTerms::HolderGrant(_) | ConsiliumTerms::SeedCapital(_) => None,
		ConsiliumTerms::FeePolicy(subject) => {
			let from = subject.from.unwrap_or(FeePolicy::NONE);
			Some(pb::ConsiliumFeePolicyTerms {
				change_id: subject.change_id.to_string(),
				service: subject.service.to_string(),
				allocation_name: detail.map(|detail| detail.allocation_name.clone()).unwrap_or_default(),
				from_configured: subject.from.is_some(),
				from_management_bps: from.management_bps(),
				from_performance_bps: from.performance_bps(),
				from_hurdle_bps: from.hurdle_bps(),
				from_basis: from.basis().as_str().to_owned(),
				from_crystallization: from.crystallization().as_str().to_owned(),
				to_management_bps: subject.to.management_bps(),
				to_performance_bps: subject.to.performance_bps(),
				to_hurdle_bps: subject.to.hurdle_bps(),
				to_basis: subject.to.basis().as_str().to_owned(),
				to_crystallization: subject.to.crystallization().as_str().to_owned(),
				effective_from: subject.requested_effective_from,
				holder_count: detail.map(|detail| detail.holder_count).unwrap_or_default(),
				reason: subject.reason.clone(),
			})
		}
	}
}

fn consilium_to_proto(view: &ConsiliumView) -> pb::Consilium {
	let c = &view.consilium;
	pb::Consilium {
		id: c.id().to_string(),
		state: state_to_proto(c.state()),
		payment: payment_terms_to_proto(c.terms()),
		valuation_override: valuation_override_terms_to_proto(c.terms()),
		fee_policy: fee_policy_terms_to_proto(c.terms(), view.fee_policy.as_ref()),
		holder_grant: holder_grant_terms_to_proto(c.terms()),
		seed_capital: seed_capital_terms_to_proto(c.terms()),
		payload_hash: c.payload_hash_hex(),
		initiator_user_id: c.initiator().to_string(),
		initiator_email: view.initiator_email.clone(),
		owner_count: c.owner_count(),
		threshold: c.threshold(),
		// The counted tally, not the raw one: a vote from a seat that has since been lost
		// is shown against its voter but never added up here.
		approvals: c.approvals(),
		rejections: c.rejections(),
		voters: view
			.voters
			.iter()
			.map(|voter| pb::ConsiliumVoter {
				user_id: voter.user_id.to_string(),
				email: voter.email.clone(),
				decision: decision_to_proto(voter.decision),
				decided_at: voter.decided_at,
				notified: voter.notified,
			})
			.collect(),
		created_at: c.created_at(),
		expires_at: c.expires_at(),
		decided_at: c.decided_at().unwrap_or_default(),
		executed_withdrawal_id: c.executed_withdrawal_id().map(|id| id.to_string()).unwrap_or_default(),
		executed_payment_id: c.executed_payment_id().map(|id| id.to_string()).unwrap_or_default(),
		executed_valuation_id: c.executed_valuation_id().map(|id| id.to_string()).unwrap_or_default(),
		executed_fee_policy_change_id: c.executed_fee_policy_change_id().map(|id| id.to_string()).unwrap_or_default(),
		executed_issuance_id: c.executed_issuance_id().map(|id| id.to_string()).unwrap_or_default(),
		executed_subscription_id: c.executed_subscription_id().map(|id| id.to_string()).unwrap_or_default(),
		failure_reason: c.failure_reason().unwrap_or_default().to_owned(),
		version: c.version(),
	}
}

fn invitation_to_proto(view: &InvitationView) -> pb::ConsiliumInvitation {
	pb::ConsiliumInvitation {
		consilium_id: view.consilium_id.to_string(),
		state: state_to_proto(view.state),
		payment: payment_terms_to_proto(&view.terms),
		valuation_override: valuation_override_terms_to_proto(&view.terms),
		fee_policy: fee_policy_terms_to_proto(&view.terms, view.fee_policy.as_ref()),
		holder_grant: holder_grant_terms_to_proto(&view.terms),
		seed_capital: seed_capital_terms_to_proto(&view.terms),
		payload_hash: view.payload_hash.clone(),
		initiator_email: mask_email(&view.initiator_email),
		voter_email: mask_email(&view.voter_email),
		threshold: view.threshold,
		approvals: view.approvals,
		owner_count: view.owner_count,
		created_at: view.created_at,
		expires_at: view.expires_at,
		decision: decision_to_proto(view.decision),
		attempts_remaining: view.attempts_remaining,
	}
}

/// The single answer every unusable token gets. Unknown, expired, spent, burned and closed
/// are one response with one message, so the surface cannot be probed for live tokens.
fn approval_err(err: DomainError) -> Status {
	match err {
		DomainError::NotFound { .. } => Status::not_found("invitation"),
		other => map_err(other),
	}
}

#[tonic::async_trait]
impl ConsiliumService for ConsiliumSvc {
	/// Seat a holder of a reserved allocation (#245). Gated on `ConsiliumManage` — the
	/// permission every owner-only surface of this service shares
	/// — and NOT on `AllocationManage`: an operator who sizes
	/// products has no say over who holds the owners' money, and the domain refuses an
	/// initiator without a seat whatever admitted them here. The person is resolved the
	/// way every admin RPC resolves a target, so the units land on the money-plane row
	/// they redeem from.
	async fn open_holder_grant(&self, request: Request<pb::OpenHolderGrantRequest>) -> Result<Response<pb::Consilium>, Status> {
		require_permission(&self.state, &request, bank::Consilium::Manage).await?;
		let initiator = caller_id(&request)?;
		let terms = request.into_inner().terms.ok_or_else(|| Status::invalid_argument("terms are required"))?;
		let allocation = ServiceId::parse(&terms.allocation).map_err(map_err)?;
		let user = resolve_target_user(&self.state, &terms.user_id).await?;
		let units = Shares::parse_decimal(&terms.units).map_err(map_err)?;
		let terms = HolderGrantTerms::new(allocation, user, units).map_err(map_err)?;
		let view = consilium_app::open_holder_grant(&self.state.consilium_ports(), initiator, terms.clone(), unix_now())
			.await
			.map_err(map_err)?;
		// WARN on success, as every open does: a request to hand somebody a share of the
		// owners' money is worth an audit line that stands out.
		tracing::warn!(
			consilium_id = %view.consilium.id(),
			initiator = %initiator,
			allocation = %terms.allocation,
			user = %terms.user,
			units = %terms.units.to_decimal_string(),
			threshold = view.consilium.threshold(),
			owner_count = view.consilium.owner_count(),
			"opened a holder-grant consilium"
		);
		Ok(Response::new(consilium_to_proto(&view)))
	}

	async fn open_valuation_override(&self, request: Request<pb::OpenValuationOverrideRequest>) -> Result<Response<pb::Consilium>, Status> {
		// Gated on `ValuationPost`, the capability that posts a mark — NOT on a second
		// permission. The second actor here is the vote: the poster proposes, the owners
		// decide, and the domain refuses an initiator who holds no seat. Granting the
		// override to a different permission would only recreate the flag one role over.
		require_permission(&self.state, &request, bank::Valuation::Post).await?;
		let initiator = caller_id(&request)?;
		let terms = parse_valuation_override_terms(request.into_inner().terms)?;
		let view = consilium_app::open_valuation_override(&self.state.consilium_ports(), initiator, terms.clone(), unix_now())
			.await
			.map_err(map_err)?;
		// WARN on success, as the payout does: a request to reprice a fund past the guard is
		// worth an audit line that stands out.
		tracing::warn!(
			consilium_id = %view.consilium.id(),
			initiator = %initiator,
			service = %terms.service,
			aum = %terms.aum,
			threshold = view.consilium.threshold(),
			owner_count = view.consilium.owner_count(),
			"opened a valuation-override consilium"
		);
		Ok(Response::new(consilium_to_proto(&view)))
	}

	async fn cancel_consilium(&self, request: Request<pb::CancelConsiliumRequest>) -> Result<Response<pb::Consilium>, Status> {
		require_permission(&self.state, &request, bank::Consilium::Manage).await?;
		let caller = caller_id(&request)?;
		let id = parse_consilium_id(&request.get_ref().consilium_id)?;
		let view = consilium_app::cancel(self.state.consilia.as_ref(), id, caller, unix_now()).await.map_err(map_err)?;
		Ok(Response::new(consilium_to_proto(&view)))
	}

	async fn get_consilium(&self, request: Request<pb::GetConsiliumRequest>) -> Result<Response<pb::Consilium>, Status> {
		require_permission(&self.state, &request, bank::Consilium::Manage).await?;
		let id = parse_consilium_id(&request.get_ref().consilium_id)?;
		let view = consilium_app::find(self.state.consilia.as_ref(), id).await.map_err(map_err)?;
		Ok(Response::new(consilium_to_proto(&view)))
	}

	async fn list_consilia(&self, request: Request<pb::ListConsiliaRequest>) -> Result<Response<pb::ConsiliumList>, Status> {
		require_permission(&self.state, &request, bank::Consilium::Manage).await?;
		let requested = request.get_ref().limit;
		let limit = if requested == 0 { DEFAULT_LIST_LIMIT } else { requested.min(MAX_LIST_LIMIT) };
		let views = consilium_app::list(self.state.consilia.as_ref(), i64::from(limit)).await.map_err(map_err)?;
		Ok(Response::new(pb::ConsiliumList {
			items: views.iter().map(consilium_to_proto).collect(),
		}))
	}
}

#[tonic::async_trait]
impl ConsiliumApprovalService for ConsiliumApprovalSvc {
	async fn get_invitation(&self, request: Request<pb::GetInvitationRequest>) -> Result<Response<pb::ConsiliumInvitation>, Status> {
		// STRICTLY side-effect free. Mail scanners fetch every URL in a message; if this
		// counted an attempt or spent the token, a corporate gateway would destroy an
		// owner's vote before they ever read the mail.
		let view = consilium_app::invitation(self.state.consilia.as_ref(), &request.get_ref().token, unix_now())
			.await
			.map_err(approval_err)?;
		Ok(Response::new(invitation_to_proto(&view)))
	}

	async fn submit_decision(&self, request: Request<pb::SubmitDecisionRequest>) -> Result<Response<pb::SubmitDecisionResponse>, Status> {
		let req = request.into_inner();
		let decision = decision_from_proto(req.decision)?;
		let audit = VoteAudit {
			client_ip: clamp(req.client_ip, MAX_AUDIT_IP_BYTES),
			user_agent: clamp(req.user_agent, MAX_AUDIT_USER_AGENT_BYTES),
		};
		let now = unix_now();
		let outcome = consilium_app::submit_decision(self.state.consilia.as_ref(), &req.token, &req.code, decision, &audit, now)
			.await
			.map_err(approval_err)?;
		if outcome.approved {
			// The vote is already committed, so an execution failure must not fail this
			// response — it is recorded on the consilium and the sweeper retries an
			// approval whose payout never got created.
			let consilium_id = outcome.invitation.consilium_id;
			if let Err(err) = consilium_app::execute(&self.state.consilium_ports(), consilium_id, now).await {
				tracing::error!(%consilium_id, "consilium: reached quorum but the payout could not be created: {err}");
			}
		}
		Ok(Response::new(pb::SubmitDecisionResponse {
			invitation: Some(invitation_to_proto(&outcome.invitation)),
			decided: outcome.decided,
		}))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pending_is_not_an_acceptable_vote() {
		assert!(decision_from_proto(pb::VoteDecision::Approve as i32).is_ok());
		assert!(decision_from_proto(pb::VoteDecision::Reject as i32).is_ok());
		assert!(decision_from_proto(pb::VoteDecision::Pending as i32).is_err());
		assert!(decision_from_proto(pb::VoteDecision::Unspecified as i32).is_err());
		assert!(decision_from_proto(99).is_err());
	}

	#[test]
	fn every_unusable_token_produces_one_identical_answer() {
		// Unknown, expired, spent and burned all arrive here as the same NotFound, and must
		// leave as the same Status — otherwise the surface can be probed for live tokens.
		let one = approval_err(crate::ports::consilium::invitation_not_found());
		let two = approval_err(DomainError::NotFound {
			entity: "invitation",
			id: "something else".to_owned(),
		});
		assert_eq!(one.code(), tonic::Code::NotFound);
		assert_eq!(one.message(), two.message());
	}
}
