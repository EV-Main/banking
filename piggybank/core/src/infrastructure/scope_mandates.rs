//! Postgres adapter for [`MandateStore`] — users' mandates (`scope_mandates`, migration 0058)
//! — plus the reads the hold store decides a draw with: the current mandate under its row lock,
//! what the client has drawn on the compartment, and a mandate's book fills.
//!
//! Lock order, so that no two of these writers wait on each other in a circle: a Reserve takes
//! the compartment's claim lock, then the mandate row; a grant and a revoke take the mandate
//! row, then the client's hold rows; a capture into another user takes its hold row, then the
//! client's book-fill lock on the compartment, and reads the mandates without locking them. A
//! revoke that locks the row first sees every hold a Reserve committed under it, and a Reserve
//! after it re-reads the row as revoked.

use async_trait::async_trait;
use domain::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, HoldTerms, MandateOperation, MoneyEventKind},
	scope_mandates::{Mandate, MandateCaps, MandateRequest, MandateRequestTerms, MandateTerms, MandateUsage},
	scopes::ScopeId,
	users::UserId,
};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
	infrastructure::{
		money_events::{self, NewMoneyEvent},
		outbox,
		scope_holds::{self, db_now},
		scope_ledger::PgScopeLedger,
	},
	ports::{
		scope_ledger::MandateDraw,
		scope_mandates::{Granted, MandateRequestStore, MandateStore, RecordedMandate, Requested},
	},
};

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

fn base_units(column: &str, raw: &str) -> Result<Usdt, DomainError> {
	raw.parse::<u128>()
		.map(Usdt::from_base_units)
		.map_err(|_| DomainError::Repository(format!("{column} is not a base-unit amount: {raw:?}")))
}

type MandateRow = (
	Uuid,
	Uuid,
	String,
	String,
	String,
	String,
	String,
	Option<String>,
	Vec<String>,
	Option<i64>,
	i64,
	Option<i64>,
	Option<i64>,
);

macro_rules! select_mandate {
	($tail:literal) => {
		concat!(
			"SELECT id, user_id, scope, client_id, cap_per_op, cap_24h, cap_outstanding, cap_book_fill_24h, operations, ",
			"EXTRACT(EPOCH FROM expires_at)::BIGINT, EXTRACT(EPOCH FROM created_at)::BIGINT, ",
			"EXTRACT(EPOCH FROM revoked_at)::BIGINT, EXTRACT(EPOCH FROM replaced_at)::BIGINT FROM scope_mandates ",
			$tail
		)
	};
}

fn mandate_from(row: MandateRow) -> Result<Mandate, DomainError> {
	let (id, user, scope, client, per_op, per_24h, outstanding, book_fill, operations, expires_at, created_at, revoked_at, replaced_at) = row;
	let corrupt = |e: DomainError| DomainError::Repository(format!("scope mandate {id}: {e}"));
	let operations = operations.iter().map(|raw| MandateOperation::parse(raw)).collect::<Result<Vec<_>, _>>().map_err(corrupt)?;
	let caps = MandateCaps {
		per_op: base_units("cap_per_op", &per_op)?,
		per_24h: base_units("cap_24h", &per_24h)?,
		outstanding: base_units("cap_outstanding", &outstanding)?,
		book_fill_24h: book_fill.as_deref().map(|raw| base_units("cap_book_fill_24h", raw)).transpose()?,
	};
	let terms = MandateTerms::new(
		UserId::from_raw(user),
		ScopeId::parse(&scope).map_err(corrupt)?,
		ClientId::parse(&client).map_err(corrupt)?,
		caps,
		&operations,
		expires_at,
	)
	.map_err(corrupt)?;
	Ok(Mandate {
		id,
		terms,
		created_at,
		revoked_at,
		replaced_at,
	})
}

/// The user's current mandate for `client` in `scope` — neither revoked nor replaced, though
/// possibly expired — with its row locked until the transaction ends.
pub(crate) async fn current_for_update(conn: &mut PgConnection, user: UserId, scope: &ScopeId, client: &ClientId) -> Result<Option<Mandate>, DomainError> {
	sqlx::query_as::<_, MandateRow>(select_mandate!(
		"WHERE user_id = $1 AND scope = $2 AND client_id = $3 AND revoked_at IS NULL AND replaced_at IS NULL FOR UPDATE"
	))
	.bind(user.raw())
	.bind(scope.as_str())
	.bind(client.as_str())
	.fetch_optional(conn)
	.await
	.map_err(repo_err)?
	.map(mandate_from)
	.transpose()
}

