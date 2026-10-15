//! Postgres adapter for [`PayoutStore`] — payouts out of scope accounts into users'
//! compartments (`scope_payouts`, `scope_payout_lines`, migration 0057) under the scope
//! ledger's idempotency keys — plus the control-plane steps of a payout's life outside the
//! request: the relay's `posted`/`failed` stamp and the reaper's replay of a `pending` payout
//! whose event was lost. Amounts are 18-dp base units as TEXT, summed in Rust.

use std::time::Duration;

use async_trait::async_trait;
use domain::{
	architecture::{AggregateRoot, DomainEvent},
	balance::LedgerAccountKey,
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, SubjectRef},
	scope_operations::{PaidLine, PayoutLine, PayoutReason, PayoutTerms, ScopePayout, ScopePayoutEvent, ScopePayoutId},
	scopes::{MoveState, ScopeAccountLabel, ScopeId},
	users::UserId,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::{
		claims::TxReads,
		compartments, outbox,
		scope_ledger::{PgScopeLedger, find_request, insert_request, key_lock, owner_on, raced_request, sum},
	},
	ports::{
		scope_ledger::{HoldUser, Recorded, RequestKey, TransferError},
		scope_operations::{OutflowFacts, PayoutAdmission, PayoutStore, RecordedPayout, ResolvedUser},
	},
};

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

fn amount(what: &str, raw: &str) -> Result<Usdt, DomainError> {
	raw.parse::<u128>()
		.map(Usdt::from_base_units)
		.map_err(|_| DomainError::Repository(format!("{what} is not a base-unit amount: {raw:?}")))
}

/// The advisory-lock name every outflow of a scope that draws on its daily budget takes —
/// automatic payouts and requested payouts alike — so two of them cannot both count the same
/// room. It also serializes a client's automatic payouts, whose own ceiling it therefore
/// covers.
pub(crate) fn budget_lock(scope: &ScopeId) -> String {
	format!("scope_ledger_budget:{scope}")
}

/// Every user named in `subjects` that banking knows, with their standing.
pub(crate) async fn users_by_subject(conn: &mut PgConnection, subjects: &[String]) -> Result<Vec<ResolvedUser>, DomainError> {
	if subjects.is_empty() {
		return Ok(Vec::new());
	}
	let rows: Vec<(String, Uuid, bool)> = sqlx::query_as("SELECT auth_subject, id, (frozen OR status = 'disabled') FROM users WHERE auth_subject = ANY($1)")
		.bind(subjects)
		.fetch_all(conn)
		.await
		.map_err(repo_err)?;
	Ok(rows
		.into_iter()
		.map(|(auth_subject, id, blocked)| ResolvedUser {
			auth_subject,
			user: HoldUser { id: UserId::from_raw(id), blocked },
		})
		.collect())
}

/// Σ the client's automatic payouts in the scope over the rolling 24 h, `pending` and `posted`.
async fn paid_24h(conn: &mut PgConnection, client: &ClientId, scope: &ScopeId) -> Result<Usdt, DomainError> {
	let amounts: Vec<String> = sqlx::query_scalar(
		"SELECT total FROM scope_payouts WHERE client_id = $1 AND scope = $2 AND operation_id IS NULL AND state IN ('pending', 'posted') \
		 AND created_at > now() - interval '24 hours'",
	)
	.bind(client.as_str())
	.bind(scope.as_str())
	.fetch_all(conn)
	.await
	.map_err(repo_err)?;
	sum(&amounts, "payouts in 24 h")
}

/// Σ what left the scope's accounts over the rolling 24 h without the platform owners (D24(4)):
/// every client's automatic payouts, and the payout decisions taken inside the scope — live
/// ones (an unopened one included: its level is not known yet, so it is counted as the
/// scope's) and those executed in the window, counted from their execution. A decision the
/// owners took, or one that ended unexecuted, does not count. Read under [`budget_lock`].
pub(crate) async fn scope_out_24h(conn: &mut PgConnection, scope: &ScopeId) -> Result<Usdt, DomainError> {
	let amounts: Vec<String> = sqlx::query_scalar(
		"SELECT total FROM scope_payouts WHERE scope = $1 AND operation_id IS NULL AND state IN ('pending', 'posted') AND created_at > now() - interval '24 hours' \
		 UNION ALL SELECT total FROM scope_operations WHERE scope = $1 AND kind = 'bank:scope_payout' AND COALESCE(approval_level, 'scope') = 'scope' \
		 AND (state = 'awaiting_approval' OR (state = 'executed' AND decided_at > now() - interval '24 hours'))",
	)
	.bind(scope.as_str())
	.fetch_all(conn)
	.await
	.map_err(repo_err)?;
	sum(&amounts, "scope outflow in 24 h")
}

