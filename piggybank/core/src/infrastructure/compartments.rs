//! Postgres adapter for [`CompartmentStore`] — the scope registry, the scope policies and
//! users' compartment moves (`scopes`, `scope_policies`, `compartment_moves`, migration 0052).
//! Amounts are 18-dp base units as TEXT; nothing here sums money the ledger holds — the
//! rolling 24 h total is a policy figure over the recorded moves, added up in Rust.

use async_trait::async_trait;
use domain::{
	error::DomainError,
	money::Usdt,
	scopes::{Compartment, CompartmentMove, CompartmentMoveId, ScopeId, ScopePolicy, ScopeState},
	users::UserId,
};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{
	infrastructure::outbox,
	ports::compartments::{CompartmentStore, MoveOutcome, RecordedMove, ScopeRecord},
};

pub struct PgCompartments {
	pool: PgPool,
}

impl PgCompartments {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

/// A scope without a policy row reads as the column defaults — what the migration gives a
/// row that states nothing — so a scope announced before its policy is set is never
/// mistaken for one without limits it should have had.
macro_rules! select_scopes {
	() => {
		"SELECT s.id, s.state, s.display_name, \
		 COALESCE(p.min_kyc_level, 0), p.max_per_move, p.max_moved_in_24h, p.max_balance, \
		 COALESCE(p.auto_payout_per_op, '1000000000000000000000'), COALESCE(p.auto_payout_24h, '5000000000000000000000'), \
		 COALESCE(p.auto_transfer_24h, '5000000000000000000000'), \
		 EXTRACT(EPOCH FROM COALESCE(p.updated_at, s.created_at))::BIGINT \
		 FROM scopes s LEFT JOIN scope_policies p ON p.scope = s.id"
	};
}
const SELECT_ALL_SCOPES: &str = concat!(select_scopes!(), " ORDER BY s.id");
const SELECT_ONE_SCOPE: &str = concat!(select_scopes!(), " WHERE s.id = $1");

type ScopeRow = (String, String, String, i32, Option<String>, Option<String>, Option<String>, String, String, String, i64);

fn amount(column: &str, raw: &str) -> Result<Usdt, DomainError> {
	raw.parse::<u128>()
		.map(Usdt::from_base_units)
		.map_err(|_| DomainError::Repository(format!("{column} is not a base-unit amount: {raw:?}")))
}

fn limit(column: &str, raw: Option<String>) -> Result<Option<Usdt>, DomainError> {
	raw.as_deref().map(|raw| amount(column, raw)).transpose()
}

fn scope_record(row: ScopeRow) -> Result<ScopeRecord, DomainError> {
	let (id, state, display_name, min_kyc_level, max_per_move, max_moved_in_24h, max_balance, payout_per_op, payout_24h, transfer_24h, updated_at) = row;
	let corrupt = |what: &str, e: DomainError| DomainError::Repository(format!("scope {id}: {what}: {e}"));
	Ok(ScopeRecord {
		id: ScopeId::parse(&id).map_err(|e| corrupt("id", e))?,
		state: ScopeState::parse(&state).map_err(|e| corrupt("state", e))?,
		display_name,
		policy: ScopePolicy {
			min_kyc_level: u32::try_from(min_kyc_level).map_err(|_| DomainError::Repository(format!("scope {id}: negative min_kyc_level")))?,
			max_per_move: limit("max_per_move", max_per_move)?,
			max_moved_in_24h: limit("max_moved_in_24h", max_moved_in_24h)?,
			max_balance: limit("max_balance", max_balance)?,
			auto_payout_per_op: amount("auto_payout_per_op", &payout_per_op)?,
			auto_payout_24h: amount("auto_payout_24h", &payout_24h)?,
			auto_transfer_24h: amount("auto_transfer_24h", &transfer_24h)?,
		},
		policy_updated_at: updated_at,
	})
}

const SELECT_MOVE: &str = "SELECT id, user_id, from_scope, to_scope, amount, EXTRACT(EPOCH FROM created_at)::BIGINT FROM compartment_moves WHERE user_id = $1 AND idempotency_key = $2";

type MoveRow = (Uuid, Uuid, String, String, String, i64);

fn recorded_move(row: MoveRow) -> Result<RecordedMove, DomainError> {
	let (id, user, from, to, raw_amount, created_at) = row;
	let corrupt = |e: DomainError| DomainError::Repository(format!("compartment move {id}: {e}"));
	let movement = CompartmentMove::restore(
		CompartmentMoveId::from_raw(id),
		UserId::from_raw(user),
		Compartment::parse(&from).map_err(corrupt)?,
		Compartment::parse(&to).map_err(corrupt)?,
		amount("amount", &raw_amount)?,
	)
	.map_err(corrupt)?;
	Ok(RecordedMove { movement, created_at })
}

async fn find_move_in(tx: &mut Transaction<'_, Postgres>, user: UserId, idempotency_key: &str) -> Result<Option<RecordedMove>, DomainError> {
	sqlx::query_as::<_, MoveRow>(SELECT_MOVE)
		.bind(user.raw())
		.bind(idempotency_key)
		.fetch_optional(&mut **tx)
		.await
		.map_err(repo_err)?
		.map(recorded_move)
		.transpose()
}

#[async_trait]
impl CompartmentStore for PgCompartments {
	async fn scopes(&self) -> Result<Vec<ScopeRecord>, DomainError> {
		sqlx::query_as::<_, ScopeRow>(SELECT_ALL_SCOPES)
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?
			.into_iter()
			.map(scope_record)
			.collect()
	}

