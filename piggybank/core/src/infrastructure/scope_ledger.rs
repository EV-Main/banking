//! Postgres adapter for [`ScopeLedgerStore`] — the owners of scope accounts (`scope_accounts`,
//! migration 0052), the service clients' idempotency keys and their transfers
//! (`scope_ledger_requests`, `scope_transfers`, migration 0054) — plus the control-plane steps
//! of a transfer's life outside the request: the relay's `posted`/`failed` stamp, the reaper's
//! replay of a `pending` transfer whose event was lost, and the pending sum every spend of a
//! scope account counts. Amounts are 18-dp base units as TEXT, summed in Rust.

use std::time::Duration;

use async_trait::async_trait;
use domain::{
	architecture::{AggregateRoot, DomainEvent},
	balance::LedgerAccountKey,
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, ScopeLedgerEvent, ScopeTransfer, ScopeTransferId, SubjectRef, TransferReason, TransferTerms},
	scopes::{MoveState, ScopeAccountLabel, ScopeId},
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::{claims::TxReads, compartments, money_events, outbox},
	ports::scope_ledger::{OwnedAccount, Recorded, RecordedTransfer, RequestKey, ScopeLedgerStore, StoredRequest, TransferAdmission, TransferError, TransferFacts},
};

pub struct PgScopeLedger {
	pool: PgPool,
}

impl PgScopeLedger {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}

	pub(crate) fn pool(&self) -> &PgPool {
		&self.pool
	}
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

fn amount(raw: &str) -> Result<Usdt, DomainError> {
	raw.parse::<u128>()
		.map(Usdt::from_base_units)
		.map_err(|_| DomainError::Repository(format!("not a base-unit amount: {raw:?}")))
}

type AccountRow = (String, String, String, i64);

fn owned_account(row: AccountRow) -> Result<OwnedAccount, DomainError> {
	let (scope, label, owner, opened_at) = row;
	let corrupt = |e: DomainError| DomainError::Repository(format!("scope account {scope}:{label}: {e}"));
	Ok(OwnedAccount {
		scope: ScopeId::parse(&scope).map_err(corrupt)?,
		label: ScopeAccountLabel::parse(&label).map_err(corrupt)?,
		owner: ClientId::parse(&owner).map_err(corrupt)?,
		opened_at,
	})
}

const SELECT_ACCOUNT: &str = "SELECT scope, label, owner_client, EXTRACT(EPOCH FROM created_at)::BIGINT FROM scope_accounts WHERE scope = $1 AND label = $2";

/// The advisory-lock name of one idempotency key. The RPC is part of it: keys are unique per
/// (client, scope, RPC), so the same key under two RPCs is two keys.
pub(crate) fn key_lock(key: &RequestKey<'_>) -> String {
	format!("scope_ledger_key:{}:{}:{}:{}", key.client, key.scope, key.rpc, key.key)
}

pub(crate) async fn find_request(conn: &mut PgConnection, key: &RequestKey<'_>) -> Result<Option<StoredRequest>, DomainError> {
	sqlx::query_as::<_, (String, String)>("SELECT terms, result_id FROM scope_ledger_requests WHERE client_id = $1 AND scope = $2 AND rpc = $3 AND idempotency_key = $4")
		.bind(key.client.as_str())
		.bind(key.scope.as_str())
		.bind(key.rpc)
		.bind(key.key)
		.fetch_optional(conn)
		.await
		.map_err(repo_err)
		.map(|row| row.map(|(terms, result_id)| StoredRequest { terms, result_id }))
}

/// Record the key in the transaction that records what it produced. `Ok(false)` is a unique
/// violation (23505): another call recorded the key first. The key lock makes that
/// unreachable while every writer takes it; should one ever not, the caller rolls back and
/// answers with what the other call recorded instead of an internal error.
pub(crate) async fn insert_request(conn: &mut PgConnection, key: &RequestKey<'_>, terms: &str, result_id: &str) -> Result<bool, DomainError> {
	let inserted = sqlx::query("INSERT INTO scope_ledger_requests (client_id, scope, rpc, idempotency_key, terms, result_id) VALUES ($1, $2, $3, $4, $5, $6)")
		.bind(key.client.as_str())
		.bind(key.scope.as_str())
		.bind(key.rpc)
		.bind(key.key)
		.bind(terms)
		.bind(result_id)
		.execute(conn)
		.await;
	match inserted {
		Ok(_) => Ok(true),
		Err(sqlx::Error::Database(err)) if err.is_unique_violation() => Ok(false),
		Err(err) => Err(repo_err(err)),
	}
}