type PayoutRow = (Uuid, String, String, String, String, String, String, Option<Uuid>, String, Option<String>, i64);

macro_rules! select_payout {
	($tail:literal) => {
		concat!(
			"SELECT id, client_id, scope, from_label, reason, subject_ref, memo, operation_id, state, failure_reason, EXTRACT(EPOCH FROM created_at)::BIGINT ",
			"FROM scope_payouts ",
			$tail
		)
	};
}

async fn payout_lines(conn: &mut PgConnection, id: Uuid) -> Result<Vec<PayoutLine>, DomainError> {
	let rows: Vec<(Uuid, String, String)> = sqlx::query_as("SELECT user_id, auth_subject, amount FROM scope_payout_lines WHERE payout_id = $1 ORDER BY line_no")
		.bind(id)
		.fetch_all(conn)
		.await
		.map_err(repo_err)?;
	rows.into_iter()
		.map(|(user, auth_subject, raw)| {
			Ok(PayoutLine {
				user: UserId::from_raw(user),
				auth_subject,
				amount: amount("payout line", &raw)?,
			})
		})
		.collect()
}

async fn recorded_payout(conn: &mut PgConnection, row: PayoutRow) -> Result<RecordedPayout, DomainError> {
	let (id, client, scope, from, reason, subject_ref, memo, operation_id, state, failure_reason, created_at) = row;
	let corrupt = |e: DomainError| DomainError::Repository(format!("scope payout {id}: {e}"));
	let terms = PayoutTerms {
		client: ClientId::parse(&client).map_err(corrupt)?,
		scope: ScopeId::parse(&scope).map_err(corrupt)?,
		from: ScopeAccountLabel::parse(&from).map_err(corrupt)?,
		lines: payout_lines(conn, id).await?,
		reason: PayoutReason::parse(&reason).map_err(corrupt)?,
		subject_ref: SubjectRef::parse(&subject_ref).map_err(corrupt)?,
		memo,
		operation_id,
	};
	Ok(RecordedPayout {
		payout: ScopePayout::restore(ScopePayoutId::from_raw(id), terms).map_err(corrupt)?,
		state: MoveState::parse(&state).map_err(corrupt)?,
		failure_reason,
		created_at,
	})
}

/// The payout `id` as recorded, whoever made it.
pub(crate) async fn payout_on(conn: &mut PgConnection, id: Uuid) -> Result<Option<RecordedPayout>, DomainError> {
	let row = sqlx::query_as::<_, PayoutRow>(select_payout!("WHERE id = $1"))
		.bind(id)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
	match row {
		Some(row) => Ok(Some(recorded_payout(conn, row).await?)),
		None => Ok(None),
	}
}

/// Write a payout, its lines and its chain's event. Returns its creation time, unix seconds.
pub(crate) async fn insert_payout(conn: &mut PgConnection, payout: &mut ScopePayout) -> Result<i64, DomainError> {
	let terms = payout.terms().clone();
	let recipients = i32::try_from(terms.lines.len()).map_err(|_| DomainError::Validation("too many payout lines".into()))?;
	let created_at: i64 = sqlx::query_scalar(
		"INSERT INTO scope_payouts (id, client_id, scope, from_label, total, recipients, reason, operation_id, subject_ref, memo) \
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING EXTRACT(EPOCH FROM created_at)::BIGINT",
	)
	.bind(payout.id().raw())
	.bind(terms.client.as_str())
	.bind(terms.scope.as_str())
	.bind(terms.from.as_str())
	.bind(payout.total().base_units().to_string())
	.bind(recipients)
	.bind(terms.reason.as_str())
	.bind(terms.operation_id)
	.bind(terms.subject_ref.as_str())
	.bind(&terms.memo)
	.fetch_one(&mut *conn)
	.await
	.map_err(repo_err)?;
	// One statement for up to 1000 lines: `line_no` is the leg's place in the ledger chain.
	let line_no: Vec<i32> = (0..recipients).collect();
	let users: Vec<Uuid> = terms.lines.iter().map(|line| line.user.raw()).collect();
	let subjects: Vec<&str> = terms.lines.iter().map(|line| line.auth_subject.as_str()).collect();
	let amounts: Vec<String> = terms.lines.iter().map(|line| line.amount.base_units().to_string()).collect();
	sqlx::query(
		"INSERT INTO scope_payout_lines (payout_id, line_no, user_id, auth_subject, amount) \
		 SELECT $1, line_no, user_id, auth_subject, amount FROM unnest($2::int[], $3::uuid[], $4::text[], $5::text[]) AS t(line_no, user_id, auth_subject, amount)",
	)
	.bind(payout.id().raw())
	.bind(&line_no)
	.bind(&users)
	.bind(&subjects)
	.bind(&amounts)
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	outbox::drain_to_outbox(conn, payout, true).await?;
	Ok(created_at)
}