/// [`current_for_update`] without the row lock, for a capture: it already holds a hold row,
/// which a grant or a revoke takes after the mandate row.
async fn current(conn: &mut PgConnection, user: UserId, scope: &ScopeId, client: &ClientId) -> Result<Option<Mandate>, DomainError> {
	sqlx::query_as::<_, MandateRow>(select_mandate!(
		"WHERE user_id = $1 AND scope = $2 AND client_id = $3 AND revoked_at IS NULL AND replaced_at IS NULL"
	))
	.bind(user.raw())
	.bind(scope.as_str())
	.bind(client.as_str())
	.fetch_optional(conn)
	.await
	.map_err(repo_err)?
	.map(mandate_from)
	.transpose()
}

/// What `client` has drawn on `user`'s compartment in `scope`, under any of the user's mandates,
/// on the database's clock (one `now()` for the whole statement). Exact sums in `numeric`: the
/// amounts are 18-dp base units, past what a float holds. Only active holds and those placed in
/// the window are read, so a long-lived client's history costs nothing. A charge counts in full
/// from the moment it is recorded until the ledger refuses it (`failed` gave nothing).
pub(crate) async fn usage(conn: &mut PgConnection, user: UserId, scope: &ScopeId, client: &ClientId) -> Result<MandateUsage, DomainError> {
	let (drawn, outstanding, filled): (String, String, String) = sqlx::query_as(
		"SELECT \
		   (COALESCE(sum(CASE WHEN state IN ('voided', 'expired') THEN captured ELSE amount END::numeric) \
		      FILTER (WHERE created_at > now() - interval '24 hours'), 0) \
		    + (SELECT COALESCE(sum(c.amount::numeric), 0) FROM scope_hold_captures c JOIN scope_holds old ON old.id = c.hold_id \
		       WHERE c.user_id = $1 AND c.scope = $2 AND c.client_id = $3 AND c.created_at > now() - interval '24 hours' \
		         AND old.created_at <= now() - interval '24 hours') \
		    + (SELECT COALESCE(sum(ch.amount::numeric), 0) FROM scope_charges ch WHERE ch.user_id = $1 AND ch.scope = $2 AND ch.client_id = $3 \
		       AND ch.state <> 'failed' AND ch.created_at > now() - interval '24 hours'))::text, \
		   COALESCE(sum(amount::numeric - captured::numeric) FILTER (WHERE state = 'active'), 0)::text, \
		   (SELECT COALESCE(sum(amount::numeric), 0)::text FROM scope_book_fills \
		     WHERE user_id = $1 AND scope = $2 AND client_id = $3 AND created_at > now() - interval '24 hours') \
		 FROM scope_holds WHERE user_id = $1 AND scope = $2 AND client_id = $3 \
		   AND (state = 'active' OR created_at > now() - interval '24 hours')",
	)
	.bind(user.raw())
	.bind(scope.as_str())
	.bind(client.as_str())
	.fetch_one(conn)
	.await
	.map_err(repo_err)?;
	Ok(MandateUsage {
		drawn_24h: base_units("drawn in 24 h", &drawn)?,
		outstanding: base_units("outstanding", &outstanding)?,
		book_filled_24h: base_units("book filled in 24 h", &filled)?,
	})
}

/// For a capture of `hold` into another user: the mandate the hold was placed under with the
/// client's usage of the compartment, and the user's current mandate for the client. The
/// client's fills on one compartment serialize on a lock of their own — not a mandate row,
/// which a grant or a revoke takes before the hold rows a capture already holds.
pub(crate) async fn book_fill_draw(conn: &mut PgConnection, hold: &HoldTerms) -> Result<(Option<MandateDraw>, Option<Mandate>), DomainError> {
	outbox::lock_named(conn, &format!("scope_book_fill:{}:{}:{}", hold.user, hold.scope, hold.client)).await?;
	let placed = sqlx::query_as::<_, MandateRow>(select_mandate!("WHERE id = $1"))
		.bind(hold.mandate_id)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.map(mandate_from)
		.transpose()?;
	let current = current(conn, hold.user, &hold.scope, &hold.client).await?;
	let Some(mandate) = placed else { return Ok((None, current)) };
	let usage = usage(conn, hold.user, &hold.scope, &hold.client).await?;
	Ok((Some(MandateDraw { mandate, usage }), current))
}

