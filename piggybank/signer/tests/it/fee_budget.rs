//! The fee budget is enforced by every signing handler, on every wallet class, before any
//! key is touched. Real Postgres, the real local vault, the real handlers (no mocks).
//!
//! Two observations per rail:
//!   - an over-budget quote from a wallet that was NEVER provisioned is refused with
//!     `PermissionDenied` — not `FailedPrecondition` ("not provisioned") — which proves the
//!     gate runs before the backend is consulted at all, let alone asked to sign;
//!   - a quote exactly at the default ceiling on a legitimate flow — a sweep from a provisioned
//!     deposit wallet into the treasury, a gas top-up from the station onto a deposit wallet
//!     (not a treasury payout: this is not a treasury control) — is signed.
//!
//! The same file also covers the treasury-only sinks of a jetton transfer (`our_jetton_wallet`,
//! `response_destination`) and the EVM chain-id cross-check, since they share the harness.
//!
//! Runs when `SIGNER_DATABASE_URL`/`DATABASE_URL` is set; skips otherwise.

use crate::common;

use std::str::FromStr as _;

use domain::money::Network;
use evbanking_contracts::signer::v1::{SignErc20TransferRequest, SignJettonTransferRequest, SignNativeTransferRequest, SignTrc20TransferRequest, signer_service_server::SignerService};
use piggybank_signer::{key_vault::Vault, policy::SignerPolicy, provision, secrets::WalletSecrets, service::Signer};
use tonic::{Code, Request};
use uuid::Uuid;

const GWEI: u128 = 1_000_000_000;
const FOREIGN_TON: &str = "0:8d8c9d8a8e8b8c8d8e8f808182838485868788898a8b8c8d8e8f80818283848f";

/// The hub's reserved gas-station wallet id (`piggybank/core/src/infrastructure/rails.rs`).
const GAS_STATION: Uuid = Uuid::from_u128(1);

fn test_vault() -> Vault {
	Vault::from_hex(&hex::encode([9u8; 32])).unwrap()
}

/// The default policy with Tron signing switched on — the rail is frozen by default (#369),
/// and these tests are about the fee budget, which must be reachable on Tron too.
fn policy() -> SignerPolicy {
	SignerPolicy::from_lookup(&|name| (name == "SIGNER_TRON_SIGNING_ENABLED").then(|| "true".to_owned())).unwrap()
}

/// A signer with a deposit wallet, the treasury and the gas station all provisioned on one
/// network, so a sweep (deposit → treasury) and a top-up (station → deposit) are both
/// legitimate under the class rules.
struct Rail {
	signer: Signer,
	user: Uuid,
	user_address: String,
	treasury_address: String,
}

async fn rail(db: &common::TestDb, network: Network) -> Rail {
	let secrets = WalletSecrets::new(db.pool.clone());
	let user = Uuid::new_v4();
	let user_address = provision::provision(&test_vault(), &secrets, user, network).await.expect("provision a deposit wallet").address;
	let treasury_address = provision::provision(&test_vault(), &secrets, Uuid::nil(), network).await.expect("provision the treasury").address;
	provision::provision(&test_vault(), &secrets, GAS_STATION, network).await.expect("provision the gas station");
	Rail {
		signer: Signer::new(test_vault(), secrets, policy()),
		user,
		user_address,
		treasury_address,
	}
}

fn erc20(from: Uuid, gas_price: u128, gas_limit: u64) -> Request<SignErc20TransferRequest> {
	erc20_to(from, "0x024da544a76714a3812096e9ef84d40b2c8863e8", gas_price, gas_limit)
}

fn erc20_to(from: Uuid, to_address: &str, gas_price: u128, gas_limit: u64) -> Request<SignErc20TransferRequest> {
	Request::new(SignErc20TransferRequest {
		from_user_id: from.to_string(),
		network: "bep20".to_owned(),
		token_contract: "0x55d398326f99059ff775485246999027b3197955".to_owned(),
		to_address: to_address.to_owned(),
		amount: "1".to_owned(),
		chain_id: 56,
		nonce: 0,
		gas_price: gas_price.to_string(),
		gas_limit,
	})
}

