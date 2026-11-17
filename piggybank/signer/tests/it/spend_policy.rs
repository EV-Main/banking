//! The per-wallet-class spend rules, end to end: real Postgres, the real local vault, the real
//! handlers (no mocks). One harness for #183 (wallet classes), #184 (treasury native + token
//! pin), #369 (the native spend window) and the security review of that branch (the treasury
//! USDT window, jetton wallets pinned on first use and retired with their wallet row,
//! reservations released on a failed signature).
//!
//! Every refusal here is `PermissionDenied`, and where the sending wallet is deliberately
//! left unprovisioned it is still `PermissionDenied` — not `FailedPrecondition` — which proves
//! the rule runs before the backend is consulted for a key.
//!
//! Runs when `SIGNER_DATABASE_URL`/`DATABASE_URL` is set; skips otherwise.

use crate::common;

use std::{collections::HashMap, str::FromStr as _, sync::Arc};

use domain::money::Network;
use ed25519_dalek::Signer as _;
use evbanking_contracts::signer::v1::{
	GetKeyHealthRequest, MigrateAddressToCustodianRequest, ProvisionAddressRequest, RotateAddressRequest, SignErc20TransferRequest, SignJettonTransferRequest, SignNativeTransferRequest,
	SignTonTransferRequest, SignTrc20TransferRequest, SignTrxTransferRequest, signer_service_server::SignerService,
};
use piggybank_signer::{
	backend::{BackendError, ChainSignature, Curve, CustodyMinter, KeyBackend, KeyHandle, LocalVault, MintedCustodyKey},
	key_vault::{Vault, ed25519_pubkey, gen_ed25519, ton_address},
	policy::SignerPolicy,
	provision::{self, ProvisionedAddress},
	secrets::WalletSecrets,
	service::Signer,
};
use sqlx::AssertSqlSafe;
use tonic::{Code, Request, Status};
use uuid::Uuid;

const GWEI: u128 = 1_000_000_000;
/// The hub's reserved gas-station wallet id (`piggybank/core/src/infrastructure/rails.rs`).
const GAS_STATION: Uuid = Uuid::from_u128(1);
const TREASURY: Uuid = Uuid::nil();

const OTHER_EVM: &str = "0x024da544a76714a3812096e9ef84d40b2c8863e8";
const OTHER_TRON: &str = "TJRabPrwbZy45sbavfcjinPJC18kjpRTv8";
const OTHER_TON: &str = "EQB3ncyBUTjZUA5EnFKR5_EnOMI9V1tTEAAPaiU71gc4TiUt";
const USDT_BEP20: &str = "0x55d398326f99059fF775485246999027B3197955";
const USDT_TRC20: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";
const JETTON_WALLET: &str = "0:e4d954ef9f4e1250a26b5bbad76a1cdd17cfd08babad6f4c23e372270aef6f76";
/// A second, unrelated jetton wallet — what a forged transfer would name after the first pinned.
const OTHER_JETTON_WALLET: &str = "0:1111111111111111111111111111111111111111111111111111111111111111";

fn test_vault() -> Vault {
	Vault::from_hex(&hex::encode([9u8; 32])).unwrap()
}

/// A KEK the signer under test does NOT hold: a key sealed under it is provably dead, which
/// is the one state `RotateAddress` accepts.
fn other_vault() -> Vault {
	Vault::from_hex(&hex::encode([8u8; 32])).unwrap()
}

/// A stand-in TON custodian for the migration path: mints genuine Ed25519 keys, keeps their
/// seeds in memory and signs for the custody rows it minted; everything else — the local rows
/// every test provisions — goes to the real local vault. Only the network call is missing.
struct TonCustody {
	local: LocalVault,
	secrets: WalletSecrets,
	seeds: std::sync::Mutex<HashMap<String, [u8; 32]>>,
}

impl TonCustody {
	fn signer(secrets: WalletSecrets, policy: SignerPolicy) -> Signer {
		let vault = Arc::new(test_vault());
		let custody = Arc::new(Self {
			local: LocalVault::new(Arc::clone(&vault), secrets.clone()),
			secrets: secrets.clone(),
			seeds: std::sync::Mutex::new(HashMap::new()),
		});
		Signer::with_backend(Arc::clone(&custody) as Arc<dyn KeyBackend>, Some(custody as Arc<dyn CustodyMinter>), vault, secrets, policy)
	}

	/// The minted seed for a custody row, or `None` for a local row.
	async fn minted_seed(&self, handle: KeyHandle) -> Result<Option<[u8; 32]>, BackendError> {
		Ok(match self.secrets.find_turnkey(handle.wallet_id, handle.network).await? {
			Some(Ok(key)) => Some(*self.seeds.lock().unwrap().get(&key.sign_with).expect("a custody row this stand-in minted")),
			_ => None,
		})
	}
}

#[tonic::async_trait]
impl CustodyMinter for TonCustody {
	async fn mint(&self, _user_id: Uuid, _network: Network) -> Result<MintedCustodyKey, BackendError> {
		let seed = gen_ed25519();
		let public_key = ed25519_pubkey(&seed);
		let address = ton_address(&public_key).expect("derive the TON address of a fresh key");
		self.seeds.lock().unwrap().insert(address.clone(), *seed);
		Ok(MintedCustodyKey {
			sign_with: address.clone(),
			public_key: public_key.to_vec(),
			address,
			key_alg: "ed25519",
			derivation_index: 0,
		})
	}
}

#[tonic::async_trait]
impl KeyBackend for TonCustody {
	async fn provision(&self, user_id: Uuid, network: Network) -> Result<ProvisionedAddress, BackendError> {
		self.local.provision(user_id, network).await
	}

	async fn public_key(&self, handle: KeyHandle) -> Result<Vec<u8>, BackendError> {
		match self.minted_seed(handle).await? {
			Some(seed) => Ok(ed25519_pubkey(&seed).to_vec()),
			None => self.local.public_key(handle).await,
		}
	}

	async fn sign_digest(&self, handle: KeyHandle, curve: Curve, digest: &[u8; 32]) -> Result<ChainSignature, BackendError> {
		match self.minted_seed(handle).await? {
			Some(seed) => Ok(ChainSignature::Ed25519(ed25519_dalek::SigningKey::from_bytes(&seed).sign(digest).to_bytes())),
			None => self.local.sign_digest(handle, curve, digest).await,
		}
	}
}

/// A policy from `vars` on top of the defaults, with Tron signing switched on: the rail is
/// frozen by default (#369) and these tests exercise the Tron handlers past that gate. The
/// gate itself is tested with the real default below.
fn policy(vars: &[(&str, &str)]) -> SignerPolicy {
	SignerPolicy::from_lookup(&|name| {
		if name == "SIGNER_TRON_SIGNING_ENABLED" {
			return Some("true".to_owned());
		}
		vars.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_owned())
	})
	.unwrap()
}

/// `from_user_id` as the wire carries it: empty for the treasury.
fn from(wallet: Uuid) -> String {
	if wallet == TREASURY { String::new() } else { wallet.to_string() }
}

fn denied<T>(result: Result<T, Status>, what: &str) -> Status {
	let status = match result {
		Ok(_) => panic!("{what}: expected a refusal, got a signature"),
		Err(status) => status,
	};
	assert_eq!(status.code(), Code::PermissionDenied, "{what}: {status:?}");
	status
}

/// One network with a deposit wallet, the treasury and the gas station provisioned, under
/// `policy`. Addresses are the signer's own rendering (EIP-55 / Base58Check / raw `0:<hex>`).
struct Rail {
	signer: Signer,
	secrets: WalletSecrets,
	user: Uuid,
	user_address: String,
	treasury_address: String,
	gas_station_address: String,
}

impl Rail {
	async fn new(db: &common::TestDb, network: Network, policy: SignerPolicy) -> Self {
		let secrets = WalletSecrets::new(db.pool.clone());
		let user = Uuid::new_v4();
		let user_address = provision::provision(&test_vault(), &secrets, user, network).await.expect("provision a deposit wallet").address;
		let treasury_address = provision::provision(&test_vault(), &secrets, TREASURY, network).await.expect("provision the treasury").address;
		let gas_station_address = provision::provision(&test_vault(), &secrets, GAS_STATION, network)
			.await
			.expect("provision the gas station")
			.address;
		Self {
			signer: Signer::new(test_vault(), secrets.clone(), policy),
			secrets,
			user,
			user_address,
			treasury_address,
			gas_station_address,
		}
	}

	/// A second deposit wallet on `network`, for "another wallet is unaffected" checks.
	async fn another_user(&self, network: Network) -> (Uuid, String) {
		let user = Uuid::new_v4();
		let address = provision::provision(&test_vault(), &self.secrets, user, network)
			.await
			.expect("provision another deposit wallet")
			.address;
		(user, address)
	}
}