fn stored_version(hold_id: Uuid, version: u64) -> Result<i64, DomainError> {
	i64::try_from(version).map_err(|_| DomainError::Repository(format!("scope hold {hold_id}: version {version} overflows")))
}

/// Count `amount` of hold `hold_id`'s capture `version` against `cap_24h`.
pub(crate) async fn record_capture(conn: &mut PgConnection, hold_id: Uuid, version: u64, hold: &HoldTerms, amount: Usdt) -> Result<(), DomainError> {
	sqlx::query("INSERT INTO scope_hold_captures (hold_id, version, user_id, scope, client_id, amount) VALUES ($1, $2, $3, $4, $5, $6)")
		.bind(hold_id)
		.bind(stored_version(hold_id, version)?)
		.bind(hold.user.raw())
		.bind(hold.scope.as_str())
		.bind(hold.client.as_str())
		.bind(amount.base_units().to_string())
		.execute(conn)
		.await
		.map_err(repo_err)?;
	Ok(())
}

/// Count `amount` of hold `hold_id`'s capture `version` against the BOOK_FILL cap.
pub(crate) async fn record_book_fill(conn: &mut PgConnection, hold_id: Uuid, version: u64, hold: &HoldTerms, amount: Usdt) -> Result<(), DomainError> {
	sqlx::query("INSERT INTO scope_book_fills (hold_id, version, mandate_id, user_id, scope, client_id, amount) VALUES ($1, $2, $3, $4, $5, $6, $7)")
		.bind(hold_id)
		.bind(stored_version(hold_id, version)?)
		.bind(hold.mandate_id)
		.bind(hold.user.raw())
		.bind(hold.scope.as_str())
		.bind(hold.client.as_str())
		.bind(amount.base_units().to_string())
		.execute(conn)
		.await
		.map_err(repo_err)?;
	Ok(())
}

async fn auth_subject_of(conn: &mut PgConnection, user: UserId) -> Result<String, DomainError> {
	sqlx::query_scalar("SELECT auth_subject FROM users WHERE id = $1")
		.bind(user.raw())
		.fetch_optional(conn)
		.await
		.map_err(repo_err)?
		.ok_or_else(|| DomainError::NotFound {
			entity: "user",
			id: user.to_string(),
		})
}

/// MANDATE_GRANTED is a mandate's version 1 and MANDATE_REVOKED its version 2: a mandate
/// changes once at most.
const GRANTED_VERSION: u64 = 1;
const REVOKED_VERSION: u64 = 2;

/// A mandate event, for the mandate's client alone. The user caused it, so no client is its
/// initiator.
async fn append_event(conn: &mut PgConnection, mandate: &Mandate, kind: MoneyEventKind, version: u64) -> Result<(), DomainError> {
	let auth_subject = auth_subject_of(conn, mandate.terms.user).await?;
	let event = NewMoneyEvent {
		event_id: money_events::event_id(&format!("mandate:{}:{version}", mandate.id)),
		scope: mandate.terms.scope.clone(),
		kind,
		ref_id: mandate.id.to_string(),
		aggregate_version: version,
		auth_subject,
		amount: None,
		from: None,
		to: None,
		subject_ref: String::new(),
		initiator_client: String::new(),
		balances: Vec::new(),
		hold_remaining: None,
		counterparty_scope: String::new(),
		failure_reason: String::new(),
		paid_from: String::new(),
		audience: vec![mandate.terms.client.clone()],
	};
	money_events::append(conn, &event).await
}

