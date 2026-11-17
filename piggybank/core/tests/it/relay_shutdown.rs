//! Integration test for the relay's graceful shutdown (BANK-FAULT-02) — real Postgres
//! **and** TigerBeetle (no mocks, per the project rules). Runs when `DATABASE_URL` is set
//! and a TigerBeetle replica is reachable (`nix run .#db` + `.#tb`), and skips otherwise.
//!
//! Before this fix `Relay::run` looped forever and the composition root's `select!` simply
//! aborted it on shutdown — a deploy/restart could tear the relay down mid-`dispatch`. It
//! now takes a `CancellationToken` and observes it only at the wait point *between* drains,
//! so the current iteration always finishes (already crash-safe between rows) and the
//! process winds down cleanly instead of being killed at an arbitrary `await`.
//!
//! This lives in its own test binary (own process) so its long-lived relay's session
//! advisory lock never overlaps the singleton test's lock assertions on a shared DB.
//! The database and the outbox are still shared with every other suite, so the test
//! drains whatever they left behind BEFORE it starts timing, and gives the relay far
//! longer than one drain takes — see `run_finishes_its_drain_then_stops_on_cancellation`.

use std::{sync::Arc, time::Duration};

use domain::{
	balance::Party,
	money::{Network, TxRef, Usdt},
	users::UserId,
};
use piggybank_core::{
	application::balance as balance_app,
	infrastructure::{custody::StubCustody, deposits::PgDeposits, relay::Relay},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::common;

use crate::common::ledger_for;

struct Harness {
	pool: PgPool,
	deposits: PgDeposits,
	notify: Arc<Notify>,
}
impl Harness {
	async fn claim_balance(&self, party: &Party) -> u128 {
		ledger_for(&self.pool).balance(&party.claim_key()).await.unwrap().posted
	}
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	common::seeded_ledger(&pool, "relay shutdown test").await?;
	Some(Harness {
		deposits: PgDeposits::new(pool.clone()),
		pool,
		notify: Arc::new(Notify::new()),
	})
}

/// A running `Relay::run` finishes its current drain iteration and then exits cleanly when
/// its `CancellationToken` is cancelled — it is never torn down mid-`drain`, and it does not
/// loop forever ignoring the token. Run `run` concurrently (structured `join!`, no detached
/// spawn) with a driver that commits a deposit, waits until the relay has applied it (the
/// iteration completed), then cancels the token. `join!` only returns once `run` has itself
/// observed the cancellation and exited; the outer timeout proves it does not loop forever.
///
/// The outbox is shared with every suite that ran before this binary, and `run` applies it
/// in strict `seq` order: the deposit below is applied only after everything a sibling left
/// undrained — including a retryable row, which costs `run` a 2 s back-off per attempt. So
/// the backlog is drained first, unfenced like every other test does it, and the deadlines
/// are set for a relay that has to re-acquire a lock or ride out a throttle, not for the
/// milliseconds a clean drain takes.
#[tokio::test]
async fn run_finishes_its_drain_then_stops_on_cancellation() {
	let _relay_owner = common::outbox_serial().await;
	let Some(h) = harness().await else { return };
	let relay = Relay::new(h.pool.clone(), ledger_for(&h.pool), Arc::new(StubCustody), h.notify.clone());
	drain_the_shared_backlog(&relay).await;

	let user = UserId::new();
	let party = Party::User(user);
	let before = h.claim_balance(&party).await;
	balance_app::record_deposit(&h.deposits, &h.notify, tx_ref(), user, Network::Bep20, usdt("125")).await.unwrap();
	let expected = before.saturating_add(usdt("125").base_units());

	let shutdown = CancellationToken::new();
	let driver = drive_until_applied_then_cancel(&h, &party, expected, shutdown.clone());

	// `join!` returns only after `run` exits; the timeout guards against a regression that
	// ignored the token and looped forever (the driver always cancels, so a correct run ends).
	// 90 s: the driver's own deadline plus a lock re-acquisition back-off (capped at 30 s).
	let (applied, ()) = tokio::time::timeout(Duration::from_secs(90), async { tokio::join!(driver, relay.run(shutdown)) })
		.await
		.expect("run returns promptly after the current drain iteration on cancellation");
	assert_eq!(applied, expected, "the relay applied the committed deposit before shutdown");
}

/// Walk the whole shared backlog through the unfenced `drain` until it reports nothing
/// left. A throttled drain (a row the ledger asked to retry) returns early; calling again
/// advances that row's bounded attempt counter until it is parked, so the loop is bounded
/// too — and if the backlog is truly stuck the timed test below fails with a clear
/// message rather than this loop spinning.
async fn drain_the_shared_backlog(relay: &Relay) {
	for _ in 0..64 {
		if !relay.drain().await {
			return;
		}
	}
	eprintln!("relay shutdown test: the shared outbox backlog is still throttling after 64 drains");
}

/// Poll until the relay has applied the committed deposit (proving the in-flight drain
/// iteration ran to completion while the token was still live), then cancel the token so the
/// relay observes shutdown at its wait point and exits. Returns the observed balance.
///
/// 60 s, not the few hundred milliseconds a healthy relay needs: the outbox lock is one
/// per database, and a holder left by a sibling process (a dev hub on the same
/// `DATABASE_URL`, a suite run in parallel) keeps `run` queued on it for as long as it
/// lives. The deadline is the cost of sharing the lock, not the time the relay takes.
async fn drive_until_applied_then_cancel(h: &Harness, party: &Party, expected: u128, shutdown: CancellationToken) -> u128 {
	let mut applied = h.claim_balance(party).await;
	for _ in 0..600 {
		if applied == expected {
			break;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
		applied = h.claim_balance(party).await;
	}
	shutdown.cancel();
	applied
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn tx_ref() -> TxRef {
	TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap()
}