fn erc20(from_wallet: Uuid, token: &str, to: &str, amount: u128, gas_price: u128, gas_limit: u64) -> Request<SignErc20TransferRequest> {
	Request::new(SignErc20TransferRequest {
		from_user_id: from(from_wallet),
		network: "bep20".to_owned(),
		token_contract: token.to_owned(),
		to_address: to.to_owned(),
		amount: amount.to_string(),
		chain_id: 56,
		nonce: 0,
		gas_price: gas_price.to_string(),
		gas_limit,
	})
}

fn native(from_wallet: Uuid, network: &str, chain_id: u64, to: &str, amount: u128, gas_price: u128, gas_limit: u64) -> Request<SignNativeTransferRequest> {
	Request::new(SignNativeTransferRequest {
		from_user_id: from(from_wallet),
		network: network.to_owned(),
		to_address: to.to_owned(),
		amount: amount.to_string(),
		chain_id,
		nonce: 0,
		gas_price: gas_price.to_string(),
		gas_limit,
	})
}

fn trc20(from_wallet: Uuid, token: &str, to: &str, amount: u128, fee_limit: i64) -> Request<SignTrc20TransferRequest> {
	Request::new(SignTrc20TransferRequest {
		from_user_id: from(from_wallet),
		network: "trc20".to_owned(),
		token_contract: token.to_owned(),
		to_address: to.to_owned(),
		amount: amount.to_string(),
		ref_block_bytes: "0102".to_owned(),
		ref_block_hash: "0102030405060708".to_owned(),
		expiration: 1_800_000_060_000,
		timestamp: 1_800_000_000_000,
		fee_limit,
	})
}

fn trx(from_wallet: Uuid, to: &str, amount: u128) -> Request<SignTrxTransferRequest> {
	Request::new(SignTrxTransferRequest {
		from_user_id: from(from_wallet),
		network: "trc20".to_owned(),
		to_address: to.to_owned(),
		amount: amount.to_string(),
		ref_block_bytes: "0102".to_owned(),
		ref_block_hash: "0102030405060708".to_owned(),
		expiration: 1_800_000_060_000,
		timestamp: 1_800_000_000_000,
	})
}

fn jetton(from_wallet: Uuid, to: &str, response_destination: &str, amount: u128, msg_value: u64) -> Request<SignJettonTransferRequest> {
	Request::new(SignJettonTransferRequest {
		from_user_id: from(from_wallet),
		network: "ton".to_owned(),
		our_jetton_wallet: JETTON_WALLET.to_owned(),
		to_address: to.to_owned(),
		amount: amount.to_string(),
		response_destination: response_destination.to_owned(),
		forward_ton_amount: 1,
		msg_value,
		seqno: 0,
		valid_until: 1_800_000_000,
		is_testnet: false,
		wallet_version: String::new(),
	})
}

/// [`jetton`] with an explicit `our_jetton_wallet`.
fn jetton_via(from_wallet: Uuid, our_jetton_wallet: &str, to: &str, response_destination: &str) -> Request<SignJettonTransferRequest> {
	let mut req = jetton(from_wallet, to, response_destination, 1, 100_000_000).into_inner();
	req.our_jetton_wallet = our_jetton_wallet.to_owned();
	Request::new(req)
}

/// How many ledger rows `(wallet, asset)` holds — what a wallet's windows are charged with.
async fn ledger_rows(db: &common::TestDb, wallet: Uuid, asset: &str) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM native_spend WHERE wallet_id = $1 AND asset = $2")
		.bind(wallet)
		.bind(asset)
		.fetch_one(&db.pool)
		.await
		.expect("count native_spend rows")
}

/// The `jetton_wallets` pin for `(wallet, ton)`, if learned.
async fn pinned_jetton_wallet(db: &common::TestDb, wallet: Uuid) -> Option<String> {
	sqlx::query_scalar("SELECT jetton_wallet FROM jetton_wallets WHERE wallet_id = $1 AND network = 'ton'")
		.bind(wallet)
		.fetch_optional(&db.pool)
		.await
		.expect("read jetton_wallets")
}

fn ton(from_wallet: Uuid, to: &str, amount: u128) -> Request<SignTonTransferRequest> {
	Request::new(SignTonTransferRequest {
		from_user_id: from(from_wallet),
		network: "ton".to_owned(),
		to_address: to.to_owned(),
		amount: amount.to_string(),
		seqno: 0,
		valid_until: 1_800_000_000,
		is_testnet: false,
		wallet_version: String::new(),
	})
}

fn base64_of(raw_ton: &str) -> String {
	tonlib_core::TonAddress::from_str(raw_ton).unwrap().to_base64_url_flags(true, false)
}

macro_rules! db_or_skip {
	() => {
		match common::throwaway_db().await {
			Some(db) => db,
			None => {
				eprintln!("DATABASE_URL/SIGNER_DATABASE_URL unset — skipping signer spend policy test");
				return;
			}
		}
	};
}

// === #183: sweeps go to the treasury and nowhere else =========================

#[tokio::test]
async fn sweep_is_signed_to_the_treasury_and_refused_anywhere_else() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;

	// The hub's shape, and the treasury spelled lowercase while the signer stores EIP-55.
	rail.signer
		.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, &rail.treasury_address.to_ascii_lowercase(), 1, GWEI, 60_000))
		.await
		.expect("a sweep to the treasury is signed");

	let status = denied(rail.signer.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000)).await, "sweep elsewhere");
	assert!(status.message().contains("treasury"), "{status:?}");

	// Even to the gas station — the only sink is the treasury.
	denied(
		rail.signer.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, &rail.gas_station_address, 1, GWEI, 60_000)).await,
		"sweep to the gas station",
	);

	// An unprovisioned deposit wallet is refused on the rule, before any key lookup.
	denied(
		rail.signer.sign_erc20_transfer(erc20(Uuid::new_v4(), USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000)).await,
		"unprovisioned sweep",
	);
	db.cleanup().await;
}

#[tokio::test]
async fn sweep_is_refused_while_the_treasury_is_not_provisioned_on_the_network() {
	let db = db_or_skip!();
	let secrets = WalletSecrets::new(db.pool.clone());
	let user = Uuid::new_v4();
	provision::provision(&test_vault(), &secrets, user, Network::Trc20).await.expect("provision a deposit wallet");
	let signer = Signer::new(test_vault(), secrets, policy(&[]));

	let status = denied(signer.sign_trc20_transfer(trc20(user, USDT_TRC20, OTHER_TRON, 1, 1_000_000)).await, "sweep with no treasury");
	assert!(status.message().contains("not provisioned"), "{status:?}");
	db.cleanup().await;
}

#[tokio::test]
async fn deposit_wallet_never_signs_a_native_transfer() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Polygon, policy(&[])).await;
	let ton_rail = Rail::new(&db, Network::Ton, policy(&[])).await;
	let tron_rail = Rail::new(&db, Network::Trc20, policy(&[])).await;

	// Not even to the treasury: a sweep moves USDT, and its gas arrives FROM the station.
	denied(
		rail.signer.sign_native_transfer(native(rail.user, "polygon", 137, &rail.treasury_address, 1, GWEI, 21_000)).await,
		"evm native from deposit",
	);
	denied(tron_rail.signer.sign_trx_transfer(trx(tron_rail.user, &tron_rail.treasury_address, 1)).await, "trx from deposit");
	denied(ton_rail.signer.sign_ton_transfer(ton(ton_rail.user, &ton_rail.treasury_address, 1)).await, "ton from deposit");
	// And unprovisioned: the refusal is the rule's, not the missing key's.
	denied(
		rail.signer.sign_native_transfer(native(Uuid::new_v4(), "polygon", 137, OTHER_EVM, 1, GWEI, 21_000)).await,
		"unprovisioned native from deposit",
	);
	db.cleanup().await;
}

#[tokio::test]
async fn ton_sweep_returns_excess_to_the_station_the_treasury_or_itself() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Ton, policy(&[])).await;
	let treasury_base64 = base64_of(&rail.treasury_address);

	// The hub's shape: swept to the treasury (base64), excess back to the gas station.
	rail.signer
		.sign_jetton_transfer(jetton(rail.user, &treasury_base64, &rail.gas_station_address, 1, 100_000_000))
		.await
		.expect("a sweep returning excess to the gas station is signed");
	rail.signer
		.sign_jetton_transfer(jetton(rail.user, &rail.treasury_address, &treasury_base64, 1, 100_000_000))
		.await
		.expect("a sweep returning excess to the treasury is signed");
	rail.signer
		.sign_jetton_transfer(jetton(rail.user, &rail.treasury_address, &base64_of(&rail.user_address), 1, 100_000_000))
		.await
		.expect("a sweep returning excess to the sender is signed");

	let status = denied(
		rail.signer.sign_jetton_transfer(jetton(rail.user, &rail.treasury_address, OTHER_TON, 1, 100_000_000)).await,
		"excess elsewhere",
	);
	assert!(status.message().contains("response_destination"), "{status:?}");
	denied(
		rail.signer.sign_jetton_transfer(jetton(rail.user, OTHER_TON, &rail.gas_station_address, 1, 100_000_000)).await,
		"jetton sweep elsewhere",
	);
	db.cleanup().await;
}

