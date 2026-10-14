//! Postgres adapter for [`CompartmentStore`] — the scope registry, the scope policies and
//! users' compartment moves (`scopes`, `scope_policies`, `compartment_moves`, migration 0052)
//! — plus the two control-plane steps of a move's life outside the request: the relay's
//! `posted`/`failed` stamp and the reaper's replay of a `pending` move whose event was lost.
//! Amounts are 18-dp base units as TEXT; nothing here sums money the ledger holds — the
//! in-flight and 24 h figures are sums over recorded moves, added up in Rust.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use domain::{
	architecture::{AggregateRoot, DomainEvent},
	error::DomainError,
	money::Usdt,
	scopes::{Compartment, CompartmentEvent, CompartmentMove, CompartmentMoveId, MAX_MOVES_PER_MINUTE, MoveRefusal, MoveState, ScopeId, ScopePolicy, ScopeState},
	users::UserId,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::{claims::TxReads, outbox, outflow},
	ports::compartments::{CompartmentStore, InFlight, MoveAdmission, MoveError, MoveOutcome, RecordedMove, ScopeRecord},
};

pub struct PgCompartments {
	pool: PgPool,
	/// The scope list, read once per [`SCOPES_TTL`]: every wallet and balance read sums over
	/// it, and it changes when concierge announces a scope — rarely. A move never reads it:
	/// [`CompartmentStore::scope`] goes to the table every time.
	scopes: std::sync::Mutex<Option<(Instant, Vec<ScopeRecord>)>>,
}

/// How long the cached scope list stands. A scope announced or frozen shows in the listing
/// within this; it is enforced on a move at once.
const SCOPES_TTL: Duration = Duration::from_secs(30);

impl PgCompartments {
	pub fn new(pool: PgPool) -> Self {
		Self {
			pool,
			scopes: std::sync::Mutex::new(None),
		}
	}

	fn cached_scopes(&self) -> Option<Vec<ScopeRecord>> {
		// A poisoned lock only means a reader panicked; the cached list is still whole.
		let cache = self.scopes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		cache.as_ref().filter(|(read_at, _)| read_at.elapsed() < SCOPES_TTL).map(|(_, scopes)| scopes.clone())
	}
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

/// A scope without a policy row reads as the column defaults, exactly like a row that states
/// nothing: `min_kyc_level` 0, no `max_*` limit on moves in, the automatic ceilings at their
/// defaults — open to every user. Only the seeded scopes rely on that, and they have rows; a
/// scope the stream registers gets a row closed to moves in
/// (`scope_events::CLOSED_UNTIL_SET_KYC`) until an operator sets its policy.
macro_rules! select_scopes {
	() => {
		"SELECT s.id, s.state, s.display_name, \
		 COALESCE(p.min_kyc_level, 0), p.max_per_move, p.max_moved_in_24h, p.max_balance, \
		 COALESCE(p.auto_payout_per_op, '1000000000000000000000'), COALESCE(p.auto_payout_24h, '5000000000000000000000'), \
		 COALESCE(p.auto_transfer_per_op, '1000000000000000000000'), COALESCE(p.auto_transfer_24h, '5000000000000000000000'), \
		 EXTRACT(EPOCH FROM COALESCE(p.updated_at, s.created_at))::BIGINT \
		 FROM scopes s LEFT JOIN scope_policies p ON p.scope = s.id"
	};
}
macro_rules! concat_move_select {
	($tail:literal) => {
		concat!(
			"SELECT id, user_id, from_scope, to_scope, amount, state, failure_reason, EXTRACT(EPOCH FROM created_at)::BIGINT FROM compartment_moves ",
			$tail
		)
	};
}
const SELECT_ALL_SCOPES: &str = concat!(select_scopes!(), " ORDER BY s.id");
const SELECT_ONE_SCOPE: &str = concat!(select_scopes!(), " WHERE s.id = $1");
/// The scope a move in decides against, held until the move commits: the scope stream's
/// consumer writes a state under `FOR UPDATE`, so a freeze cannot commit between this read
/// and the move it records, and a move that starts after the freeze reads it. A move
/// recorded before the freeze still posts — the freeze stops new moves in, not those
/// already admitted.
const SELECT_ONE_SCOPE_FOR_SHARE: &str = concat!(select_scopes!(), " WHERE s.id = $1 FOR SHARE OF s");

type ScopeRow = (String, String, String, i32, Option<String>, Option<String>, Option<String>, String, String, String, String, i64);

fn amount(column: &str, raw: &str) -> Result<Usdt, DomainError> {
	raw.parse::<u128>()
		.map(Usdt::from_base_units)
		.map_err(|_| DomainError::Repository(format!("{column} is not a base-unit amount: {raw:?}")))
}

fn limit(column: &str, raw: Option<String>) -> Result<Option<Usdt>, DomainError> {
	raw.as_deref().map(|raw| amount(column, raw)).transpose()
}

fn scope_record(row: ScopeRow) -> Result<ScopeRecord, DomainError> {
	let (id, state, display_name, min_kyc_level, max_per_move, max_moved_in_24h, max_balance, payout_per_op, payout_24h, transfer_per_op, transfer_24h, updated_at) = row;
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
			auto_transfer_per_op: amount("auto_transfer_per_op", &transfer_per_op)?,
			auto_transfer_24h: amount("auto_transfer_24h", &transfer_24h)?,
		},
		policy_updated_at: updated_at,
	})
}

