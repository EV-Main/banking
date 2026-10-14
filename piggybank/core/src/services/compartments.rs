//! `CompartmentService` — the caller's wallet split by scope (`wallet.proto`). Every RPC
//! acts on the caller's own access-token `sub`.
//!
//! Mandates are not served yet: granting one needs banking's per-client grants, which
//! arrive with `ScopeLedgerService`, and nothing may draw on a mandate before then. So
//! `GrantMandate` / `RevokeMandate` are UNIMPLEMENTED and `ListMandates` is the truthful
//! empty list.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use domain::{
	money::Usdt,
	scopes::{Compartment, ScopeId},
};
use evbanking_contracts::banking::v1::{self as pb, compartment_service_server::CompartmentService};
use tonic::{Request, Response, Status};

use crate::{
	AppState,
	application::compartments::{self as compartments_app, CompartmentPorts, CompartmentView, MoveError},
	ports::compartments::{RecordedMove, ScopeRecord},
	services::support::{caller_id, map_err, unpaused_caller},
};

#[derive(Clone)]
pub struct CompartmentSvc {
	pub state: AppState,
}

impl CompartmentSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}

	fn ports(&self) -> CompartmentPorts<'_> {
		CompartmentPorts {
			store: self.state.compartments.as_ref(),
			ledger: self.state.ledger.as_ref(),
			policy: self.state.outflow.as_ref(),
			relay: &self.state.relay_notify,
		}
	}
}

/// Mandates wait for the scope ledger's client grants (module header).
const MANDATES_NOT_YET: &str = "mandates are not available yet";

#[tonic::async_trait]
impl CompartmentService for CompartmentSvc {
	async fn list_compartments(&self, request: Request<pb::ListCompartmentsRequest>) -> Result<Response<pb::CompartmentList>, Status> {
		let user = caller_id(&request)?;
		let (views, kyc_level) = compartments_app::list_compartments(&self.ports(), user).await.map_err(map_err)?;
		Ok(Response::new(pb::CompartmentList {
			compartments: views.iter().map(|view| compartment_to_proto(view, kyc_level)).collect(),
		}))
	}

	async fn move_funds(&self, request: Request<pb::MoveFundsRequest>) -> Result<Response<pb::CompartmentMove>, Status> {
		let user = unpaused_caller(&self.state, &request).await?;
		let req = request.into_inner();
		let from = Compartment::parse(&req.from_scope).map_err(map_err)?;
		let to = Compartment::parse(&req.to_scope).map_err(map_err)?;
		let amount = Usdt::parse_decimal(&req.amount).map_err(map_err)?;
		let recorded = compartments_app::move_funds(&self.ports(), user, from, to, amount, &req.idempotency_key)
			.await
			.map_err(|err| match err {
				// The reason is the whole message: clients branch on (code, message).
				MoveError::Refused(refusal) => Status::failed_precondition(refusal.reason()),
				MoveError::Domain(err) => map_err(err),
			})?;
		Ok(Response::new(move_to_proto(&recorded)))
	}

	async fn get_scope_policy(&self, request: Request<pb::GetScopePolicyRequest>) -> Result<Response<pb::ScopePolicy>, Status> {
		let user = caller_id(&request)?;
		let scope = ScopeId::parse(&request.get_ref().scope).map_err(map_err)?;
		let view = compartments_app::get_scope_policy(&self.ports(), user, &scope).await.map_err(map_err)?;
		Ok(Response::new(policy_to_proto(&view.scope, view.caller_eligible)))
	}

	async fn grant_mandate(&self, request: Request<pb::GrantMandateRequest>) -> Result<Response<pb::Mandate>, Status> {
		caller_id(&request)?;
		Err(Status::unimplemented(MANDATES_NOT_YET))
	}

	async fn revoke_mandate(&self, request: Request<pb::RevokeMandateRequest>) -> Result<Response<pb::Mandate>, Status> {
		caller_id(&request)?;
		Err(Status::unimplemented(MANDATES_NOT_YET))
	}

	async fn list_mandates(&self, request: Request<pb::ListMandatesRequest>) -> Result<Response<pb::MandateList>, Status> {
		caller_id(&request)?;
		Ok(Response::new(pb::MandateList { mandates: Vec::new() }))
	}
}

fn optional_amount(amount: Option<Usdt>) -> String {
	amount.map(Usdt::to_decimal_string).unwrap_or_default()
}

fn policy_to_proto(scope: &ScopeRecord, caller_eligible: bool) -> pb::ScopePolicy {
	let policy = &scope.policy;
	pb::ScopePolicy {
		scope: scope.id.as_str().to_owned(),
		min_kyc_level: policy.min_kyc_level,
		max_per_move: optional_amount(policy.max_per_move),
		max_moved_in_24h: optional_amount(policy.max_moved_in_24h),
		max_balance: optional_amount(policy.max_balance),
		caller_eligible,
		updated_at: scope.policy_updated_at,
		auto_payout_per_op: policy.auto_payout_per_op.to_decimal_string(),
		auto_payout_24h: policy.auto_payout_24h.to_decimal_string(),
		auto_transfer_24h: policy.auto_transfer_24h.to_decimal_string(),
	}
}

fn compartment_to_proto(view: &CompartmentView, kyc_level: u32) -> pb::Compartment {
	pb::Compartment {
		scope: view.compartment.as_str().to_owned(),
		display_name: view.display_name.clone(),
		available: view.available.to_decimal_string(),
		held: view.held.to_decimal_string(),
		total: view.total.to_decimal_string(),
		policy: view.scope.as_ref().map(|scope| policy_to_proto(scope, scope.policy.admits_kyc(kyc_level))),
	}
}

fn move_to_proto(recorded: &RecordedMove) -> pb::CompartmentMove {
	let movement = &recorded.movement;
	pb::CompartmentMove {
		id: movement.id().to_string(),
		from_scope: movement.from().as_str().to_owned(),
		to_scope: movement.to().as_str().to_owned(),
		amount: movement.amount().to_decimal_string(),
		created_at: recorded.created_at,
	}
}
