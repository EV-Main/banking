//! `operations` context — the caller's unified activity timeline (read-only).
//!
//! The one job beyond delegating to the query use case is flattening the
//! [`Operation`] sum type onto the wire's discriminated message: the port models the
//! kinds as variants so an impossible row cannot be constructed, while the wire
//! keeps one flat shape so the generated TypeScript stays a single type. The mapping
//! below is the seam where that trade is paid, once.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use evbanking_contracts::banking::v1::{self as pb, operations_service_server::OperationsService};
use tonic::{Request, Response, Status};

use crate::{
	AppState,
	application::operations as operations_app,
	ports::operations::Operation,
	services::support::{caller_id, map_err},
};

/// The compartment every row of the feed belongs to today.
const MAIN_SCOPE: &str = "main";

#[derive(Clone)]
pub struct OperationsSvc {
	pub state: AppState,
}

impl OperationsSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}
}

#[tonic::async_trait]
impl OperationsService for OperationsSvc {
	async fn list_operations(&self, request: Request<pb::ListOperationsRequest>) -> Result<Response<pb::OperationList>, Status> {
		let user = caller_id(&request)?;
		let page = operations_app::list_operations(self.state.operations.as_ref(), user, request.get_ref().limit)
			.await
			.map_err(map_err)?;
		Ok(Response::new(pb::OperationList {
			operations: page.operations.iter().map(operation_to_proto).collect(),
			truncated: page.truncated,
		}))
	}
}

fn operation_to_proto(operation: &Operation) -> pb::Operation {
	// Every field absent for a kind is left at its proto3 default (the empty string),
	// which is what the wire contract documents as "this kind does not carry it".
	let base = pb::Operation {
		kind: operation.kind().to_owned(),
		created_at: operation.created_at(),
		// Every kind in the feed moves the caller's main balance; compartment events join the
		// feed with their own scope when they are projected into it.
		scope: MAIN_SCOPE.to_owned(),
		..Default::default()
	};
	match operation {
		Operation::Deposit { tx_ref, network, amount, .. } => pb::Operation {
			id: tx_ref.as_str().to_owned(),
			state: "credited".to_owned(),
			amount: amount.to_decimal_string(),
			network: network.as_str().to_owned(),
			tx_ref: tx_ref.as_str().to_owned(),
			..base
		},
		Operation::Withdrawal {
			id,
			network,
			address,
			amount,
			fee,
			state,
			tx_ref,
			..
		} => pb::Operation {
			id: id.to_string(),
			state: state.as_str().to_owned(),
			amount: amount.to_decimal_string(),
			fee: fee.to_decimal_string(),
			// The fee is retained out of the gross, so it can never exceed it — but a
			// saturating floor beats an unwrap on a money figure the user reads.
			net_amount: amount.checked_sub(*fee).unwrap_or(domain::money::Usdt::ZERO).to_decimal_string(),
			network: network.as_str().to_owned(),
			address: address.clone(),
			tx_ref: tx_ref.as_ref().map(|tx_ref| tx_ref.as_str().to_owned()).unwrap_or_default(),
			..base
		},
	}
}