fn native(from: Uuid, gas_price: u128, gas_limit: u64) -> Request<SignNativeTransferRequest> {
	native_to(from, "0x024da544a76714a3812096e9ef84d40b2c8863e8", gas_price, gas_limit)
}

fn native_to(from: Uuid, to_address: &str, gas_price: u128, gas_limit: u64) -> Request<SignNativeTransferRequest> {
	Request::new(SignNativeTransferRequest {
		from_user_id: from.to_string(),
		network: "polygon".to_owned(),
		to_address: to_address.to_owned(),
		amount: "1".to_owned(),
		chain_id: 137,
		nonce: 0,
		gas_price: gas_price.to_string(),
		gas_limit,
	})
}

fn trc20(from: Uuid, fee_limit: i64) -> Request<SignTrc20TransferRequest> {
	trc20_to(from, "TJRabPrwbZy45sbavfcjinPJC18kjpRTv8", fee_limit)
}

fn trc20_to(from: Uuid, to_address: &str, fee_limit: i64) -> Request<SignTrc20TransferRequest> {
	Request::new(SignTrc20TransferRequest {
		from_user_id: from.to_string(),
		network: "trc20".to_owned(),
		token_contract: "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".to_owned(),
		to_address: to_address.to_owned(),
		amount: "1".to_owned(),
		ref_block_bytes: "0102".to_owned(),
		ref_block_hash: "0102030405060708".to_owned(),
		expiration: 1_800_000_060_000,
		timestamp: 1_800_000_000_000,
		fee_limit,
	})
}

fn jetton(from: Uuid, msg_value: u64, forward_ton_amount: u64) -> Request<SignJettonTransferRequest> {
	jetton_to(from, "EQB3ncyBUTjZUA5EnFKR5_EnOMI9V1tTEAAPaiU71gc4TiUt", FOREIGN_TON, msg_value, forward_ton_amount)
}

fn jetton_to(from: Uuid, to_address: &str, response_destination: &str, msg_value: u64, forward_ton_amount: u64) -> Request<SignJettonTransferRequest> {
	Request::new(SignJettonTransferRequest {
		from_user_id: from.to_string(),
		network: "ton".to_owned(),
		our_jetton_wallet: "0:e4d954ef9f4e1250a26b5bbad76a1cdd17cfd08babad6f4c23e372270aef6f76".to_owned(),
		to_address: to_address.to_owned(),
		amount: "1".to_owned(),
		response_destination: response_destination.to_owned(),
		forward_ton_amount,
		msg_value,
		seqno: 0,
		valid_until: 1_800_000_000,
		is_testnet: false,
		wallet_version: String::new(),
	})
}

#[tokio::test]
async fn erc20_transfer_is_bounded_by_the_evm_budget() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping signer fee budget test");
		return;
	};
	let rail = rail(&db, Network::Bep20).await;

	let refused = rail.signer.sign_erc20_transfer(erc20(Uuid::new_v4(), 100 * GWEI + 1, 100_000)).await.unwrap_err();
	assert_eq!(refused.code(), Code::PermissionDenied, "{refused:?}");
	assert!(refused.message().contains("gas_price"), "{refused:?}");

	rail.signer
		.sign_erc20_transfer(erc20_to(rail.user, &rail.treasury_address, 100 * GWEI, 100_000))
		.await
		.expect("a sweep at the ceiling is signed");
	db.cleanup().await;
}

#[tokio::test]
async fn native_transfer_is_bounded_by_the_evm_budget() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping signer fee budget test");
		return;
	};
	let rail = rail(&db, Network::Polygon).await;

	let refused = rail.signer.sign_native_transfer(native(Uuid::new_v4(), 1, 100_001)).await.unwrap_err();
	assert_eq!(refused.code(), Code::PermissionDenied, "{refused:?}");
	assert!(refused.message().contains("gas_limit"), "{refused:?}");

	// Polygon's own ceiling, not BSC's: 5_000 gwei is signed here — on a gas top-up, the one
	// legitimate native flow.
	rail.signer
		.sign_native_transfer(native_to(GAS_STATION, &rail.user_address, 5_000 * GWEI, 21_000))
		.await
		.expect("a top-up at the ceiling is signed");
	db.cleanup().await;
}

