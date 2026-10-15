//! Postgres adapter for [`HoldStore`] — clients' holds on users' compartments (`scope_holds`,
//! migration 0055) under the same idempotency keys as the rest of the scope ledger — plus the
//! reaper's release of holds past their TTL, a revoked mandate's release of its client's
//! holds, the relay's voiding of a hold whose reservation the ledger refused (banking#514), and
//! the reconciliation's expectation of `hold_clearing`. The ledger side of every step is the
//! relay's, from the `scope_holds` outbox events written in the same transaction as the row.

use async_trait::async_trait;
use domain::{
	architecture::{DomainEvent as _, Id},
	balance::LedgerAccountKey,
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, Hold, HoldEvent, HoldProgress, HoldState, HoldTerms, SubjectRef},
	scopes::ScopeId,
	users::UserId,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::{
		claims::TxReads,
		compartments, outbox,
		scope_ledger::{PgScopeLedger, find_request, insert_request, key_lock, owner_on, raced_request},
		scope_mandates,
	},
	ports::scope_ledger::{CaptureTarget, HoldAdmission, HoldChange, HoldChangeFacts, HoldFacts, HoldStore, HoldUser, MandateDraw, Recorded, RecordedHold, RequestKey, TransferError},
};

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

fn amount(column: &str, raw: &str) -> Result<Usdt, DomainError> {
	raw.parse::<u128>()
		.map(Usdt::from_base_units)
		.map_err(|_| DomainError::Repository(format!("{column} is not a base-unit amount: {raw:?}")))
}

type HoldRow = (Uuid, String, String, Uuid, String, String, String, String, i64, i64, Uuid, String, Option<i64>, i64);

macro_rules! select_hold {
	($tail:literal) => {
		concat!(
			"SELECT id, client_id, scope, user_id, auth_subject, amount, captured, state, version, pending_version, mandate_id, subject_ref, ",
			"EXTRACT(EPOCH FROM expires_at)::BIGINT, EXTRACT(EPOCH FROM created_at)::BIGINT FROM scope_holds ",
			$tail
		)
	};
}

fn recorded_hold(row: HoldRow) -> Result<RecordedHold, DomainError> {
	let (id, client, scope, user, auth_subject, raw_amount, raw_captured, state, version, pending_version, mandate_id, subject_ref, expires_at, created_at) = row;
	let corrupt = |e: DomainError| DomainError::Repository(format!("scope hold {id}: {e}"));
	let count = |raw: i64| u64::try_from(raw).map_err(|_| DomainError::Repository(format!("scope hold {id}: negative version {raw}")));
	let terms = HoldTerms {
		client: ClientId::parse(&client).map_err(corrupt)?,
		scope: ScopeId::parse(&scope).map_err(corrupt)?,
		user: UserId::from_raw(user),
		amount: amount("amount", &raw_amount)?,
		expires_at,
		subject_ref: SubjectRef::parse(&subject_ref).map_err(corrupt)?,
		mandate_id,
	};
	let progress = HoldProgress {
		captured: amount("captured", &raw_captured)?,
		state: HoldState::parse(&state).map_err(corrupt)?,
		version: count(version)?,
		pending_version: count(pending_version)?,
	};
	Ok(RecordedHold {
		hold: Hold::restore(Id::from_raw(id), terms, progress).map_err(corrupt)?,
		auth_subject,
		created_at,
	})
}

/// The database's clock: one clock for every TTL, whichever replica asks.
pub(crate) async fn db_now(conn: &mut PgConnection) -> Result<i64, DomainError> {
	sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::BIGINT").fetch_one(conn).await.map_err(repo_err)
}

macro_rules! user_standing {
	($tail:literal) => {
		concat!("SELECT id, (frozen OR status = 'disabled') FROM users ", $tail)
	};
}

async fn user_by_subject(conn: &mut PgConnection, auth_subject: &str) -> Result<Option<HoldUser>, DomainError> {
	let row: Option<(Uuid, bool)> = sqlx::query_as(user_standing!("WHERE auth_subject = $1"))
		.bind(auth_subject)
		.fetch_optional(conn)
		.await
		.map_err(repo_err)?;
	Ok(row.map(|(id, blocked)| HoldUser { id: UserId::from_raw(id), blocked }))
}

