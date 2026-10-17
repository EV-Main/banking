//! The read behind the page that shows an approver a pending `bank:scope_payout`'s lines
//! (`scope_operations`, `scope_operation_lines`, migration 0057; banking#529).
//!
//! It reads the operation as it was STORED — the rows the approval's `lines_digest` was taken
//! over and the executor pays from — and takes nothing from the caller but the ids that pick
//! the row. Who may read is not decided here (the BFF holds the caller's concierge session);
//! what is decided here is that an operation, its kind and its approval all match.

use domain::{
	error::DomainError,
	money::Usdt,
	scope_operations::{RequestedLine, lines_digest_input},
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

fn base_units(raw: &str) -> Result<Usdt, DomainError> {
	raw.parse::<u128>()
		.map(Usdt::from_base_units)
		.map_err(|_| DomainError::Repository(format!("not a base-unit amount: {raw:?}")))
}

/// One line of the payout.
#[derive(Clone, Debug)]
pub struct ReviewLine {
	pub line_no: u32,
	pub recipient: String,
	pub amount: Usdt,
	/// The recipient has no earlier POSTED approved payout in the operation's scope.
	pub no_history: bool,
}

/// A page of lines with the summary of the whole payout.
#[derive(Clone, Debug)]
pub struct ReviewPage {
	pub state: String,
	pub from_account: String,
	pub total: Usdt,
	pub max_line: Usdt,
	pub recipients: u32,
	pub recipients_without_history: u32,
	pub distribution: bool,
	pub over_scope_budget: bool,
	pub lines_digest: String,
	pub lines: Vec<ReviewLine>,
}

/// The approval opened for a payout operation; `None` for an unknown id, for an operation
/// that is not a payout and for one whose approval is not opened yet.
pub async fn approval_of(pool: &PgPool, operation_id: Uuid) -> Result<Option<String>, DomainError> {
	sqlx::query_scalar("SELECT approval_id FROM scope_operations WHERE id = $1 AND kind = 'bank:scope_payout' AND approval_id IS NOT NULL")
		.bind(operation_id)
		.fetch_optional(pool)
		.await
		.map_err(repo_err)
		.map(Option::flatten)
}

/// Lines `offset..offset + limit` of the payout, or `None` when `approval_id` is not the
/// approval the operation was opened with (or the operation is not a payout).
pub async fn page(pool: &PgPool, operation_id: Uuid, approval_id: &str, offset: usize, limit: usize) -> Result<Option<ReviewPage>, DomainError> {
	let header: Option<(String, String, String, String, String, bool, bool)> = sqlx::query_as(
		"SELECT scope, from_label, state, total, lines_digest, distribution, over_scope_budget FROM scope_operations \
		 WHERE id = $1 AND kind = 'bank:scope_payout' AND approval_id = $2",
	)
	.bind(operation_id)
	.bind(approval_id)
	.fetch_optional(pool)
	.await
	.map_err(repo_err)?;
	let Some((scope, from_label, state, total, lines_digest, distribution, over_scope_budget)) = header else {
		return Ok(None);
	};
	// A payout holds at most 1000 lines (`line_no` CHECK), so the whole set is read and paged
	// in memory: the max and the count must cover every line anyway, and one ordered read is
	// the same for every page.
	let rows: Vec<(i32, String, String, bool)> = sqlx::query_as(
		"SELECT l.line_no, l.auth_subject, l.amount, NOT EXISTS ( \
		     SELECT 1 FROM scope_payout_lines pl JOIN scope_payouts p ON p.id = pl.payout_id \
		     WHERE pl.user_id = l.user_id AND p.scope = $2 AND p.state = 'posted' AND p.reason = 'approved' AND p.operation_id IS DISTINCT FROM l.operation_id \
		 ) AS no_history \
		 FROM scope_operation_lines l WHERE l.operation_id = $1 ORDER BY l.line_no",
	)
	.bind(operation_id)
	.bind(&scope)
	.fetch_all(pool)
	.await
	.map_err(repo_err)?;
	let mut all = Vec::with_capacity(rows.len());
	for (line_no, recipient, amount, no_history) in rows {
		all.push(ReviewLine {
			line_no: u32::try_from(line_no).map_err(|_| DomainError::Repository(format!("negative line_no on operation {operation_id}")))?,
			recipient,
			amount: base_units(&amount)?,
			no_history,
		});
	}
	// The approval the voters hold binds `lines_digest`, and the executor pays from these
	// rows. If they no longer hash to it, showing them would let an approver vote on lines
	// that are not the ones the digest covers, so nothing is shown.
	let requested: Vec<RequestedLine> = all
		.iter()
		.map(|line| RequestedLine {
			auth_subject: line.recipient.clone(),
			amount: line.amount,
		})
		.collect();
	let recomputed = hex::encode(Sha256::digest(lines_digest_input(&requested).as_bytes()));
	if recomputed != lines_digest {
		tracing::error!(operation = %operation_id, stored = %lines_digest, %recomputed, "payout lines do not hash to the operation's lines_digest");
		return Err(DomainError::Repository(format!("payout operation {operation_id}: stored lines do not match lines_digest")));
	}
	let max_line = all.iter().map(|line| line.amount).max().unwrap_or(Usdt::ZERO);
	let recipients_without_history = all.iter().filter(|line| line.no_history).count();
	let recipients = all.len();
	Ok(Some(ReviewPage {
		state,
		from_account: format!("{scope}:{from_label}"),
		total: base_units(&total)?,
		max_line,
		recipients: u32::try_from(recipients).unwrap_or(u32::MAX),
		recipients_without_history: u32::try_from(recipients_without_history).unwrap_or(u32::MAX),
		distribution,
		over_scope_budget,
		lines_digest,
		lines: all.into_iter().skip(offset).take(limit).collect(),
	}))
}