/// What the call that won the key recorded, read after this one rolled back.
pub(crate) async fn raced_request(pool: &PgPool, key: &RequestKey<'_>) -> Result<StoredRequest, DomainError> {
	let mut conn = pool.acquire().await.map_err(repo_err)?;
	find_request(&mut conn, key)
		.await?
		.ok_or_else(|| DomainError::Repository(format!("idempotency key {} was taken, then not found", key.key)))
}

type TransferRow = (
	Uuid,
	String,
	String,
	String,
	String,
	String,
	String,
	String,
	String,
	String,
	String,
	Option<String>,
	i64,
	Option<Uuid>,
);

macro_rules! select_transfer {
	($tail:literal) => {
		concat!(
			"SELECT id, client_id, scope, from_label, to_scope, to_label, amount, reason, subject_ref, memo, state, failure_reason, ",
			"EXTRACT(EPOCH FROM created_at)::BIGINT, operation_id FROM scope_transfers WHERE id = $1",
			$tail
		)
	};
}
const SELECT_TRANSFER: &str = select_transfer!("");

fn recorded_transfer(row: TransferRow) -> Result<RecordedTransfer, DomainError> {
	let (id, client, scope, from, to_scope, to, raw_amount, reason, subject_ref, memo, state, failure_reason, created_at, operation_id) = row;
	let corrupt = |e: DomainError| DomainError::Repository(format!("scope transfer {id}: {e}"));
	let to_scope = ScopeId::parse(&to_scope).map_err(corrupt)?;
	let terms = TransferTerms {
		client: ClientId::parse(&client).map_err(corrupt)?,
		scope: ScopeId::parse(&scope).map_err(corrupt)?,
		from: ScopeAccountLabel::parse(&from).map_err(corrupt)?,
		to_scope: to_scope.clone(),
		to: ScopeAccountLabel::parse(&to).map_err(corrupt)?,
		amount: amount(&raw_amount)?,
		reason: TransferReason::parse(&reason).map_err(corrupt)?,
		subject_ref: SubjectRef::parse(&subject_ref).map_err(corrupt)?,
		memo,
		operation_id,
	};
	Ok(RecordedTransfer {
		transfer: ScopeTransfer::restore(ScopeTransferId::from_raw(id), terms).map_err(corrupt)?,
		to_scope,
		state: MoveState::parse(&state).map_err(corrupt)?,
		failure_reason,
		created_at,
	})
}

pub(crate) async fn transfer_on(conn: &mut PgConnection, id: Uuid) -> Result<Option<RecordedTransfer>, DomainError> {
	sqlx::query_as::<_, TransferRow>(SELECT_TRANSFER)
		.bind(id)
		.fetch_optional(conn)
		.await
		.map_err(repo_err)?
		.map(recorded_transfer)
		.transpose()
}

pub(crate) async fn owner_on(conn: &mut PgConnection, scope: &ScopeId, label: &ScopeAccountLabel) -> Result<Option<ClientId>, DomainError> {
	sqlx::query_as::<_, AccountRow>(SELECT_ACCOUNT)
		.bind(scope.as_str())
		.bind(label.as_str())
		.fetch_optional(conn)
		.await
		.map_err(repo_err)?
		.map(|row| owned_account(row).map(|account| account.owner))
		.transpose()
}

pub(crate) fn sum(amounts: &[String], what: &str) -> Result<Usdt, DomainError> {
	amounts.iter().try_fold(Usdt::ZERO, |total, raw| {
		total.checked_add(amount(raw)?).ok_or_else(|| DomainError::Repository(format!("{what} overflow")))
	})
}