async fn user_by_id(conn: &mut PgConnection, id: UserId) -> Result<Option<HoldUser>, DomainError> {
	let row: Option<(Uuid, bool)> = sqlx::query_as(user_standing!("WHERE id = $1")).bind(id.raw()).fetch_optional(conn).await.map_err(repo_err)?;
	Ok(row.map(|(id, blocked)| HoldUser { id: UserId::from_raw(id), blocked }))
}

/// Write a changed hold's progress. Called only when the version moved.
async fn update_progress(conn: &mut PgConnection, hold: &Hold) -> Result<(), DomainError> {
	let progress = hold.progress();
	let version = |raw: u64| i64::try_from(raw).map_err(|_| DomainError::Repository(format!("scope hold {}: version {raw} overflows", hold.id())));
	sqlx::query("UPDATE scope_holds SET captured = $2, state = $3, version = $4, pending_version = $5, updated_at = now() WHERE id = $1")
		.bind(hold.id().raw())
		.bind(progress.captured.base_units().to_string())
		.bind(progress.state.as_str())
		.bind(version(progress.version)?)
		.bind(version(progress.pending_version)?)
		.execute(conn)
		.await
		.map_err(repo_err)?;
	Ok(())
}

#[async_trait]
impl HoldStore for PgScopeLedger {
	async fn hold(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<Option<RecordedHold>, DomainError> {
		sqlx::query_as::<_, HoldRow>(select_hold!("WHERE id = $1 AND scope = $2 AND client_id = $3"))
			.bind(id)
			.bind(scope.as_str())
			.bind(client.as_str())
			.fetch_optional(self.pool())
			.await
			.map_err(repo_err)?
			.map(recorded_hold)
			.transpose()
	}

	async fn record_hold(&self, key: RequestKey<'_>, terms: &str, auth_subject: &str, admission: &dyn HoldAdmission) -> Result<Recorded<RecordedHold>, TransferError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		outbox::lock_named(&mut tx, &key_lock(&key)).await?;
		if let Some(existing) = find_request(&mut tx, &key).await? {
			tx.commit().await.map_err(repo_err)?;
			return Ok(Recorded::Existing(existing));
		}
		let scope = compartments::scope_for_share(&mut tx, key.scope).await?;
		let user = user_by_subject(&mut tx, auth_subject).await?;
		// The lock every spender of the compartment takes (a move out, another hold), so the
		// spendable the admission reads cannot be spent twice.
		let mut mandate = None;
		if let Some(user) = user {
			outbox::lock_claim(&mut tx, &LedgerAccountKey::ScopeClaim(user.id, key.scope.clone())).await?;
			// Under the claim lock, so the usage below counts every hold this client placed on
			// the compartment, and the mandate row's lock holds a revoke off until this commits.
			if let Some(current) = scope_mandates::current_for_update(&mut tx, user.id, key.scope, key.client).await? {
				let usage = scope_mandates::usage(&mut tx, user.id, key.scope, key.client, current.id).await?;
				mandate = Some(MandateDraw { mandate: current, usage });
			}
		}
		let now = db_now(&mut tx).await?;
		let facts = HoldFacts { scope, user, now, mandate };
		// The refusal rolls the transaction back with nothing written.
		let mut hold = admission.admit(&facts, &mut TxReads::new(&mut tx)).await?;
		let held = hold.terms().clone();
		let created_at: i64 = sqlx::query_scalar(
			"INSERT INTO scope_holds (id, client_id, scope, user_id, auth_subject, amount, mandate_id, subject_ref, expires_at) \
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, to_timestamp($9)) RETURNING EXTRACT(EPOCH FROM created_at)::BIGINT",
		)
		.bind(hold.id().raw())
		.bind(held.client.as_str())
		.bind(held.scope.as_str())
		.bind(held.user.raw())
		.bind(auth_subject)
		.bind(held.amount.base_units().to_string())
		.bind(held.mandate_id)
		.bind(held.subject_ref.as_str())
		.bind(held.expires_at.map(|at| at as f64))
		.fetch_one(&mut *tx)
		.await
		.map_err(repo_err)?;
		if !insert_request(&mut tx, &key, terms, &hold.id().to_string()).await? {
			drop(tx);
			return Ok(Recorded::Existing(raced_request(self.pool(), &key).await?));
		}
		outbox::drain_to_outbox(&mut tx, &mut hold, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(Recorded::New(RecordedHold {
			hold,
			auth_subject: auth_subject.to_owned(),
			created_at,
		}))
	}

	async fn change_hold(&self, key: RequestKey<'_>, terms: &str, hold_id: Uuid, target: Option<&CaptureTarget>, change: &dyn HoldChange) -> Result<Recorded<RecordedHold>, TransferError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		outbox::lock_named(&mut tx, &key_lock(&key)).await?;
		if let Some(existing) = find_request(&mut tx, &key).await? {
			tx.commit().await.map_err(repo_err)?;
			return Ok(Recorded::Existing(existing));
		}
		// Another client's hold is the same NOT_FOUND as none: a client cannot learn which
		// hold ids exist.
		let row = sqlx::query_as::<_, HoldRow>(select_hold!("WHERE id = $1 AND scope = $2 AND client_id = $3 FOR UPDATE"))
			.bind(hold_id)
			.bind(key.scope.as_str())
			.bind(key.client.as_str())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?
			.ok_or_else(|| DomainError::NotFound {
				entity: "hold",
				id: hold_id.to_string(),
			})?;
		let RecordedHold { mut hold, auth_subject, created_at } = recorded_hold(row)?;
		let held_user = user_by_id(&mut tx, hold.terms().user).await?;
		let (destination_owner, destination_user, book_fill) = match target {
			Some(CaptureTarget::Account(label)) => (owner_on(&mut tx, key.scope, label).await?, None, None),
			Some(CaptureTarget::User(subject)) => (
				None,
				user_by_subject(&mut tx, subject).await?,
				scope_mandates::book_fill_draw(&mut tx, hold.terms().mandate_id).await?,
			),
			None => (None, None, None),
		};
		let facts = HoldChangeFacts {
			scope: compartments::scope_for_share(&mut tx, key.scope).await?,
			now: db_now(&mut tx).await?,
			held_user,
			destination_owner,
			destination_user,
			book_fill,
		};
		let before = hold.progress();
		change.apply(&mut hold, &facts).await?;
		let after = hold.progress();
		if after.version != before.version {
			update_progress(&mut tx, &hold).await?;
		}
		if matches!(target, Some(CaptureTarget::User(_)))
			&& let Some(filled) = after.captured.checked_sub(before.captured).filter(|filled| !filled.is_zero())
		{
			scope_mandates::record_book_fill(&mut tx, hold_id, after.version, hold.terms().mandate_id, filled).await?;
		}
		if !insert_request(&mut tx, &key, terms, &hold_id.to_string()).await? {
			drop(tx);
			return Ok(Recorded::Existing(raced_request(self.pool(), &key).await?));
		}
		outbox::drain_to_outbox(&mut tx, &mut hold, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(Recorded::New(RecordedHold { hold, auth_subject, created_at }))
	}
}

/// Release one hold past its TTL; `false` when it was captured, voided or expired since the
/// scan.
async fn expire_one(pool: &PgPool, id: Uuid) -> Result<bool, DomainError> {
	let mut tx = pool.begin().await.map_err(repo_err)?;
	let row = sqlx::query_as::<_, HoldRow>(select_hold!("WHERE id = $1 FOR UPDATE"))
		.bind(id)
		.fetch_optional(&mut *tx)
		.await
		.map_err(repo_err)?;
	let Some(row) = row else { return Ok(false) };
	let mut hold = recorded_hold(row)?.hold;
	let now = db_now(&mut tx).await?;
	if !hold.expire(now) {
		return Ok(false);
	}
	update_progress(&mut tx, &hold).await?;
	outbox::drain_to_outbox(&mut tx, &mut hold, true).await?;
	tx.commit().await.map_err(repo_err)?;
	Ok(true)
}

/// Release every active hold past its expiry, a batch at a time — the reaper's step. A hold
/// that cannot be read is logged and left for the next sweep, so one bad row never stops the
/// others. Returns how many it released.
pub async fn expire_due_holds(pool: &PgPool) -> Result<usize, sqlx::Error> {
	let due: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM scope_holds WHERE state = 'active' AND expires_at <= now() ORDER BY expires_at LIMIT 100")
		.fetch_all(pool)
		.await?;
	let mut expired = 0;
	for id in due {
		match expire_one(pool, id).await {
			Ok(true) => expired += 1,
			Ok(false) => {}
			Err(err) => tracing::warn!(hold_id = %id, "reaper: could not release an expired scope hold: {err}"),
		}
	}
	Ok(expired)
}

/// Release, in the caller's transaction, every active hold `client` has on `user`'s compartment
/// in `scope` — the user revoked the client's mandate. Each release drains its event for the
/// relay. Returns how many it released.
pub(crate) async fn void_for_revoke(conn: &mut PgConnection, client: &ClientId, scope: &ScopeId, user: UserId) -> Result<usize, DomainError> {
	let rows = sqlx::query_as::<_, HoldRow>(select_hold!("WHERE client_id = $1 AND scope = $2 AND user_id = $3 AND state = 'active' ORDER BY id FOR UPDATE"))
		.bind(client.as_str())
		.bind(scope.as_str())
		.bind(user.raw())
		.fetch_all(&mut *conn)
		.await
		.map_err(repo_err)?;
	let now = db_now(conn).await?;
	let mut released = 0;
	for row in rows {
		let mut hold = recorded_hold(row)?.hold;
		if hold.void_by_user(now) {
			update_progress(conn, &hold).await?;
			outbox::drain_to_outbox(conn, &mut hold, true).await?;
			released += 1;
		}
	}
	Ok(released)
}

/// Whether the ledger refused this hold's reservation and the relay voided the hold for it
/// ([`park_refused_reserve`]).
pub async fn reserve_refused(pool: &PgPool, hold_id: Uuid) -> Result<bool, sqlx::Error> {
	Ok(sqlx::query_scalar::<_, Option<bool>>("SELECT ledger_refused_at IS NOT NULL FROM scope_holds WHERE id = $1")
		.bind(hold_id)
		.fetch_optional(pool)
		.await?
		.flatten()
		.unwrap_or(false))
}

/// Park the outbox row `seq` — a hold's reservation the ledger did not place — and, in the
/// same transaction, turn the hold VOIDED with nothing captured, so no Capture answers
/// CAPTURED for money the ledger never held (banking#514). Captures recorded before the park
/// park after it in the relay (their reservation is missing), so `captured` goes back to zero.
pub async fn park_refused_reserve(pool: &PgPool, seq: i64, reason: &str, hold_id: Uuid) -> Result<(), sqlx::Error> {
	let mut tx = pool.begin().await?;
	outbox::mark_parked_on(&mut tx, seq, reason).await?;
	sqlx::query("UPDATE scope_holds SET state = 'voided', captured = '0', ledger_refused_at = now(), updated_at = now() WHERE id = $1 AND ledger_refused_at IS NULL")
		.bind(hold_id)
		.execute(&mut *tx)
		.await?;
	tx.commit().await
}

/// What `hold_clearing` must hold on the ledger: Σ what every active hold still holds. `None`
/// while any hold event is still on its way to the ledger — the two sides then legitimately
/// differ by it, and the next quiet pass compares them.
pub async fn hold_clearing_expected(pool: &PgPool) -> Result<Option<Usdt>, DomainError> {
	let (in_flight, expected): (bool, String) = sqlx::query_as(
		"SELECT EXISTS (SELECT 1 FROM outbox WHERE kind = $1 AND dispatched_at IS NULL AND parked_at IS NULL), \
		 (SELECT COALESCE(sum(amount::numeric - captured::numeric), 0)::text FROM scope_holds WHERE state = 'active')",
	)
	.bind(HoldEvent::KIND)
	.fetch_one(pool)
	.await
	.map_err(repo_err)?;
	if in_flight {
		return Ok(None);
	}
	amount("Σ active holds", &expected).map(Some)
}
