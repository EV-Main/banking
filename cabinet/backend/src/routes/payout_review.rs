//! A pending scope payout's lines, for the people asked to approve it (banking#529).
//!
//! `GET /api/approvals/payouts/{operation_id}/lines` — read-only, behind the session. The
//! approval concierge shows carries totals, the largest line and two flags; this is where an
//! approver sees who is paid what.
//!
//! Who may read is decided HERE, because only the BFF holds both planes' tokens. The order
//! matters: the operation's approval id is read from banking first (no line, no amount, no
//! name), then concierge is asked — with the CALLER's own token — for that approval, and
//! only a voter on its frozen roster gets past. Lines are read from banking last, bound to
//! that approval id. Every refusal on the way is one identical 404, so the route is no
//! oracle for which operations exist or who sits on whose roster.
//!
//! Concierge's `GetApproval` is wider than a roster (platform owners and the scope's admins
//! see it too, and a requester sees it as a bystander); membership is therefore checked on
//! the roster itself, by the caller's verified email, not on the call succeeding.

use axum::{
	Json,
	extract::{Path, Query, State},
};
use axum_extra::extract::cookie::CookieJar;
use evbanking_contracts::banking::v1 as bk;
use evconcierge_contracts::concierge::v1 as cc;
use serde::Deserialize;
use tonic::{Code, Status};

use crate::{
	dto,
	error::ApiError,
	routes::{money_token_for, require_identity},
	state::AppState,
};

/// The kind and proposer of the approvals this route serves; anything else is somebody
/// else's approval whose subject merely looks alike.
const KIND: &str = "bank:scope_payout";
const PROPOSER: &str = "bank";

#[derive(Deserialize)]
pub struct PageQuery {
	offset: Option<u32>,
	limit: Option<u32>,
}

/// The answer to every refusal that must not be told apart.
fn hidden() -> ApiError {
	ApiError::Grpc(Status::not_found("not found"))
}

/// Upstream verdicts that mean "not yours / not there" fold into [`hidden`]; a plane that is
/// merely down keeps its own status, which tells a prober nothing about any id.
fn refusal(status: Status) -> ApiError {
	match status.code() {
		Code::NotFound | Code::PermissionDenied | Code::InvalidArgument | Code::FailedPrecondition => hidden(),
		_ => ApiError::read(status, "payout lines unavailable"),
	}
}

/// A lowercase hyphenated UUID. Checked by shape: the BFF carries no `uuid` dependency and
/// banking re-parses the id anyway; this only keeps junk from costing an upstream call.
fn is_operation_id(raw: &str) -> bool {
	raw.len() == 36
		&& raw.bytes().enumerate().all(|(i, b)| match i {
			8 | 13 | 18 | 23 => b == b'-',
			_ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
		})
}

/// Whether `approval` is the payout approval of `operation_id` and `email` is on its roster.
fn on_roster(approval: &cc::Approval, approval_id: &str, operation_id: &str, email: &str) -> bool {
	let email = email.trim().to_lowercase();
	approval.id == approval_id
		&& approval.kind == KIND
		&& approval.proposer == PROPOSER
		&& approval.subject_ref == format!("pending_operation:{operation_id}")
		&& !email.is_empty()
		&& approval.voters.iter().any(|voter| voter.email.trim().to_lowercase() == email)
}

pub async fn lines(State(st): State<AppState>, jar: CookieJar, Path(operation_id): Path<String>, Query(q): Query<PageQuery>) -> Result<Json<dto::PayoutLinesPage>, ApiError> {
	let (token, claims) = require_identity(&st, &jar).await?;
	// Lowercase hyphenated only: it is what the approval's `subject_ref` carries.
	if !is_operation_id(&operation_id) {
		return Err(hidden());
	}
	let money = money_token_for(&st, &claims).await?;

	let approval_id = st.grpc.get_payout_approval(&money, &operation_id).await.map_err(refusal)?.approval_id;
	let (me, approval) = tokio::try_join!(st.grpc.get_me(&token), st.grpc.get_approval(&token, &approval_id)).map_err(refusal)?;
	if !on_roster(&approval, &approval_id, &operation_id, &me.email) {
		return Err(hidden());
	}

	let req = bk::ListPayoutLinesRequest {
		operation_id,
		approval_id,
		offset: q.offset.unwrap_or(0),
		limit: q.limit.unwrap_or(0),
	};
	let page = st.grpc.list_payout_lines(&money, req).await.map_err(refusal)?;
	Ok(Json(page.into()))
}

#[cfg(test)]
mod tests {
	use super::*;

	const OP: &str = "0b9d3c1e-3c3a-4f6e-9a55-2f1d7b7f0a10";

	fn approval() -> cc::Approval {
		cc::Approval {
			id: "a-1".into(),
			kind: KIND.into(),
			proposer: PROPOSER.into(),
			subject_ref: format!("pending_operation:{OP}"),
			voters: vec![cc::ApprovalVoter {
				email: "Owner@Example.com".into(),
				..Default::default()
			}],
			..Default::default()
		}
	}

	#[test]
	fn only_a_lowercase_hyphenated_uuid_passes_the_shape_check() {
		assert!(is_operation_id(OP));
		for bad in ["", "x", &OP.to_uppercase(), &OP.replace('-', ""), &format!("{OP} ")] {
			assert!(!is_operation_id(bad), "{bad:?}");
		}
	}

	#[test]
	fn a_voter_of_the_operations_own_approval_is_on_the_roster() {
		assert!(on_roster(&approval(), "a-1", OP, "owner@example.com"));
		assert!(on_roster(&approval(), "a-1", OP, " OWNER@example.com "));
	}

	#[test]
	fn anyone_off_the_roster_is_refused() {
		assert!(!on_roster(&approval(), "a-1", OP, "other@example.com"));
		assert!(!on_roster(&approval(), "a-1", OP, ""));
		// A bystander reads the roster with the addresses stripped: nobody matches, not even
		// an empty address.
		let mut stripped = approval();
		stripped.voters[0].email.clear();
		assert!(!on_roster(&stripped, "a-1", OP, ""));
		assert!(!on_roster(&stripped, "a-1", OP, "owner@example.com"));
	}

	#[test]
	fn an_approval_that_is_not_this_operations_payout_is_refused() {
		let email = "owner@example.com";
		assert!(!on_roster(&approval(), "a-2", OP, email), "another approval id");
		assert!(!on_roster(&approval(), "a-1", "0b9d3c1e-3c3a-4f6e-9a55-2f1d7b7f0a11", email), "another operation");
		let mut other_kind = approval();
		other_kind.kind = "bank:account_transfer".into();
		assert!(!on_roster(&other_kind, "a-1", OP, email));
		let mut other_proposer = approval();
		other_proposer.proposer = "sa".into();
		assert!(!on_roster(&other_proposer, "a-1", OP, email));
	}

	#[test]
	fn refusals_are_one_not_found_and_outages_stay_outages() {
		for code in [Code::NotFound, Code::PermissionDenied, Code::InvalidArgument, Code::FailedPrecondition] {
			assert!(
				matches!(refusal(Status::new(code, "x")), ApiError::Grpc(s) if s.code() == Code::NotFound && s.message() == "not found"),
				"{code:?}"
			);
		}
		assert!(matches!(refusal(Status::unavailable("down")), ApiError::ReadFailed { code: Code::Unavailable, .. }));
	}
}
