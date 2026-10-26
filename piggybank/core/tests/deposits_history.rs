//! Deposits history — `PgDeposits::list_by_user` reads the caller's credited on-chain
//! deposits back from the idempotency-gate rows. Real Postgres (no DB mocks) — runs
//! when `DATABASE_URL` is set and skips otherwise. Each test uses a fresh `user_id`,
//! so runs are isolated on shared infra.
//!
//! What's proven: only the caller's `party_kind='user'` rows are listed (another
//! user's deposits are excluded), newest first, with amounts
//! round-tripping through `Usdt`; an idempotent duplicate `record` does not duplicate
//! the listing.

use domain::{
	money::{Network, TxRef, Usdt},
	users::UserId,
};
use piggybank_core::{infrastructure::deposits::PgDeposits, ports::Deposits};
use uuid::Uuid;

mod common;
use common::pool;

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn unique_tx_ref() -> TxRef {
	TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap()
}

#[tokio::test]
async fn list_by_user_returns_only_the_users_deposits_newest_first() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping deposits-history test");
		return;
	};
	let deposits = PgDeposits::new(pool.clone());
	let user = UserId::new();
	let other = UserId::new();

	let older = unique_tx_ref();
	let newer = unique_tx_ref();
	assert!(deposits.record(older.clone(), user, Network::Bep20, usdt("125.5")).await.expect("record older"));
	// `created_at` defaults to the insert's transaction time; push the first row back a
	// minute so "newest first" is deterministic even on a fast machine.
	sqlx::query("UPDATE deposits SET created_at = created_at - interval '1 minute' WHERE tx_ref = $1")
		.bind(older.as_str())
		.execute(&pool)
		.await
		.expect("age the older row");
	assert!(deposits.record(newer.clone(), user, Network::Ton, usdt("7.25")).await.expect("record newer"));

	// Another user's deposit may not appear in this user's history.
	assert!(deposits.record(unique_tx_ref(), other, Network::Bep20, usdt("3")).await.expect("record other user"));

	let listed = deposits.list_by_user(user).await.expect("list");
	assert_eq!(listed.len(), 2, "only the user's own deposits are listed");
	assert_eq!(listed[0].tx_ref.as_str(), newer.as_str(), "newest first");
	assert_eq!(listed[0].network, Network::Ton);
	assert_eq!(listed[0].amount, usdt("7.25"), "the amount round-trips through Usdt");
	assert_eq!(listed[1].tx_ref.as_str(), older.as_str());
	assert_eq!(listed[1].amount, usdt("125.5"));
	assert!(listed.iter().all(|d| d.created_at > 0), "created_at is unix seconds");
	assert!(listed[0].created_at >= listed[1].created_at, "ordering matches the timestamps");

	// The idempotency gate: a duplicate record is refused (`false`) and the listing is
	// unchanged — the credit history can never double-count a chain tx.
	assert!(!deposits.record(newer, user, Network::Ton, usdt("7.25")).await.expect("duplicate record"));
	assert_eq!(
		deposits.list_by_user(user).await.expect("list again").len(),
		2,
		"a duplicate record does not duplicate the listing"
	);
}
