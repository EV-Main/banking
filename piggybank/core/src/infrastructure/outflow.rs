//! Postgres adapter for the [`OutflowPolicy`] port — the control-plane facts every
//! payout path clears before money leaves.
//!
//! It owns a pool clone (an `Arc` under the hood) rather than a borrow, so one instance can
//! sit in [`AppState`](crate::AppState) beside the other adapters and be lent to the
//! withdrawal, payment and consilium use cases alike — the pause is one fact, read through
//! one handle, and a use case that could not borrow it would have to be trusted to remember
//! it. It is built INSIDE `AppState::new` from the pool already passed there rather than
//! threaded through as a twenty-sixth constructor argument.

use async_trait::async_trait;
use domain::{error::DomainError, money::Network, users::UserId};
use sqlx::{PgExecutor, PgPool};

use crate::{
	infrastructure::operations,
	ports::outflow::{OutflowPolicy, PayoutStanding},
};

pub struct PgOutflowPolicy {
	pool: PgPool,
}

impl PgOutflowPolicy {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

#[async_trait]
impl OutflowPolicy for PgOutflowPolicy {
	async fn outflows_paused(&self) -> Result<bool, DomainError> {
		operations::is_read_only(&self.pool).await.map_err(|err| DomainError::Repository(err.to_string()))
	}

	async fn standing(&self, user: UserId) -> Result<Option<PayoutStanding>, DomainError> {
		standing(&self.pool, user).await
	}

	async fn frozen_rails(&self) -> Result<Vec<Network>, DomainError> {
		operations::frozen_rails(&self.pool).await.map_err(|err| DomainError::Repository(err.to_string()))
	}
}

/// [`OutflowPolicy::standing`] on any executor — a repository asks it on the connection it
/// holds a claim's lock on, never on a second one from the pool.
pub async fn standing<'c>(conn: impl PgExecutor<'c>, user: UserId) -> Result<Option<PayoutStanding>, DomainError> {
	let row: Option<(bool, i32)> = sqlx::query_as("SELECT (frozen OR status = 'disabled'), kyc_level FROM users WHERE id = $1")
		.bind(user.raw())
		.fetch_optional(conn)
		.await
		.map_err(|err| DomainError::Repository(err.to_string()))?;
	Ok(row.map(|(blocked, kyc_level)| PayoutStanding {
		blocked,
		// The column is a signed `int4` the bridge mirrors from the identity plane. A
		// negative (or otherwise impossible) tier is corrupt data, not a verification —
		// degrade it to 0 so the caller's floor refuses.
		kyc_level: u32::try_from(kyc_level).unwrap_or(0),
	}))
}
