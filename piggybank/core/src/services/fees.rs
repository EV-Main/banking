//! `fees` context — a fund's management and performance fee.
//!
//! Reads of a fund's *terms* — and of their history — are open to any authenticated
//! user who can see the product: an investor is entitled to know what they are paying
//! before they pay it, what they will be paying next, and how the terms came to be; a
//! product hidden from them is `NOT_FOUND`, exactly as `GetAllocation` answers, unless they
//! hold `bank:allocation:manage`. Reads of a *charge* are scoped
//! to the caller's own statement. Everything that changes terms or moves value is gated on
//! `bank:allocation:manage` — the same trust seam as registering the product or
//! posting its valuation, because a fee policy is part of what the product *is* — and a
//! change that tightens the terms beyond the house envelope needs the owners besides
//! (`docs/FEES.md` § "Changing the terms").
//!
//! Only [`SettleFeeShares`](FeesService::settle_fee_shares) moves money, and it
//! notifies the relay. Charging happens on the sweeper, not on this surface: a fee is
//! a consequence of time passing, never of somebody calling an RPC — and so is a change of
//! terms taking effect.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large type
//! we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use concierge_domain::authz::bank;
use domain::{
	balance::ServiceId,
	fees::{CrystallizationPeriod, FeePolicy, FeePolicyChangeId, ManagementBasis},
	money::Shares,
};
use evbanking_contracts::banking::v1::{self as pb, fees_service_server::FeesService};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{
	AppState,
	application::fees::{self as fee_app, FeePolicyPorts, PolicyChangeRequest, PolicyReader, PolicyView},
	ports::fees::{AssessmentRecord, FeePolicyChange},
	services::support::{caller_id, holds_permission, map_err, require_not_paused, require_permission, unix_now},
};

#[derive(Clone)]
pub struct FeesSvc {
	pub state: AppState,
}

impl FeesSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}
}

impl AppState {
	fn fee_policy_ports(&self) -> FeePolicyPorts<'_> {
		FeePolicyPorts {
			policies: self.fees.policies.as_ref(),
			changes: self.fees.changes.as_ref(),
			allocations: self.allocations.as_ref(),
			consilia: self.consilia.as_ref(),
			approval_url_base: &self.consilium_approval_url_base,
			governance_mail_wired: crate::infrastructure::governance_mail::is_wired(),
		}
	}
}

fn parse_change_id(raw: &str) -> Result<FeePolicyChangeId, Status> {
	Uuid::parse_str(raw)
		.map(FeePolicyChangeId::from_raw)
		.map_err(|_| Status::invalid_argument("invalid fee policy change id"))
}

/// The terms, validated into their domain form at the boundary so a bad rate or an unknown
/// vocabulary word is an `invalid_argument` about the input, not a validation error from
/// deeper in.
fn parse_policy(management_bps: u32, performance_bps: u32, hurdle_bps: u32, basis: &str, crystallization: &str) -> Result<FeePolicy, Status> {
	FeePolicy::new(
		management_bps,
		performance_bps,
		hurdle_bps,
		ManagementBasis::parse(basis).map_err(map_err)?,
		CrystallizationPeriod::parse(crystallization).map_err(map_err)?,
	)
	.map_err(map_err)
}

#[tonic::async_trait]
impl FeesService for FeesSvc {
	async fn get_fee_policy(&self, request: Request<pb::GetFeePolicyRequest>) -> Result<Response<pb::FeePolicy>, Status> {
		let (reader, audience) = reader_of(&self.state, &request).await?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let view = fee_app::policy_view(self.state.fees.policies.as_ref(), self.state.fees.changes.as_ref(), &reader, &service)
			.await
			.map_err(map_err)?;
		Ok(Response::new(policy_to_proto(&service, &view, audience)))
	}

	async fn list_fee_policies(&self, request: Request<pb::ListFeePoliciesRequest>) -> Result<Response<pb::FeePolicyList>, Status> {
		let (reader, audience) = reader_of(&self.state, &request).await?;
		let policies = fee_app::list_policies(self.state.fees.policies.as_ref(), self.state.fees.changes.as_ref(), &reader)
			.await
			.map_err(map_err)?;
		Ok(Response::new(pb::FeePolicyList {
			policies: policies.iter().map(|(service, view)| policy_to_proto(service, view, audience)).collect(),
		}))
	}

