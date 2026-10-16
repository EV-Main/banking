//! `CompartmentService` — the caller's wallet split by scope (`wallet.proto`). Every RPC
//! acts on the caller's own access-token `sub`.
//!
//! A mandate is granted only to a client banking's own scope-ledger grants let touch users'
//! compartments in the scope; with the scope ledger unconfigured there is none, and every grant
//! is INVALID_ARGUMENT. A grant is consent to future debits, so a frozen caller cannot give it;
//! a revoke takes money out of a service's reach and is never gated beyond the token.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use domain::{
	money::Usdt,
	scope_ledger::{ClientId, MandateOperation},
	scopes::{Compartment, MoveRefusal, ScopeId},
};
use evbanking_contracts::banking::v1::{self as pb, compartment_service_server::CompartmentService};
use tonic::{Request, Response, Status};

use crate::{
	AppState,
	application::{
		compartments::{self as compartments_app, CompartmentPorts, CompartmentView, MoveError},
		scope_mandates::{self as mandates_app, GrantRequest, MandatePorts},
	},
	ports::{
		compartments::{RecordedMove, ScopeRecord},
		scope_mandates::RecordedMandate,
	},
	services::support::{MoneyGate, caller_id, gated_caller, map_err},
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

	fn mandate_ports(&self) -> MandatePorts<'_> {
		MandatePorts {
			grants: &self.state.scope_grants,
			store: self.state.mandates.as_ref(),
			scopes: self.state.compartments.as_ref(),
			relay: &self.state.relay_notify,
		}
	}
}

#[tonic::async_trait]
impl CompartmentService for CompartmentSvc {
	async fn list_compartments(&self, request: Request<pb::ListCompartmentsRequest>) -> Result<Response<pb::CompartmentList>, Status> {
		let user = caller_id(&request)?;
		let (views, kyc_level) = compartments_app::list_compartments(&self.ports(), user).await.map_err(map_err)?;
		Ok(Response::new(pb::CompartmentList {
			compartments: views.iter().map(|view| compartment_to_proto(view, kyc_level)).collect(),
		}))
	}

	async fn get_compartment(&self, request: Request<pb::GetCompartmentRequest>) -> Result<Response<pb::Compartment>, Status> {
		let user = caller_id(&request)?;
		let scope = Compartment::parse(&request.get_ref().scope).map_err(map_err)?;
		let (view, kyc_level) = compartments_app::get_compartment(&self.ports(), user, &scope).await.map_err(map_err)?;
		Ok(Response::new(compartment_to_proto(&view, kyc_level)))
	}

