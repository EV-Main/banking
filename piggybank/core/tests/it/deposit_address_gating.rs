//! FB-14: a placeholder deposit address must never be served as fundable, and a cached
//! placeholder must be backfillable once real derivation lands. Real Postgres + a real
//! in-process signer gRPC server (no DB mocks) — runs when `DATABASE_URL` is set and
//! skips otherwise. Each test uses a fresh `user_id`, so runs are isolated on shared
//! infra.
//!
//! The in-process signer is a controllable stand-in for the real one: a flag flips it
//! from emitting a `placeholder` to a `derived` address, so the test exercises (1) the
//! gate — a placeholder yields no fundable address and the rail is unavailable — and (2)
//! the backfill — the cached placeholder is upgraded in place to the derived address once
//! the signer can compute it. This is the hub-side cache trap the audit flagged
//! (BANK-SIGNER-03): tag the row + present the rail unavailable + heal the cache.

use std::sync::{
	Arc,
	atomic::{AtomicBool, Ordering},
};

use domain::{money::Network, users::UserId};
use evbanking_contracts::signer::v1::{
	GetKeyHealthRequest, GetKeyHealthResponse, MigrateAddressToCustodianRequest, MigrateAddressToCustodianResponse, ProvisionAddressRequest, ProvisionAddressResponse, RotateAddressRequest,
	SignErc20TransferRequest, SignErc20TransferResponse, SignJettonTransferRequest, SignNativeTransferRequest, SignNativeTransferResponse, SignTonTransferRequest, SignTrc20TransferRequest,
	SignTrxTransferRequest, SignedTonTxResponse, SignedTronTxResponse,
	signer_service_client::SignerServiceClient,
	signer_service_server::{SignerService, SignerServiceServer},
};
use piggybank_core::{infrastructure::signer_addresses::SignerDepositAddresses, ports::DepositAddresses};
use sqlx::PgPool;
use tonic::transport::{Endpoint, Server};

use crate::common;
use common::pool;

/// A deterministic, structurally-valid address per network — a derived-grade stand-in.
const BEP20: &str = "0x52908400098527886E0F7030069857D2E4169EE7";
const TRC20: &str = "TJRabPrwbZy45sbavfcjinPJC18kjpRTv8";
const TON: &str = "EQCD39VS5jcptHL8vMjEXrzGaRcCVYto7HUn4bpAOg8xqB2N";

fn sample_address(network: Network) -> &'static str {
	match network {
		Network::Bep20 | Network::Polygon => BEP20,
		Network::Trc20 => TRC20,
		Network::Ton => TON,
	}
}

/// In-process signer: returns the same address but flips `address_kind` from
/// `placeholder` to `derived` when `derived` is set — mimicking real encoding landing.
struct FakeSigner {
	derived: Arc<AtomicBool>,
}

#[tonic::async_trait]
impl SignerService for FakeSigner {
	async fn provision_address(&self, request: tonic::Request<ProvisionAddressRequest>) -> Result<tonic::Response<ProvisionAddressResponse>, tonic::Status> {
		let network = Network::parse(&request.into_inner().network).map_err(|e| tonic::Status::invalid_argument(e.to_string()))?;
		let kind = if self.derived.load(Ordering::SeqCst) { "derived" } else { "placeholder" };
		Ok(tonic::Response::new(ProvisionAddressResponse {
			address: sample_address(network).to_owned(),
			address_kind: kind.to_owned(),
		}))
	}

