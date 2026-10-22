//! [`Approvals`] over concierge's `ApprovalExecutionService` — banking as the proposer and
//! executor of its `bank:*` kinds. It rides the bridge: the same endpoint and the same
//! `BRIDGE_SERVICE_TOKEN` as the lifecycle consumer and the governance mail, which concierge
//! maps to namespace `bank` (its `docs/SCOPES-AND-APPROVALS.md`, "Service callers"). One trust
//! relationship between the planes, as before — and so a leaked bridge token now also opens
//! and reports banking's approvals, and publishes the `bank:*` catalog.

use std::time::Duration;

use async_trait::async_trait;
use domain::scope_operations::{APPROVAL_CURRENCY, ApprovalField, ApprovalValue, EscalationTest, FieldType, KindLevel, KindSpec, TopicSpec};
use evconcierge_contracts::concierge::v1::{
	Approval, ApprovalAmount, ApprovalEscalation, ApprovalFieldSpec, ApprovalFieldType, ApprovalFieldValue, ApprovalKindSpec, ApprovalLevel as WireLevel, ApprovalOutcome as WireOutcome,
	ApprovalState as WireState, EscalationTest as WireTest, GetProposalRequest, NotificationTopicSpec, ProposeRequest, PublishScopeCatalogRequest, PullApprovedRequest, ReportOutcomeRequest,
	WithdrawProposalRequest, approval_execution_service_client::ApprovalExecutionServiceClient, approval_field_value::Value, scope_catalog_service_client::ScopeCatalogServiceClient,
};
use tonic::{Code, Request, Status, metadata::MetadataValue, transport::Channel};

use crate::ports::scope_operations::{ApprovalLevel, ApprovalOutcome, ApprovalState, ApprovalView, Approvals, ApprovalsError, Proposal, UserProof};

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
/// good; a transport or server failure is not; RESOURCE_EXHAUSTED is concierge's cap on live
/// proposals in one scope, which says nothing about the others. UNAUTHENTICATED and UNIMPLEMENTED are
/// configuration (a wrong token, a concierge that predates approvals): retried, because the
/// fix is a deploy, not a different request — and logged loudly by the caller meanwhile.
fn classify(rpc: &str, status: Status) -> ApprovalsError {
	let message = format!("{rpc}: {:?}: {}", status.code(), status.message());
	match status.code() {
		Code::ResourceExhausted => ApprovalsError::Throttled(message),
		Code::InvalidArgument | Code::NotFound | Code::AlreadyExists | Code::PermissionDenied | Code::FailedPrecondition | Code::OutOfRange => ApprovalsError::Refused(message),
		Code::Ok
		| Code::Cancelled
		| Code::Unknown
		| Code::DeadlineExceeded
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

fn kind_spec(spec: &KindSpec) -> ApprovalKindSpec {
	ApprovalKindSpec {
		name: spec.kind.to_owned(),
		label: spec.label.to_owned(),
		description: spec.description.to_owned(),
		level: match spec.level {
			KindLevel::Platform => WireLevel::Platform,
			KindLevel::Scope => WireLevel::Scope,
		} as i32,
		fields: spec
			.fields
			.iter()
			.map(|field| ApprovalFieldSpec {
				key: field.key.to_owned(),
				label: field.label.to_owned(),
				r#type: match field.field_type {
					FieldType::Text => ApprovalFieldType::Text,
					FieldType::Amount => ApprovalFieldType::Amount,
					FieldType::Flag => ApprovalFieldType::Flag,
					FieldType::User => ApprovalFieldType::User,
					FieldType::Count => ApprovalFieldType::Count,
				} as i32,
				required: field.required,
			})
			.collect(),
		escalations: spec
			.escalations
			.iter()
			.map(|escalation| ApprovalEscalation {
				parameter: escalation.parameter.to_owned(),
				field_key: escalation.field_key.to_owned(),
				test: match escalation.test {
					EscalationTest::AmountAbove => WireTest::AmountAbove,
					EscalationTest::Rolling24hAmountAbove => WireTest::Rolling24hAmountAbove,
					EscalationTest::FlagSet => WireTest::FlagSet,
				} as i32,
				default_threshold: escalation.default_threshold.to_owned(),
				currency: escalation.currency.to_owned(),
			})
			.collect(),
		// Filled by concierge on read; ignored on publish.
		proposer: String::new(),
	}
}

fn topic_spec(spec: &TopicSpec) -> NotificationTopicSpec {
	NotificationTopicSpec {
		topic: spec.topic.to_owned(),
		label: spec.label.to_owned(),
		description: spec.description.to_owned(),
		default_subscribed: spec.default_subscribed,
		// No bank topic is one a person cannot switch off: each is a notice, never the only
		// path to their money.
		required: false,
		required_reason: String::new(),
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
		execute_by: approval.execute_by,
	})
}

#[async_trait]
impl Approvals for ConciergeApprovals {
	async fn propose(&self, proposal: &Proposal) -> Result<ApprovalView, ApprovalsError> {
		self.propose_for(proposal, None).await
	}

	async fn propose_for(&self, proposal: &Proposal, proof: Option<&UserProof>) -> Result<ApprovalView, ApprovalsError> {
		let mut request = self.request(ProposeRequest {
			kind: proposal.kind.clone(),
			scope: proposal.scope.clone(),
			subject_ref: proposal.subject_ref.clone(),
			// Without a proof banking acts for a service, which cannot prove which person asked.
			requested_by: proof.map(|proof| proof.requested_by.clone()).unwrap_or_default(),
			reason: proposal.reason.clone(),
			fields: proposal.fields.iter().map(field).collect(),
			dedupe_key: proposal.dedupe_key.clone(),
		})?;
		if let Some(proof) = proof {
			// The person's own token, raw and without `Bearer`, as the scope inbox takes it too.
			let token: MetadataValue<_> = proof.token.parse().map_err(|_| ApprovalsError::Refused("the user token is not a valid header value".into()))?;
			request.metadata_mut().insert("x-ev-user-token", token);
		}
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

	async fn withdraw(&self, approval_id: &str, reason: &str) -> Result<(), ApprovalsError> {
		let request = self.request(WithdrawProposalRequest {
			approval_id: approval_id.to_owned(),
			reason: reason.to_owned(),
		})?;
		self.client().withdraw_proposal(request).await.map_err(|status| classify("WithdrawProposal", status))?;
		Ok(())
	}

	async fn publish_catalog(&self, version: u64, kinds: &[KindSpec], topics: &[TopicSpec]) -> Result<(), ApprovalsError> {
		let request = self.request(PublishScopeCatalogRequest {
			version,
			approval_kinds: kinds.iter().map(kind_spec).collect(),
			notification_topics: topics.iter().map(topic_spec).collect(),
		})?;
		ScopeCatalogServiceClient::new(self.channel.clone())
			.publish_scope_catalog(request)
			.await
			.map_err(|status| classify("PublishScopeCatalog", status))?;
		Ok(())
	}
}