#[async_trait]
impl PayoutStore for PgScopeLedger {
	async fn payout(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<Option<RecordedPayout>, DomainError> {
		let mut conn = self.pool().acquire().await.map_err(repo_err)?;
		let row = sqlx::query_as::<_, PayoutRow>(select_payout!("WHERE id = $1 AND scope = $2 AND client_id = $3"))
			.bind(id)
			.bind(scope.as_str())
			.bind(client.as_str())
			.fetch_optional(&mut *conn)
			.await
			.map_err(repo_err)?;
		match row {
			Some(row) => Ok(Some(recorded_payout(&mut conn, row).await?)),
			None => Ok(None),
		}
	}

	async fn record_payout(
		&self,
		key: RequestKey<'_>,
		terms: &str,
		from: &ScopeAccountLabel,
		subjects: &[String],
		admission: &dyn PayoutAdmission,
	) -> Result<Recorded<RecordedPayout>, TransferError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		outbox::lock_named(&mut tx, &key_lock(&key)).await?;
		if let Some(existing) = find_request(&mut tx, &key).await? {
			tx.commit().await.map_err(repo_err)?;
			return Ok(Recorded::Existing(existing));
		}
		outbox::lock_named(&mut tx, &budget_lock(key.scope)).await?;
		let facts = OutflowFacts {
			scope: compartments::scope_for_share(&mut tx, key.scope).await?,
			source_owner: owner_on(&mut tx, key.scope, from).await?,
			users: users_by_subject(&mut tx, subjects).await?,
			paid_24h: paid_24h(&mut tx, key.client, key.scope).await?,
			scope_out_24h: scope_out_24h(&mut tx, key.scope).await?,
			destination_owner: None,
			destination_scope: None,
			live_operations: 0,
		};
		// The lock every spender of the account takes, so the spendable the admission reads
		// cannot be spent twice.
		outbox::lock_claim(&mut tx, &LedgerAccountKey::ScopeAccount(key.scope.clone(), from.clone())).await?;
		// The refusal rolls the transaction back with nothing written.
		let mut payout = admission.admit(&facts, &mut TxReads::new(&mut tx)).await?;
		let created_at = insert_payout(&mut tx, &mut payout).await?;
		if !insert_request(&mut tx, &key, terms, &payout.id().to_string()).await? {
			drop(tx);
			return Ok(Recorded::Existing(raced_request(self.pool(), &key).await?));
		}
		tx.commit().await.map_err(repo_err)?;
		Ok(Recorded::New(RecordedPayout {
			payout,
			state: MoveState::Pending,
			failure_reason: None,
			created_at,
		}))
	}
}

/// Where payout `id` stands, `None` when no row holds it. The relay reads it before posting:
/// only a `pending` payout may still reach the ledger.
pub async fn payout_state(pool: &PgPool, id: Uuid) -> Result<Option<MoveState>, sqlx::Error> {
	let state: Option<String> = sqlx::query_scalar("SELECT state FROM scope_payouts WHERE id = $1").bind(id).fetch_optional(pool).await?;
	state.map(|raw| MoveState::parse(&raw).map_err(|e| sqlx::Error::Decode(Box::new(e)))).transpose()
}

/// The ledger took the chain. Only a `pending` row moves; a repeat is a no-op.
pub async fn mark_posted(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
	sqlx::query("UPDATE scope_payouts SET state = 'posted', updated_at = now() WHERE id = $1 AND state = 'pending'")
		.bind(id)
		.execute(pool)
		.await?;
	Ok(())
}