/// A mandate with what the client has drawn. A replaced one shows its own holds as
/// `outstanding`, so the user sees which money still stands on it; the 24 h figures are the
/// client's on the compartment either way, as the caps count them.
async fn recorded(conn: &mut PgConnection, mandate: Mandate) -> Result<RecordedMandate, DomainError> {
	let terms = &mandate.terms;
	let mut usage = usage(conn, terms.user, &terms.scope, &terms.client).await?;
	if mandate.replaced_at.is_some() {
		let own: String = sqlx::query_scalar("SELECT COALESCE(sum(amount::numeric - captured::numeric), 0)::text FROM scope_holds WHERE mandate_id = $1 AND state = 'active'")
			.bind(mandate.id)
			.fetch_one(&mut *conn)
			.await
			.map_err(repo_err)?;
		usage.outstanding = base_units("outstanding", &own)?;
	}
	Ok(RecordedMandate { mandate, usage })
}

#[async_trait]
impl MandateStore for PgScopeLedger {
	async fn grant(&self, terms: &MandateTerms, idempotency_key: &str) -> Result<Granted, DomainError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		outbox::lock_named(&mut tx, &format!("scope_mandate_key:{}:{idempotency_key}", terms.user)).await?;
		let existing: Option<(String, Uuid)> = sqlx::query_as("SELECT terms, id FROM scope_mandates WHERE user_id = $1 AND idempotency_key = $2")
			.bind(terms.user.raw())
			.bind(idempotency_key)
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		if let Some((stored_terms, id)) = existing {
			let row = sqlx::query_as::<_, MandateRow>(select_mandate!("WHERE id = $1"))
				.bind(id)
				.fetch_one(&mut *tx)
				.await
				.map_err(repo_err)?;
			let mandate = recorded(&mut tx, mandate_from(row)?).await?;
			tx.commit().await.map_err(repo_err)?;
			return Ok(Granted::Existing { terms: stored_terms, mandate });
		}
		terms.check_expiry(db_now(&mut tx).await?)?;
		// Two grants for one (user, scope, client) under different keys: the second replaces the
		// first instead of tripping the one-current-mandate index.
		outbox::lock_named(&mut tx, &format!("scope_mandate:{}:{}:{}", terms.user, terms.scope, terms.client)).await?;
		sqlx::query("UPDATE scope_mandates SET replaced_at = now() WHERE user_id = $1 AND scope = $2 AND client_id = $3 AND revoked_at IS NULL AND replaced_at IS NULL")
			.bind(terms.user.raw())
			.bind(terms.scope.as_str())
			.bind(terms.client.as_str())
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		// Holds placed under an earlier mandate stay with it, but none outlives the consent the
		// user gives now.
		if let Some(expires_at) = terms.expires_at {
			sqlx::query(
				"UPDATE scope_holds SET expires_at = LEAST(COALESCE(expires_at, to_timestamp($4)), to_timestamp($4)), updated_at = now() \
				 WHERE user_id = $1 AND scope = $2 AND client_id = $3 AND state = 'active'",
			)
			.bind(terms.user.raw())
			.bind(terms.scope.as_str())
			.bind(terms.client.as_str())
			.bind(expires_at as f64)
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		}
		let operations: Vec<&str> = terms.operations.iter().map(|operation| operation.as_str()).collect();
		let row = sqlx::query_as::<_, MandateRow>(concat!(
			"INSERT INTO scope_mandates (id, user_id, scope, client_id, cap_per_op, cap_24h, cap_outstanding, cap_book_fill_24h, operations, ",
			"expires_at, idempotency_key, terms) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, to_timestamp($10), $11, $12) ",
			"RETURNING id, user_id, scope, client_id, cap_per_op, cap_24h, cap_outstanding, cap_book_fill_24h, operations, ",
			"EXTRACT(EPOCH FROM expires_at)::BIGINT, EXTRACT(EPOCH FROM created_at)::BIGINT, ",
			"EXTRACT(EPOCH FROM revoked_at)::BIGINT, EXTRACT(EPOCH FROM replaced_at)::BIGINT"
		))
		.bind(Uuid::new_v4())
		.bind(terms.user.raw())
		.bind(terms.scope.as_str())
		.bind(terms.client.as_str())
		.bind(terms.caps.per_op.base_units().to_string())
		.bind(terms.caps.per_24h.base_units().to_string())
		.bind(terms.caps.outstanding.base_units().to_string())
		.bind(terms.caps.book_fill_24h.map(|cap| cap.base_units().to_string()))
		.bind(&operations)
		.bind(terms.expires_at.map(|at| at as f64))
		.bind(idempotency_key)
		.bind(terms.canonical())
		.fetch_one(&mut *tx)
		.await
		.map_err(repo_err)?;
		let mandate = mandate_from(row)?;
		append_event(&mut tx, &mandate, MoneyEventKind::MandateGranted, GRANTED_VERSION).await?;
		let mandate = recorded(&mut tx, mandate).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(Granted::New(mandate))
	}

	async fn revoke(&self, user: UserId, id: Uuid) -> Result<Option<RecordedMandate>, DomainError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		let row = sqlx::query_as::<_, MandateRow>(select_mandate!("WHERE id = $1 AND user_id = $2 FOR UPDATE"))
			.bind(id)
			.bind(user.raw())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		let Some(row) = row else { return Ok(None) };
		let mut mandate = mandate_from(row)?;
		if mandate.revoked_at.is_none() && mandate.replaced_at.is_none() {
			let revoked_at: i64 = sqlx::query_scalar("UPDATE scope_mandates SET revoked_at = now() WHERE id = $1 RETURNING EXTRACT(EPOCH FROM revoked_at)::BIGINT")
				.bind(id)
				.fetch_one(&mut *tx)
				.await
				.map_err(repo_err)?;
			mandate.revoked_at = Some(revoked_at);
			// Every hold of the client on this compartment, under this mandate or one it
			// replaced: revoking is the user taking the service off their money, not one grant.
			scope_holds::void_for_revoke(&mut tx, &mandate.terms.client, &mandate.terms.scope, user).await?;
			append_event(&mut tx, &mandate, MoneyEventKind::MandateRevoked, REVOKED_VERSION).await?;
		}
		let mandate = recorded(&mut tx, mandate).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(Some(mandate))
	}

	async fn live(&self, user: UserId) -> Result<Vec<RecordedMandate>, DomainError> {
		let mut conn = self.pool().acquire().await.map_err(repo_err)?;
		let rows = sqlx::query_as::<_, MandateRow>(select_mandate!(
			"WHERE user_id = $1 AND revoked_at IS NULL AND ( \
			   (replaced_at IS NULL AND (expires_at IS NULL OR expires_at > now())) \
			   OR (replaced_at IS NOT NULL AND EXISTS (SELECT 1 FROM scope_holds h WHERE h.mandate_id = scope_mandates.id AND h.state = 'active')) \
			 ) ORDER BY created_at DESC, id"
		))
		.bind(user.raw())
		.fetch_all(&mut *conn)
		.await
		.map_err(repo_err)?;
		let mut out = Vec::with_capacity(rows.len());
		// The current mandate per (scope, client) the user granted, and the few replaced ones
		// still holding money: a handful, read one after another on the one connection.
		for row in rows {
			out.push(recorded(&mut conn, mandate_from(row)?).await?);
		}
		Ok(out)
	}
}