// === #183: the gas station only tops up our own addresses ======================

#[tokio::test]
async fn gas_topup_is_signed_to_a_held_address_and_refused_to_a_foreign_one() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;

	// The hub sends the address as the signer handed it out; lowercase is the same address.
	rail.signer
		.sign_native_transfer(native(GAS_STATION, "bep20", 56, &rail.user_address, 30_000_000_000_000_000, GWEI, 21_000))
		.await
		.expect("a top-up to a deposit wallet is signed");
	rail.signer
		.sign_native_transfer(native(GAS_STATION, "bep20", 56, &rail.user_address.to_ascii_lowercase(), 1, GWEI, 21_000))
		.await
		.expect("a top-up to a deposit wallet spelled lowercase is signed");

	let status = denied(
		rail.signer.sign_native_transfer(native(GAS_STATION, "bep20", 56, OTHER_EVM, 1, GWEI, 21_000)).await,
		"top-up to a foreign address",
	);
	assert!(status.message().contains("holds a key"), "{status:?}");

	// Over the drip cap (0.05 BNB), even to a held address.
	let status = denied(
		rail.signer
			.sign_native_transfer(native(GAS_STATION, "bep20", 56, &rail.user_address, 50_000_000_000_000_001, GWEI, 21_000))
			.await,
		"top-up over the drip cap",
	);
	assert!(status.message().contains("gas top-up"), "{status:?}");

	// A superseded (rotated-away) address is no longer held.
	let (rotated, rotated_address) = rail.another_user(Network::Bep20).await;
	assert!(rail.secrets.supersede(rotated, Network::Bep20).await.unwrap());
	denied(
		rail.signer.sign_native_transfer(native(GAS_STATION, "bep20", 56, &rotated_address, 1, GWEI, 21_000)).await,
		"top-up to a superseded address",
	);
	db.cleanup().await;
}

#[tokio::test]
async fn gas_topup_on_ton_and_tron_resolves_the_held_address_in_any_rendering() {
	let db = db_or_skip!();
	let ton_rail = Rail::new(&db, Network::Ton, policy(&[])).await;
	let tron_rail = Rail::new(&db, Network::Trc20, policy(&[])).await;

	// TON: the signer stores raw `0:<hex>`, the hub may send base64.
	ton_rail
		.signer
		.sign_ton_transfer(ton(GAS_STATION, &base64_of(&ton_rail.user_address), 150_000_000))
		.await
		.expect("ton top-up (base64) is signed");
	ton_rail
		.signer
		.sign_ton_transfer(ton(GAS_STATION, &ton_rail.user_address, 1))
		.await
		.expect("ton top-up (raw) is signed");
	denied(ton_rail.signer.sign_ton_transfer(ton(GAS_STATION, OTHER_TON, 1)).await, "ton top-up to a foreign address");
	denied(
		ton_rail.signer.sign_ton_transfer(ton(GAS_STATION, &ton_rail.user_address, 200_000_001)).await,
		"ton top-up over the drip cap",
	);

	tron_rail
		.signer
		.sign_trx_transfer(trx(GAS_STATION, &tron_rail.user_address, 30_000_000))
		.await
		.expect("trx top-up is signed");
	denied(tron_rail.signer.sign_trx_transfer(trx(GAS_STATION, OTHER_TRON, 1)).await, "trx top-up to a foreign address");
	denied(
		tron_rail.signer.sign_trx_transfer(trx(GAS_STATION, &tron_rail.user_address, 50_000_001)).await,
		"trx top-up over the drip cap",
	);
	db.cleanup().await;
}

#[tokio::test]
async fn gas_station_never_signs_a_token_transfer() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;
	let ton_rail = Rail::new(&db, Network::Ton, policy(&[])).await;
	let tron_rail = Rail::new(&db, Network::Trc20, policy(&[])).await;

	// Not even to the treasury, and however small.
	let status = denied(
		rail.signer.sign_erc20_transfer(erc20(GAS_STATION, USDT_BEP20, &rail.treasury_address, 1, GWEI, 60_000)).await,
		"erc20 from the gas station",
	);
	assert!(status.message().contains("gas station"), "{status:?}");
	denied(
		tron_rail
			.signer
			.sign_trc20_transfer(trc20(GAS_STATION, USDT_TRC20, &tron_rail.treasury_address, 1, 1_000_000))
			.await,
		"trc20 from the gas station",
	);
	denied(
		ton_rail
			.signer
			.sign_jetton_transfer(jetton(GAS_STATION, &ton_rail.treasury_address, &ton_rail.gas_station_address, 1, 100_000_000))
			.await,
		"jetton from the gas station",
	);
	db.cleanup().await;
}

// === #183: the treasury allowlist is rendering-aware ===========================

#[tokio::test]
async fn treasury_allowlist_accepts_the_listed_address_in_another_rendering() {
	let db = db_or_skip!();
	// Listed lowercase; the hub sends EIP-55 (and vice versa).
	let rail = Rail::new(&db, Network::Bep20, policy(&[("SIGNER_DESTINATION_ALLOWLIST", &OTHER_EVM.to_ascii_lowercase())])).await;

	let eip55 = "0x024DA544A76714a3812096e9EF84D40b2C8863E8";
	assert_ne!(eip55, OTHER_EVM);
	rail.signer
		.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, eip55, 1, GWEI, 60_000))
		.await
		.expect("an allowlisted destination in another rendering is signed");
	denied(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, &rail.user_address, 1, GWEI, 60_000)).await,
		"unlisted destination",
	);

	// TON: listed base64, sent raw. The jetton wallet is pinned by the operator here — the
	// intended posture (the allowlist has no say over it either way).
	let raw = tonlib_core::TonAddress::from_str(OTHER_TON).unwrap().to_hex();
	let pinned = policy(&[("SIGNER_DESTINATION_ALLOWLIST", OTHER_TON), ("SIGNER_TON_TREASURY_JETTON_WALLET", JETTON_WALLET)]);
	let ton_rail = Rail::new(&db, Network::Ton, pinned).await;
	let treasury_base64 = base64_of(&ton_rail.treasury_address);
	ton_rail
		.signer
		.sign_jetton_transfer(jetton(TREASURY, &raw, &treasury_base64, 1, 100_000_000))
		.await
		.expect("an allowlisted TON destination in the raw rendering is signed");
	denied(
		ton_rail
			.signer
			.sign_jetton_transfer(jetton(TREASURY, &ton_rail.user_address, &treasury_base64, 1, 100_000_000))
			.await,
		"unlisted TON destination",
	);
	db.cleanup().await;
}

// === #184: native out of the treasury is off by default; the token is pinned ==

#[tokio::test]
async fn treasury_native_is_refused_by_default_on_every_rail() {
	let db = db_or_skip!();
	// Nothing provisioned at all: the refusal is the rule's, before any key is looked up.
	let signer = Signer::new(test_vault(), WalletSecrets::new(db.pool.clone()), policy(&[]));

	let status = denied(
		signer.sign_native_transfer(native(TREASURY, "polygon", 137, OTHER_EVM, 1, GWEI, 21_000)).await,
		"evm native from the treasury",
	);
	assert!(status.message().contains("SIGNER_ALLOW_TREASURY_NATIVE"), "{status:?}");
	denied(
		signer.sign_native_transfer(native(TREASURY, "bep20", 56, OTHER_EVM, 1, GWEI, 21_000)).await,
		"bsc native from the treasury",
	);
	denied(signer.sign_trx_transfer(trx(TREASURY, OTHER_TRON, 1)).await, "trx from the treasury");
	denied(signer.sign_ton_transfer(ton(TREASURY, OTHER_TON, 1)).await, "ton from the treasury");
	db.cleanup().await;
}

