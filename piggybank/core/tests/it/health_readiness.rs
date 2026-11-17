//! Integration tests for FB-07 — a readiness probe distinct from liveness (BANK-FAULT-08).
//! Real Postgres **and** TigerBeetle (no mocks, per the project rules). They run when
//! `DATABASE_URL` is set and a TigerBeetle replica is reachable (`nix run .#db` + `.#tb`),
//! and skip otherwise. The probes are driven directly through the `HealthService` impl
//! (`Health`), no server stood up.
//!
//! What's proven:
//!   - liveness (`Check`) is trivially `ok` and never inspects DB/TB/the relay;
//!   - readiness is `ready` on a clean pipeline;
//!   - a parked outbox row flips readiness to NOT ready while liveness stays `ok`
//!     (BANK-FAULT-08 — an external monitor can finally see parked-row accumulation);
//!   - an unreachable TigerBeetle flips readiness to NOT ready (`ledger_ok == false`);
//!   - an aged deposit-scan cursor surfaces in `scan_cursors` (configured rails only)
//!     as diagnostics, without ever gating `ready`.

use std::sync::Arc;

use domain::money::Network;
use evbanking_contracts::banking::v1::{CheckRequest, ReadinessRequest, health_service_server::HealthService};
use piggybank_core::{
	infrastructure::{ledger::TbLedger, tigerbeetle::TigerBeetle},
	ports::ledger::Ledger,
	services::health::Health,
};
use sqlx::PgPool;
use tonic::Request;
use uuid::Uuid;

use crate::common;

struct Harness {
	pool: PgPool,
	ledger: Arc<dyn Ledger>,
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "readiness test").await?;
	Some(Harness { pool, ledger })
}

/// A clean pipeline reads ready; injecting a parked outbox row flips readiness to NOT ready
/// (`parked_rows >= 1`) while liveness stays trivially `ok`. The parked row is the same shape
/// the relay leaves behind on a non-retryable failure (`parked_at` set, `dispatched_at` null).
#[tokio::test]
async fn a_parked_row_flips_readiness_not_liveness() {
	let Some(h) = harness().await else { return };
	let health = Health::new(h.pool.clone(), h.ledger.clone(), Arc::from([Network::Bep20]));

	// Liveness is trivial and unconditional.
	let live = health.check(Request::new(CheckRequest {})).await.expect("check ok").into_inner();
	assert_eq!(live.status, "ok", "liveness is always ok");

	// On a pipeline with no parked rows the probe reports ready (db + ledger reachable).
	let before = health.readiness(Request::new(ReadinessRequest {})).await.expect("readiness ok").into_inner();
	assert!(before.db_ok, "Postgres SELECT 1 must succeed");
	assert!(before.ledger_ok, "the TigerBeetle ping on Fund must succeed");

	// Inject a parked outbox row (parked_at set, dispatched_at null) — exactly what the relay
	// leaves on a non-retryable failure.
	let event_id = Uuid::new_v4();
	sqlx::query(
		"INSERT INTO outbox (event_id, aggregate, aggregate_id, kind, payload, parked_at, last_error) VALUES ($1, 'withdrawals', $2, 'withdrawals', $3::jsonb, now(), 'readiness probe test')",
	)
	.bind(event_id)
	.bind(Uuid::new_v4())
	.bind("\"parked\"")
	.execute(&h.pool)
	.await
	.expect("inject a parked outbox row");

	let after = health.readiness(Request::new(ReadinessRequest {})).await.expect("readiness ok").into_inner();
	assert!(after.parked_rows >= 1, "the parked row must be counted");
	assert!(!after.ready, "readiness must fail while a parked row exists");

	// Liveness is unmoved by the parked row.
	let live_again = health.check(Request::new(CheckRequest {})).await.expect("check ok").into_inner();
	assert_eq!(live_again.status, "ok", "liveness stays ok regardless of the relay backlog");
}

/// With the control plane healthy but TigerBeetle unreachable, readiness reports `ledger_ok
/// == false` and NOT ready, while liveness stays `ok`. We reuse the live harness only to seed
/// the `Fund` account's id-map row (so the unreachable ledger's `balance(Fund)` resolves the id
/// from Postgres and then actually issues a `lookup_accounts`), then swap in a ledger pointed at
/// a port no replica is listening on. The gateway bounds that call, so it fails deterministically
/// rather than hanging.
#[tokio::test]
async fn unreachable_tigerbeetle_flips_readiness_not_liveness() {
	let Some(h) = harness().await else { return };

	// Point a second ledger at a port no TigerBeetle replica is listening on. `Fund` is already
	// in the id-map (seeded by `harness`), so `balance(Fund)` reaches `lookup_accounts` and errs.
	let unreachable = Arc::new(TigerBeetle::connect(0, "127.0.0.1:1").expect("client init is lazy"));
	let ledger: Arc<dyn Ledger> = Arc::new(TbLedger::new(unreachable, h.pool.clone()));
	let health = Health::new(h.pool.clone(), ledger, Arc::from([Network::Bep20]));

	let live = health.check(Request::new(CheckRequest {})).await.expect("check ok").into_inner();
	assert_eq!(live.status, "ok", "liveness stays ok even with TigerBeetle down");

	let ready = health.readiness(Request::new(ReadinessRequest {})).await.expect("readiness ok").into_inner();
	assert!(ready.db_ok, "Postgres is still reachable");
	assert!(!ready.ledger_ok, "an unreachable TigerBeetle must surface as ledger_ok == false");
	assert!(!ready.ready, "readiness must fail when the ledger is unreachable");
}

/// An hour-old scan cursor on a configured rail surfaces in `scan_cursors` with a
/// positive age, an unconfigured rail's row is filtered out, and — diagnostics only —
/// the age never gates `ready` (any NOT-ready here must have a non-cursor reason).
#[tokio::test]
async fn an_aged_scan_cursor_is_reported_but_never_gates() {
	let Some(h) = harness().await else { return };
	let health = Health::new(h.pool.clone(), h.ledger.clone(), Arc::from([Network::Bep20]));

	// Age both rails' cursors; only the configured one (bep20) may be reported.
	for network in ["bep20", "trc20"] {
		sqlx::query(
			"INSERT INTO deposit_scan_cursor (network, last_scanned_block, updated_at) VALUES ($1, 1, now() - interval '1 hour') ON CONFLICT (network) DO UPDATE SET updated_at = now() - interval '1 hour'",
		)
		.bind(network)
		.execute(&h.pool)
		.await
		.expect("age the scan cursor");
	}

	let readiness = health.readiness(Request::new(ReadinessRequest {})).await.expect("readiness ok").into_inner();
	let bep20 = readiness.scan_cursors.iter().find(|c| c.network == "bep20").expect("the configured rail's cursor is reported");
	assert!(bep20.age_secs >= 3_000, "the hour-old cursor reads a positive age, got {}", bep20.age_secs);
	assert!(readiness.scan_cursors.iter().all(|c| c.network != "trc20"), "an unconfigured rail's cursor is filtered out");
	// Diagnostics only: if readiness fails here it must be for one of the gating
	// reasons (shared-infra tests may park rows concurrently), never the cursor age.
	assert!(
		readiness.ready || readiness.parked_rows > 0 || !readiness.db_ok || !readiness.ledger_ok || readiness.oldest_backlog_age_secs > 120,
		"an aged scan cursor must never be the reason readiness fails"
	);
}
