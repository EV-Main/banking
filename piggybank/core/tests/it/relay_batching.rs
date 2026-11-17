//! Integration test for the relay's batch-bounded drain (issue #38) — real Postgres
//! **and** TigerBeetle (no mocks, per the project rules). Runs when `DATABASE_URL` is
//! set and a TigerBeetle replica is reachable (`nix run .#db` + `.#tb`), and skips
//! otherwise.
//!
//! `drain()` now loops the per-batch `drain_batch` (which `run()` fences on the lock
//! connection between rows); this proves the loop still exhausts a backlog larger than
//! one batch (128 rows) — a regression here would silently stall a catch-up drain after
//! the first batch.

use std::sync::Arc;

use domain::{
	balance::{LedgerAccountKey, Party},
	money::{Network, TxRef, Usdt},
	users::UserId,
};
use piggybank_core::{
	application::balance as balance_app,
	infrastructure::{custody::StubCustody, deposits::PgDeposits, relay::Relay},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{common, common::ledger_for};

struct Harness {
	pool: PgPool,
	deposits: PgDeposits,
	notify: Arc<Notify>,
	_serial: tokio::sync::MutexGuard<'static, ()>,
}
impl Harness {
	async fn balance(&self, key: &LedgerAccountKey) -> u128 {
		ledger_for(&self.pool).balance(key).await.unwrap().posted
	}
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	common::seeded_ledger(&pool, "relay batching test").await?;
	Some(Harness {
		deposits: PgDeposits::new(pool.clone()),
		pool,
		notify: Arc::new(Notify::new()),
		_serial: serial,
	})
}

/// A backlog wider than one batch (129 > 128 rows) drains to exhaustion in a single
/// `drain()` call — the multi-batch continuation the batch-bounded refactor introduced.
#[tokio::test]
async fn drain_exhausts_a_backlog_wider_than_one_batch() {
	let Some(h) = harness().await else { return };
	let relay = Relay::new(h.pool.clone(), ledger_for(&h.pool), Arc::new(StubCustody), h.notify.clone());

	let user = UserId::new();
	let party = Party::User(user);
	let claim = party.claim_key();
	let before = h.balance(&claim).await;
	for _ in 0..129 {
		balance_app::record_deposit(&h.deposits, &h.notify, tx_ref(), user, Network::Bep20, usdt("1")).await.unwrap();
	}

	let throttle = relay.drain().await;

	assert!(!throttle, "a healthy multi-batch drain must not report a transient failure");
	assert_eq!(h.balance(&claim).await.saturating_sub(before), usdt("129").base_units(), "every row past the first batch applied");
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn tx_ref() -> TxRef {
	TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap()
}
