//! Postgres adapter for the [`SubscriptionRepository`] port.
//!
//! `open` takes the shared per-user claim lock, writes the immutable subscription row, and
//! drains its `Subscribed` event to the outbox — all in one transaction. The per-investor
//! `fund_positions` cost-basis projection is **not** written here: the relay applies it only
//! after the cash leg posts (see [`super::relay::project_subscription`]), so a parked cash
//! leg can never strand a phantom position (basis without units or cash). Cost basis is the
//! average-cost net cash in, used for P&L; the high-water mark is reserved for a future
//! performance fee. The relay posts the cash move and the unit mint, then the projection.

use async_trait::async_trait;
use domain::{
	architecture::Repository,
	balance::ServiceId,
	error::DomainError,
	money::{Nav, Shares, Usdt},
	subscriptions::{Subscription, SubscriptionId},
	users::UserId,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::{claims::TxReads, fees::parse_units, outbox},
	ports::{SubscriptionRepository, ledger::SpendGuard},
};

pub struct PgSubscriptions {
	pool: PgPool,
}

impl PgSubscriptions {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

impl Repository for PgSubscriptions {
	type Aggregate = Subscription;
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

#[derive(sqlx::FromRow)]
struct SubscriptionRow {
	id: Uuid,
	user_id: Uuid,
	service: String,
	cash: String,
	nav: String,
	units: String,
}

impl SubscriptionRow {
	fn into_domain(self) -> Result<Subscription, DomainError> {
		Ok(Subscription::rehydrate(
			SubscriptionId::from_raw(self.id),
			UserId::from_raw(self.user_id),
			ServiceId::parse(&self.service)?,
			Usdt::from_base_units(parse_units(&self.cash, "subscription cash")?),
			Nav::from_base_units(parse_units(&self.nav, "subscription nav")?),
			Shares::from_base_units(parse_units(&self.units, "subscription units")?),
		))
	}
}

async fn insert_row(conn: &mut PgConnection, subscription: &Subscription) -> Result<(), DomainError> {
	sqlx::query("INSERT INTO subscriptions (id, user_id, service, cash, nav, units) VALUES ($1, $2, $3, $4, $5, $6)")
		.bind(subscription.id().raw())
		.bind(subscription.user().raw())
		.bind(subscription.service().as_str())
		.bind(subscription.cash().base_units().to_string())
		.bind(subscription.nav().base_units().to_string())
		.bind(subscription.units().base_units().to_string())
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	Ok(())
}

#[async_trait]
impl SubscriptionRepository for PgSubscriptions {
	async fn open(&self, subscription: &mut Subscription, guard: &dyn SpendGuard) -> Result<(), DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// Same shared per-user lock the withdrawal path takes — both spend this user's unified
		// claim, so they must serialize on one target (see [`outbox::lock_user`]).
		outbox::lock_user(&mut tx, subscription.user().raw()).await?;
		guard.check(&mut TxReads::new(&mut tx)).await?;
		insert_row(&mut tx, subscription).await?;
		outbox::drain_to_outbox(&mut tx, subscription, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(())
	}

	async fn find_by_id(&self, id: SubscriptionId) -> Result<Option<Subscription>, DomainError> {
		let row = sqlx::query_as::<_, SubscriptionRow>("SELECT id, user_id, service, cash, nav, units FROM subscriptions WHERE id = $1")
			.bind(id.raw())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		row.map(SubscriptionRow::into_domain).transpose()
	}
}
