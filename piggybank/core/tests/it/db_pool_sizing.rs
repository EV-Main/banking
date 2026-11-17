//! Integration test for the DB-pool tuning (BANK-FAULT-07) — real Postgres, no mocks.
//! Runs when `DATABASE_URL` is set (`nix run .#db`) and skips otherwise.
//!
//! Proves the request-serving pool and the relay's pool are sized independently and are
//! distinct instances, so a burst of request traffic and money dispatch can't exhaust
//! each other's connections.

use piggybank_core::infrastructure::db;

use crate::common;

#[tokio::test]
async fn relay_uses_a_distinct_smaller_pool() {
	let Some(url) = common::database_url().await else {
		eprintln!("DATABASE_URL unset — skipping db pool sizing test");
		return;
	};

	let request_pool = db::connect_sized(&url, 20).await.expect("connect request pool");
	let relay_pool = db::connect_sized(&url, 2).await.expect("connect relay pool");

	assert_eq!(request_pool.options().get_max_connections(), 20);
	assert_eq!(relay_pool.options().get_max_connections(), 2);

	// Distinct pools, not clones: drain the relay pool to its cap and the request pool is
	// untouched — request traffic and money dispatch can't exhaust each other's connections.
	let _a = relay_pool.acquire().await.expect("relay conn 1");
	let _b = relay_pool.acquire().await.expect("relay conn 2");
	let req = tokio::time::timeout(std::time::Duration::from_secs(2), request_pool.acquire()).await;
	assert!(req.is_ok_and(|c| c.is_ok()), "the request pool still serves while the relay pool is saturated");

	let default_pool = db::connect(&url).await.expect("connect default pool");
	assert_eq!(default_pool.options().get_max_connections(), 10, "the default convenience pool keeps the sqlx default size");
}
