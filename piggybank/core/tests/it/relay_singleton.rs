//! Integration test for the relay's singleton invariant (BANK-FAULT-03/06) — real
//! Postgres **and** TigerBeetle (no mocks, per the project rules). Runs when
//! `DATABASE_URL` is set and a TigerBeetle replica is reachable (`nix run .#db` +
//! `.#tb`), and skips otherwise.
//!
//! The relay's whole ordering/atomicity argument (strict `seq`, reserve-before-complete,
//! the settle-time liquidity pre-check) is valid only with exactly one drainer. This
//! proves that invariant is now *enforced* by a fixed-key session advisory lock: a second
//! Relay against the same Postgres cannot take the lock while the first holds it, so it
//! would block in `run()` rather than concurrently draining the same outbox.

use std::sync::Arc;

use domain::{
	balance::{LedgerAccountKey, Party},
	money::{Network, TxRef, Usdt},
	users::UserId,
};
use piggybank_core::{
	application::balance as balance_app,
	infrastructure::{
		custody::StubCustody,
		deposits::PgDeposits,
		relay::{DrainStep, Relay},
	},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::common;

use crate::common::ledger_for;

struct Harness {
	pool: PgPool,
	deposits: PgDeposits,
	notify: Arc<Notify>,
}
impl Harness {
	async fn balance(&self, key: &LedgerAccountKey) -> u128 {
		ledger_for(&self.pool).balance(key).await.unwrap().posted
	}
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	common::seeded_ledger(&pool, "relay singleton test").await?;
	Some(Harness {
		deposits: PgDeposits::new(pool.clone()),
		pool,
		notify: Arc::new(Notify::new()),
	})
}

fn relay(h: &Harness) -> Relay {
	Relay::new(h.pool.clone(), ledger_for(&h.pool), Arc::new(StubCustody), h.notify.clone())
}

/// Two relays, one Postgres: the first takes the outbox lock, the second cannot — so a
/// rolling deploy / extra replica blocks on the lock instead of double-draining. After
/// the holder's connection closes (as on process exit), the standby takes over. The lock
/// holder can still drain a committed event while it owns the lock.
#[tokio::test]
async fn only_one_relay_drains_under_the_outbox_lock() {
	let _relay_owner = common::outbox_serial().await;
	let Some(h) = harness().await else { return };
	let primary = relay(&h);
	let standby = relay(&h);

	// The primary takes the singleton lock (its held connection *is* the lock).
	let lock = primary.acquire_outbox_lock().await.expect("primary takes the outbox lock");

	// A second instance cannot take it — it would block in `run()` and never drain.
	let contender = standby.try_acquire_outbox_lock().await.expect("query the lock");
	assert!(contender.is_none(), "a second relay must not acquire the outbox lock while one is held");

	// The holder still drains: commit a deposit and prove the primary moves it in TB.
	let user = UserId::new();
	let party = Party::User(user);
	let claim = party.claim_key();
	let before = h.balance(&claim).await;
	balance_app::record_deposit(&h.deposits, &h.notify, tx_ref(), user, Network::Bep20, usdt("250")).await.unwrap();
	primary.drain().await;
	let after = h.balance(&claim).await;
	assert_eq!(after.saturating_sub(before), usdt("250").base_units(), "the lock holder applied the committed deposit");

	// Close the holding connection — as a process exit does in production — releasing the
	// session lock so a standby can take over without coordination.
	lock.close().await.expect("close the lock-holding connection");
	let taken = standby.try_acquire_outbox_lock().await.expect("query the lock");
	assert!(taken.is_some(), "once the holder's connection closes, a standby acquires the lock");
	// `close()`, not drop: dropping returns the connection (lock still held) to the pool,
	// which would wedge the re-acquisition below.
	taken.unwrap().close().await.expect("close the standby's lock connection");

	// The mid-drain fence (issue #38). The lock used to be validated only *between*
	// drains: an idle-session reaper killing the lock backend mid-catch-up left a
	// lockless relay draining while a standby rightfully took over — two concurrent
	// drainers, the on-chain double-send window. `drain_batch` now probes the lock
	// connection before every row, so a reaped backend stops the drain before the next
	// row applies.
	let mut lock = primary.acquire_outbox_lock().await.expect("primary re-takes the outbox lock");
	let lock_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(lock.as_mut()).await.expect("read the lock backend pid");

	let user = UserId::new();
	let party = Party::User(user);
	let claim = party.claim_key();
	let before = h.balance(&claim).await;
	balance_app::record_deposit(&h.deposits, &h.notify, tx_ref(), user, Network::Bep20, usdt("77")).await.unwrap();

	sqlx::query("SELECT pg_terminate_backend($1)")
		.bind(lock_pid)
		.execute(&h.pool)
		.await
		.expect("reap the lock backend");
	while sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)")
		.bind(lock_pid)
		.fetch_one(&h.pool)
		.await
		.expect("poll for the reaped backend")
	{
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
	}

	let step = primary.drain_batch(Some(&mut lock)).await;
	assert!(matches!(step, DrainStep::LostLock), "a dead lock backend must stop the drain, not keep applying");
	assert_eq!(h.balance(&claim).await, before, "no event may apply once the lock is lost");

	// The reaped session released the lock — the standby takes over and applies the
	// event exactly once.
	let standby_lock = standby.try_acquire_outbox_lock().await.expect("query the lock");
	assert!(standby_lock.is_some(), "the reaped session released the lock for a standby");
	standby.drain().await;
	assert_eq!(
		h.balance(&claim).await.saturating_sub(before),
		usdt("77").base_units(),
		"the standby applied the deposit exactly once"
	);
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn tx_ref() -> TxRef {
	TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap()
}
