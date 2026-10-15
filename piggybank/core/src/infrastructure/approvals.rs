//! [`Approvals`] over concierge's `ApprovalExecutionService` — banking as the proposer and
//! executor of its `bank:*` kinds. It rides the bridge: the same endpoint and the same
//! `BRIDGE_SERVICE_TOKEN` as the lifecycle consumer and the governance mail, which concierge
//! maps to namespace `bank` (its `docs/SCOPES-AND-APPROVALS.md`, "Service callers"). One trust
//! relationship between the planes, as before — and so a leaked bridge token now also opens
//! and reports banking's approvals.

use std::time::Duration;

use async_trait::async_trait;
use domain::scope_operations::{APPROVAL_CURRENCY, ApprovalField, ApprovalValue};
use evconcierge_contracts::concierge::v1::{
	Approval, ApprovalAmount, ApprovalFieldValue, ApprovalLevel as WireLevel, ApprovalOutcome as WireOutcome, ApprovalState as WireState, GetProposalRequest, ProposeRequest,
	PullApprovedRequest, ReportOutcomeRequest, approval_execution_service_client::ApprovalExecutionServiceClient, approval_field_value::Value,
};
use tonic::{Code, Request, Status, metadata::MetadataValue, transport::Channel};

use crate::ports::scope_operations::{ApprovalLevel, ApprovalOutcome, ApprovalState, ApprovalView, Approvals, ApprovalsError, Proposal};

/// A call that has not answered by then is treated as concierge being unavailable; the
/// executor retries on its next round.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ConciergeApprovals {
	channel: Channel,
	service_token: String,
}

impl ConciergeApprovals {
	pub fn new(channel: Channel, service_token: String) -> Self {
		Self { channel, service_token }
	}

	fn request<T>(&self, message: T) -> Result<Request<T>, ApprovalsError> {
		let mut request = Request::new(message);
		let token: MetadataValue<_> = format!("Bearer {}", self.service_token)
			.parse()
			.map_err(|_| ApprovalsError::Refused("the bridge token is not a valid header value".into()))?;
		request.metadata_mut().insert("authorization", token);
		request.set_timeout(CALL_TIMEOUT);
		Ok(request)
	}

	fn client(&self) -> ApprovalExecutionServiceClient<Channel> {
		ApprovalExecutionServiceClient::new(self.channel.clone())
	}
}

/// Which failures a retry of the same call can cure. A refusal on the request itself is for
/// good; a transport or server failure is not. UNAUTHENTICATED and UNIMPLEMENTED are
/// configuration (a wrong token, a concierge that predates approvals): retried, because the
/// fix is a deploy, not a different request — and logged loudly by the caller meanwhile.
fn classify(rpc: &str, status: Status) -> ApprovalsError {
	let message = format!("{rpc}: {:?}: {}", status.code(), status.message());
	match status.code() {
		Code::InvalidArgument | Code::NotFound | Code::AlreadyExists | Code::PermissionDenied | Code::FailedPrecondition | Code::OutOfRange => ApprovalsError::Refused(message),
		Code::Ok
		| Code::Cancelled
		| Code::Unknown
		| Code::DeadlineExceeded
		| Code::ResourceExhausted
		| Code::Aborted
		| Code::Unimplemented
		| Code::Internal
		| Code::Unavailable
		| Code::DataLoss
		| Code::Unauthenticated => ApprovalsError::Unavailable(message),
	}
}

fn field(field: &ApprovalField) -> ApprovalFieldValue {
	let value = match &field.value {
		ApprovalValue::Text(text) => Value::Text(text.clone()),
		ApprovalValue::Amount(amount) => Value::Amount(ApprovalAmount {
			value: amount.to_decimal_string(),
			currency: APPROVAL_CURRENCY.to_owned(),
		}),
		ApprovalValue::Flag(flag) => Value::Flag(*flag),
		ApprovalValue::User(subject) => Value::AuthSubject(subject.clone()),
		ApprovalValue::Count(count) => Value::Count(*count),
	};
	ApprovalFieldValue {
		key: field.key.to_owned(),
		value: Some(value),
	}
}

