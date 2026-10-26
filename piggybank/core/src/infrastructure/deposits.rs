//! Postgres adapter for the [`Deposits`] port — the aggregate-less on-chain arrival
//! facts (a user's deposit, a seed booked to its depositor) and their outbox events.
//!
//! The write opens one transaction (the ACID point): the `deposits` gate row and the
//! outbox event commit together or not at all, so the relay can never move money for
//! an unrecorded fact — nor record a fact whose event was lost.

use async_trait::async_trait;
use domain::{
	architecture::DomainEvent,
	balance::{LedgerEvent, Party},
	error::DomainError,
	money::{Network, TxRef, Usdt},
	users::UserId,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
	infrastructure::outbox,
	ports::{Deposits, deposits::DepositRecord},
};

pub struct PgDeposits {
	pool: PgPool,
}

impl PgDeposits {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

#[async_trait]
impl Deposits for PgDeposits {
	async fn record(&self, tx_ref: TxRef, user: UserId, network: Network, amount: Usdt) -> Result<bool, DomainError> {
		let party = Party::User(user);
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let event_id = Uuid::new_v4();
		let inserted = sqlx::query_scalar::<_, String>(
			"INSERT INTO deposits (tx_ref, party_kind, party_id, network, amount, event_id) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (tx_ref) DO NOTHING RETURNING tx_ref",
		)
		.bind(tx_ref.as_str())
		.bind(party.kind_str())
		.bind(party.id_str())
		.bind(network.as_str())
		.bind(amount.base_units().to_string())
		.bind(event_id)
		.fetch_optional(&mut *tx)
		.await
		.map_err(repo_err)?;
		if inserted.is_none() {
			// Already recorded — drop the tx (no-op) and report idempotent success.
			return Ok(false);
		}
		let deposit_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, tx_ref.as_str().as_bytes());
		let payload = serde_json::to_string(&LedgerEvent::Deposited { party, network, amount }).map_err(|e| DomainError::Repository(e.to_string()))?;
		outbox::insert_event(&mut tx, event_id, "deposit", deposit_id, LedgerEvent::KIND, &payload, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(true)
	}

	async fn list_by_user(&self, user: UserId) -> Result<Vec<DepositRecord>, DomainError> {
		// A plain pool read — a projection, not a fact write, so no transaction. The
		// LIMIT is defensive (there is no paging API yet): 200 confirmed deposits per
		// user comfortably exceeds any realistic history at current scale.
		let rows = sqlx::query_as::<_, (String, String, String, i64)>(
			"SELECT tx_ref, network, amount, EXTRACT(EPOCH FROM created_at)::bigint FROM deposits WHERE party_kind = 'user' AND party_id = $1 ORDER BY created_at DESC LIMIT 200",
		)
		.bind(user.to_string())
		.fetch_all(&self.pool)
		.await
		.map_err(repo_err)?;
		rows.into_iter()
			.map(|(tx_ref, network, amount, created_at)| {
				Ok(DepositRecord {
					tx_ref: TxRef::parse(&tx_ref)?,
					network: Network::parse(&network)?,
					amount: Usdt::from_base_units(amount.parse::<u128>().map_err(|_| DomainError::Repository("malformed deposit amount".into()))?),
					created_at,
				})
			})
			.collect()
	}

	/// Character-for-character the sweeper's own predicate (see
	/// [`rails::deposit_addresses`](crate::infrastructure::rails)), narrowed from "every
	/// address on the rail" to this one user. Keeping the two identical is the point: the
	/// address a migration may retire is exactly the address the sweeper has stopped
	/// scanning.
	async fn has_unswept(&self, user: UserId, network: Network) -> Result<bool, DomainError> {
		let unswept: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM deposits WHERE party_kind = 'user' AND party_id = $1 AND network = $2 AND swept_at IS NULL)")
			.bind(user.to_string())
			.bind(network.as_str())
			.fetch_one(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(unswept)
	}
}
