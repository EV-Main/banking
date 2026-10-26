//! Money-plane operations mode — the global read-only kill-switch.
//!
//! A single-row config flag (`operations_mode`) toggled by an admin RPC and checked at
//! every user money mutation (see [`support::unfrozen_caller`](crate::services::support)).
//! When `read_only` is on, withdrawals, moves and payments are refused — "pause deposits &
//! withdrawals". Plain config, not a domain aggregate, so it lives in the adapter layer.

use domain::money::Network;
use sqlx::PgPool;

/// Whether the money plane is in read-only mode. A missing row (should never happen —
/// migration seeds it) reads as NOT read-only, matching the default.
pub async fn is_read_only(pool: &PgPool) -> Result<bool, sqlx::Error> {
	let value: Option<bool> = sqlx::query_scalar("SELECT read_only FROM operations_mode WHERE id = TRUE").fetch_optional(pool).await?;
	Ok(value.unwrap_or(false))
}

/// Set read-only mode. Returns the value now in effect, read back via `RETURNING` so a
/// missing singleton row surfaces as an error rather than a silent success.
pub async fn set_read_only(pool: &PgPool, read_only: bool) -> Result<bool, sqlx::Error> {
	let now: bool = sqlx::query_scalar("UPDATE operations_mode SET read_only = $1, updated_at = now() WHERE id = TRUE RETURNING read_only")
		.bind(read_only)
		.fetch_one(pool)
		.await?;
	Ok(now)
}

/// Rails an operator froze from the admin screen.
pub async fn frozen_rails(pool: &PgPool) -> Result<Vec<Network>, sqlx::Error> {
	let rows: Vec<String> = sqlx::query_scalar("SELECT network FROM frozen_rails").fetch_all(pool).await?;
	Ok(rows
		.iter()
		.map(|raw| Network::parse(raw).expect("frozen_rails.network is CHECK-constrained to the Network wire forms"))
		.collect())
}

pub async fn set_rail_frozen(pool: &PgPool, network: Network, frozen: bool) -> Result<(), sqlx::Error> {
	let query = if frozen {
		"INSERT INTO frozen_rails (network) VALUES ($1) ON CONFLICT DO NOTHING"
	} else {
		"DELETE FROM frozen_rails WHERE network = $1"
	};
	sqlx::query(query).bind(network.as_str()).execute(pool).await?;
	Ok(())
}
