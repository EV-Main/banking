//! Phase-4 custody migration, end to end against real Postgres (throwaway DB per test, no
//! DB mocks). They run when `SIGNER_DATABASE_URL`/`DATABASE_URL` is set and skip otherwise.
//!
//! `MigrateAddressToCustodian` is the mirror image of `RotateAddress`, and every gate here is
//! the inverse of one the rotation tests pin: rotation demands a key that CANNOT be opened and
//! strands the old address forever, this demands one that CAN and retires an address that has
//! already been drained. What is under test is therefore not "does it move a row" but the four
//! refusals and the all-or-nothing swap:
//!
//!   * an already custody-held row is refused (a second migration would archive a good key);
//!   * a PROVABLY DEAD key is refused (archiving it consumes its only recovery path);
//!   * an address the caller did not clear is refused (the funds gate is the hub's, so the
//!     signer checks that both sides mean the same address);
//!   * a signer with no custodian composed has no migration path at all;
//!   * and the archive+replace either both land or neither does — checked from both sides of
//!     the seam, a custodian that fails and a replacement row that cannot be inserted.
//!
//! The custodian is stood in for locally ([`FakeCustodian`]): it mints a real keypair and
//! derives the real address, so the row it produces is indistinguishable from Turnkey's except
//! that no network is involved. The Turnkey client itself is covered by the unit tests in
//! `src/turnkey.rs`.

use crate::common;

use std::sync::Arc;

use domain::money::Network;
use evbanking_contracts::signer::v1::{MigrateAddressToCustodianRequest, signer_service_server::SignerService};
use piggybank_signer::{
	backend::{BackendError, CustodyMinter, KeyBackend, LocalVault, MintedCustodyKey},
	kek_guard,
	key_vault::{Vault, evm_address, gen_secp256k1, secp256k1_pubkey},
	policy::SignerPolicy,
	provision,
	secrets::{NewTurnkeySecret, WalletSecrets},
	service::Signer,
};
use sqlx::PgPool;
use tonic::{Code, Request};
use uuid::Uuid;

const NETWORK: Network = Network::Bep20;

fn vault_a() -> Vault {
	Vault::from_hex(&hex::encode([7u8; 32])).unwrap()
}

/// The "ephemeral KEK" of the stranded-deposit incident: keys sealed under it are PROVABLY
/// DEAD under [`vault_a`], and are `RotateAddress`'s business, never this path's.
fn vault_ephemeral() -> Vault {
	Vault::from_hex(&hex::encode([8u8; 32])).unwrap()
}

/// A stand-in custodian: mints a genuine secp256k1 keypair and derives the genuine EVM
/// address, so what lands in `wallet_secrets` has the exact shape Turnkey's account would.
/// Only the network call is missing. `fail` makes it refuse, which is how the "custodian died
/// mid-migration" case is reached without a live organization.
struct FakeCustodian {
	fail: bool,
}

#[tonic::async_trait]
impl CustodyMinter for FakeCustodian {
	async fn mint(&self, _user_id: Uuid, _network: Network) -> Result<MintedCustodyKey, BackendError> {
		if self.fail {
			return Err(BackendError::Unavailable("stand-in custodian refused".to_owned()));
		}
		let secret = gen_secp256k1();
		let public_key = secp256k1_pubkey(&secret);
		let address = evm_address(&public_key).expect("derive the EVM address of a fresh key");
		Ok(MintedCustodyKey {
			// A custody account's handle is its own address; the stand-in keeps that shape.
			sign_with: address.clone(),
			public_key,
			address,
			key_alg: "secp256k1",
			derivation_index: 0,
		})
	}
}

/// A signer composed the way `KEY_BACKEND=turnkey` composes it: a custodian to mint into, and
/// the vault still loaded because every `backend='local'` row still needs it.
fn signer_with_custodian(pool: &PgPool, fail: bool) -> Signer {
	let vault = Arc::new(vault_a());
	let secrets = WalletSecrets::new(pool.clone());
	let backend: Arc<dyn KeyBackend> = Arc::new(LocalVault::new(Arc::clone(&vault), secrets.clone()));
	Signer::with_backend(backend, Some(Arc::new(FakeCustodian { fail })), vault, secrets, SignerPolicy::from_env().unwrap())
}

