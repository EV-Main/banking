//! Postgres adapter for [`ChargeStore`] — clients' one-off charges of users' compartments
//! (`scope_charges`, migration 0060) under the same idempotency keys as the rest of the scope
//! ledger — plus the control-plane steps of a charge's life outside the request: the relay's
//! `posted`/`failed` stamp, the reaper's replay of a `pending` charge whose event was lost, and
//! the pending sum every spend of the compartment counts. The ledger side is the relay's, from
//! the `scope_charges` outbox event written in the same transaction as the row.
//!
//! Lock order, the same as a Reserve's: the key, the compartment's claim lock, then the
//! mandate row. A revoke takes the mandate row first and never the claim lock, so the two
//! cannot wait on each other in a circle.

use std::time::Duration;

use async_trait::async_trait;
use domain::{
	architecture::{AggregateRoot, DomainEvent},
	balance::LedgerAccountKey,
	error::DomainError,
	money::Usdt,
	scope_charges::{ChargeEvent, ChargeId, ChargeTerms, ScopeCharge},
	scope_ledger::{ClientId, SubjectRef},
	scopes::{MoveState, ScopeAccountLabel, ScopeId},
	users::UserId,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::{
		claims::TxReads,
		compartments, outbox,
		scope_holds::{db_now, user_by_subject},
		scope_ledger::{PgScopeLedger, find_request, insert_request, key_lock, owner_on, raced_request},
		scope_mandates,
	},
	ports::{
		scope_charges::{ChargeAdmission, ChargeFacts, ChargeStore, RecordedCharge},
		scope_ledger::{MandateDraw, Recorded, RequestKey, TransferError},
	},
};

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

fn base_units(raw: &str) -> Result<Usdt, DomainError> {
	raw.parse::<u128>()
		.map(Usdt::from_base_units)
		.map_err(|_| DomainError::Repository(format!("not a base-unit amount: {raw:?}")))
}

type ChargeRow = (Uuid, String, String, Uuid, String, String, String, String, String, Uuid, String, i64);

fn recorded_charge(row: ChargeRow) -> Result<RecordedCharge, DomainError> {
	let (id, client, scope, user, auth_subject, to, amount, memo, subject_ref, mandate_id, state, created_at) = row;
	let corrupt = |e: DomainError| DomainError::Repository(format!("scope charge {id}: {e}"));
	let terms = ChargeTerms {
		client: ClientId::parse(&client).map_err(corrupt)?,
		scope: ScopeId::parse(&scope).map_err(corrupt)?,
		user: UserId::from_raw(user),
		to: ScopeAccountLabel::parse(&to).map_err(corrupt)?,
		amount: base_units(&amount).map_err(corrupt)?,
		memo,
		subject_ref: SubjectRef::parse(&subject_ref).map_err(corrupt)?,
		mandate_id,
	};
	Ok(RecordedCharge {
		charge: ScopeCharge::restore(ChargeId::from_raw(id), terms).map_err(corrupt)?,
		auth_subject,
		state: MoveState::parse(&state).map_err(corrupt)?,
		created_at,
	})
}