	async fn schedule_fee_policy(&self, request: Request<pb::ScheduleFeePolicyRequest>) -> Result<Response<pb::FeePolicyChange>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		// The requester is the trust seam on the terms, exactly as `posted_by` is on a
		// valuation — and, for a change that needs the owners, the initiator of their consilium.
		let requester = caller_id(&request)?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let policy = parse_policy(req.management_bps, req.performance_bps, req.hurdle_bps, &req.basis, &req.crystallization)?;
		let change = fee_app::schedule_policy(
			&self.state.fee_policy_ports(),
			requester,
			PolicyChangeRequest {
				service,
				policy,
				requested_effective_from_unix: req.effective_from,
				reason: req.reason,
			},
			unix_now(),
		)
		.await
		.map_err(map_err)?;
		// WARN on success on purpose: a change of the terms investors are charged on is worth
		// an audit line that stands out, whichever path it took.
		tracing::warn!(
			change_id = %change.id,
			service = %change.service,
			requester = %requester,
			requirement = change.requirement.as_str(),
			state = change.state.as_str(),
			effective_from = change.effective_from_unix,
			"scheduled a fee-policy change"
		);
		Ok(Response::new(change_to_proto(&change, Audience::Operator)))
	}

	async fn cancel_fee_policy_change(&self, request: Request<pb::CancelFeePolicyChangeRequest>) -> Result<Response<pb::FeePolicyChange>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let by = caller_id(&request)?;
		let req = request.get_ref();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let id = parse_change_id(&req.change_id)?;
		let change = fee_app::cancel_change(self.state.fees.changes.as_ref(), self.state.consilia.as_ref(), &service, id, by, unix_now())
			.await
			.map_err(map_err)?;
		Ok(Response::new(change_to_proto(&change, Audience::Operator)))
	}

	async fn acknowledge_undelivered_notices(&self, request: Request<pb::AcknowledgeUndeliveredNoticesRequest>) -> Result<Response<pb::FeePolicyChange>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let by = caller_id(&request)?;
		let req = request.get_ref();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let id = parse_change_id(&req.change_id)?;
		let now = unix_now();
		let change = fee_app::acknowledge_undelivered_notices(self.state.fees.changes.as_ref(), self.state.consilia.as_ref(), &service, id, by, now)
			.await
			.map_err(map_err)?;
		// WARN on success, as scheduling is: somebody just took responsibility for holders
		// who were never told their terms are getting dearer. Worth a line that stands out —
		// but only when THIS call wrote the record (first, or extended by holders given up on
		// since: either way the row carries this caller and this moment). A repeat that added
		// nobody returns the standing record, somebody else's or an earlier one of the
		// caller's own, and the audit log must not report an acknowledgement that did not
		// happen. The record is recognised by the caller and the moment this call passed in.
		if let Some(waiver) = change.notices_waiver.as_ref().filter(|waiver| waiver.by == by.to_string() && waiver.at_unix == now) {
			tracing::warn!(
				change_id = %change.id,
				service = %change.service,
				acknowledged_by = %waiver.by,
				holders = ?waiver.users,
				"acknowledged undelivered holder notices on a fee-policy change"
			);
		}
		Ok(Response::new(change_to_proto(&change, Audience::Operator)))
	}

	async fn list_fee_policy_changes(&self, request: Request<pb::ListFeePolicyChangesRequest>) -> Result<Response<pb::FeePolicyChangeList>, Status> {
		let (reader, audience) = reader_of(&self.state, &request).await?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let changes = fee_app::list_changes(self.state.fees.changes.as_ref(), &reader, &service).await.map_err(map_err)?;
		Ok(Response::new(pb::FeePolicyChangeList {
			changes: changes.iter().map(|change| change_to_proto(change, audience)).collect(),
		}))
	}

	async fn list_fee_assessments(&self, request: Request<pb::ListFeeAssessmentsRequest>) -> Result<Response<pb::FeeAssessmentList>, Status> {
		let caller = caller_id(&request)?;
		let records = fee_app::list_assessments(self.state.fees.assessments.as_ref(), caller).await.map_err(map_err)?;
		Ok(Response::new(pb::FeeAssessmentList {
			assessments: records.iter().map(assessment_to_proto).collect(),
		}))
	}

	async fn list_fund_fee_assessments(&self, request: Request<pb::ListFundFeeAssessmentsRequest>) -> Result<Response<pb::FeeAssessmentList>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let records = fee_app::list_fund_assessments(self.state.fees.assessments.as_ref(), &service).await.map_err(map_err)?;
		Ok(Response::new(pb::FeeAssessmentList {
			assessments: records.iter().map(assessment_to_proto).collect(),
		}))
	}

	async fn get_accrued_fees(&self, request: Request<pb::GetAccruedFeesRequest>) -> Result<Response<pb::AccruedFees>, Status> {
		let caller = caller_id(&request)?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let accrued = fee_app::accrued_fees(
			self.state.fees.policies.as_ref(),
			self.state.fees.accruals.as_ref(),
			self.state.ledger.as_ref(),
			self.state.nav.as_ref(),
			caller,
			&service,
			unix_now(),
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(match accrued {
			Some(accrued) => pb::AccruedFees {
				service: service.to_string(),
				configured: true,
				management: accrued.management.to_decimal_string(),
				performance: accrued.performance.to_decimal_string(),
				debt: accrued.debt.to_decimal_string(),
				total: accrued.total.to_decimal_string(),
				high_water_mark: accrued.high_water_mark.to_decimal_string(),
			},
			// A fund with no policy, or an investor with no position: zeros, and
			// `configured = false` so a client can tell "charges nothing" from "owes
			// nothing right now".
			None => pb::AccruedFees {
				service: service.to_string(),
				configured: false,
				management: "0".into(),
				performance: "0".into(),
				debt: "0".into(),
				total: "0".into(),
				high_water_mark: "0".into(),
			},
		}))
	}

	async fn get_fee_shares(&self, request: Request<pb::GetFeeSharesRequest>) -> Result<Response<pb::FeeShares>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let (units, value) = fee_app::fee_shares(self.state.ledger.as_ref(), self.state.nav.as_ref(), &service, unix_now())
			.await
			.map_err(map_err)?;
		Ok(Response::new(pb::FeeShares {
			service: service.to_string(),
			units: units.to_decimal_string(),
			value: value.to_decimal_string(),
		}))
	}

	async fn settle_fee_shares(&self, request: Request<pb::SettleFeeSharesRequest>) -> Result<Response<pb::FeeSettlement>, Status> {
		require_permission(&self.state, &request, bank::Allocation::Manage).await?;
		require_not_paused(&self.state).await?;
		let settler = caller_id(&request)?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		// Empty means "all of it" — the ordinary end-of-period call.
		let units = match req.units.trim() {
			"" => None,
			raw => Some(Shares::parse_decimal(raw).map_err(map_err)?),
		};
		let settlement = fee_app::settle_fee_shares(
			self.state.fees.settlements.as_ref(),
			self.state.ledger.as_ref(),
			self.state.nav.as_ref(),
			self.state.redemptions.as_ref(),
			&self.state.relay_notify,
			service,
			units,
			settler,
			unix_now(),
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(pb::FeeSettlement {
			service: settlement.service().to_string(),
			units: settlement.units().to_decimal_string(),
			nav: settlement.nav().to_decimal_string(),
			cash: settlement.cash().to_decimal_string(),
		}))
	}
}