fn migrate_request(user_id: Uuid, drained_address: &str) -> Request<MigrateAddressToCustodianRequest> {
	Request::new(MigrateAddressToCustodianRequest {
		user_id: user_id.to_string(),
		network: NETWORK.as_str().to_owned(),
		drained_address: drained_address.to_owned(),
	})
}

/// The active row's `(backend, address, sealed_key IS NOT NULL)` — read straight from the
/// table, because the whole question is what is actually persisted.
async fn active_row(pool: &PgPool, user_id: Uuid) -> Option<(String, String, bool)> {
	sqlx::query_as::<_, (String, String, bool)>("SELECT backend, address, sealed_key IS NOT NULL FROM wallet_secrets WHERE user_id = $1 AND superseded_at IS NULL")
		.bind(user_id)
		.fetch_optional(pool)
		.await
		.expect("read the active row")
}

#[tokio::test]
async fn a_healthy_local_key_moves_onto_the_custodian_and_the_old_key_is_archived_not_destroyed() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping custody migration test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());
	kek_guard::enforce(&vault_a(), &secrets).await.expect("pin epoch");
	let signer = signer_with_custodian(&db.pool, false);

	let user_id = Uuid::new_v4();
	let old = provision::provision(&vault_a(), &secrets, user_id, NETWORK).await.expect("provision a healthy local key");

	let migrated = signer
		.migrate_address_to_custodian(migrate_request(user_id, &old.address))
		.await
		.expect("a healthy, drained local key migrates")
		.into_inner();
	assert_eq!(migrated.old_address, old.address);
	assert_ne!(migrated.new_address, old.address, "the migration must mint a NEW address");
	assert_eq!(migrated.address_kind, provision::KIND_DERIVED);

	// Exactly one active row, and it is the custody-held one — the partial unique index
	// permits no other outcome, which is precisely why the swap has to be transactional.
	let (backend, address, has_blob) = active_row(&db.pool, user_id).await.expect("an active row still exists");
	assert_eq!(backend, "turnkey");
	assert_eq!(address, migrated.new_address);
	assert!(!has_blob, "a custody-held row has no sealed blob — that is the point of the migration");

	// The old row is archived, NOT deleted, and it keeps its sealed key. This is what makes a
	// mistaken funds attestation recoverable rather than fatal: the blob still opens under the
	// KEK, which stays loaded until phase 5.
	let (archived_address, archived_blob): (String, Option<Vec<u8>>) = sqlx::query_as("SELECT address, sealed_key FROM wallet_secrets WHERE user_id = $1 AND superseded_at IS NOT NULL")
		.bind(user_id)
		.fetch_one(&db.pool)
		.await
		.expect("the old row is archived, not deleted");
	assert_eq!(archived_address, old.address);
	assert!(archived_blob.is_some(), "the retired key must be retained for recovery");

	// The signing path now reads the custody handle and no longer finds a blob to unseal.
	let found = secrets.find_turnkey(user_id, NETWORK).await.expect("read back").expect("the row exists");
	assert!(found.is_ok(), "the active row must be signable through the custodian");
	assert!(
		secrets.find_sealed(user_id, NETWORK).await.expect("read back").is_none(),
		"the active row must no longer offer a sealed key to the vault"
	);

	db.cleanup().await;
}

#[tokio::test]
async fn migration_refuses_a_row_that_is_already_custody_held() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping already-migrated test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());
	let signer = signer_with_custodian(&db.pool, false);

	// A row that has already been migrated. Phase 4's completion gate counts exactly these,
	// so a repeat call must refuse — a second migration would archive a perfectly good
	// custody key and burn another derivation index for nothing.
	let user_id = Uuid::new_v4();
	let public_key = secp256k1_pubkey(&gen_secp256k1());
	let address = evm_address(&public_key).expect("derive");
	secrets
		.insert_turnkey(&NewTurnkeySecret {
			id: Uuid::new_v4(),
			user_id,
			network: NETWORK,
			public_key: &public_key,
			address: &address,
			sign_with: &address,
			key_alg: "secp256k1",
			derivation_index: 0,
		})
		.await
		.expect("seed a custody-held row");

	let refused = signer
		.migrate_address_to_custodian(migrate_request(user_id, &address))
		.await
		.expect_err("an already custody-held row must be refused");
	assert_eq!(refused.code(), Code::FailedPrecondition);
	assert!(refused.message().contains("turnkey"), "the refusal must name the backend it found: {}", refused.message());

	db.cleanup().await;
}

