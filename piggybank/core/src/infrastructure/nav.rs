//! Postgres adapter for the [`NavMarks`] port — append-only fund valuation marks.
//!
//! Amounts/prices are stored as exact integer base-unit strings (the money-plane
//! convention) and parsed back to the typed `Usdt`/`Shares`/`Nav` on read. `posted_at`
//! is DB-stamped; `floor(extract(epoch …))` exposes it as unix seconds for the staleness
//! guard — FLOORED, as `SystemTime::as_secs` and the window bounds below are, so a mark
//! reported at second N is exactly one that `history`'s second-granular window at N
//! includes. A `::bigint` cast alone rounds, and a mark stamped at N.7 then reports N+1
//! while sitting outside a window that ends at N+1.

use async_trait::async_trait;
use domain::{
	balance::{ServiceId, ValuationId},
	error::DomainError,
	money::{Nav, Shares, Usdt},
};
use sqlx::{PgPool, Row};

use crate::ports::nav::{NavMarks, Valuation};

pub struct PgNav {
	pool: PgPool,
}

impl PgNav {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

/// The projection every read here shares. sqlx 0.9 takes only a `&'static str`, so the
/// column list is spliced with `concat!` rather than built at runtime.
macro_rules! valuation_columns {
	() => {
		"service, aum, units_outstanding, nav, posted_by, FLOOR(EXTRACT(EPOCH FROM posted_at))::bigint AS posted_at_unix"
	};
}

#[async_trait]
impl NavMarks for PgNav {
	async fn current(&self, service: &ServiceId) -> Result<Option<Valuation>, DomainError> {
		let row = sqlx::query(concat!(
			"SELECT ",
			valuation_columns!(),
			" FROM fund_valuations WHERE service = $1 ORDER BY posted_at DESC LIMIT 1"
		))
		.bind(service.as_str())
		.fetch_optional(&self.pool)
		.await
		.map_err(repo_err)?;
		row.as_ref().map(valuation_from_row).transpose()
	}

	async fn anchor(&self, service: &ServiceId, at_unix: i64) -> Result<Option<Valuation>, DomainError> {
		// The newest mark old enough to sit outside the window; failing that, the oldest mark
		// there is — a fund younger than the window is measured from its first price, so the
		// window cannot be bootstrapped away by marking a fresh fund up in steps.
		let aged = sqlx::query(concat!(
			"SELECT ",
			valuation_columns!(),
			" FROM fund_valuations WHERE service = $1 AND posted_at <= to_timestamp($2) ORDER BY posted_at DESC LIMIT 1"
		))
		.bind(service.as_str())
		.bind(at_unix as f64)
		.fetch_optional(&self.pool)
		.await
		.map_err(repo_err)?;
		let row = match aged {
			Some(row) => Some(row),
			None => sqlx::query(concat!(
				"SELECT ",
				valuation_columns!(),
				" FROM fund_valuations WHERE service = $1 ORDER BY posted_at ASC LIMIT 1"
			))
			.bind(service.as_str())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?,
		};
		row.as_ref().map(valuation_from_row).transpose()
	}

	async fn posted_by_since(&self, service: &ServiceId, subject: &str, since_unix: i64) -> Result<bool, DomainError> {
		sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM fund_valuations WHERE service = $1 AND posted_by = $2 AND posted_at > to_timestamp($3))")
			.bind(service.as_str())
			.bind(subject)
			.bind(since_unix as f64)
			.fetch_one(&self.pool)
			.await
			.map_err(repo_err)
	}

	async fn history(&self, service: &ServiceId, from_unix: i64, to_unix: i64, limit: usize) -> Result<Vec<Valuation>, DomainError> {
		// Newest-first under the LIMIT so the cap drops the oldest marks, then flipped: the
		// `(service, posted_at DESC)` index serves this order without a sort. The upper
		// bound admits the whole of second `to` (`< to + 1`), matching the floored seconds
		// the rows report, so the window is inclusive at the granularity the caller speaks.
		let rows = sqlx::query(concat!(
			"SELECT ",
			valuation_columns!(),
			" FROM fund_valuations WHERE service = $1 AND posted_at >= to_timestamp($2) AND posted_at < to_timestamp($3) ORDER BY posted_at DESC LIMIT $4"
		))
		.bind(service.as_str())
		.bind(from_unix as f64)
		.bind(to_unix.saturating_add(1) as f64)
		.bind(i64::try_from(limit).unwrap_or(i64::MAX))
		.fetch_all(&self.pool)
		.await
		.map_err(repo_err)?;
		let mut marks = rows.iter().map(valuation_from_row).collect::<Result<Vec<_>, _>>()?;
		marks.reverse();
		Ok(marks)
	}

	async fn record(&self, id: ValuationId, service: &ServiceId, aum: Usdt, units_outstanding: Shares, nav: Nav, posted_by: &str) -> Result<i64, DomainError> {
		let posted_at_unix = sqlx::query_scalar::<_, i64>(
			"INSERT INTO fund_valuations (id, service, aum, units_outstanding, nav, posted_by) \
			 VALUES ($1, $2, $3, $4, $5, $6) RETURNING FLOOR(EXTRACT(EPOCH FROM posted_at))::bigint",
		)
		.bind(id.raw())
		.bind(service.as_str())
		.bind(aum.base_units().to_string())
		.bind(units_outstanding.base_units().to_string())
		.bind(nav.base_units().to_string())
		.bind(posted_by)
		.fetch_one(&self.pool)
		.await
		.map_err(repo_err)?;
		Ok(posted_at_unix)
	}
}

fn valuation_from_row(row: &sqlx::postgres::PgRow) -> Result<Valuation, DomainError> {
	Ok(Valuation {
		service: ServiceId::parse(row.try_get::<String, _>("service").map_err(repo_err)?.as_str())?,
		aum: Usdt::from_base_units(parse_base_units(row.try_get("aum").map_err(repo_err)?)?),
		units_outstanding: Shares::from_base_units(parse_base_units(row.try_get("units_outstanding").map_err(repo_err)?)?),
		nav: Nav::from_base_units(parse_base_units(row.try_get("nav").map_err(repo_err)?)?),
		posted_by: row.try_get("posted_by").map_err(repo_err)?,
		posted_at_unix: row.try_get("posted_at_unix").map_err(repo_err)?,
	})
}

fn parse_base_units(raw: String) -> Result<u128, DomainError> {
	raw.parse::<u128>().map_err(|_| DomainError::Repository("malformed base-unit amount in fund_valuations".into()))
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}