/// Who is reading a change. The terms, the state, the moments and the reason are public
/// information about a product; WHO asked and WHICH consilium decides are governance
/// detail, shown to operators and blanked for everyone else.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Audience {
	Operator,
	Investor,
}

/// The caller as a reader of terms: which products they may see, and how much of a change
/// they are shown. Both follow from the one permission — a question, not a gate, since these
/// handlers serve everyone and merely widen for a manager.
async fn reader_of<'a, T>(state: &'a AppState, request: &Request<T>) -> Result<(PolicyReader<'a>, Audience), Status> {
	let caller = caller_id(request)?;
	let unrestricted = holds_permission(state, request, bank::Allocation::Manage).await?;
	let reader = PolicyReader {
		allocations: state.allocations.as_ref(),
		caller,
		unrestricted,
	};
	Ok((reader, if unrestricted { Audience::Operator } else { Audience::Investor }))
}

fn policy_to_proto(service: &ServiceId, view: &PolicyView, audience: Audience) -> pb::FeePolicy {
	let pending = view.pending.as_ref().map(|change| change_to_proto(change, audience));
	match &view.current {
		Some(record) => pb::FeePolicy {
			service: service.to_string(),
			configured: true,
			management_bps: record.policy.management_bps(),
			performance_bps: record.policy.performance_bps(),
			hurdle_bps: record.policy.hurdle_bps(),
			basis: record.policy.basis().as_str().to_owned(),
			crystallization: record.policy.crystallization().as_str().to_owned(),
			updated_at: record.updated_at_unix,
			version: record.version,
			effective_from: record.effective_from_unix,
			pending,
		},
		// Not an error and not a 404: "this product charges no fee" is a real answer, and
		// the client renders it as such rather than as a missing policy — with the change on
		// its way, if one is, so a first policy is announced before it binds.
		None => pb::FeePolicy {
			service: service.to_string(),
			configured: false,
			management_bps: 0,
			performance_bps: 0,
			hurdle_bps: 0,
			basis: ManagementBasis::InvestedCapital.as_str().to_owned(),
			crystallization: CrystallizationPeriod::Annual.as_str().to_owned(),
			updated_at: 0,
			version: 0,
			effective_from: 0,
			pending,
		},
	}
}