type RequestRow = (Uuid, String, String, String, String, String, Option<String>, Vec<String>, Option<i64>, i64, i64);

macro_rules! select_request {
	($tail:literal) => {
		concat!(
			"SELECT id, scope, client_id, cap_per_op, cap_24h, cap_outstanding, cap_book_fill_24h, operations, ",
			"EXTRACT(EPOCH FROM mandate_expires_at)::BIGINT, EXTRACT(EPOCH FROM created_at)::BIGINT, ",
			"EXTRACT(EPOCH FROM expires_at)::BIGINT FROM scope_mandate_requests ",
			$tail
		)
	};
}

fn request_from(row: RequestRow) -> Result<MandateRequest, DomainError> {
	let (id, scope, client, per_op, per_24h, outstanding, book_fill, operations, mandate_expires_at, created_at, expires_at) = row;
	let corrupt = |e: DomainError| DomainError::Repository(format!("scope mandate request {id}: {e}"));
	let operations = operations.iter().map(|raw| MandateOperation::parse(raw)).collect::<Result<Vec<_>, _>>().map_err(corrupt)?;
	let caps = MandateCaps {
		per_op: base_units("cap_per_op", &per_op)?,
		per_24h: base_units("cap_24h", &per_24h)?,
		outstanding: base_units("cap_outstanding", &outstanding)?,
		book_fill_24h: book_fill.as_deref().map(|raw| base_units("cap_book_fill_24h", raw)).transpose()?,
	};
	let terms = MandateRequestTerms::new(
		ScopeId::parse(&scope).map_err(corrupt)?,
		ClientId::parse(&client).map_err(corrupt)?,
		caps,
		&operations,
		mandate_expires_at,
	)
	.map_err(corrupt)?;
	Ok(MandateRequest { id, terms, created_at, expires_at })
}