type MoveRow = (Uuid, Uuid, String, String, String, String, Option<String>, i64);

fn recorded_move(row: MoveRow) -> Result<RecordedMove, DomainError> {
	let (id, user, from, to, raw_amount, state, failure_reason, created_at) = row;
	let corrupt = |e: DomainError| DomainError::Repository(format!("compartment move {id}: {e}"));
	let movement = CompartmentMove::restore(
		CompartmentMoveId::from_raw(id),
		UserId::from_raw(user),
		Compartment::parse(&from).map_err(corrupt)?,
		Compartment::parse(&to).map_err(corrupt)?,
		amount("amount", &raw_amount)?,
	)
	.map_err(corrupt)?;
	Ok(RecordedMove {
		movement,
		state: MoveState::parse(&state).map_err(corrupt)?,
		failure_reason,
		created_at,
	})
}

async fn find_move_on(conn: &mut PgConnection, user: UserId, idempotency_key: &str) -> Result<Option<RecordedMove>, DomainError> {
	sqlx::query_as::<_, MoveRow>(concat_move_select!("WHERE user_id = $1 AND idempotency_key = $2"))
		.bind(user.raw())
		.bind(idempotency_key)
		.fetch_optional(conn)
		.await
		.map_err(repo_err)?
		.map(recorded_move)
		.transpose()
}

/// Σ of a set of stored amounts.
async fn sum_amounts<'c>(conn: impl sqlx::PgExecutor<'c>, sql: &'static str, user: UserId, compartment: &str) -> Result<Usdt, DomainError> {
	let amounts: Vec<String> = sqlx::query_scalar(sql).bind(user.raw()).bind(compartment).fetch_all(conn).await.map_err(repo_err)?;
	amounts.iter().try_fold(Usdt::ZERO, |total, raw| {
		total
			.checked_add(amount("amount", raw)?)
			.ok_or_else(|| DomainError::Repository(format!("moves of {compartment} overflow")))
	})
}

async fn scope_on<'c>(conn: impl sqlx::PgExecutor<'c>, sql: &'static str, id: &ScopeId) -> Result<Option<ScopeRecord>, DomainError> {
	sqlx::query_as::<_, ScopeRow>(sql)
		.bind(id.as_str())
		.fetch_optional(conn)
		.await
		.map_err(repo_err)?
		.map(scope_record)
		.transpose()
}

/// One scope, held `FOR SHARE` until the caller's transaction ends — what a debit out of the
/// scope decides against, so a freeze cannot commit between the read and the debit it admits
/// (see [`SELECT_ONE_SCOPE_FOR_SHARE`]).
pub async fn scope_for_share(conn: &mut PgConnection, id: &ScopeId) -> Result<Option<ScopeRecord>, DomainError> {
	scope_on(conn, SELECT_ONE_SCOPE_FOR_SHARE, id).await
}

const PENDING_OUT: &str = "SELECT amount FROM compartment_moves WHERE user_id = $1 AND from_scope = $2 AND state = 'pending'";
/// Σ the user's `pending` moves out of `compartment`: promised away, not yet off the ledger.
/// Part of what [`claims::unapplied_debits`](super::claims::unapplied_debits) counts.
pub async fn pending_out<'c>(conn: impl sqlx::PgExecutor<'c>, user: UserId, compartment: &Compartment) -> Result<Usdt, DomainError> {
	sum_amounts(conn, PENDING_OUT, user, compartment.as_str()).await
}

const PENDING_IN: &str = "SELECT amount FROM compartment_moves WHERE user_id = $1 AND to_scope = $2 AND state = 'pending'";
const MOVED_IN_24H: &str = "SELECT amount FROM compartment_moves WHERE user_id = $1 AND to_scope = $2 AND state IN ('pending', 'posted') AND created_at > now() - interval '24 hours'";