	async fn scope(&self, id: &ScopeId) -> Result<Option<ScopeRecord>, DomainError> {
		sqlx::query_as::<_, ScopeRow>(SELECT_ONE_SCOPE)
			.bind(id.as_str())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?
			.map(scope_record)
			.transpose()
	}

	async fn find_move(&self, user: UserId, idempotency_key: &str) -> Result<Option<RecordedMove>, DomainError> {
		sqlx::query_as::<_, MoveRow>(SELECT_MOVE)
			.bind(user.raw())
			.bind(idempotency_key)
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?
			.map(recorded_move)
			.transpose()
	}

	async fn moved_in_24h(&self, user: UserId, scope: &ScopeId) -> Result<Usdt, DomainError> {
		let amounts: Vec<String> = sqlx::query_scalar("SELECT amount FROM compartment_moves WHERE user_id = $1 AND to_scope = $2 AND created_at > now() - interval '24 hours'")
			.bind(user.raw())
			.bind(scope.as_str())
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		amounts.iter().try_fold(Usdt::ZERO, |total, raw| {
			total
				.checked_add(amount("amount", raw)?)
				.ok_or_else(|| DomainError::Repository(format!("24 h total moved into {scope} overflows")))
		})
	}

	async fn record_move(&self, movement: &mut CompartmentMove, idempotency_key: &str) -> Result<MoveOutcome, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// The lock every writer that spends this claim takes (withdraw, subscribe), so their
		// writes serialize. The caller's Read-First stays optimistic, as theirs does: a move
		// that loses a race is refused by the claim's non-negative flag and parks in the
		// relay with nothing moved.
		outbox::lock_claim(&mut tx, &movement.from().claim_key(movement.user())).await?;
		let inserted = sqlx::query_scalar::<_, Uuid>(
			"INSERT INTO compartment_moves (id, user_id, from_scope, to_scope, amount, idempotency_key) VALUES ($1, $2, $3, $4, $5, $6) \
			 ON CONFLICT (user_id, idempotency_key) DO NOTHING RETURNING id",
		)
		.bind(movement.id().raw())
		.bind(movement.user().raw())
		.bind(movement.from().as_str())
		.bind(movement.to().as_str())
		.bind(movement.amount().base_units().to_string())
		.bind(idempotency_key)
		.fetch_optional(&mut *tx)
		.await
		.map_err(repo_err)?;
		if inserted.is_none() {
			let existing = find_move_in(&mut tx, movement.user(), idempotency_key)
				.await?
				.ok_or_else(|| DomainError::Repository(format!("compartment move key {idempotency_key:?} conflicted but no row holds it")))?;
			tx.commit().await.map_err(repo_err)?;
			return Ok(MoveOutcome::Existing(existing));
		}
		outbox::drain_to_outbox(&mut tx, movement, true).await?;
		let recorded = find_move_in(&mut tx, movement.user(), idempotency_key)
			.await?
			.ok_or_else(|| DomainError::Repository(format!("compartment move {} vanished inside its own transaction", movement.id())))?;
		tx.commit().await.map_err(repo_err)?;
		Ok(MoveOutcome::Recorded(recorded))
	}
}
