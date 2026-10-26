//! `fees` context — a fund's management and performance fee.
//!
//! Reads of a fund's *terms* are open to any authenticated user who can see the product: an
//! investor is entitled to know what they are paying before they pay it; a product hidden
//! from them is `NOT_FOUND`, exactly as `GetAllocation` answers, unless they hold
//! `bank:allocation:manage`. Reads of a *charge* are scoped to the caller's own statement.
//! Everything that moves value is gated on `bank:allocation:manage` — the same trust seam as
//! registering the product or posting its valuation.
//!
//! Only [`SettleFeeShares`](FeesService::settle_fee_shares) moves money, and it
//! notifies the relay. Charging happens on the sweeper, not on this surface: a fee is
//! a consequence of time passing, never of somebody calling an RPC.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large type
//! we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use concierge_domain::authz::bank;
use domain::{
	balance::ServiceId,
	fees::{CrystallizationPeriod, ManagementBasis},
	money::Shares,
};
use evbanking_contracts::banking::v1::{self as pb, fees_service_server::FeesService};
use tonic::{Request, Response, Status};

use crate::{
	AppState,
	application::fees::{self as fee_app, PolicyReader},
	ports::fees::{AssessmentRecord, PolicyRecord},
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

#[tonic::async_trait]
impl FeesService for FeesSvc {
	async fn get_fee_policy(&self, request: Request<pb::GetFeePolicyRequest>) -> Result<Response<pb::FeePolicy>, Status> {
		let reader = reader_of(&self.state, &request).await?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let record = fee_app::policy_view(self.state.fees.policies.as_ref(), &reader, &service).await.map_err(map_err)?;
		Ok(Response::new(policy_to_proto(&service, record.as_ref())))
	}

	async fn list_fee_policies(&self, request: Request<pb::ListFeePoliciesRequest>) -> Result<Response<pb::FeePolicyList>, Status> {
		let reader = reader_of(&self.state, &request).await?;
		let policies = fee_app::list_policies(self.state.fees.policies.as_ref(), &reader).await.map_err(map_err)?;
		Ok(Response::new(pb::FeePolicyList {
			policies: policies.iter().map(|(service, record)| policy_to_proto(service, Some(record))).collect(),
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

/// The caller as a reader of terms: which products they may see. It follows from the one
/// permission — a question, not a gate, since these handlers serve everyone and merely widen
/// for a manager.
async fn reader_of<'a, T>(state: &'a AppState, request: &Request<T>) -> Result<PolicyReader<'a>, Status> {
	let caller = caller_id(request)?;
	let unrestricted = holds_permission(state, request, bank::Allocation::Manage).await?;
	Ok(PolicyReader {
		allocations: state.allocations.as_ref(),
		caller,
		unrestricted,
	})
}

fn policy_to_proto(service: &ServiceId, record: Option<&PolicyRecord>) -> pb::FeePolicy {
	match record {
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
		},
		// Not an error and not a 404: "this product charges no fee" is a real answer, and
		// the client renders it as such rather than as a missing policy.
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
		},
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