fn view(approval: Approval) -> Result<ApprovalView, ApprovalsError> {
	let state = match WireState::try_from(approval.state) {
		Ok(WireState::Open) => ApprovalState::Open,
		Ok(WireState::Passed) => ApprovalState::Passed,
		Ok(WireState::Executed) => ApprovalState::Executed,
		Ok(WireState::ExecutionFailed) => ApprovalState::ExecutionFailed,
		Ok(WireState::Rejected) => ApprovalState::Rejected,
		Ok(WireState::Vetoed) => ApprovalState::Vetoed,
		Ok(WireState::Expired) => ApprovalState::Expired,
		Ok(WireState::Cancelled) => ApprovalState::Cancelled,
		Ok(WireState::Void) => ApprovalState::Void,
		// A state this build cannot name is not one it may act on; the caller retries later.
		Ok(WireState::Unspecified) | Err(_) =>
			return Err(ApprovalsError::Unavailable(format!(
				"approval {} in a state banking cannot read: {}",
				approval.id, approval.state
			))),
	};
	let level = match WireLevel::try_from(approval.level) {
		Ok(WireLevel::Platform) => Some(ApprovalLevel::Platform),
		Ok(WireLevel::Scope) => Some(ApprovalLevel::Scope),
		Ok(WireLevel::Unspecified) | Err(_) => None,
	};
	Ok(ApprovalView {
		id: approval.id,
		kind: approval.kind,
		scope: approval.scope,
		subject_ref: approval.subject_ref,
		state,
		level,
		payload_hash: approval.payload_hash,
	})
}

#[async_trait]
impl Approvals for ConciergeApprovals {
	async fn propose(&self, proposal: &Proposal) -> Result<ApprovalView, ApprovalsError> {
		let request = self.request(ProposeRequest {
			kind: proposal.kind.clone(),
			scope: proposal.scope.clone(),
			subject_ref: proposal.subject_ref.clone(),
			// banking acts for a service, which cannot prove which person asked it.
			requested_by: String::new(),
			reason: proposal.reason.clone(),
			fields: proposal.fields.iter().map(field).collect(),
			dedupe_key: proposal.dedupe_key.clone(),
		})?;
		let approval = self.client().propose(request).await.map_err(|status| classify("Propose", status))?.into_inner();
		view(approval)
	}

	async fn get(&self, approval_id: &str) -> Result<ApprovalView, ApprovalsError> {
		let request = self.request(GetProposalRequest {
			approval_id: approval_id.to_owned(),
		})?;
		let approval = self.client().get_proposal(request).await.map_err(|status| classify("GetProposal", status))?.into_inner();
		view(approval)
	}

	async fn pull_approved(&self, limit: u32) -> Result<Vec<ApprovalView>, ApprovalsError> {
		let request = self.request(PullApprovedRequest { limit })?;
		let approved = self.client().pull_approved(request).await.map_err(|status| classify("PullApproved", status))?.into_inner();
		approved.approvals.into_iter().map(view).collect()
	}

	async fn report(&self, approval_id: &str, outcome: ApprovalOutcome, detail: &str) -> Result<(), ApprovalsError> {
		let outcome = match outcome {
			ApprovalOutcome::Executed => WireOutcome::Executed,
			ApprovalOutcome::Failed => WireOutcome::Failed,
			ApprovalOutcome::Void => WireOutcome::Void,
		};
		let request = self.request(ReportOutcomeRequest {
			approval_id: approval_id.to_owned(),
			outcome: outcome as i32,
			detail: detail.to_owned(),
		})?;
		self.client().report_outcome(request).await.map_err(|status| classify("ReportOutcome", status))?;
		Ok(())
	}
}