#[tokio::test]
async fn treasury_native_opted_in_signs_to_the_allowlist_under_the_ceiling() {
	let db = db_or_skip!();
	let opted_in = policy(&[
		("SIGNER_ALLOW_TREASURY_NATIVE", "true"),
		("SIGNER_TREASURY_NATIVE_ALLOWLIST", OTHER_EVM),
		("SIGNER_MAX_TREASURY_NATIVE_POLYGON", "1000000000000000000"),
	]);
	let rail = Rail::new(&db, Network::Polygon, opted_in.clone()).await;
	let bsc = Rail::new(&db, Network::Bep20, opted_in).await;

	// Allowlisted (in another rendering) and at the ceiling: signed.
	rail.signer
		.sign_native_transfer(native(
			TREASURY,
			"polygon",
			137,
			"0x024DA544A76714a3812096e9EF84D40b2C8863E8",
			1_000_000_000_000_000_000,
			GWEI,
			21_000,
		))
		.await
		.expect("an opted-in treasury native transfer is signed");
	let status = denied(
		rail.signer
			.sign_native_transfer(native(TREASURY, "polygon", 137, OTHER_EVM, 1_000_000_000_000_000_001, GWEI, 21_000))
			.await,
		"treasury native over the ceiling",
	);
	assert!(status.message().contains("treasury native"), "{status:?}");
	denied(
		rail.signer.sign_native_transfer(native(TREASURY, "polygon", 137, &rail.user_address, 1, GWEI, 21_000)).await,
		"treasury native off the allowlist",
	);
	// The flow is on, but BSC has no ceiling: still refused there.
	let status = denied(
		bsc.signer.sign_native_transfer(native(TREASURY, "bep20", 56, OTHER_EVM, 1, GWEI, 21_000)).await,
		"treasury native on an uncapped rail",
	);
	assert!(status.message().contains("SIGNER_MAX_TREASURY_NATIVE"), "{status:?}");
	// The native allowlist is not the USDT one: with SIGNER_DESTINATION_ALLOWLIST unset, a USDT
	// payout to an arbitrary address — a user's withdrawal — is still signed.
	let mut payout = erc20(TREASURY, "0xc2132D05D31c914a87C6611C10748AEb04B58e8F", &rail.user_address, 1, GWEI, 60_000).into_inner();
	payout.network = "polygon".to_owned();
	payout.chain_id = 137;
	rail.signer
		.sign_erc20_transfer(Request::new(payout))
		.await
		.expect("a USDT withdrawal to an address only the native list would refuse is signed");
	db.cleanup().await;
}

#[tokio::test]
async fn treasury_token_transfer_must_name_the_pinned_usdt_contract() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;
	let tron_rail = Rail::new(&db, Network::Trc20, policy(&[])).await;

	rail.signer
		.sign_erc20_transfer(erc20(TREASURY, &USDT_BEP20.to_ascii_lowercase(), OTHER_EVM, 1, GWEI, 60_000))
		.await
		.expect("the pinned token in another rendering is signed");
	let status = denied(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, OTHER_EVM, OTHER_EVM, 1, GWEI, 60_000)).await,
		"another token from the treasury",
	);
	assert!(status.message().contains("token_contract"), "{status:?}");
	denied(
		tron_rail.signer.sign_trc20_transfer(trc20(TREASURY, OTHER_TRON, OTHER_TRON, 1, 1_000_000)).await,
		"another token from the treasury on tron",
	);
	tron_rail
		.signer
		.sign_trc20_transfer(trc20(TREASURY, USDT_TRC20, OTHER_TRON, 1, 1_000_000))
		.await
		.expect("the pinned TRC20 token is signed");

	// A sweep is not pinned: its destination is the treasury, whatever token it moves.
	rail.signer
		.sign_erc20_transfer(erc20(rail.user, OTHER_EVM, &rail.treasury_address, 1, GWEI, 60_000))
		.await
		.expect("a sweep of any token into the treasury is signed");
	db.cleanup().await;
}

// === #369: native spend is bounded per wallet over a sliding hour ==============

#[tokio::test]
async fn native_spend_window_admits_n_signatures_then_refuses_the_next_from_that_wallet() {
	let db = db_or_skip!();
	// One top-up commits 21_000 wei of gas (1 wei × 21_000) + 1 wei of value; three fit exactly.
	let per_topup: u128 = 21_000 + 1;
	let cap = (per_topup * 3).to_string();
	let rail = Rail::new(&db, Network::Bep20, policy(&[("SIGNER_MAX_NATIVE_SPEND_PER_HOUR_BEP20", &cap)])).await;

	for n in 1..=3 {
		rail.signer
			.sign_native_transfer(native(GAS_STATION, "bep20", 56, &rail.user_address, 1, 1, 21_000))
			.await
			.unwrap_or_else(|status| panic!("top-up {n} of 3 within the window must be signed: {status:?}"));
	}
	let status = denied(
		rail.signer.sign_native_transfer(native(GAS_STATION, "bep20", 56, &rail.user_address, 1, 1, 21_000)).await,
		"the fourth top-up",
	);
	assert!(status.message().contains("native spend window"), "{status:?}");
	// Even the smallest possible signature from that wallet: the window is spent, not "nearly".
	denied(
		rail.signer.sign_native_transfer(native(GAS_STATION, "bep20", 56, &rail.user_address, 0, 1, 1)).await,
		"a 1-wei signature after the window is spent",
	);

	// Another wallet on the same rail has its own window: a sweep from the deposit wallet
	// (21_000 × 1 wei of gas, no value) is signed.
	rail.signer
		.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, &rail.treasury_address, 1, 1, 21_000))
		.await
		.expect("another wallet's window is untouched");
	// And the same wallet on another rail too.
	let polygon = Rail::new(&db, Network::Polygon, policy(&[("SIGNER_MAX_NATIVE_SPEND_PER_HOUR_BEP20", &cap)])).await;
	polygon
		.signer
		.sign_native_transfer(native(GAS_STATION, "polygon", 137, &polygon.user_address, 1, 1, 21_000))
		.await
		.expect("the gas station's Polygon window is untouched");
	db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_spend_window_holds_under_concurrent_requests_on_one_wallet() {
	let db = db_or_skip!();
	// Room for exactly three; ten requests race for it. Without the ledger's lock several
	// could read the window as open at once and all be signed.
	let per_topup: u128 = 21_000 + 1;
	let cap = (per_topup * 3).to_string();
	let rail = Rail::new(&db, Network::Bep20, policy(&[("SIGNER_MAX_NATIVE_SPEND_PER_HOUR_BEP20", &cap)])).await;
	let signer = std::sync::Arc::new(rail.signer);

	let mut set = tokio::task::JoinSet::new();
	for _ in 0..10 {
		let signer = std::sync::Arc::clone(&signer);
		let to = rail.user_address.clone();
		set.spawn(async move { signer.sign_native_transfer(native(GAS_STATION, "bep20", 56, &to, 1, 1, 21_000)).await.map(|_| ()) });
	}
	let mut signed = 0;
	while let Some(outcome) = set.join_next().await {
		match outcome.expect("task panicked") {
			Ok(()) => signed += 1,
			Err(status) => assert_eq!(status.code(), Code::PermissionDenied, "{status:?}"),
		}
	}
	assert_eq!(signed, 3, "exactly the window's worth is signed, however the requests interleave");
	db.cleanup().await;
}

#[tokio::test]
async fn native_spend_window_refuses_before_any_key_is_touched() {
	let db = db_or_skip!();
	// A cap below what a single ceiling-priced sweep commits: refused on the window, and from
	// a wallet that was never provisioned — PermissionDenied, not FailedPrecondition.
	let tight = policy(&[
		("SIGNER_MAX_NATIVE_SPEND_PER_HOUR_BEP20", "1"),
		("SIGNER_MAX_NATIVE_SPEND_PER_HOUR_TRC20", "1"),
		("SIGNER_MAX_NATIVE_SPEND_PER_HOUR_TON", "1"),
	]);
	let rail = Rail::new(&db, Network::Bep20, tight.clone()).await;
	let tron_rail = Rail::new(&db, Network::Trc20, tight.clone()).await;

	let status = denied(
		rail.signer.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, &rail.treasury_address, 1, GWEI, 60_000)).await,
		"sweep over the window",
	);
	assert!(status.message().contains("native spend window"), "{status:?}");
	denied(
		tron_rail
			.signer
			.sign_trc20_transfer(trc20(tron_rail.user, USDT_TRC20, &tron_rail.treasury_address, 1, 1_000_000))
			.await,
		"trc20 sweep over the window",
	);
	// TON native from the gas station to a held address, with the window at 1 nanoton.
	let ton_rail = Rail::new(&db, Network::Ton, tight.clone()).await;
	denied(ton_rail.signer.sign_ton_transfer(ton(GAS_STATION, &ton_rail.user_address, 2)).await, "ton top-up over the window");
	// A signer with NOTHING provisioned: a treasury payout passes every other rule (pinned
	// token, no cap, no allowlist) and is refused on the window before the key lookup that
	// would otherwise fail on the missing treasury.
	let bare = Signer::new(test_vault(), WalletSecrets::new(db.pool.clone()), tight);
	let status = denied(
		bare.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000)).await,
		"unprovisioned treasury over the window",
	);
	assert!(status.message().contains("native spend window"), "{status:?}");
	db.cleanup().await;
}

#[tokio::test]
async fn native_spend_window_counts_fees_of_token_transfers_too() {
	let db = db_or_skip!();
	// Two ceiling-priced sweeps' worth of gas: the third sweep from the same wallet is refused
	// although it moves no native value at all — the fee is the spend.
	let fee: u128 = 100 * GWEI * 100_000;
	let rail = Rail::new(&db, Network::Bep20, policy(&[("SIGNER_MAX_NATIVE_SPEND_PER_HOUR_BEP20", &(fee * 2).to_string())])).await;
	for _ in 0..2 {
		rail.signer
			.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, &rail.treasury_address, 1, 100 * GWEI, 100_000))
			.await
			.expect("a sweep within the window is signed");
	}
	denied(
		rail.signer
			.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, &rail.treasury_address, 1, 100 * GWEI, 100_000))
			.await,
		"a third ceiling-priced sweep",
	);
	// A cheaper one still fits nothing: the window is exactly full.
	denied(
		rail.signer.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, &rail.treasury_address, 1, 1, 21_000)).await,
		"any further sweep",
	);
	db.cleanup().await;
}

