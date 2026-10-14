//! Postgres adapter for the [`WithdrawalRepository`] port.
//!
//! Mirrors [`PgRedemptions`](super::redemptions::PgRedemptions): the command methods
//! are atomic and **row-locked** — load `FOR UPDATE`, apply the aggregate command
//! inside the lock (the aggregate is the single authority on a transition's validity),
//! then persist the new state together with the drained events (event_log + outbox)
//! in one transaction. A transition emits an event only when it actually happens, so
//! an idempotent re-settle/re-fail writes no outbox row and the relay moves money
//! exactly once.

use async_trait::async_trait;
use domain::{
	architecture::{Reader, Repository},
	error::DomainError,
	money::{Network, TxRef, Usdt, WalletAddress},
	users::UserId,
	withdrawals::{Withdrawal, WithdrawalId, WithdrawalSource, WithdrawalState},
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::outbox,
	ports::{WithdrawalRepository, withdrawals::QueuedWithdrawal},
};

const SELECT_BY_ID: &str = "SELECT id, source, user_id, network, address, amount, fee, state, tx_ref FROM withdrawals WHERE id = $1";
const SELECT_BY_ID_FOR_UPDATE: &str = "SELECT id, source, user_id, network, address, amount, fee, state, tx_ref FROM withdrawals WHERE id = $1 FOR UPDATE";
const SELECT_BY_USER: &str = "SELECT id, source, user_id, network, address, amount, fee, state, tx_ref FROM withdrawals WHERE user_id = $1 AND source = 'user' ORDER BY created_at DESC";

pub struct PgWithdrawals {
	pool: PgPool,
}

impl PgWithdrawals {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

impl Repository for PgWithdrawals {
	type Aggregate = Withdrawal;
}

impl Reader for PgWithdrawals {
	type Aggregate = Withdrawal;
}

#[derive(sqlx::FromRow)]
struct WithdrawalRow {
	id: Uuid,
	/// Always `user` (CHECKed since 0047; the retired `revenue` payout never reached
	/// production). Paired with `user_id` by a schema CHECK.
	source: String,
	user_id: Option<Uuid>,
	network: String,
	address: String,
	amount: String,
	fee: String,
	state: String,
	tx_ref: Option<String>,
}

impl WithdrawalRow {
	fn source(&self) -> Result<WithdrawalSource, DomainError> {
		source_of(self.id, &self.source, self.user_id)
	}

