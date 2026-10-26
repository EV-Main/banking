//! FB-21: the ledger-derivation drift guard. TB account flags are immutable on first
//! create, and a logical key's `(ledger, code, flags)` is derived fresh on every
//! `ensure`. If that derivation ever changes for an existing key, TB would reject the
//! changed create as a conflict and park every transfer touching it — a silent
//! foot-gun. The guard persists the resolved flags on the id-map row and, on a later
//! ensure, hard-fails loudly the moment the recomputed derivation differs.
//!
//! Real Postgres only (no DB mocks): the drift check fires in the id-map read, before
//! any TigerBeetle call, so a reachable TB replica is not required. Runs when
//! `DATABASE_URL` is set and skips otherwise. A fresh `user_id` keeps runs isolated on
//! shared infra.

use std::sync::Arc;

use domain::{balance::LedgerAccountKey, users::UserId};
use piggybank_core::ports::ledger::{Ledger, LedgerError};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pool;

/// A TB client init that does not require a live replica (no handshake until first
/// call); the drift guard returns before any TB call, so this is never exercised.
fn ledger(pool: PgPool) -> Arc<dyn Ledger> {
	common::ledger_for(&pool)
}

/// An id-map row whose persisted derivation no longer matches what the key derives
/// today must make the next `ensure` fail loudly with a `Conflict`, not park.
#[tokio::test]
async fn a_drifted_derivation_fails_loudly_on_ensure() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping ledger drift-guard test");
		return;
	};

	// `UserClaim` is credit-normal ⇒ derived flags = DebitsMustNotExceedCredits (2).
	// Seed the id-map row with the WRONG (debit-normal, 4) flags to simulate a drift.
	let key = LedgerAccountKey::UserClaim(UserId::new());
	let logical_key = key.logical_key();
	let id = Uuid::new_v4().as_u128().to_be_bytes();
	sqlx::query("INSERT INTO tb_accounts (logical_key, tb_account_id, ledger, code, network, flags) VALUES ($1, $2, $3, $4, $5, $6)")
		.bind(&logical_key)
		.bind(&id[..])
		.bind(key.ledger().id() as i32)
		.bind(key.account_code().code() as i32)
		.bind(Option::<&str>::None)
		.bind(4_i32)
		.execute(&pool)
		.await
		.expect("seed drifted id-map row");

	let ledger = ledger(pool);
	let result = ledger.ensure_account(&key).await;
	assert!(
		matches!(result, Err(LedgerError::Conflict(_))),
		"a drifted ledger derivation must surface a loud Conflict, got {result:?}"
	);
}

/// A matching persisted derivation must NOT trip the guard: re-ensuring an existing,
/// correctly-recorded key resolves cleanly (the drift check passes before the TB
/// create). This pins the guard to *drift*, not to every existing row.
#[tokio::test]
async fn a_matching_derivation_passes_the_guard() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping ledger drift-guard test");
		return;
	};

	let key = LedgerAccountKey::UserClaim(UserId::new());
	let logical_key = key.logical_key();
	let id = Uuid::new_v4().as_u128().to_be_bytes();
	// Persist the CORRECT derived flags (2 for a credit-normal claim).
	sqlx::query("INSERT INTO tb_accounts (logical_key, tb_account_id, ledger, code, network, flags) VALUES ($1, $2, $3, $4, $5, $6)")
		.bind(&logical_key)
		.bind(&id[..])
		.bind(key.ledger().id() as i32)
		.bind(key.account_code().code() as i32)
		.bind(Option::<&str>::None)
		.bind(2_i32)
		.execute(&pool)
		.await
		.expect("seed matching id-map row");

	let ledger = ledger(pool);
	// Cannot reach a guaranteed-up TB here, so assert only that the guard itself does
	// not reject — any error must be a TB-side Unavailable, never a derivation Conflict.
	if let Err(LedgerError::Conflict(msg)) = ledger.ensure_account(&key).await {
		panic!("a matching derivation must not trip the drift guard: {msg}");
	}
}