#[tokio::test]
async fn trc20_transfer_is_bounded_by_the_tron_budget() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping signer fee budget test");
		return;
	};
	let rail = rail(&db, Network::Trc20).await;

	let refused = rail.signer.sign_trc20_transfer(trc20(Uuid::new_v4(), 100_000_001)).await.unwrap_err();
	assert_eq!(refused.code(), Code::PermissionDenied, "{refused:?}");
	assert!(refused.message().contains("fee_limit"), "{refused:?}");

	rail.signer
		.sign_trc20_transfer(trc20_to(rail.user, &rail.treasury_address, 100_000_000))
		.await
		.expect("a sweep at the ceiling is signed");
	db.cleanup().await;
}

#[tokio::test]
async fn jetton_transfer_is_bounded_by_the_ton_budget() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping signer fee budget test");
		return;
	};
	let rail = rail(&db, Network::Ton).await;

	let refused = rail.signer.sign_jetton_transfer(jetton(Uuid::new_v4(), 100_000_000, 50_000_001)).await.unwrap_err();
	assert_eq!(refused.code(), Code::PermissionDenied, "{refused:?}");
	assert!(refused.message().contains("forward_ton_amount"), "{refused:?}");

	rail.signer
		.sign_jetton_transfer(jetton_to(rail.user, &rail.treasury_address, &rail.treasury_address, 100_000_000, 50_000_000))
		.await
		.expect("a sweep at the ceiling is signed");
	db.cleanup().await;
}

// === treasury jetton sinks and the chain-id cross-check ======================

const TREASURY_JETTON_WALLET: &str = "0:e4d954ef9f4e1250a26b5bbad76a1cdd17cfd08babad6f4c23e372270aef6f76";

/// A signer whose treasury TON wallet is provisioned, under a policy with the treasury's
/// jetton wallet pinned and no allowlist — the intended production posture. Returns the
/// treasury's own address in the base64 rendering (the signer stores the raw `0:<hex>`).
async fn treasury_signer(db: &common::TestDb) -> (Signer, String) {
	let secrets = WalletSecrets::new(db.pool.clone());
	let provisioned = provision::provision(&test_vault(), &secrets, Uuid::nil(), Network::Ton)
		.await
		.expect("provision the treasury TON wallet");
	let base64 = tonlib_core::TonAddress::from_str(&provisioned.address).unwrap().to_base64_url_flags(true, false);
	assert_ne!(base64, provisioned.address, "the two renderings must be spelled differently for the test to mean anything");
	let policy = SignerPolicy::from_lookup(&|name| (name == "SIGNER_TON_TREASURY_JETTON_WALLET").then(|| TREASURY_JETTON_WALLET.to_owned())).unwrap();
	(Signer::new(test_vault(), secrets, policy), base64)
}

fn treasury_jetton(our_jetton_wallet: &str, response_destination: &str) -> Request<SignJettonTransferRequest> {
	let mut req = jetton(Uuid::nil(), 100_000_000, 50_000_000).into_inner();
	req.from_user_id = String::new();
	req.our_jetton_wallet = our_jetton_wallet.to_owned();
	req.response_destination = response_destination.to_owned();
	Request::new(req)
}