fn change_to_proto(change: &FeePolicyChange, audience: Audience) -> pb::FeePolicyChange {
	let operator = audience == Audience::Operator;
	pb::FeePolicyChange {
		id: change.id.to_string(),
		service: change.service.to_string(),
		version: change.version,
		state: change.state.as_str().to_owned(),
		management_bps: change.policy.management_bps(),
		performance_bps: change.policy.performance_bps(),
		hurdle_bps: change.policy.hurdle_bps(),
		basis: change.policy.basis().as_str().to_owned(),
		crystallization: change.policy.crystallization().as_str().to_owned(),
		effective_from: change.effective_from_unix,
		requirement: change.requirement.as_str().to_owned(),
		consilium_id: change.consilium_id.filter(|_| operator).map(|id| id.to_string()).unwrap_or_default(),
		requested_by: if operator { change.requested_by.clone() } else { String::new() },
		requested_at: change.requested_at_unix,
		scheduled_at: change.scheduled_at_unix.unwrap_or_default(),
		applied_at: change.applied_at_unix.unwrap_or_default(),
		reason: change.reason.clone(),
		// The moment is public like every other moment on the change; who waived, for whom,
		// and how many are still waiting are governance detail.
		notices_waived_by: change.notices_waiver.as_ref().filter(|_| operator).map(|waiver| waiver.by.clone()).unwrap_or_default(),
		notices_waived_at: change.notices_waiver.as_ref().map(|waiver| waiver.at_unix).unwrap_or_default(),
		notices_waived_users: change
			.notices_waiver
			.as_ref()
			.filter(|_| operator)
			.map(|waiver| waiver.users.iter().map(ToString::to_string).collect())
			.unwrap_or_default(),
		undelivered_notices: if operator { change.undelivered_notices } else { 0 },
		notices_given_up: if operator { change.notices_given_up } else { 0 },
		notices_unacknowledged: if operator { change.notices_unacknowledged } else { 0 },
	}
}

fn assessment_to_proto(record: &AssessmentRecord) -> pb::FeeAssessment {
	pb::FeeAssessment {
		service: record.service.to_string(),
		trigger: record.trigger.as_str().to_owned(),
		nav: record.nav.to_decimal_string(),
		management: record.management.to_decimal_string(),
		performance: record.performance.to_decimal_string(),
		debt_opening: record.debt_opening.to_decimal_string(),
		charged_units: record.charged_units.to_decimal_string(),
		charged_cash: record.charged_cash.to_decimal_string(),
		debt_carried: record.debt_carried.to_decimal_string(),
		high_water_mark: record.high_water_mark.to_decimal_string(),
		assessed_at: record.assessed_at_unix,
	}
}

#[cfg(test)]
mod tests;
