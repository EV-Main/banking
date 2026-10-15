//! Postgres adapter for [`OperationStore`] — the operations service clients ask for and
//! concierge's approvals decide (`scope_operations`, `scope_operation_lines`, migration 0057),
//! under the scope ledger's idempotency keys. The approval is rendered from the stored row and
//! the execution reads the stored row; an operation leaves `awaiting_approval` once, under
//! its row lock, and every way out writes the client's money event in the same transaction.

use async_trait::async_trait;
use domain::{
	architecture::Id,
	balance::LedgerAccountKey,
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, MoneyEventKind, SubjectRef},
	scope_operations::{OperationBody, OperationKind, OperationProgress, OperationState, OperationTerms, PayoutLine, PendingOperation},
	scopes::{ScopeAccountLabel, ScopeId},
	users::UserId,
};
use sqlx::{PgConnection, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{
	infrastructure::{
		claims::TxReads,
		compartments, money_events, outbox,
		scope_ledger::{PgScopeLedger, find_request, insert_request, insert_transfer, key_lock, owner_on, raced_request},
		scope_payouts::{budget_lock, insert_payout, scope_out_24h, users_by_subject},
	},
	ports::{
		scope_ledger::{Recorded, RequestKey, TransferError},
		scope_operations::{
			AdmittedOperation, ApprovalLevel, Execution, ExecutionFacts, OperationAdmission, OperationEnd, OperationExecution, OperationStore, OutflowFacts, RecordedOperation,
		},
	},
};

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

macro_rules! select_operation {
	($tail:literal) => {
		concat!(
			"SELECT id, client_id, scope, kind, from_label, to_scope, to_label, total, distribution, over_scope_budget, lines_digest, payload_hash, ",
			"subject_ref, memo, state, approval_id, approval_level, result_id, failure_reason, attempts, ",
			"EXTRACT(EPOCH FROM created_at)::BIGINT AS created_secs, EXTRACT(EPOCH FROM decided_at)::BIGINT AS decided_secs FROM scope_operations ",
			$tail
		)
	};
}
const BY_ID: &str = select_operation!("WHERE id = $1");
const BY_ID_LOCKED: &str = select_operation!("WHERE id = $1 FOR UPDATE");
const OWN: &str = select_operation!("WHERE id = $1 AND scope = $2 AND client_id = $3");
const DUE: &str = select_operation!("WHERE state = 'awaiting_approval' AND (approval_id IS NOT NULL) = $1 AND next_attempt_at <= now() ORDER BY next_attempt_at LIMIT $2");

async fn operation_lines(conn: &mut PgConnection, id: Uuid) -> Result<Vec<PayoutLine>, DomainError> {
	let rows: Vec<(Uuid, String, String)> = sqlx::query_as("SELECT user_id, auth_subject, amount FROM scope_operation_lines WHERE operation_id = $1 ORDER BY line_no")
		.bind(id)
		.fetch_all(conn)
		.await
		.map_err(repo_err)?;
	rows.into_iter()
		.map(|(user, auth_subject, raw)| {
			Ok(PayoutLine {
				user: UserId::from_raw(user),
				auth_subject,
				amount: base_units(&raw)?,
			})
		})
		.collect()
}

fn base_units(raw: &str) -> Result<Usdt, DomainError> {
	raw.parse::<u128>()
		.map(Usdt::from_base_units)
		.map_err(|_| DomainError::Repository(format!("not a base-unit amount: {raw:?}")))
}

async fn recorded_operation(conn: &mut PgConnection, row: PgRow) -> Result<RecordedOperation, DomainError> {
	let id: Uuid = row.try_get("id").map_err(repo_err)?;
	let corrupt = |e: DomainError| DomainError::Repository(format!("scope operation {id}: {e}"));
	let text = |column: &str| row.try_get::<String, _>(column).map_err(repo_err);
	let optional = |column: &str| row.try_get::<Option<String>, _>(column).map_err(repo_err);
	let kind = OperationKind::parse(&text("kind")?).map_err(corrupt)?;
	let scope = ScopeId::parse(&text("scope")?).map_err(corrupt)?;
	let total = base_units(&text("total")?).map_err(corrupt)?;
	let body = match kind {
		OperationKind::ScopePayout => OperationBody::Payout {
			lines: operation_lines(conn, id).await?,
			distribution: row.try_get("distribution").map_err(repo_err)?,
			over_scope_budget: row.try_get("over_scope_budget").map_err(repo_err)?,
		},
		OperationKind::AccountTransfer | OperationKind::ScopeTransfer => {
			let (Some(to_scope), Some(to)) = (optional("to_scope")?, optional("to_label")?) else {
				return Err(corrupt(DomainError::Repository("a transfer without its destination".into())));
			};
			OperationBody::Transfer {
				to_scope: ScopeId::parse(&to_scope).map_err(corrupt)?,
				to: ScopeAccountLabel::parse(&to).map_err(corrupt)?,
				amount: total,
			}
		}
	};
	let terms = OperationTerms {
		client: ClientId::parse(&text("client_id")?).map_err(corrupt)?,
		scope,
		from: ScopeAccountLabel::parse(&text("from_label")?).map_err(corrupt)?,
		body,
		subject_ref: SubjectRef::parse(&text("subject_ref")?).map_err(corrupt)?,
		memo: text("memo")?,
	};
	let progress = OperationProgress {
		state: OperationState::parse(&text("state")?).map_err(corrupt)?,
		approval_id: optional("approval_id")?,
		result_id: optional("result_id")?,
		failure_reason: optional("failure_reason")?,
	};
	let operation = PendingOperation::restore(Id::from_raw(id), terms, progress).map_err(corrupt)?;
	// The kind is stored for the approval and the budget's scan; the record must say the same.
	if operation.kind() != kind || operation.total() != total {
		return Err(corrupt(DomainError::Repository("kind or total disagrees with the stored operation".into())));
	}
	let attempts: i32 = row.try_get("attempts").map_err(repo_err)?;
	Ok(RecordedOperation {
		operation,
		lines_digest: optional("lines_digest")?,
		payload_hash: text("payload_hash")?,
		approval_level: optional("approval_level")?.as_deref().map(ApprovalLevel::parse).transpose().map_err(corrupt)?,
		created_at: row.try_get("created_secs").map_err(repo_err)?,
		decided_at: row.try_get("decided_secs").map_err(repo_err)?,
		attempts: u32::try_from(attempts).unwrap_or(0),
	})
}

async fn fetch_one(conn: &mut PgConnection, sql: &'static str, id: Uuid) -> Result<Option<RecordedOperation>, DomainError> {
	let row = sqlx::query(sql).bind(id).fetch_optional(&mut *conn).await.map_err(repo_err)?;
	match row {
		Some(row) => Ok(Some(recorded_operation(conn, row).await?)),
		None => Ok(None),
	}
}

async fn fetch_due(conn: &mut PgConnection, opened: bool, limit: u32) -> Result<Vec<RecordedOperation>, DomainError> {
	let rows = sqlx::query(DUE).bind(opened).bind(i64::from(limit)).fetch_all(&mut *conn).await.map_err(repo_err)?;
	let mut out = Vec::with_capacity(rows.len());
	for row in rows {
		let id: Uuid = row.try_get("id").map_err(repo_err)?;
		match recorded_operation(conn, row).await {
			Ok(recorded) => out.push(recorded),
			// One row this build cannot read must not stop every other operation: it is set
			// aside for an hour, loudly, and the rest are served.
			Err(err) => {
				tracing::error!(operation_id = %id, "scope operations: an operation cannot be read, set aside for an hour: {err}");
				sqlx::query("UPDATE scope_operations SET next_attempt_at = now() + interval '1 hour', updated_at = now() WHERE id = $1")
					.bind(id)
					.execute(&mut *conn)
					.await
					.map_err(repo_err)?;
			}
		}
	}
	Ok(out)
}

/// How many operations one client may have awaiting an approval in one scope at once: each
/// is a proposal concierge holds open and voters are mailed about, so an unbounded number
/// is a way to flood both (concierge itself stops at 200 per namespace and scope).
pub const MAX_LIVE_OPERATIONS: u32 = 20;

/// Write a newly admitted operation and its lines.
async fn insert_operation(conn: &mut PgConnection, admitted: &AdmittedOperation) -> Result<i64, DomainError> {
	let operation = &admitted.operation;
	let terms = operation.terms();
	let (to_scope, to_label, distribution, over_scope_budget, lines) = match &terms.body {
		OperationBody::Payout {
			lines,
			distribution,
			over_scope_budget,
		} => (None, None, *distribution, *over_scope_budget, lines.as_slice()),
		OperationBody::Transfer { to_scope, to, .. } => (Some(to_scope.as_str()), Some(to.as_str()), false, false, [].as_slice()),
	};
	let created_at: i64 = sqlx::query_scalar(
		"INSERT INTO scope_operations (id, client_id, scope, kind, from_label, to_scope, to_label, total, distribution, over_scope_budget, lines_digest, payload_hash, \
		 subject_ref, memo) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING EXTRACT(EPOCH FROM created_at)::BIGINT",
	)
	.bind(operation.id().raw())
	.bind(terms.client.as_str())
	.bind(terms.scope.as_str())
	.bind(operation.kind().as_str())
	.bind(terms.from.as_str())
	.bind(to_scope)
	.bind(to_label)
	.bind(operation.total().base_units().to_string())
	.bind(distribution)
	.bind(over_scope_budget)
	.bind(admitted.lines_digest.as_deref())
	.bind(&admitted.payload_hash)
	.bind(terms.subject_ref.as_str())
	.bind(&terms.memo)
	.fetch_one(&mut *conn)
	.await
	.map_err(repo_err)?;
	if !lines.is_empty() {
		let line_no: Vec<i32> = (0..i32::try_from(lines.len()).map_err(|_| DomainError::Validation("too many payout lines".into()))?).collect();
		let users: Vec<Uuid> = lines.iter().map(|line| line.user.raw()).collect();
		let subjects: Vec<&str> = lines.iter().map(|line| line.auth_subject.as_str()).collect();
		let amounts: Vec<String> = lines.iter().map(|line| line.amount.base_units().to_string()).collect();
		sqlx::query(
			"INSERT INTO scope_operation_lines (operation_id, line_no, user_id, auth_subject, amount) \
			 SELECT $1, line_no, user_id, auth_subject, amount FROM unnest($2::int[], $3::uuid[], $4::text[], $5::text[]) AS t(line_no, user_id, auth_subject, amount)",
		)
		.bind(operation.id().raw())
		.bind(&line_no)
		.bind(&users)
		.bind(&subjects)
		.bind(&amounts)
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	}
	Ok(created_at)
}

/// Write where an operation stands after it left `awaiting_approval`.
async fn store_progress(conn: &mut PgConnection, operation: &PendingOperation) -> Result<(), DomainError> {
	let progress = operation.progress();
	sqlx::query("UPDATE scope_operations SET state = $2, result_id = $3, failure_reason = $4, decided_at = now(), updated_at = now() WHERE id = $1 AND state = 'awaiting_approval'")
		.bind(operation.id().raw())
		.bind(progress.state.as_str())
		.bind(progress.result_id.as_deref())
		.bind(progress.failure_reason.as_deref())
		.execute(conn)
		.await
		.map_err(repo_err)?;
	Ok(())
}

/// The client's money event for one step of its operation's life.
fn operation_event(recorded: &RecordedOperation, kind: MoneyEventKind, version: u64, failure_reason: &str) -> money_events::NewMoneyEvent {
	let operation = &recorded.operation;
	let terms = operation.terms();
	let (to, counterparty_scope) = match &terms.body {
		OperationBody::Payout { .. } => (None, String::new()),
		OperationBody::Transfer { to_scope, to, .. } => (
			Some(crate::ports::scope_ledger::EventAccount::Account(to.clone())),
			if to_scope == &terms.scope { String::new() } else { to_scope.as_str().to_owned() },
		),
	};
	money_events::NewMoneyEvent {
		event_id: money_events::event_id(&format!("operation:{}:{}", operation.id(), kind.as_str())),
		scope: terms.scope.clone(),
		kind,
		ref_id: operation.id().to_string(),
		aggregate_version: version,
		auth_subject: String::new(),
		amount: Some(operation.total()),
		from: Some(crate::ports::scope_ledger::EventAccount::Account(terms.from.clone())),
		to,
		subject_ref: terms.subject_ref.as_str().to_owned(),
		initiator_client: terms.client.as_str().to_owned(),
		balances: Vec::new(),
		hold_remaining: None,
		counterparty_scope,
		failure_reason: failure_reason.to_owned(),
		paid_from: String::new(),
		audience: vec![terms.client.clone()],
	}
}

/// An operation's events, by version: requested is 1, its end is 2. An executed one shows as
/// the PAID_OUT / TRANSFERRED events of what executed it, under the same `subject_ref`.
const REQUESTED_VERSION: u64 = 1;
const ENDED_VERSION: u64 = 2;

#[async_trait]
impl OperationStore for PgScopeLedger {
	async fn operation(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<Option<RecordedOperation>, DomainError> {
		let mut conn = self.pool().acquire().await.map_err(repo_err)?;
		let row = sqlx::query(OWN)
			.bind(id)
			.bind(scope.as_str())
			.bind(client.as_str())
			.fetch_optional(&mut *conn)
			.await
			.map_err(repo_err)?;
		match row {
			Some(row) => Ok(Some(recorded_operation(&mut conn, row).await?)),
			None => Ok(None),
		}
	}

	async fn operation_by_id(&self, id: Uuid) -> Result<Option<RecordedOperation>, DomainError> {
		let mut conn = self.pool().acquire().await.map_err(repo_err)?;
		fetch_one(&mut conn, BY_ID, id).await
	}

	async fn record_operation(
		&self,
		key: RequestKey<'_>,
		terms: &str,
		from: &ScopeAccountLabel,
		to: Option<(&ScopeId, &ScopeAccountLabel)>,
		subjects: &[String],
		admission: &dyn OperationAdmission,
	) -> Result<Recorded<RecordedOperation>, TransferError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		outbox::lock_named(&mut tx, &key_lock(&key)).await?;
		if let Some(existing) = find_request(&mut tx, &key).await? {
			tx.commit().await.map_err(repo_err)?;
			return Ok(Recorded::Existing(existing));
		}
		// Serializes one client's requests in one scope, so the live count below cannot be
		// passed by two requests that both read room for one more.
		outbox::lock_named(&mut tx, &format!("scope_ledger_operations:{}:{}", key.client, key.scope)).await?;
		let live: i64 = sqlx::query_scalar("SELECT count(*) FROM scope_operations WHERE client_id = $1 AND scope = $2 AND state = 'awaiting_approval'")
			.bind(key.client.as_str())
			.bind(key.scope.as_str())
			.fetch_one(&mut *tx)
			.await
			.map_err(repo_err)?;
		// A requested payout draws on the scope's budget the moment it is stored (its flag is
		// decided here), so it serializes with every other outflow of the scope; a transfer
		// does not leave the scope's users' hands and has no budget to read.
		let scope_out = if to.is_none() {
			outbox::lock_named(&mut tx, &budget_lock(key.scope)).await?;
			scope_out_24h(&mut tx, key.scope).await?
		} else {
			Usdt::ZERO
		};
		let destination_owner = match to {
			Some((to_scope, to_label)) => owner_on(&mut tx, to_scope, to_label).await?,
			None => None,
		};
		let destination_scope = match to {
			Some((to_scope, _)) if to_scope != key.scope => compartments::scope_for_share(&mut tx, to_scope).await?,
			Some(_) | None => None,
		};
		let facts = OutflowFacts {
			scope: compartments::scope_for_share(&mut tx, key.scope).await?,
			source_owner: owner_on(&mut tx, key.scope, from).await?,
			users: users_by_subject(&mut tx, subjects).await?,
			paid_24h: Usdt::ZERO,
			scope_out_24h: scope_out,
			destination_owner,
			destination_scope,
			live_operations: u32::try_from(live).unwrap_or(u32::MAX),
		};
		// Held so the spendable the admission reads is not being spent under it; the request
		// reserves nothing, so a request is only refused for a shortfall it has right now.
		outbox::lock_claim(&mut tx, &LedgerAccountKey::ScopeAccount(key.scope.clone(), from.clone())).await?;
		// The refusal rolls the transaction back with nothing written.
		let admitted = admission.admit(&facts, &mut TxReads::new(&mut tx)).await?;
		let created_at = insert_operation(&mut tx, &admitted).await?;
		if !insert_request(&mut tx, &key, terms, &admitted.operation.id().to_string()).await? {
			drop(tx);
			return Ok(Recorded::Existing(raced_request(self.pool(), &key).await?));
		}
		tx.commit().await.map_err(repo_err)?;
		Ok(Recorded::New(RecordedOperation {
			operation: admitted.operation,
			lines_digest: admitted.lines_digest,
			payload_hash: admitted.payload_hash,
			approval_level: None,
			created_at,
			decided_at: None,
			attempts: 0,
		}))
	}

	async fn due_to_propose(&self, limit: u32) -> Result<Vec<RecordedOperation>, DomainError> {
		let mut conn = self.pool().acquire().await.map_err(repo_err)?;
		fetch_due(&mut conn, false, limit).await
	}

	async fn due_to_check(&self, limit: u32) -> Result<Vec<RecordedOperation>, DomainError> {
		let mut conn = self.pool().acquire().await.map_err(repo_err)?;
		fetch_due(&mut conn, true, limit).await
	}

	async fn opened(&self, id: Uuid, approval_id: &str, level: Option<ApprovalLevel>) -> Result<(), DomainError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		let recorded = fetch_one(&mut tx, BY_ID_LOCKED, id).await?.ok_or_else(|| DomainError::NotFound {
			entity: "operation",
			id: id.to_string(),
		})?;
		match recorded.operation.progress().approval_id.as_deref() {
			Some(known) if known == approval_id => {}
			Some(known) => return Err(DomainError::Conflict(format!("operation {id} is decided by approval {known}, not {approval_id}"))),
			None => {
				sqlx::query("UPDATE scope_operations SET approval_id = $2, approval_level = $3, next_attempt_at = now(), attempts = 0, updated_at = now() WHERE id = $1")
					.bind(id)
					.bind(approval_id)
					.bind(level.map(ApprovalLevel::as_str))
					.execute(&mut *tx)
					.await
					.map_err(repo_err)?;
				if recorded.operation.state() == OperationState::AwaitingApproval {
					money_events::append(&mut tx, &operation_event(&recorded, MoneyEventKind::OperationRequested, REQUESTED_VERSION, "")).await?;
				}
			}
		}
		tx.commit().await.map_err(repo_err)
	}

	async fn postpone(&self, id: Uuid, delay_secs: u32) -> Result<(), DomainError> {
		sqlx::query(
			"UPDATE scope_operations SET next_attempt_at = now() + make_interval(secs => $2), attempts = attempts + 1, updated_at = now() \
			 WHERE id = $1 AND state = 'awaiting_approval'",
		)
		.bind(id)
		.bind(f64::from(delay_secs))
		.execute(self.pool())
		.await
		.map_err(repo_err)?;
		Ok(())
	}

	async fn expire_unopened(&self, max_age_secs: u32, reason: &str) -> Result<usize, DomainError> {
		let stale: Vec<Uuid> = sqlx::query_scalar(
			"SELECT id FROM scope_operations WHERE state = 'awaiting_approval' AND approval_id IS NULL AND created_at < now() - make_interval(secs => $1) \
			 ORDER BY created_at LIMIT 100",
		)
		.bind(f64::from(max_age_secs))
		.fetch_all(self.pool())
		.await
		.map_err(repo_err)?;
		let mut ended = 0;
		for id in stale {
			if self.close(id, OperationEnd::Rejected, reason).await? {
				ended += 1;
			}
		}
		Ok(ended)
	}

	async fn close(&self, id: Uuid, end: OperationEnd, reason: &str) -> Result<bool, DomainError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		let Some(mut recorded) = fetch_one(&mut tx, BY_ID_LOCKED, id).await? else {
			return Ok(false);
		};
		if recorded.operation.state() != OperationState::AwaitingApproval {
			return Ok(false);
		}
		let kind = match end {
			OperationEnd::Rejected => {
				recorded.operation.reject(reason.to_owned())?;
				MoneyEventKind::OperationRejected
			}
			OperationEnd::Failed => {
				recorded.operation.fail(reason.to_owned())?;
				MoneyEventKind::OperationFailed
			}
		};
		store_progress(&mut tx, &recorded.operation).await?;
		money_events::append(&mut tx, &operation_event(&recorded, kind, ENDED_VERSION, reason)).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(true)
	}

	async fn execute(&self, id: Uuid, execution: &dyn OperationExecution) -> Result<RecordedOperation, TransferError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		let mut recorded = fetch_one(&mut tx, BY_ID_LOCKED, id).await?.ok_or_else(|| DomainError::NotFound {
			entity: "operation",
			id: id.to_string(),
		})?;
		// Once only: whatever pulled it again (a second replica, a repeated pull) finds it
		// decided and does nothing.
		if recorded.operation.state() != OperationState::AwaitingApproval {
			tx.commit().await.map_err(repo_err)?;
			return Ok(recorded);
		}
		let terms = recorded.operation.terms().clone();
		let (destination_owner, destination_scope, blocked_users) = match &terms.body {
			OperationBody::Transfer { to_scope, to, .. } => (
				owner_on(&mut tx, to_scope, to).await?,
				if to_scope == &terms.scope {
					None
				} else {
					compartments::scope_for_share(&mut tx, to_scope).await?
				},
				Vec::new(),
			),
			OperationBody::Payout { lines, .. } => {
				let ids: Vec<Uuid> = lines.iter().map(|line| line.user.raw()).collect();
				// `FOR SHARE`: a freeze landing on one of them waits for this execution to commit,
				// and one that landed first is read here.
				let blocked: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE id = ANY($1) AND (frozen OR status = 'disabled') FOR SHARE")
					.bind(&ids)
					.fetch_all(&mut *tx)
					.await
					.map_err(repo_err)?;
				(None, None, blocked.into_iter().map(UserId::from_raw).collect())
			}
		};
		let facts = ExecutionFacts {
			// Held until commit, as for any debit: a freeze cannot commit between this read and
			// the execution it admits.
			scope: compartments::scope_for_share(&mut tx, &terms.scope).await?,
			source_owner: owner_on(&mut tx, &terms.scope, &terms.from).await?,
			destination_owner,
			destination_scope,
			blocked_users,
		};
		outbox::lock_claim(&mut tx, &LedgerAccountKey::ScopeAccount(terms.scope.clone(), terms.from.clone())).await?;
		match execution.execute(&recorded.operation, &facts, &mut TxReads::new(&mut tx)).await? {
			Execution::Payout(mut payout) => {
				insert_payout(&mut tx, &mut payout).await?;
				recorded.operation.execute(payout.id().to_string())?;
				store_progress(&mut tx, &recorded.operation).await?;
			}
			Execution::Transfer(mut transfer) => {
				insert_transfer(&mut tx, &transfer).await?;
				outbox::drain_to_outbox(&mut tx, &mut transfer, true).await?;
				recorded.operation.execute(transfer.id().to_string())?;
				store_progress(&mut tx, &recorded.operation).await?;
			}
			Execution::Fail(reason) => {
				recorded.operation.fail(reason.clone())?;
				store_progress(&mut tx, &recorded.operation).await?;
				money_events::append(&mut tx, &operation_event(&recorded, MoneyEventKind::OperationFailed, ENDED_VERSION, &reason)).await?;
			}
		}
		let done = fetch_one(&mut tx, BY_ID, id)
			.await?
			.ok_or_else(|| DomainError::Repository(format!("operation {id} vanished under its lock")))?;
		tx.commit().await.map_err(repo_err)?;
		Ok(done)
	}
}