	fn into_domain(self) -> Result<Withdrawal, DomainError> {
		let source = self.source()?;
		let network = Network::parse(&self.network)?;
		let amount = Usdt::from_base_units(self.amount.parse::<u128>().map_err(|_| DomainError::Repository("malformed withdrawal amount".into()))?);
		let fee = Usdt::from_base_units(self.fee.parse::<u128>().map_err(|_| DomainError::Repository("malformed withdrawal fee".into()))?);
		let address = WalletAddress::parse(network, &self.address)?;
		let tx_ref = self.tx_ref.as_deref().map(TxRef::parse).transpose()?;
		Ok(Withdrawal::rehydrate(
			WithdrawalId::from_raw(self.id),
			source,
			network,
			address,
			amount,
			fee,
			WithdrawalState::parse(&self.state)?,
			tx_ref,
		))
	}
}

/// The `(source, user_id)` column pair back into a source; the schema pairs them by CHECK.
fn source_of(id: Uuid, source: &str, user_id: Option<Uuid>) -> Result<WithdrawalSource, DomainError> {
	match (source, user_id) {
		("user", Some(user)) => Ok(WithdrawalSource::User(UserId::from_raw(user))),
		("treasury", None) => Ok(WithdrawalSource::Treasury),
		_ => Err(DomainError::Repository(format!("withdrawal {id} has an inconsistent source"))),
	}
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

async fn insert_row(conn: &mut PgConnection, withdrawal: &Withdrawal) -> Result<(), DomainError> {
	sqlx::query("INSERT INTO withdrawals (id, source, user_id, network, address, amount, fee, state, tx_ref) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
		.bind(withdrawal.id().raw())
		.bind(withdrawal.source().column())
		.bind(withdrawal.user().map(|user| user.raw()))
		.bind(withdrawal.network().as_str())
		.bind(withdrawal.address().as_str())
		.bind(withdrawal.amount().base_units().to_string())
		.bind(withdrawal.fee().base_units().to_string())
		.bind(withdrawal.state().as_str())
		.bind(withdrawal.tx_ref().map(TxRef::as_str))
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	Ok(())
}

/// Persist a state transition (settle/fail) — `state` and `tx_ref` are the only
/// mutable columns. We hold the row lock, so exactly one row must update.
/// Cancel a still-queued withdrawal on the caller's open transaction: load `FOR UPDATE`,
/// apply [`Withdrawal::cancel`] (which refuses anything past `Queued` — a broadcast may have
/// landed), persist, drain the void to the outbox. Crate-visible so the payments adapter can
/// void the withdrawal an L1 execution created in the SAME transaction that refuses that
/// execution over a moved consent pin — the one place two aggregates must move together,
/// because leaving the withdrawal queued would let a revoked consent's money ship anyway.
pub(crate) async fn cancel_on(conn: &mut PgConnection, id: WithdrawalId) -> Result<Withdrawal, DomainError> {
	let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_ID_FOR_UPDATE)
		.bind(id.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
	let mut withdrawal = row
		.ok_or_else(|| DomainError::NotFound {
			entity: "withdrawal",
			id: id.to_string(),
		})?
		.into_domain()?;
	withdrawal.cancel()?;
	update_row(conn, &withdrawal).await?;
	outbox::drain_to_outbox(conn, &mut withdrawal, true).await?;
	Ok(withdrawal)
}

async fn update_row(conn: &mut PgConnection, withdrawal: &Withdrawal) -> Result<(), DomainError> {
	let result = sqlx::query("UPDATE withdrawals SET state = $2, tx_ref = $3, updated_at = now() WHERE id = $1")
		.bind(withdrawal.id().raw())
		.bind(withdrawal.state().as_str())
		.bind(withdrawal.tx_ref().map(TxRef::as_str))
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	if result.rows_affected() != 1 {
		return Err(DomainError::Repository("withdrawal row vanished under lock".into()));
	}
	Ok(())
}

#[async_trait]
impl WithdrawalRepository for PgWithdrawals {
	async fn open(&self, withdrawal: &mut Withdrawal) -> Result<(), DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// Serialize every flow that spends this claim on one shared lock target (see
		// [`outbox::lock_user`]) so two of them can't both pass the optimistic Read-First
		// and both park a reserve (TB's flag is the money backstop; this lock keeps the PG
		// projection from diverging). For a user that's withdraw + subscribe on their
		// unified claim.
		outbox::lock_claim(&mut tx, &withdrawal.source().claim_key()).await?;
		insert_row(&mut tx, withdrawal).await?;
		outbox::drain_to_outbox(&mut tx, withdrawal, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(())
	}

	async fn dispatch(&self, id: WithdrawalId) -> Result<Withdrawal, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_ID_FOR_UPDATE)
			.bind(id.raw())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		let mut withdrawal = row
			.ok_or_else(|| DomainError::NotFound {
				entity: "withdrawal",
				id: id.to_string(),
			})?
			.into_domain()?;
		withdrawal.dispatch()?;
		update_row(&mut tx, &withdrawal).await?;
		outbox::drain_to_outbox(&mut tx, &mut withdrawal, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(withdrawal)
	}

	async fn settle(&self, id: WithdrawalId, tx_ref: TxRef) -> Result<Withdrawal, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_ID_FOR_UPDATE)
			.bind(id.raw())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		let mut withdrawal = row
			.ok_or_else(|| DomainError::NotFound {
				entity: "withdrawal",
				id: id.to_string(),
			})?
			.into_domain()?;
		withdrawal.settle(tx_ref)?;
		update_row(&mut tx, &withdrawal).await?;
		outbox::drain_to_outbox(&mut tx, &mut withdrawal, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(withdrawal)
	}

	async fn fail(&self, id: WithdrawalId) -> Result<Withdrawal, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_ID_FOR_UPDATE)
			.bind(id.raw())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		let mut withdrawal = row
			.ok_or_else(|| DomainError::NotFound {
				entity: "withdrawal",
				id: id.to_string(),
			})?
			.into_domain()?;
		withdrawal.fail()?;
		update_row(&mut tx, &withdrawal).await?;
		outbox::drain_to_outbox(&mut tx, &mut withdrawal, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(withdrawal)
	}

	async fn cancel(&self, id: WithdrawalId) -> Result<Withdrawal, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let withdrawal = cancel_on(&mut tx, id).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(withdrawal)
	}

	async fn find_by_id(&self, id: WithdrawalId) -> Result<Option<Withdrawal>, DomainError> {
		let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_ID)
			.bind(id.raw())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		row.map(WithdrawalRow::into_domain).transpose()
	}

	async fn list_by_user(&self, user: UserId) -> Result<Vec<Withdrawal>, DomainError> {
		let rows = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_USER)
			.bind(user.raw())
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.into_iter().map(WithdrawalRow::into_domain).collect()
	}

	async fn list_actionable(&self) -> Result<Vec<QueuedWithdrawal>, DomainError> {
		let rows = sqlx::query_as::<_, (Uuid, String, Option<Uuid>, Option<String>, String, String, String, String, String, i64)>(
			"SELECT w.id, w.source, w.user_id, u.email, w.network, w.address, w.amount, w.fee, w.state, EXTRACT(EPOCH FROM w.created_at)::BIGINT \
			 FROM withdrawals w LEFT JOIN users u ON u.id = w.user_id \
			 WHERE w.state IN ('queued', 'processing') ORDER BY w.created_at ASC",
		)
		.fetch_all(&self.pool)
		.await
		.map_err(repo_err)?;
		rows.into_iter()
			.map(|(id, source, user_id, email, network, address, amount, fee, state, created_at)| {
				let amount = Usdt::from_base_units(amount.parse::<u128>().map_err(|_| DomainError::Repository("malformed withdrawal amount".into()))?);
				let fee = Usdt::from_base_units(fee.parse::<u128>().map_err(|_| DomainError::Repository("malformed withdrawal fee".into()))?);
				let source = source_of(id, &source, user_id)?;
				Ok(QueuedWithdrawal {
					id: WithdrawalId::from_raw(id),
					source,
					email: email.unwrap_or_default(),
					network: Network::parse(&network)?,
					address,
					amount,
					net_amount: amount.checked_sub(fee).unwrap_or(Usdt::ZERO),
					state,
					created_at,
				})
			})
			.collect()
	}
}
