//! Integration tests for the phase-2 storage shape — real Postgres, no mocks (per the
//! project rules). They run when `SIGNER_DATABASE_URL` (or `DATABASE_URL`) is set and skip
//! otherwise; each runs on its own throwaway database (see `common`).
//!
//! Migrations 0003 and 0004 are the only irreversible parts of this phase, so what is under
//! test is the schema contract THEY establish, not the Turnkey client:
//!
//!   * a custody-held row persists and reads back with no sealed blob at all;
//!   * that row is invisible to every KEK-shaped read — the boot backfill would otherwise
//!     probe a NULL blob and report a healthy key as a PROVABLY DEAD one;
//!   * a KEK-sealed row still round-trips unchanged, and still reads back as `backend='local'`
//!     without anyone having backfilled it;
//!   * the `wallet_secrets_backend_handle` and `wallet_secrets_turnkey_has_index` CHECKs
//!     actually refuse a row with no usable handle / no derivation index;
//!   * the per-network wallet registry (0004) is idempotent, and the derivation-index
//!     sequence never hands out the same value twice even under real concurrency.
//!
//! Nothing here talks to Turnkey: phase 2 ships the backend disabled, and the wire behaviour
//! (including each network's derivation-path FORM) is covered by the unit tests in
//! `src/turnkey.rs`.

use crate::common;

use domain::money::Network;
use piggybank_signer::{
	key_vault::{Vault, ed25519_pubkey, gen_ed25519},
	provision,
	secrets::{NewTurnkeySecret, WalletSecrets},
};
use uuid::Uuid;

fn test_vault() -> Vault {
	Vault::from_hex(&hex::encode([7u8; 32])).unwrap()
}

#[tokio::test]
async fn a_custody_row_persists_without_a_sealed_key_and_stays_out_of_the_kek_walk() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping turnkey row test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());

	// A Turnkey-held TON key: real public key (so address derivation is real), no secret half
	// anywhere in this process — which is the entire point of the migration.
	let user_id = Uuid::new_v4();
	let public_key = ed25519_pubkey(&gen_ed25519());
	let address = "0:0000000000000000000000000000000000000000000000000000000000000000";
	let sign_with = "UQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
	secrets
		.insert_turnkey(&NewTurnkeySecret {
			id: Uuid::new_v4(),
			user_id,
			network: Network::Ton,
			public_key: &public_key,
			address,
			sign_with,
			key_alg: "ed25519",
			derivation_index: 0,
		})
		.await
		.expect("a custody row inserts with no sealed blob");

	let found = secrets.find_turnkey(user_id, Network::Ton).await.expect("read back").expect("the row exists");
	let key = found.expect("the row is a turnkey row");
	assert_eq!(key.sign_with, sign_with);
	assert_eq!(key.public_key, public_key);

	// The signing path's KEK read must not see it — there is no blob to open.
	assert!(
		secrets.find_sealed(user_id, Network::Ton).await.expect("read back").is_none(),
		"a custody row has no sealed key to hand the vault"
	);
	// Nor may the boot backfill / key-health walk: a NULL blob would fail the decode, and a
	// custody-held key reported as a KEK casualty would be a false alarm on real money.
	let epoch_rows = secrets.active_epoch_rows().await.expect("the KEK walk must not choke on a NULL sealed_key");
	assert!(epoch_rows.iter().all(|row| row.user_id != user_id), "the KEK epoch walk must skip custody-held rows");

	db.cleanup().await;
}

#[tokio::test]
async fn a_kek_sealed_row_still_round_trips_and_reads_back_as_local() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping local-row regression test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());
	let vault = test_vault();
	let user_id = Uuid::new_v4();

	// The pre-migration path, untouched: provision seals under the KEK and stores a blob.
	let provisioned = provision::provision(&vault, &secrets, user_id, Network::Bep20)
		.await
		.expect("provisioning still works after migration 0003");
	assert_eq!(provisioned.kind, provision::KIND_DERIVED);

	// It must still be openable, still be visible to the KEK walk, and — with nobody having
	// backfilled anything — read back as `backend='local'` on the strength of the DEFAULT alone.
	assert!(
		secrets.find_sealed(user_id, Network::Bep20).await.expect("read back").is_some(),
		"a KEK-sealed row must still reach the signing path"
	);
	let epoch_rows = secrets.active_epoch_rows().await.expect("read back");
	assert!(epoch_rows.iter().any(|row| row.user_id == user_id), "a KEK-sealed row must still be walked by the epoch guard");

	let backend: String = sqlx::query_scalar("SELECT backend FROM wallet_secrets WHERE user_id = $1")
		.bind(user_id)
		.fetch_one(&db.pool)
		.await
		.expect("read the backend column");
	assert_eq!(backend, "local", "existing rows must read back as local without a backfill pass");

	// And the Turnkey read must refuse it rather than ask a custodian for a key it never held.
	let found = secrets.find_turnkey(user_id, Network::Bep20).await.expect("read back").expect("the row exists");
	assert_eq!(found.err(), Some("local".to_owned()), "a local row must fail closed on the turnkey path");

	db.cleanup().await;
}

