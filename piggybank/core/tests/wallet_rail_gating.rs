//! Rail gating — an unconfigured rail (no running on-chain watcher) is never
//! provisioned or offered. Real Postgres + a real in-process signer gRPC server (no
//! DB mocks) — runs when `DATABASE_URL` is set and skips otherwise; the `get_wallet`
//! test additionally needs a reachable TigerBeetle. Each test uses a fresh `user_id`,
//! so runs are isolated on shared infra.
//!
//! What's proven (the money-stranding fix): `get_deposit_address` on an unconfigured
//! rail returns `None` WITHOUT touching the signer — zero provision calls, no cached
//! row, no key minted for a rail no watcher scans — while a configured rail still
//! provisions; `request_withdrawal` on an unconfigured rail is refused up front
//! (`DomainError::Validation`); and `get_wallet` presents exactly the configured
//! rails in both the deposit and withdrawable views.

use std::sync::{
	Arc,
	atomic::{AtomicUsize, Ordering},
};

use domain::{
	auth::AuthSubject,
	error::DomainError,
	money::{Network, Usdt, WalletAddress},
	users::{Email, UserId},
	withdrawals::WithdrawalId,
};
use evbanking_contracts::signer::v1::{
	GetKeyHealthRequest, GetKeyHealthResponse, MigrateAddressToCustodianRequest, MigrateAddressToCustodianResponse, ProvisionAddressRequest, ProvisionAddressResponse, RotateAddressRequest,
	SignErc20TransferRequest, SignErc20TransferResponse, SignJettonTransferRequest, SignNativeTransferRequest, SignNativeTransferResponse, SignTonTransferRequest, SignTrc20TransferRequest,
	SignTrxTransferRequest, SignedTonTxResponse, SignedTronTxResponse,
	signer_service_client::SignerServiceClient,
	signer_service_server::{SignerService, SignerServiceServer},
};
use piggybank_core::{
	application::{wallet as wallet_app, withdrawals as withdrawal_app},
	config::KycGate,
	infrastructure::{
		custody::StubCustody,
		ledger::{self, TbLedger},
		nav::PgNav,
		outflow::PgOutflowPolicy,
		positions::PgFundPositions,
		signer_addresses::SignerDepositAddresses,
		tigerbeetle::TigerBeetle,
		users::PgUsers,
		withdrawals::PgWithdrawals,
	},
	ports::{UserRepository, ledger::Ledger},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use tonic::transport::{Endpoint, Server};
use uuid::Uuid;

mod common;
use common::pool;

/// A deterministic, structurally-valid derived-grade address per network.
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

/// In-process signer that counts provision calls — the gate under test must keep the
/// count at ZERO for an unconfigured rail (no key is ever minted for a dead rail).
struct CountingSigner {
	provisions: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl SignerService for CountingSigner {
	async fn provision_address(&self, request: tonic::Request<ProvisionAddressRequest>) -> Result<tonic::Response<ProvisionAddressResponse>, tonic::Status> {
		let network = Network::parse(&request.into_inner().network).map_err(|e| tonic::Status::invalid_argument(e.to_string()))?;
		self.provisions.fetch_add(1, Ordering::SeqCst);
		Ok(tonic::Response::new(ProvisionAddressResponse {
			address: sample_address(network).to_owned(),
			address_kind: "derived".to_owned(),
		}))
	}

	async fn sign_erc20_transfer(&self, _request: tonic::Request<SignErc20TransferRequest>) -> Result<tonic::Response<SignErc20TransferResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_erc20_transfer is not exercised by the rail-gating test"))
	}

	async fn sign_native_transfer(&self, _request: tonic::Request<SignNativeTransferRequest>) -> Result<tonic::Response<SignNativeTransferResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_native_transfer is not exercised by the rail-gating test"))
	}

	async fn sign_trc20_transfer(&self, _request: tonic::Request<SignTrc20TransferRequest>) -> Result<tonic::Response<SignedTronTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_trc20_transfer is not exercised by the rail-gating test"))
	}

	async fn sign_trx_transfer(&self, _request: tonic::Request<SignTrxTransferRequest>) -> Result<tonic::Response<SignedTronTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_trx_transfer is not exercised by the rail-gating test"))
	}

	async fn sign_jetton_transfer(&self, _request: tonic::Request<SignJettonTransferRequest>) -> Result<tonic::Response<SignedTonTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_jetton_transfer is not exercised by the rail-gating test"))
	}

	async fn sign_ton_transfer(&self, _request: tonic::Request<SignTonTransferRequest>) -> Result<tonic::Response<SignedTonTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("sign_ton_transfer is not exercised by the rail-gating test"))
	}

	async fn get_key_health(&self, _request: tonic::Request<GetKeyHealthRequest>) -> Result<tonic::Response<GetKeyHealthResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("get_key_health is not exercised by the rail-gating test"))
	}

	async fn rotate_address(&self, _request: tonic::Request<RotateAddressRequest>) -> Result<tonic::Response<ProvisionAddressResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("rotate_address is not exercised by the rail-gating test"))
	}

	async fn migrate_address_to_custodian(&self, _request: tonic::Request<MigrateAddressToCustodianRequest>) -> Result<tonic::Response<MigrateAddressToCustodianResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("migrate_address_to_custodian is not exercised by the rail-gating test"))
	}
}