#[async_trait]
impl ChargeStore for PgScopeLedger {
	async fn charge(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<Option<RecordedCharge>, DomainError> {
		sqlx::query_as::<_, ChargeRow>(concat!(
			"SELECT id, client_id, scope, user_id, auth_subject, to_label, amount, memo, subject_ref, mandate_id, state, ",
			"EXTRACT(EPOCH FROM created_at)::BIGINT FROM scope_charges WHERE id = $1 AND scope = $2 AND client_id = $3"
		))
		.bind(id)
		.bind(scope.as_str())
		.bind(client.as_str())
		.fetch_optional(self.pool())
		.await
		.map_err(repo_err)?
		.map(recorded_charge)
		.transpose()
	}

	async fn record_charge(
		&self,
		key: RequestKey<'_>,
		terms: &str,
		auth_subject: &str,
		to: &ScopeAccountLabel,
		admission: &dyn ChargeAdmission,
	) -> Result<Recorded<RecordedCharge>, TransferError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		outbox::lock_named(&mut tx, &key_lock(&key)).await?;
		if let Some(existing) = find_request(&mut tx, &key).await? {
			tx.commit().await.map_err(repo_err)?;
			return Ok(Recorded::Existing(existing));
		}
		// The scope stream's consumer writes a state under `FOR UPDATE`, so a freeze cannot
		// commit between this read and the charge it admits.
		let scope = compartments::scope_for_share(&mut tx, key.scope).await?;
		let user = user_by_subject(&mut tx, auth_subject).await?;
		let destination_owner = owner_on(&mut tx, key.scope, to).await?;
		let mut mandate = None;
		if let Some(user) = user {
			// The lock every spender of the compartment takes (a move out, a hold, a payment,
			// another charge), so the spendable the admission reads cannot be spent twice and
			// the usage below counts every draw already committed.
			outbox::lock_claim(&mut tx, &LedgerAccountKey::ScopeClaim(user.id, key.scope.clone())).await?;
			if let Some(current) = scope_mandates::current_for_update(&mut tx, user.id, key.scope, key.client).await? {
				let usage = scope_mandates::usage(&mut tx, user.id, key.scope, key.client).await?;
				mandate = Some(MandateDraw { mandate: current, usage });
			}
		}
		let now = db_now(&mut tx).await?;
		let facts = ChargeFacts {
			scope,
			user,
			destination_owner,
			mandate,
			now,
		};
		// The refusal rolls the transaction back with nothing written.
		let mut charge = admission.admit(&facts, &mut TxReads::new(&mut tx)).await?;
		let charged = charge.terms().clone();
		let created_at: i64 = sqlx::query_scalar(
			"INSERT INTO scope_charges (id, client_id, scope, user_id, auth_subject, to_label, amount, memo, subject_ref, mandate_id) \
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING EXTRACT(EPOCH FROM created_at)::BIGINT",
		)
		.bind(charge.id().raw())
		.bind(charged.client.as_str())
		.bind(charged.scope.as_str())
		.bind(charged.user.raw())
		.bind(auth_subject)
		.bind(charged.to.as_str())
		.bind(charged.amount.base_units().to_string())
		.bind(&charged.memo)
		.bind(charged.subject_ref.as_str())
		.bind(charged.mandate_id)
		.fetch_one(&mut *tx)
		.await
		.map_err(repo_err)?;
		if !insert_request(&mut tx, &key, terms, &charge.id().to_string()).await? {
			drop(tx);
			return Ok(Recorded::Existing(raced_request(self.pool(), &key).await?));
		}
		outbox::drain_to_outbox(&mut tx, &mut charge, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(Recorded::New(RecordedCharge {
			charge,
			auth_subject: auth_subject.to_owned(),
			state: MoveState::Pending,
			created_at,
		}))
	}
}

/// Σ the `pending` charges of `user`'s compartment in `scope`: promised away, not yet off the
/// ledger. Part of what [`claims::unapplied_debits`](super::claims::unapplied_debits) counts —
/// from this table rather than the outbox, so a charge whose event an older relay dispatched
/// without posting is still counted until the reaper replays it.
pub async fn pending_charges(conn: &mut PgConnection, user: UserId, scope: &ScopeId) -> Result<Usdt, DomainError> {
	let amounts: Vec<String> = sqlx::query_scalar("SELECT amount FROM scope_charges WHERE user_id = $1 AND scope = $2 AND state = 'pending'")
		.bind(user.raw())
		.bind(scope.as_str())
		.fetch_all(conn)
		.await
		.map_err(repo_err)?;
	amounts.iter().try_fold(Usdt::ZERO, |total, raw| {
		total.checked_add(base_units(raw)?).ok_or_else(|| DomainError::Repository("pending charges overflow".into()))
	})
}

/// Where charge `id` stands, `None` when no row holds it. The relay reads it before posting:
/// only a `pending` charge may still reach the ledger.
pub async fn charge_state(pool: &PgPool, id: Uuid) -> Result<Option<MoveState>, sqlx::Error> {
	let state: Option<String> = sqlx::query_scalar("SELECT state FROM scope_charges WHERE id = $1").bind(id).fetch_optional(pool).await?;
	state.map(|raw| MoveState::parse(&raw).map_err(|e| sqlx::Error::Decode(Box::new(e)))).transpose()
}

/// The ledger took the charge. Only a `pending` row moves; a repeat is a no-op.
pub async fn mark_posted(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
	sqlx::query("UPDATE scope_charges SET state = 'posted', updated_at = now() WHERE id = $1 AND state = 'pending'")
		.bind(id)
		.execute(pool)
		.await?;
	Ok(())
}

/// The ledger refused the charge: terminal, nothing moved, never carried out later.
pub async fn mark_failed(pool: &PgPool, id: Uuid, reason: &str) -> Result<(), sqlx::Error> {
	sqlx::query("UPDATE scope_charges SET state = 'failed', failure_reason = $2, updated_at = now() WHERE id = $1 AND state = 'pending'")
		.bind(id)
		.bind(reason)
		.execute(pool)
		.await?;
	Ok(())
}

/// Put one stale charge's event back on the outbox; `false` when it was posted or failed since
/// the scan.
async fn redrive_one(pool: &PgPool, id: Uuid) -> Result<bool, DomainError> {
	let mut tx = pool.begin().await.map_err(repo_err)?;
	let row = sqlx::query_as::<_, ChargeRow>(concat!(
		"SELECT id, client_id, scope, user_id, auth_subject, to_label, amount, memo, subject_ref, mandate_id, state, ",
		"EXTRACT(EPOCH FROM created_at)::BIGINT FROM scope_charges WHERE id = $1 AND state = 'pending' FOR UPDATE"
	))
	.bind(id)
	.fetch_optional(&mut *tx)
	.await
	.map_err(repo_err)?;
	let Some(row) = row else { return Ok(false) };
	let recorded = recorded_charge(row)?;
	let terms = recorded.charge.terms();
	let event = ChargeEvent::Charged {
		charge_id: ChargeId::from_raw(id),
		scope: terms.scope.clone(),
		user: terms.user,
		to_label: terms.to.clone(),
		amount: terms.amount,
	};
	let payload = serde_json::to_string(&event).map_err(|e| DomainError::Repository(e.to_string()))?;
	outbox::insert_event(&mut tx, Uuid::new_v4(), ScopeCharge::NAME, id, ChargeEvent::KIND, &payload, true).await?;
	sqlx::query("UPDATE scope_charges SET updated_at = now() WHERE id = $1")
		.bind(id)
		.execute(&mut *tx)
		.await
		.map_err(repo_err)?;
	tx.commit().await.map_err(repo_err)?;
	Ok(true)
}

/// Re-enqueue every `pending` charge untouched for `older_than` whose event is no longer
/// waiting in the outbox — the relay that dispatched it did not post it (an older build that
/// did not know the kind). The ledger id derives from the charge id, so a replay of one that
/// did post answers `Exists` and is only stamped `posted`. Returns how many it re-enqueued.
pub async fn redrive_stale_charges(pool: &PgPool, older_than: Duration) -> Result<usize, sqlx::Error> {
	let stale: Vec<Uuid> = sqlx::query_scalar(
		"SELECT c.id FROM scope_charges c WHERE c.state = 'pending' AND c.updated_at < now() - make_interval(secs => $1) \
		 AND NOT EXISTS (SELECT 1 FROM outbox o WHERE o.dispatched_at IS NULL AND o.aggregate_id = c.id) ORDER BY c.updated_at LIMIT 100",
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
				tracing::warn!(charge_id = %id, "reaper: could not replay a pending scope charge: {err}");
				if let Err(err) = sqlx::query("UPDATE scope_charges SET updated_at = now() WHERE id = $1").bind(id).execute(pool).await {
					tracing::warn!(charge_id = %id, "reaper: could not push the unreplayable scope charge back in the queue: {err}");
				}
			}
		}
	}
	Ok(redriven)
}