#[async_trait]
impl CompartmentStore for PgCompartments {
	async fn scopes(&self) -> Result<Vec<ScopeRecord>, DomainError> {
		if let Some(scopes) = self.cached_scopes() {
			return Ok(scopes);
		}
		let scopes = sqlx::query_as::<_, ScopeRow>(SELECT_ALL_SCOPES)
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?
			.into_iter()
			.map(scope_record)
			.collect::<Result<Vec<_>, _>>()?;
		*self.scopes.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), scopes.clone()));
		Ok(scopes)
	}

	async fn scope(&self, id: &ScopeId) -> Result<Option<ScopeRecord>, DomainError> {
		scope_on(&self.pool, SELECT_ONE_SCOPE, id).await
	}

	async fn find_move(&self, user: UserId, idempotency_key: &str) -> Result<Option<RecordedMove>, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		find_move_on(&mut conn, user, idempotency_key).await
	}

	async fn record_move(&self, movement: &mut CompartmentMove, idempotency_key: &str, admission: &dyn MoveAdmission) -> Result<MoveOutcome, MoveError> {
		let user = movement.user();
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// The key first: two requests under one key may name different sources, so the
		// claim locks alone would let both through to the unique index. With the key held,
		// the second finds the first's row below.
		outbox::lock_named(&mut tx, &format!("compartment_key:{user}:{idempotency_key}")).await?;
		if let Some(existing) = find_move_on(&mut tx, user, idempotency_key).await? {
			tx.commit().await.map_err(repo_err)?;
			return Ok(MoveOutcome::Existing(existing));
		}
		// One lock per user so the per-minute count below cannot be raced past by moves out
		// of two different compartments.
		outbox::lock_named(&mut tx, &format!("compartment_user:{user}")).await?;
		let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM compartment_moves WHERE user_id = $1 AND created_at > now() - interval '1 minute'")
			.bind(user.raw())
			.fetch_one(&mut *tx)
			.await
			.map_err(repo_err)?;
		if recent >= MAX_MOVES_PER_MINUTE {
			return Err(MoveError::Refused(MoveRefusal::RateLimited));
		}
		// The source's lock is the one every writer that spends that claim takes (withdraw,
		// subscribe), so their writes serialize with this one. A move in also takes the
		// target's, always second: only a move in ever holds two, and it takes them in one
		// order, so two moves cannot wait on each other.
		outbox::lock_claim(&mut tx, &movement.from().claim_key(user)).await?;
		if movement.is_move_in() {
			outbox::lock_claim(&mut tx, &movement.to().claim_key(user)).await?;
		}
		let mut in_flight = InFlight::default();
		if movement.is_move_in() {
			in_flight.pending_into_target = sum_amounts(&mut *tx, PENDING_IN, user, movement.to().as_str()).await?;
			in_flight.moved_in_24h = sum_amounts(&mut *tx, MOVED_IN_24H, user, movement.scope().as_str()).await?;
			in_flight.scope = scope_on(&mut *tx, SELECT_ONE_SCOPE_FOR_SHARE, movement.scope()).await?;
			in_flight.standing = outflow::standing(&mut *tx, user).await?;
		}
		// The refusal rolls the transaction back with nothing written.
		admission.admit(&in_flight, &mut TxReads::new(&mut tx)).await?;
		let inserted = sqlx::query_as::<_, (Uuid, i64)>(
			"INSERT INTO compartment_moves (id, user_id, from_scope, to_scope, amount, idempotency_key) VALUES ($1, $2, $3, $4, $5, $6) \
			 RETURNING id, EXTRACT(EPOCH FROM created_at)::BIGINT",
		)
		.bind(movement.id().raw())
		.bind(user.raw())
		.bind(movement.from().as_str())
		.bind(movement.to().as_str())
		.bind(movement.amount().base_units().to_string())
		.bind(idempotency_key)
		.fetch_one(&mut *tx)
		.await;
		let (id, created_at) = match inserted {
			Ok(row) => row,
			// The key lock above makes this unreachable; should it ever not, the caller is
			// told the key is taken, never an internal error.
			Err(sqlx::Error::Database(err)) if err.is_unique_violation() => return Err(DomainError::Conflict("idempotency_key was used for a different move".into()).into()),
			Err(err) => return Err(repo_err(err).into()),
		};
		outbox::drain_to_outbox(&mut tx, movement, true).await?;
		tx.commit().await.map_err(repo_err)?;
		let recorded = CompartmentMove::restore(CompartmentMoveId::from_raw(id), user, movement.from().clone(), movement.to().clone(), movement.amount())?;
		Ok(MoveOutcome::Recorded(RecordedMove {
			movement: recorded,
			state: MoveState::Pending,
			failure_reason: None,
			created_at,
		}))
	}
}