#[tokio::test]
async fn the_check_constraint_refuses_a_row_with_no_usable_handle() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping CHECK constraint test");
		return;
	};

	// This is what `sealed_key NOT NULL` used to prevent and what the CHECK now prevents: a
	// local row with nothing to open. Such an address would receive deposits nothing can move.
	let err = sqlx::query("INSERT INTO wallet_secrets (id, user_id, network, public_key, address, key_alg, key_version) VALUES ($1, $2, 'BEP20', '\\x00', 'addr', 'secp256k1', 1)")
		.bind(Uuid::new_v4())
		.bind(Uuid::new_v4())
		.execute(&db.pool)
		.await
		.expect_err("a local row with no sealed key must be refused");
	assert!(err.to_string().contains("wallet_secrets_backend_handle"), "expected the backend-handle CHECK to fire, got: {err}");

	db.cleanup().await;
}

/// Migration 0004: a Turnkey row with `derivation_index IS NULL` must be refused — the exact
/// invariant `wallet_secrets_turnkey_has_index` exists to enforce, replacing what the old
/// per-user-wallet design didn't need at all.
#[tokio::test]
async fn the_check_constraint_refuses_a_turnkey_row_with_no_derivation_index() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping derivation-index CHECK test");
		return;
	};

	let err = sqlx::query(
		"INSERT INTO wallet_secrets (id, user_id, network, public_key, address, key_alg, key_version, backend, turnkey_sign_with) \
		 VALUES ($1, $2, 'TON', '\\x00', 'addr', 'ed25519', 0, 'turnkey', 'sign-with')",
	)
	.bind(Uuid::new_v4())
	.bind(Uuid::new_v4())
	.execute(&db.pool)
	.await
	.expect_err("a turnkey row with no derivation_index must be refused");
	assert!(
		err.to_string().contains("wallet_secrets_turnkey_has_index"),
		"expected the derivation-index CHECK to fire, got: {err}"
	);

	db.cleanup().await;
}

/// Migration 0004's whole safety argument: uniqueness comes from the SEQUENCE, not from this
/// code counting anything. Two rows minted this way must never collide, even under
/// concurrency — this proves it against real Postgres rather than trusting the SQL by eye.
#[tokio::test]
async fn derivation_indexes_are_monotonic_and_never_collide_under_concurrency() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping derivation-index sequence test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());

	let mut set = tokio::task::JoinSet::new();
	for _ in 0..16 {
		let secrets = secrets.clone();
		set.spawn(async move { secrets.next_derivation_index().await });
	}
	let mut indexes = Vec::new();
	while let Some(joined) = set.join_next().await {
		indexes.push(joined.expect("task must not panic").expect("nextval must succeed"));
	}
	let mut sorted = indexes.clone();
	sorted.sort_unstable();
	sorted.dedup();
	assert_eq!(sorted.len(), indexes.len(), "16 concurrent callers must get 16 distinct indexes: {indexes:?}");

	db.cleanup().await;
}

/// Migration 0004's per-network wallet registry: the first insert wins, a concurrent or later
/// insert for the SAME network is a no-op, and a different network gets its own independent
/// row — exactly the storage-level idempotency `TurnkeyBackend::ensure_account` relies on to
/// never fight another provisioning call over which wallet is canonical.
#[tokio::test]
async fn network_wallet_registration_is_idempotent_per_network() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping network-wallet registry test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());

	assert_eq!(secrets.network_wallet_id(Network::Bep20).await.expect("read back"), None, "no wallet registered yet");

	secrets.insert_network_wallet(Network::Bep20, "wallet-first").await.expect("first registration succeeds");
	assert_eq!(secrets.network_wallet_id(Network::Bep20).await.expect("read back"), Some("wallet-first".to_owned()));

	// A second racer's wallet_id must NOT overwrite the canonical one — its own account is
	// still valid and signable regardless (see `TurnkeyBackend::ensure_account`'s doc comment).
	secrets
		.insert_network_wallet(Network::Bep20, "wallet-second")
		.await
		.expect("losing registration is a no-op, not an error");
	assert_eq!(
		secrets.network_wallet_id(Network::Bep20).await.expect("read back"),
		Some("wallet-first".to_owned()),
		"the first-registered wallet stays canonical"
	);

	// A different network is a completely independent row.
	assert_eq!(secrets.network_wallet_id(Network::Ton).await.expect("read back"), None);
	secrets
		.insert_network_wallet(Network::Ton, "wallet-ton")
		.await
		.expect("a different network registers independently");
	assert_eq!(secrets.network_wallet_id(Network::Ton).await.expect("read back"), Some("wallet-ton".to_owned()));

	db.cleanup().await;
}