#[tokio::test]
async fn treasury_jetton_transfer_returns_excess_only_to_the_treasury() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping signer fee budget test");
		return;
	};
	let (signer, treasury_base64) = treasury_signer(&db).await;

	// (a) The hub's shape: the excess returns to the treasury, spelled base64 while the signer
	// derived raw — `render_address` and `addresses_agree` must agree across renderings.
	signer
		.sign_jetton_transfer(treasury_jetton(TREASURY_JETTON_WALLET, &treasury_base64))
		.await
		.expect("a withdrawal returning excess to the treasury is signed");

	// (b) Anywhere else is refused, with no allowlist configured at all.
	let refused = signer.sign_jetton_transfer(treasury_jetton(TREASURY_JETTON_WALLET, FOREIGN_TON)).await.unwrap_err();
	assert_eq!(refused.code(), Code::PermissionDenied, "{refused:?}");
	assert!(refused.message().contains("response_destination"), "{refused:?}");
	db.cleanup().await;
}

#[tokio::test]
async fn treasury_jetton_transfer_refuses_an_unpinned_jetton_wallet() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping signer fee budget test");
		return;
	};
	let (signer, treasury_base64) = treasury_signer(&db).await;

	// (c) `msg_value` would go to whatever sits at `our_jetton_wallet`; only the pin is accepted.
	let refused = signer.sign_jetton_transfer(treasury_jetton(FOREIGN_TON, &treasury_base64)).await.unwrap_err();
	assert_eq!(refused.code(), Code::PermissionDenied, "{refused:?}");
	assert!(refused.message().contains("our_jetton_wallet"), "{refused:?}");
	db.cleanup().await;
}

#[tokio::test]
async fn evm_transfer_refuses_a_chain_id_from_the_other_rail() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping signer fee budget test");
		return;
	};
	let Rail { signer, user: wallet, .. } = rail(&db, Network::Polygon).await;

	// (d) network=polygon with BSC's chain id: refused as malformed — and BEFORE the fee
	// budget, so a quote that is also over budget comes back InvalidArgument, not
	// PermissionDenied. Otherwise the gas-price ceiling would have been Polygon's on a
	// BSC-valid transaction.
	let mut req = native(wallet, 5_000 * GWEI + 1, 21_000).into_inner();
	req.chain_id = 56;
	let refused = signer.sign_native_transfer(Request::new(req)).await.unwrap_err();
	assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
	assert!(refused.message().contains("chain_id"), "{refused:?}");

	// The mirror image from the BSC side: network=bep20 carrying Polygon's chain id.
	let mut req = erc20(wallet, 100 * GWEI + 1, 100_000).into_inner();
	req.chain_id = 137;
	let refused = signer.sign_erc20_transfer(Request::new(req)).await.unwrap_err();
	assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
	assert!(refused.message().contains("chain_id"), "{refused:?}");
	db.cleanup().await;
}

#[tokio::test]
async fn fee_budget_applies_to_the_treasury_and_the_gas_station_too() {
	let Some(db) = common::throwaway_db().await else {
		eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping signer fee budget test");
		return;
	};
	let signer = Signer::new(test_vault(), WalletSecrets::new(db.pool.clone()), policy());

	// Neither wallet is provisioned: an over-budget quote is refused on the budget, not on
	// the missing key, so the gate is not a treasury control and runs before any key lookup.
	let mut req = erc20(Uuid::nil(), 100 * GWEI + 1, 100_000).into_inner();
	req.from_user_id = String::new();
	let refused = signer.sign_erc20_transfer(Request::new(req)).await.unwrap_err();
	assert_eq!(refused.code(), Code::PermissionDenied, "treasury: {refused:?}");

	let refused = signer.sign_native_transfer(native(GAS_STATION, 5_000 * GWEI + 1, 21_000)).await.unwrap_err();
	assert_eq!(refused.code(), Code::PermissionDenied, "gas station: {refused:?}");
	let refused = signer.sign_trc20_transfer(trc20(GAS_STATION, 100_000_001)).await.unwrap_err();
	assert_eq!(refused.code(), Code::PermissionDenied, "gas station: {refused:?}");
	let refused = signer.sign_jetton_transfer(jetton(GAS_STATION, 100_000_001, 1)).await.unwrap_err();
	assert_eq!(refused.code(), Code::PermissionDenied, "gas station: {refused:?}");
	db.cleanup().await;
}