#[async_trait]
impl ScopeLedgerStore for PgScopeLedger {
	async fn accounts_of(&self, client: &ClientId, scope: &ScopeId) -> Result<Vec<OwnedAccount>, DomainError> {
		sqlx::query_as::<_, AccountRow>("SELECT scope, label, owner_client, EXTRACT(EPOCH FROM created_at)::BIGINT FROM scope_accounts WHERE scope = $1 AND owner_client = $2 ORDER BY label")
			.bind(scope.as_str())
			.bind(client.as_str())
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?
			.into_iter()
			.map(owned_account)
			.collect()
	}

	async fn open_account(&self, key: RequestKey<'_>, label: &ScopeAccountLabel, terms: &str) -> Result<Recorded<OwnedAccount>, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		outbox::lock_named(&mut tx, &key_lock(&key)).await?;
		if let Some(existing) = find_request(&mut tx, &key).await? {
			tx.commit().await.map_err(repo_err)?;
			return Ok(Recorded::Existing(existing));
		}
		// Two first opens of one label by two clients race on the primary key, never on a
		// lock either of them holds: `ON CONFLICT DO NOTHING` lets exactly one insert, and the
		// other reads the winner's row.
		let opened = sqlx::query("INSERT INTO scope_accounts (scope, label, owner_client) VALUES ($1, $2, $3) ON CONFLICT (scope, label) DO NOTHING")
			.bind(key.scope.as_str())
			.bind(label.as_str())
			.bind(key.client.as_str())
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?
			.rows_affected()
			== 1;
		let account = sqlx::query_as::<_, AccountRow>(SELECT_ACCOUNT)
			.bind(key.scope.as_str())
			.bind(label.as_str())
			.fetch_one(&mut *tx)
			.await
			.map_err(repo_err)
			.and_then(owned_account)?;
		if &account.owner != key.client {
			return Err(DomainError::Conflict(format!("scope account {}:{label} belongs to another client", key.scope)));
		}
		if !insert_request(&mut tx, &key, terms, label.as_str()).await? {
			drop(tx);
			return Ok(Recorded::Existing(raced_request(&self.pool, &key).await?));
		}
		// Only the call that created the row announces it: a second key naming an account the
		// client already owns opened nothing.
		if opened {
			money_events::append(&mut tx, &money_events::account_opened(&account)).await?;
		}
		tx.commit().await.map_err(repo_err)?;
		Ok(Recorded::New(account))
	}

