//! The hub's funds gate on the phase-4 custody migration — real Postgres + a real in-process
//! signer gRPC server (no DB mocks). Runs when `DATABASE_URL` is set and skips otherwise; each
//! test uses a fresh `user_id`, so runs are isolated on shared infra.
//!
//! Balances live on the chain and the signer has no chain view, so the "is anything still on
//! that address" question can only be answered here — and this is the test that it IS asked,
//! before anything is retired. The predicate is the sweeper's own (`deposits.swept_at IS
//! NULL`): while a credited deposit has not been observed drained, the address may still hold
//! money and must not be retired.
//!
//! Two properties, both about the value travelling rather than the call succeeding:
//!
//!   * an unswept credited deposit blocks the migration, and the signer is never called;
//!   * once swept, the migration runs, the hub caches the NEW address (so watchers and
//!     `GetDepositAddress` stop pointing at the retired one), and the address handed to the
//!     signer is the very address the gate cleared.

use std::sync::{
	Arc,
	atomic::{AtomicUsize, Ordering},
};

use domain::{
	money::{Network, TxRef, Usdt},
	users::UserId,
};
use evbanking_contracts::signer::v1::{
	GetKeyHealthRequest, GetKeyHealthResponse, MigrateAddressToCustodianRequest, MigrateAddressToCustodianResponse, ProvisionAddressRequest, ProvisionAddressResponse, RotateAddressRequest,
	SignErc20TransferRequest, SignErc20TransferResponse, SignJettonTransferRequest, SignNativeTransferRequest, SignNativeTransferResponse, SignTonTransferRequest, SignTrc20TransferRequest,
	SignTrxTransferRequest, SignedTonTxResponse, SignedTronTxResponse,
	signer_service_client::SignerServiceClient,
	signer_service_server::{SignerService, SignerServiceServer},
};
use piggybank_core::{
	application::wallet as wallet_app,
	infrastructure::{deposits::PgDeposits, signer_addresses::SignerDepositAddresses},
	ports::{DepositAddresses, Deposits},
};
use sqlx::PgPool;
use tonic::transport::{Endpoint, Server};

use crate::common;
use common::pool;

const NETWORK: Network = Network::Bep20;
/// The address the fake signer provisions, and the one the gate must clear before anything is
/// retired. Checksummed exactly as the real signer renders an EVM address.
const OLD_ADDRESS: &str = "0x52908400098527886E0F7030069857D2E4169EE7";
const NEW_ADDRESS: &str = "0x8617E340B3D01FA5F11F306F4090FD50E238070D";

/// In-process signer, recording what the hub actually asked it. `drained_seen` is the
/// interesting one: the gate is only worth anything if the address it cleared is the address
/// that reaches the signer.
#[derive(Default)]
struct RecordingSigner {
	migrate_calls: AtomicUsize,
	drained_seen: std::sync::Mutex<Vec<String>>,
}

#[tonic::async_trait]
impl SignerService for RecordingSigner {
	async fn provision_address(&self, _request: tonic::Request<ProvisionAddressRequest>) -> Result<tonic::Response<ProvisionAddressResponse>, tonic::Status> {
		Ok(tonic::Response::new(ProvisionAddressResponse {
			address: OLD_ADDRESS.to_owned(),
			address_kind: "derived".to_owned(),
		}))
	}

	async fn migrate_address_to_custodian(&self, request: tonic::Request<MigrateAddressToCustodianRequest>) -> Result<tonic::Response<MigrateAddressToCustodianResponse>, tonic::Status> {
		self.migrate_calls.fetch_add(1, Ordering::SeqCst);
		let req = request.into_inner();
		self.drained_seen.lock().expect("record the drained address").push(req.drained_address);
		Ok(tonic::Response::new(MigrateAddressToCustodianResponse {
			old_address: OLD_ADDRESS.to_owned(),
			new_address: NEW_ADDRESS.to_owned(),
			address_kind: "derived".to_owned(),
		}))
	}

	async fn sign_erc20_transfer(&self, _request: tonic::Request<SignErc20TransferRequest>) -> Result<tonic::Response<SignErc20TransferResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_erc20_transfer is not exercised by the custody-migration gate test"))
	}

	async fn sign_native_transfer(&self, _request: tonic::Request<SignNativeTransferRequest>) -> Result<tonic::Response<SignNativeTransferResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_native_transfer is not exercised by the custody-migration gate test"))
	}

	async fn sign_trc20_transfer(&self, _request: tonic::Request<SignTrc20TransferRequest>) -> Result<tonic::Response<SignedTronTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_trc20_transfer is not exercised by the custody-migration gate test"))
	}

	async fn sign_trx_transfer(&self, _request: tonic::Request<SignTrxTransferRequest>) -> Result<tonic::Response<SignedTronTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_trx_transfer is not exercised by the custody-migration gate test"))
	}

	async fn sign_jetton_transfer(&self, _request: tonic::Request<SignJettonTransferRequest>) -> Result<tonic::Response<SignedTonTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_jetton_transfer is not exercised by the custody-migration gate test"))
	}

	async fn sign_ton_transfer(&self, _request: tonic::Request<SignTonTransferRequest>) -> Result<tonic::Response<SignedTonTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_ton_transfer is not exercised by the custody-migration gate test"))
	}

	async fn get_key_health(&self, _request: tonic::Request<GetKeyHealthRequest>) -> Result<tonic::Response<GetKeyHealthResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("get_key_health is not exercised by the custody-migration gate test"))
	}

	async fn rotate_address(&self, _request: tonic::Request<RotateAddressRequest>) -> Result<tonic::Response<ProvisionAddressResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("rotate_address is not exercised by the custody-migration gate test"))
	}
}