/// Where move `id` stands, `None` when no row holds it. The relay reads it before posting:
/// only a `pending` move may still reach the ledger.
pub async fn move_state(pool: &PgPool, id: Uuid) -> Result<Option<MoveState>, sqlx::Error> {
	let state: Option<String> = sqlx::query_scalar("SELECT state FROM compartment_moves WHERE id = $1").bind(id).fetch_optional(pool).await?;
	state.map(|raw| MoveState::parse(&raw).map_err(|e| sqlx::Error::Decode(Box::new(e)))).transpose()
}

/// The ledger took the move. Only a `pending` row moves; a repeat is a no-op.
pub async fn mark_posted(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
	sqlx::query("UPDATE compartment_moves SET state = 'posted', updated_at = now() WHERE id = $1 AND state = 'pending'")
		.bind(id)
		.execute(pool)
		.await?;
	Ok(())
}

/// The ledger refused the move: terminal, nothing moved, never executed later.
pub async fn mark_failed(pool: &PgPool, id: Uuid, reason: &str) -> Result<(), sqlx::Error> {
	sqlx::query("UPDATE compartment_moves SET state = 'failed', failure_reason = $2, updated_at = now() WHERE id = $1 AND state = 'pending'")
		.bind(id)
		.bind(reason)
		.execute(pool)
		.await?;
	Ok(())
}

/// Put one stale move's event back on the outbox; `false` when it was posted or failed
/// since the scan.
async fn redrive_one(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
	let mut tx = pool.begin().await?;
	let row = sqlx::query_as::<_, MoveRow>(concat_move_select!("WHERE id = $1 AND state = 'pending' FOR UPDATE"))
		.bind(id)
		.fetch_optional(&mut *tx)
		.await?;
	let Some(row) = row else { return Ok(false) };
	let recorded = recorded_move(row).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
	let movement = &recorded.movement;
	let event = CompartmentEvent::Moved {
		move_id: movement.id(),
		user: movement.user(),
		from: movement.from().clone(),
		to: movement.to().clone(),
		amount: movement.amount(),
	};
	let payload = serde_json::to_string(&event).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
	outbox::insert_event(&mut tx, Uuid::new_v4(), CompartmentMove::NAME, id, CompartmentEvent::KIND, &payload, true)
		.await
		.map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
	sqlx::query("UPDATE compartment_moves SET updated_at = now() WHERE id = $1").bind(id).execute(&mut *tx).await?;
	tx.commit().await?;
	Ok(true)
}

/// Re-enqueue every `pending` move untouched for `older_than` whose event is no longer
/// waiting in the outbox — the relay that dispatched it did not post it (an older build that
/// did not know the kind, during a rolling deploy or a rollback). A parked event is still in
/// the outbox (parked rows are undispatched) and is left to the operator. The replay is
/// safe however often it runs: the ledger transfer id derives from the move id, so a move
/// that did post answers `Exists` and is stamped `posted`. A row that cannot be replayed is
/// logged and skipped. Returns how many it re-enqueued.
pub async fn redrive_stale_moves(pool: &PgPool, older_than: Duration) -> Result<usize, sqlx::Error> {
	let stale: Vec<Uuid> = sqlx::query_scalar(
		"SELECT m.id FROM compartment_moves m WHERE m.state = 'pending' AND m.updated_at < now() - make_interval(secs => $1) \
		 AND NOT EXISTS (SELECT 1 FROM outbox o WHERE o.dispatched_at IS NULL AND o.aggregate_id = m.id) ORDER BY m.updated_at LIMIT 100",
	)
	.bind(older_than.as_secs_f64())
	.fetch_all(pool)
	.await?;
	let mut redriven = 0;
	for id in stale {
		match redrive_one(pool, id).await {
			Ok(true) => redriven += 1,
			Ok(false) => {}
			// One unreadable row must not stop the others being replayed. Its `updated_at`
			// is bumped so it waits its turn again instead of heading every scan.
			Err(err) => {
				tracing::warn!(move_id = %id, "reaper: could not replay a pending compartment move: {err}");
				if let Err(err) = sqlx::query("UPDATE compartment_moves SET updated_at = now() WHERE id = $1").bind(id).execute(pool).await {
					tracing::warn!(move_id = %id, "reaper: could not push the unreplayable move back in the queue: {err}");
				}
			}
		}
	}
	Ok(redriven)
}