/// Bind an ephemeral port for the in-process signer, then serve + assert as two
/// branches of a `select!` (structured concurrency — no detached task; `connect_lazy`
/// retries until the server branch is listening).
fn ephemeral_addr() -> std::net::SocketAddr {
	let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
	probe.local_addr().expect("local addr")
}

/// A fresh user who is active AND KYC-verified. Both the deposit-address and the
/// withdrawal path now gate on the mirrored tier, and `provision` leaves the row at the
/// schema default (tier 0), so the tier is set the way the lifecycle bridge sets it —
/// otherwise every rail assertion below would be answered by the verification gate
/// instead of the rail gate under test.
async fn active_user(pool: &PgPool, users: &dyn UserRepository) -> UserId {
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("u{}@example.com", Uuid::new_v4().simple())).unwrap();
	let user = users.provision(subject, email, true).await.unwrap().id();
	common::set_kyc_level(pool, user, 1).await;
	user
}

/// The borrowed driven ports the single-rail address read takes. A helper because both
/// assertions below need the same pair and the struct is the use case's whole wiring.
fn address_ports<'a>(addresses: &'a SignerDepositAddresses, users: &'a PgUsers) -> wallet_app::DepositAddressPorts<'a> {
	wallet_app::DepositAddressPorts {
		deposit_addresses: addresses,
		users,
	}
}

#[tokio::test]
async fn an_unconfigured_rail_is_never_provisioned() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping rail-gating test");
		return;
	};
	let addr = ephemeral_addr();
	let provisions = Arc::new(AtomicUsize::new(0));
	let server = Server::builder()
		.add_service(SignerServiceServer::new(CountingSigner { provisions: provisions.clone() }))
		.serve(addr);
	let channel = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint").connect_lazy();
	// The fake signer mounts no auth layer, so no service token is attached here.
	let addresses = SignerDepositAddresses::new(pool.clone(), SignerServiceClient::new(channel), None);

	tokio::select! {
		result = server => result.expect("serve fake signer"),
		() = assert_unconfigured_rail_gate(&pool, &addresses, &provisions) => {}
	}
}

async fn assert_unconfigured_rail_gate(pool: &PgPool, addresses: &SignerDepositAddresses, provisions: &AtomicUsize) {
	let configured = [Network::Bep20];
	let users = PgUsers::new(pool.clone());
	let user = active_user(pool, &users).await;

	// The unconfigured rails: no address, ZERO signer provision calls, no cached row —
	// the gate sits above the port, so no key is minted for a rail no watcher scans. Checked for
	// both a non-EVM rail (Trc20) and the second EVM rail (Polygon), which is gated identically
	// even though it shares BEP20's address shape.
	for dead_rail in [Network::Trc20, Network::Polygon] {
		let unavailable = wallet_app::get_deposit_address(&address_ports(addresses, &users), &configured, KycGate::ENFORCED, user, dead_rail)
			.await
			.expect("gated read");
		assert!(unavailable.is_none(), "an unconfigured rail ({dead_rail}) must serve no address");
	}
	assert_eq!(provisions.load(Ordering::SeqCst), 0, "the signer must never be asked to provision a dead rail");
	let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_deposit_addresses WHERE user_id = $1")
		.bind(user.raw())
		.fetch_one(pool)
		.await
		.expect("count cached rows");
	assert_eq!(rows, 0, "no key/address row may exist for a dead rail");

	// The configured rail still provisions exactly as before.
	let fundable = wallet_app::get_deposit_address(&address_ports(addresses, &users), &configured, KycGate::ENFORCED, user, Network::Bep20)
		.await
		.expect("provision")
		.expect("a configured rail serves the derived address");
	assert_eq!(fundable.as_str(), BEP20);
	assert_eq!(provisions.load(Ordering::SeqCst), 1, "the configured rail provisions once");
}