#[tokio::test]
async fn an_unswept_deposit_blocks_the_migration_until_the_address_is_drained() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping custody-migration gate test");
		return;
	};

	// Structured concurrency, as in `deposit_address_gating`: the fake signer and the
	// assertions are two branches of a `select!`, no detached task. The server future never
	// resolves, so the test ends when the assertion branch returns.
	let addr = {
		let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
		probe.local_addr().expect("local addr")
	};
	let signer = Arc::new(RecordingSigner::default());
	let server = Server::builder().add_service(SignerServiceServer::from_arc(Arc::clone(&signer))).serve(addr);
	let channel = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint").connect_lazy();
	// The fake signer mounts no auth layer, so no service token is attached here.
	let addresses = SignerDepositAddresses::new(pool.clone(), SignerServiceClient::new(channel), None);

	tokio::select! {
		result = server => result.expect("serve fake signer"),
		() = assert_gate(&pool, &addresses, &signer) => {}
	}
}

async fn assert_gate(pool: &PgPool, addresses: &SignerDepositAddresses, signer: &RecordingSigner) {
	let deposits = PgDeposits::new(pool.clone());
	let user = UserId::new();
	let configured = [NETWORK];

	// The user has an address, and a credited deposit no sweep has consolidated yet.
	let address = addresses.address(user, NETWORK).await.expect("provision").expect("a derived address");
	assert_eq!(address.as_str(), OLD_ADDRESS);
	let tx_ref = TxRef::parse(&format!("0xcustodygate{}", user.raw().simple())).expect("tx ref");
	assert!(
		deposits
			.record(tx_ref, user, NETWORK, Usdt::from_base_units(5_000_000_000_000_000_000))
			.await
			.expect("record a deposit"),
		"the deposit must be newly recorded"
	);
	assert!(
		deposits.has_unswept(user, NETWORK).await.expect("read the gate"),
		"an un-swept deposit is money that may still be on the address"
	);

	// THE gate: money may still be sitting there, so the address is not retirable — and the
	// signer must never even be asked.
	let refused = wallet_app::migrate_deposit_address_to_custodian(&deposits, addresses, &configured, user, NETWORK)
		.await
		.expect_err("an un-swept deposit must block the migration");
	assert!(format!("{refused}").contains("unswept"), "the refusal must say why, got: {refused}");
	assert_eq!(signer.migrate_calls.load(Ordering::SeqCst), 0, "the gate must refuse BEFORE the signer is called");
	let cached: String = sqlx::query_scalar("SELECT address FROM user_deposit_addresses WHERE user_id = $1 AND network = $2")
		.bind(user.raw())
		.bind(NETWORK.as_str())
		.fetch_one(pool)
		.await
		.expect("the cached address is untouched");
	assert_eq!(cached, OLD_ADDRESS, "a refused migration must not disturb the served address");

	// The sweep consolidates and stamps the deposit — exactly what `rails::mark_swept` does at
	// the end of a cycle that saw the address drained.
	sqlx::query("UPDATE deposits SET swept_at = now() WHERE party_kind = 'user' AND party_id = $1 AND network = $2 AND swept_at IS NULL")
		.bind(user.to_string())
		.bind(NETWORK.as_str())
		.execute(pool)
		.await
		.expect("sweep the deposit");
	assert!(!deposits.has_unswept(user, NETWORK).await.expect("read the gate"));

	// Now it migrates, and the hub swaps what it serves to the NEW address in the same breath —
	// otherwise watchers and `GetDepositAddress` would keep pointing at the retired one.
	let migrated = wallet_app::migrate_deposit_address_to_custodian(&deposits, addresses, &configured, user, NETWORK)
		.await
		.expect("a drained address migrates");
	assert_eq!(migrated.old_address, OLD_ADDRESS);
	assert_eq!(migrated.new_address.as_str(), NEW_ADDRESS);
	let served: String = sqlx::query_scalar("SELECT address FROM user_deposit_addresses WHERE user_id = $1 AND network = $2")
		.bind(user.raw())
		.bind(NETWORK.as_str())
		.fetch_one(pool)
		.await
		.expect("the cache is refreshed");
	assert_eq!(served, NEW_ADDRESS, "the hub must stop serving the retired address immediately");

	// And the whole point of passing the address along: the signer was told to retire exactly
	// the address this gate cleared, not one it re-read for itself.
	assert_eq!(signer.migrate_calls.load(Ordering::SeqCst), 1);
	assert_eq!(signer.drained_seen.lock().expect("read the recorded addresses").as_slice(), [OLD_ADDRESS.to_owned()]);

	// An unconfigured rail has no watcher, so it is refused before anything else happens.
	let unconfigured = wallet_app::migrate_deposit_address_to_custodian(&deposits, addresses, &[], user, NETWORK)
		.await
		.expect_err("an unconfigured rail must be refused");
	assert!(format!("{unconfigured}").contains("not a configured rail"), "got: {unconfigured}");
	assert_eq!(signer.migrate_calls.load(Ordering::SeqCst), 1, "an unconfigured rail must not reach the signer");
}