	async fn transfer(&self, id: Uuid) -> Result<Option<RecordedTransfer>, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		transfer_on(&mut conn, id).await
	}

	async fn record_transfer(&self, transfer: &mut ScopeTransfer, key: RequestKey<'_>, terms: &str, admission: &dyn TransferAdmission) -> Result<Recorded<RecordedTransfer>, TransferError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// The key first, for the reason a compartment move takes it first: two requests under
		// one key may name different sources, and only the key serializes them.
		outbox::lock_named(&mut tx, &key_lock(&key)).await?;
		if let Some(existing) = find_request(&mut tx, &key).await? {
			tx.commit().await.map_err(repo_err)?;
			return Ok(Recorded::Existing(existing));
		}
		// The ceiling is per (client, scope) and spans every source account the client owns
		// there, so the sum below needs a lock of its own: the source's lock alone would let
		// two transfers out of two accounts both count the same room.
		outbox::lock_named(&mut tx, &format!("scope_ledger_ceiling:{}:{}", key.client, key.scope)).await?;
		let scope = transfer.scope().clone();
		// The scope stream's consumer writes a state under `FOR UPDATE`, so a freeze cannot
		// commit between this read and the transfer it admits.
		let scope_record = compartments::scope_for_share(&mut tx, &scope).await?;
		let source_owner = owner_on(&mut tx, &scope, transfer.from()).await?;
		let destination_owner = owner_on(&mut tx, &scope, transfer.to()).await?;
		// The lock every spender of this account takes, so the spendable read below cannot be
		// raced by another debit of it.
		outbox::lock_claim(&mut tx, &LedgerAccountKey::ScopeAccount(scope.clone(), transfer.from().clone())).await?;
		// The client's own transfers only: an approved one was the approvers' to allow.
		let recent: Vec<String> = sqlx::query_scalar(
			"SELECT amount FROM scope_transfers WHERE client_id = $1 AND scope = $2 AND operation_id IS NULL AND state IN ('pending', 'posted') \
			 AND created_at > now() - interval '24 hours'",
		)
		.bind(key.client.as_str())
		.bind(scope.as_str())
		.fetch_all(&mut *tx)
		.await
		.map_err(repo_err)?;
		let facts = TransferFacts {
			scope: scope_record,
			source_owner,
			destination_owner,
			transferred_24h: sum(&recent, "transfers in 24 h")?,
		};
		// The refusal rolls the transaction back with nothing written.
		admission.admit(&facts, &mut TxReads::new(&mut tx)).await?;
		let created_at = insert_transfer(&mut tx, transfer).await?;
		if !insert_request(&mut tx, &key, terms, &transfer.id().to_string()).await? {
			drop(tx);
			return Ok(Recorded::Existing(raced_request(&self.pool, &key).await?));
		}
		outbox::drain_to_outbox(&mut tx, transfer, true).await?;
		tx.commit().await.map_err(repo_err)?;
		let recorded = ScopeTransfer::restore(transfer.id(), transfer_terms(transfer))?;
		Ok(Recorded::New(RecordedTransfer {
			transfer: recorded,
			to_scope: transfer.to_scope().clone(),
			state: MoveState::Pending,
			failure_reason: None,
			created_at,
		}))
	}
}

pub(crate) fn transfer_terms(transfer: &ScopeTransfer) -> TransferTerms {
	TransferTerms {
		client: transfer.client().clone(),
		scope: transfer.scope().clone(),
		from: transfer.from().clone(),
		to_scope: transfer.to_scope().clone(),
		to: transfer.to().clone(),
		amount: transfer.amount(),
		reason: transfer.reason(),
		subject_ref: transfer.subject_ref().clone(),
		memo: transfer.memo().to_owned(),
		operation_id: transfer.operation_id(),
	}
}

/// Write a transfer's row; its event is the caller's to drain in the same transaction.
/// Returns its creation time, unix seconds.
pub(crate) async fn insert_transfer(conn: &mut PgConnection, transfer: &ScopeTransfer) -> Result<i64, DomainError> {
	sqlx::query_scalar(
		"INSERT INTO scope_transfers (id, client_id, scope, from_label, to_scope, to_label, amount, reason, subject_ref, memo, operation_id) \
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING EXTRACT(EPOCH FROM created_at)::BIGINT",
	)
	.bind(transfer.id().raw())
	.bind(transfer.client().as_str())
	.bind(transfer.scope().as_str())
	.bind(transfer.from().as_str())
	.bind(transfer.to_scope().as_str())
	.bind(transfer.to().as_str())
	.bind(transfer.amount().base_units().to_string())
	.bind(transfer.reason().as_str())
	.bind(transfer.subject_ref().as_str())
	.bind(transfer.memo())
	.bind(transfer.operation_id())
	.fetch_one(conn)
	.await
	.map_err(repo_err)
}

/// Σ the `pending` transfers and payouts out of `(scope, label)`: promised away, not yet off
/// the ledger. Part of what [`claims::unapplied_debits`](super::claims::unapplied_debits)
/// counts — from these tables rather than the outbox, so a transfer or payout whose event an
/// older relay dispatched without posting is still counted until the reaper replays it.
pub async fn pending_out(conn: &mut PgConnection, scope: &ScopeId, label: &ScopeAccountLabel) -> Result<Usdt, DomainError> {
	let amounts: Vec<String> = sqlx::query_scalar(
		"SELECT amount FROM scope_transfers WHERE scope = $1 AND from_label = $2 AND state = 'pending' \
		 UNION ALL SELECT total FROM scope_payouts WHERE scope = $1 AND from_label = $2 AND state = 'pending'",
	)
	.bind(scope.as_str())
	.bind(label.as_str())
	.fetch_all(conn)
	.await
	.map_err(repo_err)?;
	sum(&amounts, "pending transfers and payouts")
}

