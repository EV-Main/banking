//! KEK-epoch safety end-to-end (real Postgres, throwaway DB per test — no mocks):
//! the boot sentinel pins a database to one KEK and refuses any other; the backfill
//! stamps pre-epoch rows and reports provably dead ones; GetKeyHealth surfaces the
//! casualties; RotateAddress supersedes ONLY a dead key. This is the regression net
//! for the stranded-deposit incident: a key sealed under an ephemeral KEK must be
//! caught at boot / provisioning / diagnostics — never first at withdrawal time.

use crate::common;

use evbanking_contracts::signer::v1::{GetKeyHealthRequest, RotateAddressRequest, signer_service_server::SignerService};
use piggybank_signer::{kek_guard, key_vault::Vault, policy::SignerPolicy, provision, secrets::WalletSecrets, service::Signer};
use tonic::{Code, Request};
use uuid::Uuid;

fn vault_a() -> Vault {
	Vault::from_hex(&hex::encode([7u8; 32])).unwrap()
}
/// The "ephemeral KEK" of the incident: keys sealed under it are dead under [`vault_a`].
fn vault_ephemeral() -> Vault {
	Vault::from_hex(&hex::encode([8u8; 32])).unwrap()
}

#[tokio::test]
async fn sentinel_pins_the_epoch_and_refuses_a_different_kek() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping KEK sentinel test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());

	// First boot pins the epoch; a re-boot under the same KEK verifies clean.
	let report = kek_guard::enforce(&vault_a(), &secrets).await.expect("first boot pins the sentinel");
	assert_eq!((report.active_keys, report.dead_keys), (0, 0));
	kek_guard::enforce(&vault_a(), &secrets).await.expect("same-KEK re-boot verifies");

	// A different KEK must be refused loudly — this is the guard the incident bought.
	let err = kek_guard::enforce(&vault_ephemeral(), &secrets).await.expect_err("wrong KEK must refuse to serve");
	assert!(err.to_string().contains("Refusing to serve"), "operator-facing refusal, got: {err}");

	db.cleanup().await;
}

#[tokio::test]
async fn backfill_stamps_pre_epoch_rows_and_health_reports_dead_ones() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping KEK backfill test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());
	let (healthy_user, dead_user) = (Uuid::new_v4(), Uuid::new_v4());
	let network = domain::money::Network::Bep20;

	// Two rows minted before the epoch stamp existed (kek_fp NULL): one sealed under
	// the boot KEK, one under the incident's ephemeral KEK.
	provision::provision(&vault_a(), &secrets, healthy_user, network).await.expect("provision healthy");
	provision::provision(&vault_ephemeral(), &secrets, dead_user, network).await.expect("provision dead");
	sqlx::query("UPDATE wallet_secrets SET kek_fp = NULL").execute(&db.pool).await.expect("simulate pre-epoch rows");

	let report = kek_guard::enforce(&vault_a(), &secrets).await.expect("boot proceeds despite per-row casualties");
	assert_eq!((report.active_keys, report.dead_keys), (2, 1), "one survivor, one dead");

	// The survivor got stamped with the boot KEK's fingerprint.
	let stamped: Option<Vec<u8>> = sqlx::query_scalar("SELECT kek_fp FROM wallet_secrets WHERE user_id = $1")
		.bind(healthy_user)
		.fetch_one(&db.pool)
		.await
		.expect("read stamped fp");
	assert_eq!(stamped.as_deref(), Some(&vault_a().fingerprint()[..]));

	// GetKeyHealth lists exactly the dead key, with operator-facing metadata.
	let signer = Signer::new(vault_a(), WalletSecrets::new(db.pool.clone()), SignerPolicy::from_env().unwrap());
	let health = signer.get_key_health(Request::new(GetKeyHealthRequest {})).await.expect("health RPC").into_inner();
	assert_eq!((health.total_keys, health.healthy_keys), (2, 1));
	assert_eq!(health.dead_keys.len(), 1);
	let dead = &health.dead_keys[0];
	assert_eq!(dead.user_id, dead_user.to_string());
	assert_eq!(dead.network, "bep20");
	assert!(!dead.address.is_empty() && !dead.reason.is_empty() && !dead.created_at.is_empty());

	db.cleanup().await;
}

#[tokio::test]
async fn rotation_replaces_a_dead_key_and_refuses_a_healthy_one() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping rotation test");
		return;
	};
	let secrets = WalletSecrets::new(db.pool.clone());
	let network = domain::money::Network::Bep20;
	kek_guard::enforce(&vault_a(), &secrets).await.expect("pin epoch");
	let signer = Signer::new(vault_a(), WalletSecrets::new(db.pool.clone()), SignerPolicy::from_env().unwrap());

	// A healthy key must NOT be rotatable — rotation is recovery, not re-keying.
	let healthy_user = Uuid::new_v4();
	provision::provision(&vault_a(), &secrets, healthy_user, network).await.expect("provision healthy");
	let refused = signer
		.rotate_address(Request::new(RotateAddressRequest {
			user_id: healthy_user.to_string(),
			network: "bep20".to_owned(),
		}))
		.await
		.expect_err("healthy key rotation must be refused");
	assert_eq!(refused.code(), Code::FailedPrecondition);

	// No key at all → nothing to rotate.
	let missing = signer
		.rotate_address(Request::new(RotateAddressRequest {
			user_id: Uuid::new_v4().to_string(),
			network: "bep20".to_owned(),
		}))
		.await
		.expect_err("no active key must be a precondition failure");
	assert_eq!(missing.code(), Code::FailedPrecondition);

	// A dead key (sealed under the ephemeral KEK) rotates: old row archived, fresh
	// derived address served, and the new key unseals under the boot KEK.
	let victim = Uuid::new_v4();
	let old = provision::provision(&vault_ephemeral(), &secrets, victim, network).await.expect("provision dead key");
	let rotated = signer
		.rotate_address(Request::new(RotateAddressRequest {
			user_id: victim.to_string(),
			network: "bep20".to_owned(),
		}))
		.await
		.expect("dead key rotates")
		.into_inner();
	assert_eq!(rotated.address_kind, "derived");
	assert_ne!(rotated.address, old.address, "rotation must mint a NEW address");

	let archived: i64 = sqlx::query_scalar("SELECT count(*) FROM wallet_secrets WHERE user_id = $1 AND superseded_at IS NOT NULL")
		.bind(victim)
		.fetch_one(&db.pool)
		.await
		.expect("count archived");
	assert_eq!(archived, 1, "the dead row is archived, not deleted");

	let sealed = secrets.find_sealed(victim, network).await.expect("query").expect("active row exists");
	assert!(
		vault_a().open(provision::chain_of(network), &sealed.id.to_string(), &sealed.sealed_key).is_ok(),
		"the replacement key must unseal under the CURRENT KEK"
	);
	// And the ordinary provisioning path now idempotently serves the new address.
	let again = provision::provision(&vault_a(), &secrets, victim, network).await.expect("re-provision");
	assert_eq!(again.address, rotated.address);

	db.cleanup().await;
}