// === review: jetton wallets are pinned on first use ================================

#[tokio::test]
async fn first_sweep_pins_the_jetton_wallet_and_later_sweeps_are_held_to_it() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Ton, policy(&[])).await;
	let to = &rail.treasury_address;
	let excess = &rail.gas_station_address;
	assert_eq!(pinned_jetton_wallet(&db, rail.user).await, None);

	// Whatever the first sweep names is learned — in the stored raw rendering.
	rail.signer
		.sign_jetton_transfer(jetton_via(rail.user, &base64_of(OTHER_JETTON_WALLET), to, excess))
		.await
		.expect("the first sweep is signed and pins its jetton wallet");
	assert_eq!(pinned_jetton_wallet(&db, rail.user).await.as_deref(), Some(OTHER_JETTON_WALLET));

	// From then on only that address, in either rendering.
	let status = denied(
		rail.signer.sign_jetton_transfer(jetton_via(rail.user, JETTON_WALLET, to, excess)).await,
		"a sweep via another jetton wallet",
	);
	assert!(status.message().contains("first use"), "{status:?}");
	rail.signer
		.sign_jetton_transfer(jetton_via(rail.user, OTHER_JETTON_WALLET, to, excess))
		.await
		.expect("the pinned jetton wallet in the raw rendering is signed");
	rail.signer
		.sign_jetton_transfer(jetton_via(rail.user, &base64_of(OTHER_JETTON_WALLET), to, excess))
		.await
		.expect("the pinned jetton wallet in the base64 rendering is signed");
	// The refusal did not move the pin.
	assert_eq!(pinned_jetton_wallet(&db, rail.user).await.as_deref(), Some(OTHER_JETTON_WALLET));

	// Every wallet has its own pin: another deposit wallet's first sweep names a different one.
	let (other, _) = rail.another_user(Network::Ton).await;
	rail.signer
		.sign_jetton_transfer(jetton_via(other, JETTON_WALLET, to, excess))
		.await
		.expect("another wallet pins its own jetton wallet");
	assert_eq!(pinned_jetton_wallet(&db, other).await.as_deref(), Some(JETTON_WALLET));
	db.cleanup().await;
}

#[tokio::test]
async fn treasury_operator_pin_outranks_the_first_use_table() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Ton, policy(&[("SIGNER_TON_TREASURY_JETTON_WALLET", JETTON_WALLET)])).await;
	let excess = &base64_of(&rail.treasury_address);
	// A first-use row already exists for the treasury, naming a different jetton wallet.
	sqlx::query("INSERT INTO jetton_wallets (wallet_id, network, jetton_wallet) VALUES ($1, 'ton', $2)")
		.bind(TREASURY)
		.bind(OTHER_JETTON_WALLET)
		.execute(&db.pool)
		.await
		.expect("seed a first-use row");

	// The operator's pin is what counts: the table's address is refused, the pin is signed.
	let status = denied(
		rail.signer.sign_jetton_transfer(jetton_via(TREASURY, OTHER_JETTON_WALLET, OTHER_TON, excess)).await,
		"the table's jetton wallet against an operator pin",
	);
	assert!(status.message().contains("SIGNER_TON_TREASURY_JETTON_WALLET"), "{status:?}");
	rail.signer
		.sign_jetton_transfer(jetton_via(TREASURY, &base64_of(JETTON_WALLET), OTHER_TON, excess))
		.await
		.expect("the operator's pin is signed");
	// And the table was neither consulted nor rewritten.
	assert_eq!(pinned_jetton_wallet(&db, TREASURY).await.as_deref(), Some(OTHER_JETTON_WALLET));
	db.cleanup().await;
}

#[tokio::test]
async fn unpinned_treasury_pins_on_first_use_and_the_allowlist_has_no_say() {
	let db = db_or_skip!();
	// The production posture today: no operator pin. An allowlist that does NOT list the
	// jetton wallet used to refuse every TON withdrawal; it is not the jetton wallet's gate.
	let rail = Rail::new(&db, Network::Ton, policy(&[("SIGNER_DESTINATION_ALLOWLIST", OTHER_TON)])).await;
	let excess = &base64_of(&rail.treasury_address);

	rail.signer
		.sign_jetton_transfer(jetton_via(TREASURY, JETTON_WALLET, OTHER_TON, excess))
		.await
		.expect("the treasury's first withdrawal pins its jetton wallet");
	assert_eq!(pinned_jetton_wallet(&db, TREASURY).await.as_deref(), Some(JETTON_WALLET));
	let status = denied(
		rail.signer.sign_jetton_transfer(jetton_via(TREASURY, OTHER_JETTON_WALLET, OTHER_TON, excess)).await,
		"a treasury withdrawal via another jetton wallet",
	);
	assert!(status.message().contains("first use"), "{status:?}");
	db.cleanup().await;
}

#[tokio::test]
async fn a_request_refused_before_the_pin_learns_nothing() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Ton, policy(&[])).await;
	let to = &rail.treasury_address;
	let excess = &rail.gas_station_address;

	// Not a TON address: malformed, never pinned (pinning it would refuse every later sweep).
	let status = rail
		.signer
		.sign_jetton_transfer(jetton_via(rail.user, "not-an-address", to, excess))
		.await
		.expect_err("garbage is not pinned");
	assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
	// Refused on a cheaper rule (a sweep elsewhere): the jetton wallet it named is not learned.
	denied(
		rail.signer.sign_jetton_transfer(jetton_via(rail.user, OTHER_JETTON_WALLET, OTHER_TON, excess)).await,
		"a sweep elsewhere",
	);
	assert_eq!(pinned_jetton_wallet(&db, rail.user).await, None);
	// So the legitimate first sweep still gets to pin.
	rail.signer
		.sign_jetton_transfer(jetton_via(rail.user, JETTON_WALLET, to, excess))
		.await
		.expect("the first legitimate sweep pins");
	assert_eq!(pinned_jetton_wallet(&db, rail.user).await.as_deref(), Some(JETTON_WALLET));
	db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_sweeps_pin_exactly_one_jetton_wallet() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Ton, policy(&[])).await;
	let signer = std::sync::Arc::new(rail.signer);

	// Eight first sweeps race, naming two different jetton wallets. Whichever wins the INSERT
	// is the pin; every signature must have named it.
	let mut set = tokio::task::JoinSet::new();
	for n in 0..8 {
		let signer = std::sync::Arc::clone(&signer);
		let (to, excess) = (rail.treasury_address.clone(), rail.gas_station_address.clone());
		let via = if n % 2 == 0 { JETTON_WALLET } else { OTHER_JETTON_WALLET };
		set.spawn(async move { signer.sign_jetton_transfer(jetton_via(rail.user, via, &to, &excess)).await.map(|_| via) });
	}
	let mut signed_via = Vec::new();
	while let Some(outcome) = set.join_next().await {
		match outcome.expect("task panicked") {
			Ok(via) => signed_via.push(via),
			Err(status) => assert_eq!(status.code(), Code::PermissionDenied, "{status:?}"),
		}
	}
	let pinned = pinned_jetton_wallet(&db, rail.user).await.expect("one jetton wallet was pinned");
	assert!(!signed_via.is_empty(), "the winner's own sweep is signed");
	assert!(signed_via.iter().all(|via| *via == pinned), "every signature named the pin {pinned}: {signed_via:?}");
	db.cleanup().await;
}

#[tokio::test]
async fn migrating_a_wallet_to_the_custodian_retires_its_jetton_pin() {
	let db = db_or_skip!();
	let secrets = WalletSecrets::new(db.pool.clone());
	let user = Uuid::new_v4();
	let old_address = provision::provision(&test_vault(), &secrets, user, Network::Ton)
		.await
		.expect("provision a deposit wallet")
		.address;
	let treasury = provision::provision(&test_vault(), &secrets, TREASURY, Network::Ton)
		.await
		.expect("provision the treasury")
		.address;
	let station = provision::provision(&test_vault(), &secrets, GAS_STATION, Network::Ton)
		.await
		.expect("provision the gas station")
		.address;
	let signer = TonCustody::signer(secrets.clone(), policy(&[]));

	// The first sweep from the local key pins its jetton wallet.
	signer
		.sign_jetton_transfer(jetton_via(user, JETTON_WALLET, &treasury, &station))
		.await
		.expect("the first sweep pins");
	assert_eq!(pinned_jetton_wallet(&db, user).await.as_deref(), Some(JETTON_WALLET));

	// The custody-held replacement has a new address, hence a new jetton wallet: the pin goes
	// with the archived row.
	let migrated = signer
		.migrate_address_to_custodian(Request::new(MigrateAddressToCustodianRequest {
			user_id: user.to_string(),
			network: "ton".to_owned(),
			drained_address: base64_of(&old_address),
		}))
		.await
		.expect("a healthy local key migrates")
		.into_inner();
	assert_ne!(migrated.new_address, old_address);
	assert_eq!(pinned_jetton_wallet(&db, user).await, None, "the pin was retired with the row");

	// So the new key's first sweep, naming a different jetton wallet, is signed and pins afresh
	// — where a surviving pin would have refused it forever.
	signer
		.sign_jetton_transfer(jetton_via(user, OTHER_JETTON_WALLET, &treasury, &station))
		.await
		.expect("the migrated wallet's first sweep pins its new jetton wallet");
	assert_eq!(pinned_jetton_wallet(&db, user).await.as_deref(), Some(OTHER_JETTON_WALLET));
	denied(
		signer.sign_jetton_transfer(jetton_via(user, JETTON_WALLET, &treasury, &station)).await,
		"the old jetton wallet after migration",
	);
	db.cleanup().await;
}