/// What the client's key recorded, expired or not: a repeat is answered with the original.
async fn by_key(conn: &mut PgConnection, client: &ClientId, idempotency_key: &str) -> Result<Option<(String, MandateRequest)>, DomainError> {
	let stored: Option<(String, Uuid)> = sqlx::query_as("SELECT terms, id FROM scope_mandate_requests WHERE client_id = $1 AND idempotency_key = $2")
		.bind(client.as_str())
		.bind(idempotency_key)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
	let Some((terms, id)) = stored else { return Ok(None) };
	let row = sqlx::query_as::<_, RequestRow>(select_request!("WHERE id = $1"))
		.bind(id)
		.fetch_one(conn)
		.await
		.map_err(repo_err)?;
	Ok(Some((terms, request_from(row)?)))
}

#[async_trait]
impl MandateRequestStore for PgScopeLedger {
	async fn create(&self, terms: &MandateRequestTerms, idempotency_key: &str, ttl_seconds: i64) -> Result<Requested, DomainError> {
		let mut conn = self.pool().acquire().await.map_err(repo_err)?;
		if let Some((stored, request)) = by_key(&mut conn, &terms.client, idempotency_key).await? {
			return Ok(Requested::Existing { terms: stored, request });
		}
		terms.check_expiry(db_now(&mut conn).await?)?;
		let operations: Vec<&str> = terms.operations.iter().map(|operation| operation.as_str()).collect();
		// A concurrent create under the same key loses the insert and reads the winner's row.
		let inserted = sqlx::query_as::<_, RequestRow>(concat!(
			"INSERT INTO scope_mandate_requests (id, scope, client_id, cap_per_op, cap_24h, cap_outstanding, cap_book_fill_24h, operations, ",
			"mandate_expires_at, idempotency_key, terms, expires_at) ",
			"VALUES ($1, $2, $3, $4, $5, $6, $7, $8, to_timestamp($9), $10, $11, now() + make_interval(secs => $12)) ",
			"ON CONFLICT (client_id, idempotency_key) DO NOTHING ",
			"RETURNING id, scope, client_id, cap_per_op, cap_24h, cap_outstanding, cap_book_fill_24h, operations, ",
			"EXTRACT(EPOCH FROM mandate_expires_at)::BIGINT, EXTRACT(EPOCH FROM created_at)::BIGINT, EXTRACT(EPOCH FROM expires_at)::BIGINT"
		))
		.bind(Uuid::new_v4())
		.bind(terms.scope.as_str())
		.bind(terms.client.as_str())
		.bind(terms.caps.per_op.base_units().to_string())
		.bind(terms.caps.per_24h.base_units().to_string())
		.bind(terms.caps.outstanding.base_units().to_string())
		.bind(terms.caps.book_fill_24h.map(|cap| cap.base_units().to_string()))
		.bind(&operations)
		.bind(terms.mandate_expires_at.map(|at| at as f64))
		.bind(idempotency_key)
		.bind(terms.canonical())
		.bind(ttl_seconds as f64)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
		if let Some(row) = inserted {
			return Ok(Requested::New(request_from(row)?));
		}
		let (stored, request) = by_key(&mut conn, &terms.client, idempotency_key)
			.await?
			.ok_or_else(|| DomainError::Repository(format!("mandate request key {idempotency_key} conflicted, then vanished")))?;
		Ok(Requested::Existing { terms: stored, request })
	}

	async fn get(&self, id: Uuid) -> Result<Option<MandateRequest>, DomainError> {
		sqlx::query_as::<_, RequestRow>(select_request!("WHERE id = $1 AND expires_at > now()"))
			.bind(id)
			.fetch_optional(self.pool())
			.await
			.map_err(repo_err)?
			.map(request_from)
			.transpose()
	}
}
