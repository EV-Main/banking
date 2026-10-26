//! Value on an account of a retired code is a reconciliation finding — real Postgres and
//! TigerBeetle, no mocks. The retired accounts exist for good (TigerBeetle deletes nothing)
//! and hold zero; no live key reaches one, so money on one is money nobody can move, and the
//! scan says so instead of filing it as an unknown kind or counting it as anybody's.
//!
//! Its own binary on purpose: the planted value makes this database's reconciliation dirty
//! for good, which any sibling asserting a clean pass would read.

use domain::{
	balance::{AccountCode, Ledger as Plane, LedgerAccountKey},
	money::{Network, Usdt},
};
use piggybank_core::infrastructure::{reconciliation::Reconciliation, tigerbeetle::TigerBeetle};
use sqlx::PgPool;
use tigerbeetle as tb;
use uuid::Uuid;

mod common;

fn raw_tigerbeetle() -> TigerBeetle {
	let address = std::env::var("TIGERBEETLE_ADDRESS").unwrap_or_else(|_| "127.0.0.1:3033".to_owned());
	let cluster = std::env::var("TIGERBEETLE_CLUSTER_ID").ok().and_then(|s| s.parse().ok()).unwrap_or(0u128);
	TigerBeetle::connect(cluster, &address).expect("connect to TigerBeetle")
}

async fn account_id(pool: &PgPool, logical_key: &str) -> u128 {
	let bytes: Vec<u8> = sqlx::query_scalar("SELECT tb_account_id FROM tb_accounts WHERE logical_key = $1")
		.bind(logical_key)
		.fetch_one(pool)
		.await
		.unwrap_or_else(|err| panic!("{logical_key} is mapped: {err}"));
	u128::from_be_bytes(bytes.try_into().unwrap())
}

/// A claim account of `code` the way an older release created it: in TigerBeetle and in the
/// id-map, under a key this build no longer spells.
async fn plant_retired_claim(pool: &PgPool, tb: &TigerBeetle, code: u16) -> u128 {
	let id = Uuid::new_v4().as_u128();
	let account = tb::Account {
		id,
		ledger: Plane::Usdt.id(),
		code,
		flags: tb::AccountFlags::DebitsMustNotExceedCredits,
		..Default::default()
	};
	let created = tb.client().create_accounts(&[account]).expect("tigerbeetle open").await.expect("create_accounts");
	assert!(created.iter().all(|r| r.status == tb::CreateAccountStatus::Created), "{created:?}");
	sqlx::query("INSERT INTO tb_accounts (logical_key, tb_account_id, ledger, code, network, flags) VALUES ($1, $2, $3, $4, NULL, $5)")
		.bind(format!("service:retired-{id:x}"))
		.bind(&id.to_be_bytes()[..])
		.bind(Plane::Usdt.id() as i32)
		.bind(i32::from(code))
		.bind(tb::AccountFlags::DebitsMustNotExceedCredits.bits() as i32)
		.execute(pool)
		.await
		.unwrap();
	id
}

#[tokio::test]
async fn value_on_a_retired_claim_fails_reconciliation_and_is_never_unclassified() {
	let Some(pool) = common::pool().await else { return };
	let Some(ledger) = common::seeded_ledger(&pool, "retired claims").await else { return };
	let tb = raw_tigerbeetle();
	let wallet = account_id(&pool, &LedgerAccountKey::CryptoWallet(Network::Bep20).logical_key()).await;
	let reconciliation = Reconciliation::new(pool.clone(), ledger);
	let before = reconciliation.scan().await.unwrap();
	assert_eq!(before.retired_claims, 0, "{before:?}");

	let amount = Usdt::parse_decimal("3").unwrap().base_units();
	for code in [30, 65] {
		assert!(AccountCode::is_retired(code));
		let claim = plant_retired_claim(&pool, &tb, code).await;
		let transfer = tb::Transfer {
			id: Uuid::new_v4().as_u128(),
			debit_account_id: wallet,
			credit_account_id: claim,
			amount,
			ledger: Plane::Usdt.id(),
			code: 2,
			..Default::default()
		};
		let posted = tb.client().create_transfers(&[transfer]).expect("tigerbeetle open").await.expect("create_transfers");
		assert!(posted.iter().all(|r| matches!(r.status, tb::CreateTransferStatus::Created)), "{posted:?}");
	}

	let after = reconciliation.scan().await.unwrap();
	assert_eq!(after.retired_claims, 2 * amount, "both retired claims are counted: {after:?}");
	assert_eq!(after.unclassified_claims, before.unclassified_claims, "a retired code is a known kind: {after:?}");
	assert_eq!(after.custody, after.claims, "conservation still holds — the value is counted, not lost");
	assert!(!after.clean(), "money on a retired claim is a finding: {after:?}");
}