	async fn move_funds(&self, request: Request<pb::MoveFundsRequest>) -> Result<Response<pb::CompartmentMove>, Status> {
		let from = Compartment::parse(&request.get_ref().from_scope).map_err(map_err)?;
		let to = Compartment::parse(&request.get_ref().to_scope).map_err(map_err)?;
		// Money going back to `main` never leaves the platform: neither the pause nor the
		// freeze stops it (wallet.proto). A move in answers the freeze itself, as
		// `user_frozen`, so only the pause is the gate's.
		let gate = MoneyGate {
			pause: from == Compartment::Main,
			freeze: false,
		};
		let user = gated_caller(&self.state, &request, gate).await?;
		let req = request.into_inner();
		let amount = Usdt::parse_decimal(&req.amount).map_err(map_err)?;
		let recorded = compartments_app::move_funds(&self.ports(), user, from, to, amount, &req.idempotency_key)
			.await
			.map_err(|err| match err {
				// The reason is the whole message: clients branch on (code, message).
				MoveError::Refused(refusal @ MoveRefusal::AmountBelowMinimum) => Status::invalid_argument(refusal.reason()),
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
		let user = gated_caller(&self.state, &request, MoneyGate { pause: false, freeze: true }).await?;
		let req = request.into_inner();
		let parsed = GrantRequest {
			scope: ScopeId::parse(&req.scope).map_err(map_err)?,
			client: ClientId::parse(&req.client_id).map_err(map_err)?,
			cap_per_op: Usdt::parse_decimal(&req.cap_per_op).map_err(map_err)?,
			cap_24h: Usdt::parse_decimal(&req.cap_24h).map_err(map_err)?,
			cap_outstanding: optional_cap(&req.cap_outstanding)?,
			cap_book_fill_24h: optional_cap(&req.cap_book_fill_24h)?,
			operations: req.operations.iter().map(|raw| operation_of(*raw)).collect::<Result<_, _>>()?,
			expires_at: match req.expires_at {
				0 => None,
				at if at < 0 => return Err(Status::invalid_argument("expires_at must be unix seconds, or 0")),
				at => Some(at),
			},
			idempotency_key: req.idempotency_key,
		};
		let granted = mandates_app::grant(&self.mandate_ports(), user, parsed).await.map_err(map_err)?;
		Ok(Response::new(mandate_to_proto(&granted)))
	}

	async fn revoke_mandate(&self, request: Request<pb::RevokeMandateRequest>) -> Result<Response<pb::Mandate>, Status> {
		let user = gated_caller(&self.state, &request, MoneyGate { pause: false, freeze: false }).await?;
		let raw = &request.get_ref().mandate_id;
		// Lowercase only, as banking writes it: one mandate, one spelling.
		let id = uuid::Uuid::parse_str(raw)
			.ok()
			.filter(|id| id.to_string() == *raw)
			.ok_or_else(|| Status::invalid_argument("mandate_id must be a lowercase uuid"))?;
		let revoked = mandates_app::revoke(&self.mandate_ports(), user, id).await.map_err(map_err)?;
		Ok(Response::new(mandate_to_proto(&revoked)))
	}

	async fn list_mandates(&self, request: Request<pb::ListMandatesRequest>) -> Result<Response<pb::MandateList>, Status> {
		let user = caller_id(&request)?;
		let live = mandates_app::list(&self.mandate_ports(), user).await.map_err(map_err)?;
		Ok(Response::new(pb::MandateList {
			mandates: live.iter().map(mandate_to_proto).collect(),
		}))
	}
}

fn optional_cap(raw: &str) -> Result<Option<Usdt>, Status> {
	if raw.is_empty() {
		return Ok(None);
	}
	Usdt::parse_decimal(raw).map(Some).map_err(map_err)
}

fn operation_of(raw: i32) -> Result<MandateOperation, Status> {
	match pb::MandateOperation::try_from(raw) {
		Ok(pb::MandateOperation::Reserve) => Ok(MandateOperation::Reserve),
		Ok(pb::MandateOperation::Charge) => Ok(MandateOperation::Charge),
		Ok(pb::MandateOperation::BookFill) => Ok(MandateOperation::BookFill),
		Ok(pb::MandateOperation::Unspecified) | Err(_) => Err(Status::invalid_argument("operations must name RESERVE, CHARGE or BOOK_FILL")),
	}
}

fn operation_to_proto(operation: MandateOperation) -> pb::MandateOperation {
	match operation {
		MandateOperation::Reserve => pb::MandateOperation::Reserve,
		MandateOperation::Charge => pb::MandateOperation::Charge,
		MandateOperation::BookFill => pb::MandateOperation::BookFill,
	}
}

fn mandate_to_proto(recorded: &RecordedMandate) -> pb::Mandate {
	let mandate = &recorded.mandate;
	let terms = &mandate.terms;
	pb::Mandate {
		mandate_id: mandate.id.to_string(),
		scope: terms.scope.as_str().to_owned(),
		client_id: terms.client.as_str().to_owned(),
		// Banking's grants carry no human name for a client yet; its id is what the user sees.
		client_display_name: terms.client.as_str().to_owned(),
		cap_per_op: terms.caps.per_op.to_decimal_string(),
		cap_24h: terms.caps.per_24h.to_decimal_string(),
		operations: terms.operations.iter().map(|operation| operation_to_proto(*operation) as i32).collect(),
		expires_at: terms.expires_at.unwrap_or(0),
		created_at: mandate.created_at,
		revoked_at: mandate.revoked_at.unwrap_or(0),
		replaced_at: mandate.replaced_at.unwrap_or(0),
		used_24h: recorded.usage.drawn_24h.to_decimal_string(),
		cap_outstanding: terms.caps.outstanding.to_decimal_string(),
		cap_book_fill_24h: optional_amount(terms.caps.book_fill_24h),
		outstanding: recorded.usage.outstanding.to_decimal_string(),
		book_filled_24h: recorded.usage.book_filled_24h.to_decimal_string(),
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
		state: recorded.state.as_str().to_owned(),
		failure_reason: recorded.failure_reason.clone().unwrap_or_default(),
	}
}
