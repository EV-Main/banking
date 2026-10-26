//! Session-scoped `pg_advisory_lock` — the "exactly one of me is running" primitive.
//!
//! The outbox relay has held one of these since it was written, because multi-replica is an
//! anticipated shape of this deployment rather than a hypothetical: a second replica draining
//! the same queue is not a slowdown, it is two workers racing the same rows. Other
//! workers drain shared queues too and had no such guard, so this lifts the relay's pattern
//! into one place every such worker uses.
//!
//! The lock's lifetime IS the returned connection's lifetime. Postgres releases a
//! session-level advisory lock when the session ends, so a crashed or reaped holder frees it
//! for a standby with no lease to expire and no tombstone to clean up. Dropping the
//! connection returns it to the pool still holding the lock — which is why the callers here
//! `close()` rather than drop when they mean to release it.

use sqlx::{PgPool, Postgres, pool::PoolConnection};

/// `pg_advisory_lock` keys. Arbitrary but STABLE constants: only stability matters, and
/// changing one would let two cohorts run concurrently across a deploy that mixed old and
/// new keys. The ASCII spelling is a debugging affordance — these show up in `pg_locks`.
pub mod keys {
	/// The payments sweeper.
	pub const PAYMENTS_SWEEPER: i64 = 0x4556_424b_4353_575f_u64 as i64; // `EVBKCSW_`, the key of the sweeper this replaced, so an older pod running that one excludes this one

	/// ASCII `EVBKSCN_` — the scope-inbox notification worker.
	pub const SCOPE_NOTIFICATIONS: i64 = 0x4556_424b_5343_4e5f_u64 as i64;
	/// ASCII `EVBKSCE_` — the consumer of concierge's scope stream.
	pub const SCOPE_EVENTS: i64 = 0x4556_424b_5343_455f_u64 as i64;
}

/// Block on a dedicated pooled connection until the lock is held, then return that
/// connection. The first instance returns immediately; any other blocks inside Postgres
/// until the holder's session ends.
pub async fn acquire(pool: &PgPool, key: i64) -> Result<PoolConnection<Postgres>, sqlx::Error> {
	let mut conn = pool.acquire().await?;
	sqlx::query("SELECT pg_advisory_lock($1)").bind(key).execute(conn.as_mut()).await?;
	Ok(conn)
}

/// Non-blocking sibling of [`acquire`]: take the lock if it is free, else `None`. Used by
/// tests to observe that a held lock makes a second worker back off rather than block.
pub async fn try_acquire(pool: &PgPool, key: i64) -> Result<Option<PoolConnection<Postgres>>, sqlx::Error> {
	let mut conn = pool.acquire().await?;
	let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)").bind(key).fetch_one(conn.as_mut()).await?;
	Ok(acquired.then_some(conn))
}

/// Whether the lock-holding session is still alive. A reaped backend releases the lock
/// silently, so a long-running worker re-checks rather than assuming it still holds it.
pub async fn still_held(conn: &mut PoolConnection<Postgres>) -> bool {
	sqlx::query("SELECT 1").execute(conn.as_mut()).await.is_ok()
}

#[cfg(test)]
mod tests {
	#[test]
	fn the_payments_sweeper_holds_the_v2_20_consilium_sweeper_key() {
		assert_eq!(super::keys::PAYMENTS_SWEEPER.to_be_bytes(), *b"EVBKCSW_");
	}
}
