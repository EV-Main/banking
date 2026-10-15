//! `InvoicePaymentService` — the payer's side of a scope's invoice (`banking/v1/invoices.proto`).
//! Every RPC acts on the caller's own access-token `sub`.
//!
//! The first-party rule (`Origin: https://evinvest.ltd` for `PayInvoice`) is the BFF's: this
//! listener sees no browser origin. Here the payment passes the revocation and the outflow
//! pause gates; the freeze is answered by the use case, under its locks, as `user_frozen`.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use domain::{error::DomainError, scope_invoices::InvoiceRefusal, scopes::Compartment};
use evbanking_contracts::banking::v1::{self as pb, invoice_payment_service_server::InvoicePaymentService};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{
	AppState,
	application::scope_invoices::{self as invoices_app, PayableView, PayerPorts},
	ports::scope_invoices::PaymentError,
	services::support::{MoneyGate, caller_id, gated_caller, map_err},
};

#[derive(Clone)]
pub struct InvoicePaymentSvc {
	pub state: AppState,
}

impl InvoicePaymentSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}

	fn ports(&self) -> PayerPorts<'_> {
		PayerPorts {
			invoices: self.state.invoices.as_ref(),
			scopes: self.state.compartments.as_ref(),
			ledger: self.state.ledger.as_ref(),
			relay: &self.state.relay_notify,
		}
	}
}

fn invoice_id_of(raw: &str) -> Result<Uuid, Status> {
	// Any spelling of an id that is not banking's own names no invoice it issued; the answer is
	// the one an unknown id gets.
	Uuid::parse_str(raw).ok().filter(|id| id.to_string() == raw).ok_or_else(|| Status::not_found("invoice"))
}

#[tonic::async_trait]
impl InvoicePaymentService for InvoicePaymentSvc {
	async fn get_payable_invoice(&self, request: Request<pb::GetPayableInvoiceRequest>) -> Result<Response<pb::PayableInvoice>, Status> {
		let user = caller_id(&request)?;
		let id = invoice_id_of(&request.get_ref().invoice_id)?;
		let view = invoices_app::get_payable(&self.ports(), user, id).await.map_err(map_err)?;
		Ok(Response::new(payable_to_proto(&view)))
	}

	async fn pay_invoice(&self, request: Request<pb::PayInvoiceRequest>) -> Result<Response<pb::PayableInvoice>, Status> {
		// The money stays on the platform but leaves the payer for a scope that can pay it out,
		// so the outflow pause stops it like a move in; the freeze is the use case's to answer
		// as `user_frozen`.
		let gate = MoneyGate { pause: true, freeze: false };
		let user = gated_caller(&self.state, &request, gate).await?;
		let req = request.into_inner();
		let id = invoice_id_of(&req.invoice_id)?;
		let from = Compartment::parse(&req.pay_from).map_err(map_err)?;
		let view = invoices_app::pay(&self.ports(), user, id, from, &req.idempotency_key).await.map_err(|err| match err {
			// Another payer's invoice is the same NOT_FOUND as an unknown one.
			PaymentError::Refused(InvoiceRefusal::OtherPayer) => map_err(DomainError::NotFound {
				entity: "invoice",
				id: id.to_string(),
			}),
			// The reason is the whole message: clients branch on (code, message).
			PaymentError::Refused(refusal) => Status::failed_precondition(refusal.reason()),
			PaymentError::Domain(err) => map_err(err),
		})?;
		Ok(Response::new(payable_to_proto(&view)))
	}
}

fn payable_to_proto(view: &PayableView) -> pb::PayableInvoice {
	let invoice = &view.recorded.invoice;
	let terms = invoice.terms();
	let payment = invoice.payment();
	pb::PayableInvoice {
		invoice_id: invoice.id().to_string(),
		scope: terms.scope.as_str().to_owned(),
		display_name: view.display_name.clone(),
		amount: terms.amount.to_decimal_string(),
		memo: terms.memo.clone(),
		state: view.state().as_str().to_owned(),
		expires_at: terms.expires_at,
		paid_at: payment.map_or(0, |paid| paid.paid_at),
		paid_from: payment.map(|paid| paid.from.as_str().to_owned()).unwrap_or_default(),
		available_main: view.available_main.to_decimal_string(),
		available_scope: view.available_scope.to_decimal_string(),
	}
}