#[tokio::test]
async fn migration_refuses_a_provably_dead_key_and_points_at_rotation() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping dead-key migration test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());
	kek_guard::enforce(&vault_a(), &secrets).await.expect("pin epoch");
	let signer = signer_with_custodian(&db.pool, false);

	// Sealed under the incident's ephemeral KEK: its funds already cannot move. Migrating it
	// would archive the row and quietly consume `RotateAddress` — the one recovery path it has.
	let user_id = Uuid::new_v4();
	let dead = provision::provision(&vault_ephemeral(), &secrets, user_id, NETWORK).await.expect("provision a dead key");

	let refused = signer
		.migrate_address_to_custodian(migrate_request(user_id, &dead.address))
		.await
		.expect_err("a dead key must not be migratable");
	assert_eq!(refused.code(), Code::FailedPrecondition);
	assert!(
		refused.message().contains("RotateAddress"),
		"the refusal must send the operator to the recovery path, got: {}",
		refused.message()
	);

	// And nothing moved: the dead row is still the active one, still awaiting rotation.
	let (backend, address, _) = active_row(&db.pool, user_id).await.expect("the dead row is still active");
	assert_eq!((backend.as_str(), address.as_str()), ("local", dead.address.as_str()));

	// There is no active key at all for an unknown user — a different refusal, same code.
	let missing = signer
		.migrate_address_to_custodian(migrate_request(Uuid::new_v4(), &dead.address))
		.await
		.expect_err("no active key must be a precondition failure");
	assert_eq!(missing.code(), Code::FailedPrecondition);

	db.cleanup().await;
}

#[tokio::test]
async fn migration_refuses_an_address_the_caller_did_not_clear() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping drained-address evidence test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());
	kek_guard::enforce(&vault_a(), &secrets).await.expect("pin epoch");
	let signer = signer_with_custodian(&db.pool, false);

	let user_id = Uuid::new_v4();
	let old = provision::provision(&vault_a(), &secrets, user_id, NETWORK).await.expect("provision a healthy local key");

	// The funds check is the hub's, so the signer's only local defence is that both sides
	// name the same address. A gate computed for somebody else's address must not authorize
	// retiring this one.
	let someone_else = evm_address(&secp256k1_pubkey(&gen_secp256k1())).expect("derive");
	let refused = signer
		.migrate_address_to_custodian(migrate_request(user_id, &someone_else))
		.await
		.expect_err("a foreign drained_address must be refused");
	assert_eq!(refused.code(), Code::FailedPrecondition);
	let (_, address, _) = active_row(&db.pool, user_id).await.expect("the row is untouched");
	assert_eq!(address, old.address);

	// EIP-55 casing is a display checksum over a case-insensitive address, not a different
	// address, so the same rule the Turnkey cross-check uses applies here: this must PASS.
	let migrated = signer
		.migrate_address_to_custodian(migrate_request(user_id, &old.address.to_lowercase()))
		.await
		.expect("a differently-cased rendering of the same EVM address is the same address")
		.into_inner();
	assert_eq!(migrated.old_address, old.address);

	db.cleanup().await;
}

#[tokio::test]
async fn a_signer_with_no_custodian_has_no_migration_path() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping no-custodian test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());
	kek_guard::enforce(&vault_a(), &secrets).await.expect("pin epoch");
	// `KEY_BACKEND=local`: there is nothing to migrate ONTO. The refusal must be explicit,
	// never a silent re-mint of another KEK-sealed key dressed up as a migration.
	let signer = Signer::new(vault_a(), WalletSecrets::new(db.pool.clone()), SignerPolicy::from_env().unwrap());

	let user_id = Uuid::new_v4();
	let old = provision::provision(&vault_a(), &secrets, user_id, NETWORK).await.expect("provision");
	let refused = signer
		.migrate_address_to_custodian(migrate_request(user_id, &old.address))
		.await
		.expect_err("a signer with no custodian must refuse");
	assert_eq!(refused.code(), Code::FailedPrecondition);

	let (backend, address, _) = active_row(&db.pool, user_id).await.expect("the row is untouched");
	assert_eq!((backend.as_str(), address.as_str()), ("local", old.address.as_str()));

	db.cleanup().await;
}