/// Where transfer `id` stands, `None` when no row holds it. The relay reads it before posting:
/// only a `pending` transfer may still reach the ledger.
pub async fn transfer_state(pool: &PgPool, id: Uuid) -> Result<Option<MoveState>, sqlx::Error> {
	let state: Option<String> = sqlx::query_scalar("SELECT state FROM scope_transfers WHERE id = $1").bind(id).fetch_optional(pool).await?;
	state.map(|raw| MoveState::parse(&raw).map_err(|e| sqlx::Error::Decode(Box::new(e)))).transpose()
}

/// The ledger took the transfer. Only a `pending` row moves; a repeat is a no-op. A client's fee
/// sweep, posted into `(scope, fee)`, takes banking's route out of it with the same stamp, in
/// the same transaction: the sweep is never `posted` without its route recorded, and a replay
/// of the stamp records nothing twice.
pub async fn mark_posted(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
	let mut tx = pool.begin().await?;
	let stamped: Option<(String, String, String)> =
		sqlx::query_as("UPDATE scope_transfers SET state = 'posted', updated_at = now() WHERE id = $1 AND state = 'pending' RETURNING reason, scope, amount")
			.bind(id)
			.fetch_optional(&mut *tx)
			.await?;
	if let Some((reason, scope, raw_amount)) = stamped
		&& reason == TransferReason::FeeSweep.as_str()
	{
		route_swept_fee(&mut tx, id, &scope, &raw_amount).await.map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
	}
	tx.commit().await
}

/// Record banking's route of a posted fee sweep: `(scope, fee)` to wherever the scope's fees
/// go (`domain::scope_ledger::fee_route`), as one transfer the relay posts like any other, so
/// the route is atomic — it moves whole or not at all — and has its own money events. The
/// unique `fee_route_of` is its idempotency key.
///
/// A destination account that does not exist (invest has not opened `alloc:fee`) is recorded
/// as a `failed` route with nothing posted: a missing account must not poison the relay's
/// queue, and the fees stay on `fee`, visible, for an operator to re-route once it exists.
async fn route_swept_fee(conn: &mut PgConnection, sweep: Uuid, scope: &str, raw_amount: &str) -> Result<(), DomainError> {
	let scope = ScopeId::parse(scope)?;
	let mut route = ScopeTransfer::route_fee(ScopeTransferId::new(), ScopeTransferId::from_raw(sweep), scope, amount(raw_amount)?)?;
	let destination_open = owner_on(conn, route.to_scope(), route.to()).await?.is_some();
	let (state, failure) = if destination_open { ("pending", None) } else { ("failed", Some("destination_missing")) };
	let inserted: Option<Uuid> = sqlx::query_scalar(
		"INSERT INTO scope_transfers (id, client_id, scope, from_label, to_scope, to_label, amount, reason, subject_ref, memo, fee_route_of, state, failure_reason) \
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, '', $10, $11, $12) ON CONFLICT (fee_route_of) DO NOTHING RETURNING id",
	)
	.bind(route.id().raw())
	.bind(route.client().as_str())
	.bind(route.scope().as_str())
	.bind(route.from().as_str())
	.bind(route.to_scope().as_str())
	.bind(route.to().as_str())
	.bind(route.amount().base_units().to_string())
	.bind(route.reason().as_str())
	.bind(route.subject_ref().as_str())
	.bind(sweep)
	.bind(state)
	.bind(failure)
	.fetch_optional(&mut *conn)
	.await
	.map_err(repo_err)?;
	if inserted.is_none() {
		return Ok(());
	}
	if let Some(why) = failure {
		tracing::warn!(sweep_id = %sweep, scope = %route.scope(), to = %format!("{}:{}", route.to_scope(), route.to()), "scope ledger: fee route not made: {why}; the swept fee stays on `fee`");
		return Ok(());
	}
	outbox::drain_to_outbox(conn, &mut route, true).await
}

