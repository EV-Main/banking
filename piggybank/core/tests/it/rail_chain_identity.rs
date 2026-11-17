//! Integration test for the EVM rail chain-identity boot guard — real Postgres, no mocks.
//! Runs when `DATABASE_URL` is set (`nix run .#db`) and skips otherwise.
//!
//! Proves `db::bind_chain_identity` binds a rail's chain on first boot (trust-on-first-use),
//! accepts a matching re-bind (an RPC-provider swap must not trip it), refuses a mismatched
//! re-bind with a message naming both chains, never mutates the persisted row on refusal, and
//! is race-safe for two replicas booting the same config.
//!
//! Keyed by a fixed 4-variant network enum, so there is no random-id isolation lane: the test
//! uses the frozen `Trc20`/`Ton` rows (no hub instance binds them), clears them first, and keeps
//! every assertion in one test so cargo's intra-file parallelism can't race the shared rows.

use domain::money::Network;
use piggybank_core::infrastructure::db;

use crate::common;

#[tokio::test]
async fn chain_identity_binds_once_and_refuses_a_chain_flip() {
	let Some(url) = common::database_url().await else {
		eprintln!("DATABASE_URL unset — skipping rail chain-identity test");
		return;
	};
	eprintln!("rail_chain_identity: running against {url}");
	let pool = db::connect(&url).await.expect("connect Postgres");
	db::migrate(&pool).await.expect("apply migrations");

	// Fresh slate: an unbound rail re-adopts on the next bind, which is the table's design.
	for net in [Network::Trc20, Network::Ton] {
		sqlx::query("DELETE FROM rail_chain_identity WHERE network = $1")
			.bind(net.as_str())
			.execute(&pool)
			.await
			.expect("clear test rows");
	}

	let tron = Network::Trc20; // Tron mainnet id, used purely as an unclaimed key here.
	let mainnet = 728_126_428;
	let shasta = 2_494_104_990;

	// (1) First bind adopts.
	db::bind_chain_identity(&pool, tron, mainnet).await.expect("first bind adopts");
	let row: i64 = sqlx::query_scalar("SELECT chain_id FROM rail_chain_identity WHERE network = $1")
		.bind(tron.as_str())
		.fetch_one(&pool)
		.await
		.expect("row exists");
	assert_eq!(row, mainnet as i64);

	// (2) Same chain again is fine — a provider swap keeps the chain id, so it must not trip.
	db::bind_chain_identity(&pool, tron, mainnet).await.expect("idempotent re-bind");

	// (3) A different chain refuses, and the message names both chains (an operator acts on it).
	let err = db::bind_chain_identity(&pool, tron, shasta).await.unwrap_err().to_string();
	assert!(err.contains(&mainnet.to_string()), "names the bound chain: {err}");
	assert!(err.contains(&shasta.to_string()), "names the configured chain: {err}");

	// (4) Refusal is non-destructive: the persisted identity is untouched, so restoring the
	// original config boots clean. This is the difference between a guard and a rubber stamp.
	let still: i64 = sqlx::query_scalar("SELECT chain_id FROM rail_chain_identity WHERE network = $1")
		.bind(tron.as_str())
		.fetch_one(&pool)
		.await
		.expect("row still there");
	assert_eq!(still, mainnet as i64, "a refused bind must not overwrite the identity");
	db::bind_chain_identity(&pool, tron, mainnet).await.expect("original config still boots");

	// (5) Concurrent first-bind race on a fresh row: both succeed, exactly one row.
	let ton = Network::Ton;
	let (a, b) = tokio::join!(db::bind_chain_identity(&pool, ton, 1), db::bind_chain_identity(&pool, ton, 1));
	a.expect("racer A");
	b.expect("racer B");
	let count: i64 = sqlx::query_scalar("SELECT count(*) FROM rail_chain_identity WHERE network = $1")
		.bind(ton.as_str())
		.fetch_one(&pool)
		.await
		.expect("count");
	assert_eq!(count, 1, "the ON CONFLICT DO NOTHING + read-back yields exactly one row");
}