/// Atomicity from the custodian's side: if minting fails, the old key must still be serving.
/// This is the ordinary Turnkey outage — the one `classify` maps to `Unavailable` — and it
/// must cost nothing but a retry.
#[tokio::test]
async fn a_custodian_failure_leaves_the_old_key_serving() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping custodian-failure test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());
	kek_guard::enforce(&vault_a(), &secrets).await.expect("pin epoch");
	let signer = signer_with_custodian(&db.pool, true);

	let user_id = Uuid::new_v4();
	let old = provision::provision(&vault_a(), &secrets, user_id, NETWORK).await.expect("provision");

	let failed = signer
		.migrate_address_to_custodian(migrate_request(user_id, &old.address))
		.await
		.expect_err("a custodian that cannot mint must not migrate anything");
	assert_eq!(failed.code(), Code::Unavailable, "a custodian outage is retryable, not terminal");

	let (backend, address, has_blob) = active_row(&db.pool, user_id).await.expect("the row is untouched");
	assert_eq!((backend.as_str(), address.as_str()), ("local", old.address.as_str()));
	assert!(has_blob, "the old key must still be openable");
	let archived: i64 = sqlx::query_scalar("SELECT count(*) FROM wallet_secrets WHERE user_id = $1 AND superseded_at IS NOT NULL")
		.bind(user_id)
		.fetch_one(&db.pool)
		.await
		.expect("count archived");
	assert_eq!(archived, 0, "nothing may be archived when no replacement was minted");

	db.cleanup().await;
}

/// Atomicity from the database's side, and the reason `migrate_to_custodian` runs both writes
/// in one transaction. The supersede succeeds and the replacement INSERT then fails — the
/// half-applied state this guards against is a user with NO active row at all, an address
/// that receives deposits nothing serves. Forced deterministically with a colliding primary
/// key rather than by timing.
#[tokio::test]
async fn a_failed_replacement_insert_rolls_the_supersede_back() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping migration atomicity test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());
	let user_id = Uuid::new_v4();
	let old = provision::provision(&vault_a(), &secrets, user_id, NETWORK).await.expect("provision");
	let old_id: Uuid = sqlx::query_scalar("SELECT id FROM wallet_secrets WHERE user_id = $1 AND superseded_at IS NULL")
		.bind(user_id)
		.fetch_one(&db.pool)
		.await
		.expect("read the active row id");

	let public_key = secp256k1_pubkey(&gen_secp256k1());
	let address = evm_address(&public_key).expect("derive");
	let err = secrets
		.migrate_to_custodian(
			old_id,
			&NewTurnkeySecret {
				// Reusing the row being archived as the NEW row's primary key makes the second
				// statement fail after the first has already succeeded.
				id: old_id,
				user_id,
				network: NETWORK,
				public_key: &public_key,
				address: &address,
				sign_with: &address,
				key_alg: "secp256k1",
				derivation_index: 1,
			},
		)
		.await
		.expect_err("a replacement row that cannot be inserted must fail the whole migration");
	assert!(format!("{err}").contains("wallet_secrets_pkey"), "expected the PK conflict to surface, got: {err}");

	// The supersede must have gone with it: the old key is still the active one.
	let (backend, active_address, has_blob) = active_row(&db.pool, user_id).await.expect("the old row is still active");
	assert_eq!((backend.as_str(), active_address.as_str()), ("local", old.address.as_str()));
	assert!(has_blob, "the old key is still openable");

	// And a well-formed replacement still migrates cleanly afterwards — the failed attempt
	// left no residue to trip over.
	let migrated = secrets
		.migrate_to_custodian(
			old_id,
			&NewTurnkeySecret {
				id: Uuid::new_v4(),
				user_id,
				network: NETWORK,
				public_key: &public_key,
				address: &address,
				sign_with: &address,
				key_alg: "secp256k1",
				derivation_index: 1,
			},
		)
		.await
		.expect("the retry commits");
	assert!(migrated);
	assert_eq!(active_row(&db.pool, user_id).await.expect("active row").0, "turnkey");

	// A second attempt against the now-archived row reports the lost race instead of
	// inserting a second active key behind the first one's back.
	let lost = secrets
		.migrate_to_custodian(
			old_id,
			&NewTurnkeySecret {
				id: Uuid::new_v4(),
				user_id,
				network: NETWORK,
				public_key: &public_key,
				address: &address,
				sign_with: &address,
				key_alg: "secp256k1",
				derivation_index: 2,
			},
		)
		.await
		.expect("a lost race is not an error");
	assert!(!lost, "an already-archived row must not be migrated twice");

	db.cleanup().await;
}