	async fn sign_erc20_transfer(&self, _request: tonic::Request<SignErc20TransferRequest>) -> Result<tonic::Response<SignErc20TransferResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_erc20_transfer is not exercised by the deposit-address gating test"))
	}

	async fn sign_native_transfer(&self, _request: tonic::Request<SignNativeTransferRequest>) -> Result<tonic::Response<SignNativeTransferResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_native_transfer is not exercised by the deposit-address gating test"))
	}

	async fn sign_trc20_transfer(&self, _request: tonic::Request<SignTrc20TransferRequest>) -> Result<tonic::Response<SignedTronTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_trc20_transfer is not exercised by the deposit-address gating test"))
	}

	async fn sign_trx_transfer(&self, _request: tonic::Request<SignTrxTransferRequest>) -> Result<tonic::Response<SignedTronTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_trx_transfer is not exercised by the deposit-address gating test"))
	}

	async fn sign_jetton_transfer(&self, _request: tonic::Request<SignJettonTransferRequest>) -> Result<tonic::Response<SignedTonTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_jetton_transfer is not exercised by the deposit-address gating test"))
	}

	async fn sign_ton_transfer(&self, _request: tonic::Request<SignTonTransferRequest>) -> Result<tonic::Response<SignedTonTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_ton_transfer is not exercised by the deposit-address gating test"))
	}

	async fn get_key_health(&self, _request: tonic::Request<GetKeyHealthRequest>) -> Result<tonic::Response<GetKeyHealthResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("get_key_health is not exercised by the deposit-address gating test"))
	}

	async fn rotate_address(&self, _request: tonic::Request<RotateAddressRequest>) -> Result<tonic::Response<ProvisionAddressResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("rotate_address is not exercised by the deposit-address gating test"))
	}

	async fn migrate_address_to_custodian(&self, _request: tonic::Request<MigrateAddressToCustodianRequest>) -> Result<tonic::Response<MigrateAddressToCustodianResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("migrate_address_to_custodian is not exercised by the deposit-address gating test"))
	}
}

#[tokio::test]
async fn placeholder_is_not_served_as_fundable_then_backfilled() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping deposit-address gating test");
		return;
	};

	// Structured concurrency: the fake signer and the assertions run as two branches of a
	// `select!` (no detached task). Bind an ephemeral port up front; `connect_lazy` retries
	// until the server branch is listening. The server future never resolves, so the test
	// completes when the assertion branch returns and the server is dropped.
	let addr = {
		let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
		probe.local_addr().expect("local addr")
	};
	let derived = Arc::new(AtomicBool::new(false));
	let server = Server::builder().add_service(SignerServiceServer::new(FakeSigner { derived: derived.clone() })).serve(addr);
	let channel = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint").connect_lazy();
	// The fake signer mounts no auth layer, so no service token is attached here.
	let addresses = SignerDepositAddresses::new(pool.clone(), SignerServiceClient::new(channel), None);

	tokio::select! {
		result = server => result.expect("serve fake signer"),
		() = assert_gating_and_backfill(&pool, &addresses, &derived) => {}
	}
}

async fn assert_gating_and_backfill(pool: &PgPool, addresses: &SignerDepositAddresses, derived: &AtomicBool) {
	for network in Network::ALL {
		let user = UserId::new();

		// Signer still emits a placeholder → the rail is unavailable (no fundable address),
		// yet the row IS cached (tagged placeholder) so it can be backfilled later.
		let unavailable = addresses.address(user, network).await.expect("provision");
		assert!(unavailable.is_none(), "{network}: a placeholder must NOT be served as fundable");
		let cached_kind: String = sqlx::query_scalar("SELECT address_kind FROM user_deposit_addresses WHERE user_id = $1 AND network = $2")
			.bind(user.raw())
			.bind(network.as_str())
			.fetch_one(pool)
			.await
			.expect("row cached");
		assert_eq!(cached_kind, "placeholder", "{network}: the cached row is tagged placeholder");

		// Real derivation lands: the next read re-asks the signer, gets a `derived` address,
		// and backfills the cached row in place — the rail becomes fundable.
		derived.store(true, Ordering::SeqCst);
		let fundable = addresses.address(user, network).await.expect("recompute");
		let fundable = fundable.expect("derived address is now served as fundable");
		assert_eq!(fundable.as_str(), sample_address(network), "{network}: backfilled to the derived address");

		let healed_kind: String = sqlx::query_scalar("SELECT address_kind FROM user_deposit_addresses WHERE user_id = $1 AND network = $2")
			.bind(user.raw())
			.bind(network.as_str())
			.fetch_one(pool)
			.await
			.expect("row still present");
		assert_eq!(healed_kind, "derived", "{network}: the cache is healed in place, not left a placeholder");

		// Once derived, reads short-circuit on the cache (signer toggled back off proves it
		// is no longer consulted).
		derived.store(false, Ordering::SeqCst);
		let still_fundable = addresses.address(user, network).await.expect("cached read");
		assert_eq!(
			still_fundable.map(|a| a.as_str().to_owned()),
			Some(sample_address(network).to_owned()),
			"{network}: a derived address is served from cache"
		);
	}
}
