//! `payout_review` context — the lines of a pending `bank:scope_payout`, for its approvers
//! (banking#529). Read-only; see `payout_review.proto` for what is decided where.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use evbanking_contracts::banking::v1::{self as pb, payout_review_service_server::PayoutReviewService};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{
	AppState,
	infrastructure::payout_review::{self, ReviewLine, ReviewPage},
	services::support::{MoneyGate, gated_caller, map_err},
};

const DEFAULT_PAGE: u32 = 50;
const MAX_PAGE: u32 = 200;

/// The one refusal for every id that does not lead to a readable payout — unknown, not a
/// payout, approval not opened, approval id not its own, malformed — so a probe cannot tell
/// which ids exist.
fn hidden() -> Status {
	Status::not_found("payout operation")
}

/// A lowercase hyphenated UUID, the form `pending_operation:<id>` carries; anything else
/// cannot name an operation.
fn operation_id(raw: &str) -> Result<Uuid, Status> {
	match Uuid::parse_str(raw) {
		Ok(id) if id.hyphenated().to_string() == raw => Ok(id),
		_ => Err(hidden()),
	}
}

#[derive(Clone)]
pub struct PayoutReviewSvc {
	pub state: AppState,
}

impl PayoutReviewSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}

	/// An access token whose tokens were not revoked. The pause and the freeze guard money
	/// that is about to move; this moves none, and a frozen approver must still be able to read
	/// what they are asked to decide.
	async fn session<T>(&self, request: &Request<T>) -> Result<(), Status> {
		gated_caller(&self.state, request, MoneyGate { pause: false, freeze: false }).await.map(|_| ())
	}
}

#[tonic::async_trait]
impl PayoutReviewService for PayoutReviewSvc {
	async fn get_payout_approval(&self, request: Request<pb::GetPayoutApprovalRequest>) -> Result<Response<pb::PayoutApprovalRef>, Status> {
		self.session(&request).await?;
		let id = operation_id(&request.get_ref().operation_id)?;
		let approval_id = payout_review::approval_of(&self.state.pool, id).await.map_err(map_err)?.ok_or_else(hidden)?;
		Ok(Response::new(pb::PayoutApprovalRef { approval_id }))
	}

	async fn list_payout_lines(&self, request: Request<pb::ListPayoutLinesRequest>) -> Result<Response<pb::PayoutLinesPage>, Status> {
		self.session(&request).await?;
		let req = request.get_ref();
		let id = operation_id(&req.operation_id)?;
		let limit = match req.limit {
			0 => DEFAULT_PAGE,
			n => n.min(MAX_PAGE),
		};
		let page = payout_review::page(&self.state.pool, id, &req.approval_id, req.offset as usize, limit as usize)
			.await
			.map_err(map_err)?
			.ok_or_else(hidden)?;
		Ok(Response::new(page_to_proto(id, req.offset, page)))
	}
}

fn page_to_proto(id: Uuid, offset: u32, page: ReviewPage) -> pb::PayoutLinesPage {
	pb::PayoutLinesPage {
		operation_id: id.to_string(),
		state: page.state,
		from_account: page.from_account,
		total: page.total.to_decimal_string(),
		max_line: page.max_line.to_decimal_string(),
		recipients: page.recipients,
		recipients_without_history: page.recipients_without_history,
		distribution: page.distribution,
		over_scope_budget: page.over_scope_budget,
		lines_digest: page.lines_digest,
		offset,
		lines: page.lines.into_iter().map(line_to_proto).collect(),
	}
}

fn line_to_proto(line: ReviewLine) -> pb::PayoutReviewLine {
	pb::PayoutReviewLine {
		line_no: line.line_no,
		recipient: line.recipient,
		amount: line.amount.to_decimal_string(),
		no_history: line.no_history,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_a_lowercase_hyphenated_uuid_names_an_operation() {
		let id = Uuid::new_v4();
		assert_eq!(operation_id(&id.to_string()).unwrap(), id);
		for bad in [id.to_string().to_uppercase(), id.simple().to_string(), String::new(), "x".into(), format!("{id} ")] {
			assert_eq!(operation_id(&bad).unwrap_err().code(), tonic::Code::NotFound, "{bad:?}");
		}
	}
}