#[tokio::test]
async fn rotating_a_dead_key_retires_its_jetton_pin() {
	let db = db_or_skip!();
	let secrets = WalletSecrets::new(db.pool.clone());
	let user = Uuid::new_v4();
	// A key sealed under another KEK: dead under the signer's, so it cannot sweep — the pin it
	// learned while alive is seeded directly.
	let old_address = provision::provision(&other_vault(), &secrets, user, Network::Ton)
		.await
		.expect("provision a dead deposit wallet")
		.address;
	let treasury = provision::provision(&test_vault(), &secrets, TREASURY, Network::Ton)
		.await
		.expect("provision the treasury")
		.address;
	let station = provision::provision(&test_vault(), &secrets, GAS_STATION, Network::Ton)
		.await
		.expect("provision the gas station")
		.address;
	sqlx::query("INSERT INTO jetton_wallets (wallet_id, network, jetton_wallet) VALUES ($1, 'ton', $2)")
		.bind(user)
		.bind(JETTON_WALLET)
		.execute(&db.pool)
		.await
		.expect("seed the pin the dead key learned");
	let signer = Signer::new(test_vault(), secrets.clone(), policy(&[]));

	let rotated = signer
		.rotate_address(Request::new(RotateAddressRequest {
			user_id: user.to_string(),
			network: "ton".to_owned(),
		}))
		.await
		.expect("a dead key rotates")
		.into_inner();
	assert_ne!(rotated.address, old_address);
	assert_eq!(pinned_jetton_wallet(&db, user).await, None, "the pin was retired with the row");

	signer
		.sign_jetton_transfer(jetton_via(user, OTHER_JETTON_WALLET, &treasury, &station))
		.await
		.expect("the rotated wallet's first sweep pins its new jetton wallet");
	assert_eq!(pinned_jetton_wallet(&db, user).await.as_deref(), Some(OTHER_JETTON_WALLET));
	db.cleanup().await;
}

// === review: treasury USDT is capped per payout AND per hour ======================

#[tokio::test]
async fn treasury_usdt_window_admits_ten_payouts_at_the_cap_then_refuses_the_eleventh() {
	let db = db_or_skip!();
	// The real defaults: 100 USDT per payout, 1_000 USDT per hour — nothing configured.
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;
	let hundred_usdt: u128 = 100 * 1_000_000_000_000_000_000;

	for n in 1..=10 {
		rail.signer
			.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, hundred_usdt, GWEI, 60_000))
			.await
			.unwrap_or_else(|status| panic!("payout {n} of 10 within the hour must be signed: {status:?}"));
	}
	let status = denied(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, hundred_usdt, GWEI, 60_000)).await,
		"the eleventh payout",
	);
	assert!(status.message().contains("treasury USDT window"), "{status:?}");
	// Even the smallest payout: the hour is spent, and the per-transfer cap alone (100 USDT)
	// would have admitted it.
	denied(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000)).await,
		"a 1-unit payout after the hour is spent",
	);
	// A payout over the per-transfer cap is refused on that cap, before the window is even
	// consulted — the message names the cap, not the window.
	let status = denied(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, hundred_usdt + 1, GWEI, 60_000)).await,
		"a payout over the per-transfer cap",
	);
	assert!(status.message().contains("per-transfer cap of 100 USDT"), "{status:?}");

	// The USDT window is per rail: the treasury's Polygon hour is untouched.
	let polygon = Rail::new(&db, Network::Polygon, policy(&[])).await;
	// Polygon USDT is 6-dp, unlike BSC: 100 USDT is 1e8 base units there.
	let mut req = erc20(TREASURY, "0xc2132D05D31c914a87C6611C10748AEb04B58e8F", OTHER_EVM, 100_000_000, GWEI, 60_000).into_inner();
	req.network = "polygon".to_owned();
	req.chain_id = 137;
	polygon.signer.sign_erc20_transfer(Request::new(req)).await.expect("the treasury's Polygon hour is untouched");
	// And a sweep INTO the treasury on the spent rail is not a payout: signed.
	rail.signer
		.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, &rail.treasury_address, hundred_usdt, GWEI, 60_000))
		.await
		.expect("a sweep into the treasury is not charged to its payout window");
	db.cleanup().await;
}

#[tokio::test]
async fn treasury_usdt_window_is_raised_by_its_variable_and_counted_in_chain_precision() {
	let db = db_or_skip!();
	// 6-dp Tron: 30 USDT per hour is 30_000_000 base units; 12 USDT payouts, two and a half fit.
	let rail = Rail::new(&db, Network::Trc20, policy(&[("SIGNER_MAX_TREASURY_USDT_PER_HOUR", "30")])).await;
	for _ in 0..2 {
		rail.signer
			.sign_trc20_transfer(trc20(TREASURY, USDT_TRC20, OTHER_TRON, 12_000_000, 1_000_000))
			.await
			.expect("a payout within the raised hour is signed");
	}
	denied(
		rail.signer.sign_trc20_transfer(trc20(TREASURY, USDT_TRC20, OTHER_TRON, 12_000_000, 1_000_000)).await,
		"a third 12 USDT payout against a 30 USDT hour",
	);
	rail.signer
		.sign_trc20_transfer(trc20(TREASURY, USDT_TRC20, OTHER_TRON, 6_000_000, 1_000_000))
		.await
		.expect("the remaining 6 USDT of the hour is signed");
	db.cleanup().await;
}

// === review: a failed signature gives its reservations back =======================

#[tokio::test]
async fn a_backend_failure_releases_the_windows_it_charged() {
	let db = db_or_skip!();
	// Tron: a deposit wallet but NO gas station provisioned, and room for exactly two drips.
	let secrets = WalletSecrets::new(db.pool.clone());
	let user = Uuid::new_v4();
	let user_address = provision::provision(&test_vault(), &secrets, user, Network::Trc20)
		.await
		.expect("provision a deposit wallet")
		.address;
	let signer = Signer::new(test_vault(), secrets.clone(), policy(&[("SIGNER_MAX_NATIVE_SPEND_PER_HOUR_TRC20", "60000000")]));

	// Every policy check passes (the destination IS held), the window is charged, and then the
	// backend has no key for the station: the charge must not survive that.
	let status = signer
		.sign_trx_transfer(trx(GAS_STATION, &user_address, 30_000_000))
		.await
		.expect_err("no key for the gas station");
	assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
	assert_eq!(ledger_rows(&db, GAS_STATION, "native").await, 0, "the failed attempt was released");

	provision::provision(&test_vault(), &secrets, GAS_STATION, Network::Trc20)
		.await
		.expect("provision the gas station");
	for n in 1..=2 {
		signer
			.sign_trx_transfer(trx(GAS_STATION, &user_address, 30_000_000))
			.await
			.unwrap_or_else(|status| panic!("drip {n} of 2 must fit the window the failure gave back: {status:?}"));
	}
	denied(signer.sign_trx_transfer(trx(GAS_STATION, &user_address, 30_000_000)).await, "the third drip");
	assert_eq!(ledger_rows(&db, GAS_STATION, "native").await, 2);

	// EVM treasury payout: both windows (USDT and native) are charged before the signature,
	// and both are given back when the backend refuses.
	let bare = Signer::new(test_vault(), secrets.clone(), policy(&[("SIGNER_MAX_TREASURY_USDT_PER_HOUR", "100")]));
	let hundred_usdt: u128 = 100 * 1_000_000_000_000_000_000;
	let status = bare
		.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, hundred_usdt, GWEI, 60_000))
		.await
		.expect_err("no key for the treasury");
	assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
	assert_eq!(ledger_rows(&db, TREASURY, "usdt").await, 0);
	assert_eq!(ledger_rows(&db, TREASURY, "native").await, 0);
	provision::provision(&test_vault(), &secrets, TREASURY, Network::Bep20).await.expect("provision the treasury");
	bare.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, hundred_usdt, GWEI, 60_000))
		.await
		.expect("the whole hour is still available after the failed attempt");
	assert_eq!(ledger_rows(&db, TREASURY, "usdt").await, 1);
	assert_eq!(ledger_rows(&db, TREASURY, "native").await, 1);
	// A signature that succeeded keeps its rows: the next payout is refused on the hour.
	denied(
		bare.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000)).await,
		"a payout after the hour is spent",
	);
	db.cleanup().await;
}