/// The ledger refused the transfer: terminal, nothing moved, never executed later.
pub async fn mark_failed(pool: &PgPool, id: Uuid, reason: &str) -> Result<(), sqlx::Error> {
	sqlx::query("UPDATE scope_transfers SET state = 'failed', failure_reason = $2, updated_at = now() WHERE id = $1 AND state = 'pending'")
		.bind(id)
		.bind(reason)
		.execute(pool)
		.await?;
	Ok(())
}

/// Put one stale transfer's event back on the outbox; `false` when it was posted or failed
/// since the scan.
async fn redrive_one(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
	let mut tx = pool.begin().await?;
	let row = sqlx::query_as::<_, TransferRow>(select_transfer!(" AND state = 'pending' FOR UPDATE"))
		.bind(id)
		.fetch_optional(&mut *tx)
		.await?;
	let Some(row) = row else { return Ok(false) };
	let recorded = recorded_transfer(row).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
	let transfer = &recorded.transfer;
	let event = ScopeLedgerEvent::Transferred {
		transfer_id: transfer.id(),
		scope: transfer.scope().clone(),
		from_label: transfer.from().clone(),
		to_scope: recorded.to_scope.clone(),
		to_label: transfer.to().clone(),
		amount: transfer.amount(),
	};
	let payload = serde_json::to_string(&event).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
	outbox::insert_event(&mut tx, Uuid::new_v4(), ScopeTransfer::NAME, id, ScopeLedgerEvent::KIND, &payload, true)
		.await
		.map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
	sqlx::query("UPDATE scope_transfers SET updated_at = now() WHERE id = $1").bind(id).execute(&mut *tx).await?;
	tx.commit().await?;
	Ok(true)
}

/// Re-enqueue every `pending` transfer untouched for `older_than` whose event is no longer
/// waiting in the outbox — the relay that dispatched it did not post it (an older build that
/// did not know the kind). The same contract as
/// [`compartments::redrive_stale_moves`](super::compartments::redrive_stale_moves): the ledger
/// transfer id derives from the transfer id, so a replay of one that did post answers
/// `Exists` and is only stamped `posted`. Returns how many it re-enqueued.
pub async fn redrive_stale_transfers(pool: &PgPool, older_than: Duration) -> Result<usize, sqlx::Error> {
	let stale: Vec<Uuid> = sqlx::query_scalar(
		"SELECT t.id FROM scope_transfers t WHERE t.state = 'pending' AND t.updated_at < now() - make_interval(secs => $1) \
		 AND NOT EXISTS (SELECT 1 FROM outbox o WHERE o.dispatched_at IS NULL AND o.aggregate_id = t.id) ORDER BY t.updated_at LIMIT 100",
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
				tracing::warn!(transfer_id = %id, "reaper: could not replay a pending scope transfer: {err}");
				if let Err(err) = sqlx::query("UPDATE scope_transfers SET updated_at = now() WHERE id = $1").bind(id).execute(pool).await {
					tracing::warn!(transfer_id = %id, "reaper: could not push the unreplayable scope transfer back in the queue: {err}");
				}
			}
		}
	}
	Ok(redriven)
}

/// Scope accounts the ledger holds value on with no owner on record — value nobody can say is
/// whose. Every account a client can reach is opened through [`ScopeLedgerStore::open_account`],
/// which writes the owner before any money can arrive, and banking's own are seeded or opened
/// by the scope stream; a hit here is a bug or a hand-made ledger account. Returns the logical
/// keys; reconciliation reads their balances.
pub async fn unowned_scope_accounts(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
	sqlx::query_scalar(
		"SELECT t.logical_key FROM tb_accounts t WHERE t.code = $1 \
		 AND NOT EXISTS (SELECT 1 FROM scope_accounts a WHERE t.logical_key = 'scope_account:' || a.scope || ':' || a.label) ORDER BY t.logical_key",
	)
	.bind(i32::from(domain::balance::AccountCode::ScopeAccount.code()))
	.fetch_all(pool)
	.await
}