/// The ledger refused the chain: terminal, no line moved, never executed later.
pub async fn mark_failed(pool: &PgPool, id: Uuid, reason: &str) -> Result<(), sqlx::Error> {
	sqlx::query("UPDATE scope_payouts SET state = 'failed', failure_reason = $2, updated_at = now() WHERE id = $1 AND state = 'pending'")
		.bind(id)
		.bind(reason)
		.execute(pool)
		.await?;
	Ok(())
}

/// Put one stale payout's event back on the outbox; `false` when it was posted or failed since
/// the scan.
async fn redrive_one(pool: &PgPool, id: Uuid) -> Result<bool, DomainError> {
	let mut tx = pool.begin().await.map_err(repo_err)?;
	let still_pending: Option<Uuid> = sqlx::query_scalar("SELECT id FROM scope_payouts WHERE id = $1 AND state = 'pending' FOR UPDATE")
		.bind(id)
		.fetch_optional(&mut *tx)
		.await
		.map_err(repo_err)?;
	if still_pending.is_none() {
		return Ok(false);
	}
	let recorded = payout_on(&mut tx, id)
		.await?
		.ok_or_else(|| DomainError::Repository(format!("scope payout {id} vanished under its lock")))?;
	let terms = recorded.payout.terms();
	let event = ScopePayoutEvent::PaidOut {
		payout_id: recorded.payout.id(),
		scope: terms.scope.clone(),
		from_label: terms.from.clone(),
		lines: terms
			.lines
			.iter()
			.map(|line| PaidLine {
				user: line.user,
				amount: line.amount,
			})
			.collect(),
	};
	let payload = serde_json::to_string(&event).map_err(|e| DomainError::Repository(e.to_string()))?;
	outbox::insert_event(&mut tx, Uuid::new_v4(), ScopePayout::NAME, id, ScopePayoutEvent::KIND, &payload, true).await?;
	sqlx::query("UPDATE scope_payouts SET updated_at = now() WHERE id = $1")
		.bind(id)
		.execute(&mut *tx)
		.await
		.map_err(repo_err)?;
	tx.commit().await.map_err(repo_err)?;
	Ok(true)
}

/// Re-enqueue every `pending` payout untouched for `older_than` whose event is no longer
/// waiting in the outbox — the relay that dispatched it did not post it (an older build that
/// did not know the kind). The chain's ledger ids derive from the payout id, so a replay of
/// one that did post answers `Exists` and is only stamped `posted`. Returns how many it
/// re-enqueued.
pub async fn redrive_stale_payouts(pool: &PgPool, older_than: Duration) -> Result<usize, sqlx::Error> {
	let stale: Vec<Uuid> = sqlx::query_scalar(
		"SELECT p.id FROM scope_payouts p WHERE p.state = 'pending' AND p.updated_at < now() - make_interval(secs => $1) \
		 AND NOT EXISTS (SELECT 1 FROM outbox o WHERE o.dispatched_at IS NULL AND o.aggregate_id = p.id) ORDER BY p.updated_at LIMIT 100",
	)
	.bind(older_than.as_secs_f64())
	.fetch_all(pool)
	.await?;
	let mut redriven = 0;
	for id in stale {
		match redrive_one(pool, id).await {
			Ok(true) => redriven += 1,
			Ok(false) => {}
			// One unreadable row must not stop the others; it waits its turn again.
			Err(err) => {
				tracing::warn!(payout_id = %id, "reaper: could not replay a pending scope payout: {err}");
				if let Err(err) = sqlx::query("UPDATE scope_payouts SET updated_at = now() WHERE id = $1").bind(id).execute(pool).await {
					tracing::warn!(payout_id = %id, "reaper: could not push the unreplayable scope payout back in the queue: {err}");
				}
			}
		}
	}
	Ok(redriven)
}

/// Scope money recorded and not yet on the ledger for longer than `older_than`: `pending`
/// transfers and payouts the relay should have posted within seconds. Reconciliation's
/// in-flight gauge.
pub async fn stale_postings(pool: &PgPool, older_than: Duration) -> Result<i64, sqlx::Error> {
	sqlx::query_scalar(
		"SELECT (SELECT COUNT(*) FROM scope_transfers WHERE state = 'pending' AND created_at < now() - make_interval(secs => $1)) \
		 + (SELECT COUNT(*) FROM scope_payouts WHERE state = 'pending' AND created_at < now() - make_interval(secs => $1))",
	)
	.bind(older_than.as_secs_f64())
	.fetch_one(pool)
	.await
}