#[tokio::test]
async fn a_refusal_on_the_second_window_releases_the_first() {
	let db = db_or_skip!();
	// The USDT window admits the payout; the native window (1 wei) refuses its gas. The USDT
	// charge made first must not stay behind.
	let rail = Rail::new(&db, Network::Bep20, policy(&[("SIGNER_MAX_NATIVE_SPEND_PER_HOUR_BEP20", "1")])).await;
	let status = denied(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000)).await,
		"a payout over the native window",
	);
	assert!(status.message().contains("native spend window"), "{status:?}");
	assert_eq!(ledger_rows(&db, TREASURY, "usdt").await, 0, "the USDT charge was released");
	assert_eq!(ledger_rows(&db, TREASURY, "native").await, 0);
	db.cleanup().await;
}

// === review: the top-up gate's address lookup is indexed ==========================

#[tokio::test]
async fn held_address_lookup_uses_the_active_address_index() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;
	// The planner would seq-scan a three-row table whatever the index; forbidding that shows
	// whether the query's expression matches the index at all (migration 0008).
	let mut conn = db.pool.acquire().await.expect("a connection");
	sqlx::query("SET enable_seqscan = off").execute(&mut *conn).await.expect("disable seq scans");
	let plan: Vec<String> = sqlx::query_scalar("EXPLAIN SELECT address FROM wallet_secrets WHERE network = $1 AND superseded_at IS NULL AND lower(address) = lower($2)")
		.bind("bep20")
		.bind(rail.user_address.to_ascii_lowercase())
		.fetch_all(&mut *conn)
		.await
		.expect("explain the lookup");
	let plan = plan.join("\n");
	assert!(plan.contains("wallet_secrets_active_network_lower_address"), "{plan}");
	drop(conn);
	// And the lookup still finds the EIP-55 row from a lowercase spelling, and only active rows.
	assert_eq!(
		rail.secrets
			.find_active_by_address(Network::Bep20, &rail.user_address.to_ascii_lowercase())
			.await
			.unwrap()
			.as_deref(),
		Some(rail.user_address.as_str())
	);
	assert!(rail.secrets.supersede(rail.user, Network::Bep20).await.unwrap());
	assert_eq!(rail.secrets.find_active_by_address(Network::Bep20, &rail.user_address).await.unwrap(), None);
	db.cleanup().await;
}

// === #369: Tron is frozen by default =============================================

#[tokio::test]
async fn tron_handlers_refuse_everything_while_signing_is_disabled() {
	let db = db_or_skip!();
	// The REAL default — not the test policy — and a fully provisioned rail, so nothing but
	// the freeze can be what refuses.
	let rail = Rail::new(&db, Network::Trc20, SignerPolicy::default()).await;

	let status = denied(
		rail.signer.sign_trc20_transfer(trc20(rail.user, USDT_TRC20, &rail.treasury_address, 1, 1_000_000)).await,
		"a legitimate sweep while frozen",
	);
	assert!(status.message().contains("SIGNER_TRON_SIGNING_ENABLED"), "{status:?}");
	denied(rail.signer.sign_trx_transfer(trx(GAS_STATION, &rail.user_address, 1)).await, "a legitimate top-up while frozen");
	// Before any other check: a malformed request is refused on the freeze, not as malformed.
	let mut req = trc20(rail.user, USDT_TRC20, &rail.treasury_address, 1, -1).into_inner();
	req.network = "bogus".to_owned();
	denied(rail.signer.sign_trc20_transfer(Request::new(req)).await, "a malformed request while frozen");
	db.cleanup().await;
}

// === #194: the spend brake — an operator's SQL that tightens or halts, never raises ======

/// The operator's hand on the brake: one `UPDATE spend_brake SET <assignments>` — what
/// `docs/RUNBOOK-withdrawals.md` has an operator type at the signer's own database.
async fn set_brake(db: &common::TestDb, assignments: &str) {
	sqlx::query(AssertSqlSafe(format!("UPDATE spend_brake SET {assignments} WHERE id = 1")))
		.execute(&db.pool)
		.await
		.expect("update the spend brake");
}

fn internal<T>(result: Result<T, Status>, what: &str) -> Status {
	let status = match result {
		Ok(_) => panic!("{what}: expected a refusal, got a signature"),
		Err(status) => status,
	};
	assert_eq!(status.code(), Code::Internal, "{what}: {status:?}");
	status
}

#[tokio::test]
async fn spend_brake_tightens_the_per_transfer_cap_to_min_of_env_and_brake() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[("SIGNER_MAX_TRANSFER_USDT", "100")])).await;
	let usdt: u128 = 1_000_000_000_000_000_000;

	set_brake(&db, "max_transfer_usdt = 50").await;
	let status = denied(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 60 * usdt, GWEI, 60_000)).await,
		"60 USDT against a 50 USDT brake",
	);
	assert!(status.message().contains("per-transfer cap of 50 USDT"), "{status:?}");
	rail.signer
		.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 50 * usdt, GWEI, 60_000))
		.await
		.expect("50 USDT under the brake is signed");

	// A brake above env is not a raise: env stays the ceiling.
	set_brake(&db, "max_transfer_usdt = 500").await;
	let status = denied(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 150 * usdt, GWEI, 60_000)).await,
		"150 USDT against env 100 with a 500 USDT brake",
	);
	assert!(status.message().contains("per-transfer cap of 100 USDT"), "{status:?}");

	// NULL: the brake lets go and env alone applies.
	set_brake(&db, "max_transfer_usdt = NULL").await;
	rail.signer
		.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 100 * usdt, GWEI, 60_000))
		.await
		.expect("100 USDT signs again once the brake column is NULL");
	db.cleanup().await;
}

#[tokio::test]
async fn spend_brake_tightens_the_treasury_usdt_window() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[("SIGNER_MAX_TREASURY_USDT_PER_HOUR", "1000")])).await;
	let hundred_usdt: u128 = 100 * 1_000_000_000_000_000_000;

	set_brake(&db, "max_treasury_usdt_per_hour = 200").await;
	for n in 1..=2 {
		rail.signer
			.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, hundred_usdt, GWEI, 60_000))
			.await
			.unwrap_or_else(|status| panic!("payout {n} of 2 within the braked hour must be signed: {status:?}"));
	}
	let status = denied(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, hundred_usdt, GWEI, 60_000)).await,
		"the third payout against a 200 USDT braked hour",
	);
	assert!(status.message().contains("treasury USDT window cap of 200 USDT"), "{status:?}");
	db.cleanup().await;
}

#[tokio::test]
async fn spend_brake_tightens_the_native_spend_window_per_rail() {
	let db = db_or_skip!();
	// One ceiling-priced sweep's worth of gas on BEP20 (env admits a hundred of them).
	let fee: u128 = 100 * GWEI * 100_000;
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;
	set_brake(&db, &format!("max_native_spend_per_hour_bep20 = {fee}")).await;

	rail.signer
		.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, &rail.treasury_address, 1, 100 * GWEI, 100_000))
		.await
		.expect("the first ceiling-priced sweep fills the braked window exactly");
	let status = denied(
		rail.signer
			.sign_erc20_transfer(erc20(rail.user, USDT_BEP20, &rail.treasury_address, 1, 100 * GWEI, 100_000))
			.await,
		"the second ceiling-priced sweep against the braked BEP20 window",
	);
	assert!(status.message().contains("native spend window"), "{status:?}");

	// Polygon has no brake column set: env applies and the same two sweeps both sign.
	let polygon = Rail::new(&db, Network::Polygon, policy(&[])).await;
	for n in 1..=2 {
		let mut req = erc20(polygon.user, "0xc2132D05D31c914a87C6611C10748AEb04B58e8F", &polygon.treasury_address, 1, 100 * GWEI, 100_000).into_inner();
		req.network = "polygon".to_owned();
		req.chain_id = 137;
		polygon
			.signer
			.sign_erc20_transfer(Request::new(req))
			.await
			.unwrap_or_else(|status| panic!("Polygon sweep {n} of 2 is not braked: {status:?}"));
	}
	db.cleanup().await;
}