#[tokio::test]
async fn an_unconfigured_rail_withdrawal_is_rejected() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping rail-gating test");
		return;
	};
	// The rail gate fires before any ledger read, so the (lazily-connected)
	// TigerBeetle client is never dialled — this test needs only Postgres.
	let address = std::env::var("TIGERBEETLE_ADDRESS").unwrap_or_else(|_| "127.0.0.1:3033".to_owned());
	let cluster = std::env::var("TIGERBEETLE_CLUSTER_ID").ok().and_then(|s| s.parse().ok()).unwrap_or(0u128);
	let tigerbeetle = Arc::new(TigerBeetle::connect(cluster, &address).expect("client init is lazy"));
	let ledger: Arc<dyn Ledger> = Arc::new(TbLedger::new(tigerbeetle, pool.clone()));
	let withdrawals = PgWithdrawals::new(pool.clone());
	let users = PgUsers::new(pool.clone());
	let policy = PgOutflowPolicy::new(pool.clone());
	let notify = Notify::new();
	let user = active_user(&pool, &users).await;

	let destination = WalletAddress::parse(Network::Ton, TON).unwrap();
	let err = withdrawal_app::request_withdrawal(
		&withdrawal_app::WithdrawalPorts {
			withdrawals: &withdrawals,
			ledger: ledger.as_ref(),
			custody: &StubCustody,
			relay: &notify,
		},
		&withdrawal_app::AdmissionGates {
			policy: &policy,
			configured: &[Network::Bep20],
			kyc: KycGate::ENFORCED,
		},
		WithdrawalId::new(),
		user,
		Network::Ton,
		destination,
		Usdt::parse_decimal("50").unwrap(),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "an unconfigured rail must be refused as validation, got {err:?}");
}

#[tokio::test]
async fn get_wallet_presents_only_configured_rails() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping rail-gating test");
		return;
	};
	let address = std::env::var("TIGERBEETLE_ADDRESS").unwrap_or_else(|_| "127.0.0.1:3033".to_owned());
	let cluster = std::env::var("TIGERBEETLE_CLUSTER_ID").ok().and_then(|s| s.parse().ok()).unwrap_or(0u128);
	let tigerbeetle = Arc::new(TigerBeetle::connect(cluster, &address).expect("connect to TigerBeetle"));
	let ledger: Arc<dyn Ledger> = Arc::new(TbLedger::new(tigerbeetle, pool.clone()));
	if ledger::seed_singletons(ledger.as_ref()).await.is_err() {
		eprintln!("TigerBeetle unreachable — skipping get_wallet rail-gating test");
		return;
	}

	let addr = ephemeral_addr();
	let provisions = Arc::new(AtomicUsize::new(0));
	let server = Server::builder()
		.add_service(SignerServiceServer::new(CountingSigner { provisions: provisions.clone() }))
		.serve(addr);
	let channel = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint").connect_lazy();
	let addresses = SignerDepositAddresses::new(pool.clone(), SignerServiceClient::new(channel), None);
	let positions = PgFundPositions::new(pool.clone());
	let nav = PgNav::new(pool.clone());
	let users = PgUsers::new(pool.clone());
	let user = active_user(&pool, &users).await;

	tokio::select! {
		result = server => result.expect("serve fake signer"),
		() = async {
			let wallet = wallet_app::get_wallet(
				&wallet_app::WalletPorts {
					ledger: ledger.as_ref(),
					positions: &positions,
					nav: &nav,
					deposit_addresses: &addresses,
					users: &users,
					compartments: &piggybank_core::infrastructure::compartments::PgCompartments::new(pool.clone()),
				},
				&[Network::Bep20],
				KycGate::ENFORCED,
				user,
			)
			.await
			.expect("wallet");
			assert_eq!(wallet.deposit_addresses.len(), 1, "exactly the configured rail is offered for deposit");
			assert_eq!(wallet.deposit_addresses[0].network, Network::Bep20);
			assert_eq!(
				wallet.deposit_addresses[0].address.as_ref().map(|a| a.as_str().to_owned()),
				Some(BEP20.to_owned()),
				"the configured rail carries its derived address"
			);
			assert_eq!(wallet.withdrawable.len(), 1, "exactly the configured rail is offered for withdrawal");
			assert_eq!(wallet.withdrawable[0].network, Network::Bep20);
			assert_eq!(provisions.load(Ordering::SeqCst), 1, "only the configured rail was provisioned");
		} => {}
	}
}