#[tokio::test]
async fn halted_brake_refuses_every_signature_before_any_key_is_touched() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;
	set_brake(&db, "halted = true, reason = 'drill'").await;

	// A wallet nothing was ever provisioned for, on every rail: a refusal that is
	// PermissionDenied naming the brake — not FailedPrecondition on the missing key — proves
	// the brake runs before the backend is consulted.
	let nobody = Uuid::new_v4();
	let halted = |result: Result<(), Status>, what: &str| {
		let status = denied(result, what);
		assert!(status.message().contains("spend brake"), "{what}: {status:?}");
		assert!(status.message().contains("drill"), "{what}: {status:?}");
	};
	halted(
		rail.signer
			.sign_erc20_transfer(erc20(nobody, USDT_BEP20, &rail.treasury_address, 1, GWEI, 60_000))
			.await
			.map(drop),
		"erc20",
	);
	halted(
		rail.signer.sign_native_transfer(native(nobody, "bep20", 56, &rail.user_address, 1, GWEI, 21_000)).await.map(drop),
		"native",
	);
	halted(rail.signer.sign_trc20_transfer(trc20(nobody, USDT_TRC20, OTHER_TRON, 1, 1_000_000)).await.map(drop), "trc20");
	halted(rail.signer.sign_trx_transfer(trx(nobody, OTHER_TRON, 1)).await.map(drop), "trx");
	halted(rail.signer.sign_jetton_transfer(jetton(nobody, OTHER_TON, OTHER_TON, 1, 100_000_000)).await.map(drop), "jetton");
	halted(rail.signer.sign_ton_transfer(ton(nobody, OTHER_TON, 1)).await.map(drop), "ton");
	// Before parsing, too: a malformed request is refused on the brake, not as malformed.
	let mut req = erc20(TREASURY, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000).into_inner();
	req.network = "bogus".to_owned();
	halted(rail.signer.sign_erc20_transfer(Request::new(req)).await.map(drop), "malformed while halted");
	// And a legitimate payout from the provisioned treasury: same verdict.
	halted(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000)).await.map(drop),
		"treasury payout while halted",
	);

	// Nothing was charged to any window on the way to those refusals.
	for wallet in [nobody, TREASURY, GAS_STATION] {
		assert_eq!(ledger_rows(&db, wallet, "native").await, 0, "no native charge while halted");
		assert_eq!(ledger_rows(&db, wallet, "usdt").await, 0, "no USDT charge while halted");
	}

	// Release: the very request that was refused now signs.
	set_brake(&db, "halted = false, reason = NULL").await;
	rail.signer
		.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000))
		.await
		.expect("the same payout signs once the brake is released");
	db.cleanup().await;
}

#[tokio::test]
async fn halted_brake_leaves_provisioning_and_health_working() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;
	set_brake(&db, "halted = true, reason = 'drill'").await;

	// Minting a key moves no money, and health must keep answering during a halt.
	let provisioned = rail
		.signer
		.provision_address(Request::new(ProvisionAddressRequest {
			user_id: Uuid::new_v4().to_string(),
			network: "polygon".to_owned(),
		}))
		.await
		.expect("provisioning is not gated by the brake")
		.into_inner();
	assert!(!provisioned.address.is_empty());
	let health = rail
		.signer
		.get_key_health(Request::new(GetKeyHealthRequest {}))
		.await
		.expect("health is not gated by the brake")
		.into_inner();
	assert_eq!(health.total_keys, 4, "three from the rail plus the one just minted");
	db.cleanup().await;
}

#[tokio::test]
async fn a_missing_spend_brake_row_refuses_every_signature() {
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;
	sqlx::query("DELETE FROM spend_brake").execute(&db.pool).await.expect("delete the brake row");

	// A deleted row is NOT a release: the signer cannot tell it from a broken database and
	// fails closed on every handler.
	internal(rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000)).await.map(drop), "erc20");
	internal(
		rail.signer
			.sign_native_transfer(native(GAS_STATION, "bep20", 56, &rail.user_address, 1, GWEI, 21_000))
			.await
			.map(drop),
		"native",
	);
	internal(rail.signer.sign_trc20_transfer(trc20(TREASURY, USDT_TRC20, OTHER_TRON, 1, 1_000_000)).await.map(drop), "trc20");
	internal(rail.signer.sign_trx_transfer(trx(GAS_STATION, OTHER_TRON, 1)).await.map(drop), "trx");
	internal(rail.signer.sign_jetton_transfer(jetton(TREASURY, OTHER_TON, OTHER_TON, 1, 100_000_000)).await.map(drop), "jetton");
	internal(rail.signer.sign_ton_transfer(ton(GAS_STATION, OTHER_TON, 1)).await.map(drop), "ton");

	sqlx::query("INSERT INTO spend_brake (id) VALUES (1)").execute(&db.pool).await.expect("re-seed the brake row");
	rail.signer
		.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000))
		.await
		.expect("a re-seeded row releases the signer");
	db.cleanup().await;

	// A missing TABLE is the same verdict: a read error is a refusal, never a bypass.
	let db = db_or_skip!();
	let rail = Rail::new(&db, Network::Bep20, policy(&[])).await;
	sqlx::query("DROP TABLE spend_brake").execute(&db.pool).await.expect("drop the brake table");
	internal(
		rail.signer.sign_erc20_transfer(erc20(TREASURY, USDT_BEP20, OTHER_EVM, 1, GWEI, 60_000)).await.map(drop),
		"erc20 with no table",
	);
	db.cleanup().await;
}

/// `spend_brake` as an operator sees it: id, halted, the two USDT ceilings, the four native
/// ceilings (NUMERIC read as text) and the reason.
type BrakeRow = (
	i16,
	bool,
	Option<i64>,
	Option<i64>,
	Option<String>,
	Option<String>,
	Option<String>,
	Option<String>,
	Option<String>,
);

#[tokio::test]
async fn spend_brake_is_a_single_seeded_row() {
	let db = db_or_skip!();
	let rows: Vec<BrakeRow> = sqlx::query_as(
		"SELECT id, halted, max_transfer_usdt, max_treasury_usdt_per_hour, max_native_spend_per_hour_bep20::text, max_native_spend_per_hour_polygon::text, \
		 max_native_spend_per_hour_trc20::text, max_native_spend_per_hour_ton::text, reason FROM spend_brake",
	)
	.fetch_all(&db.pool)
	.await
	.expect("read spend_brake");
	assert_eq!(
		rows,
		vec![(1, false, None, None, None, None, None, None, None)],
		"the migration seeds one released row with no ceiling"
	);

	// One row, by constraint: a second id fails the CHECK, and so does a zero ceiling — a
	// zero is what `halted` is for.
	assert!(sqlx::query("INSERT INTO spend_brake (id) VALUES (2)").execute(&db.pool).await.is_err(), "a second row is refused");
	assert!(
		sqlx::query("UPDATE spend_brake SET max_transfer_usdt = 0").execute(&db.pool).await.is_err(),
		"a zero ceiling is refused"
	);

	// `updated_at` moves on every UPDATE without the operator naming it.
	let before: f64 = sqlx::query_scalar("SELECT extract(epoch FROM updated_at)::float8 FROM spend_brake")
		.fetch_one(&db.pool)
		.await
		.unwrap();
	tokio::time::sleep(std::time::Duration::from_millis(20)).await;
	set_brake(&db, "reason = 'drill'").await;
	let after: f64 = sqlx::query_scalar("SELECT extract(epoch FROM updated_at)::float8 FROM spend_brake")
		.fetch_one(&db.pool)
		.await
		.unwrap();
	assert!(after > before, "updated_at is bumped by the trigger: {before} -> {after}");
	db.cleanup().await;
}

/// One `spend_brake_history` entry as the runbook reads it: who, whether it came over the
/// unix socket, and the halt flag before and after.
type HistoryRow = (String, bool, String, String, bool);

#[tokio::test]
async fn every_brake_update_leaves_a_history_row_naming_the_role_and_both_states() {
	let db = db_or_skip!();
	let before: i64 = sqlx::query_scalar("SELECT count(*) FROM spend_brake_history").fetch_one(&db.pool).await.unwrap();
	assert_eq!(before, 0, "the seed INSERT is not an operator's hand on the brake");

	set_brake(&db, "halted = true, reason = 'drill'").await;
	// Two transactions a few microseconds apart carry distinct `now()`s; the pause is what
	// the assertion on `updated_at` relies on.
	tokio::time::sleep(std::time::Duration::from_millis(20)).await;
	set_brake(&db, "halted = false, reason = NULL").await;

	// `from_addr` is whatever this test's own connection presents (NULL over a socket, an
	// address over TCP) — the same value the trigger saw, whichever transport runs the suite.
	let rows: Vec<HistoryRow> = sqlx::query_as(
		"SELECT by_role, from_addr IS NOT DISTINCT FROM inet_client_addr(), old_row->>'halted', new_row->>'halted', \
		 new_row->>'updated_at' <> old_row->>'updated_at' FROM spend_brake_history ORDER BY id",
	)
	.fetch_all(&db.pool)
	.await
	.expect("read spend_brake_history");
	let role: String = sqlx::query_scalar("SELECT current_user::text").fetch_one(&db.pool).await.unwrap();
	assert_eq!(
		rows,
		vec![
			(role.clone(), true, "false".to_owned(), "true".to_owned(), true),
			(role, true, "true".to_owned(), "false".to_owned(), true)
		],
		"the halt and the release are each one row, by this role, with the stamp moved"
	);
	db.cleanup().await;
}
