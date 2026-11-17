//! A withdrawal whose broadcast row a restore took back must adopt the transfer the chain
//! already carries, never sign a second one. Real Postgres; the signer and the EVM node are
//! in-process fakes (the signer counts what it signs, the node serves one mined transfer).
//!
//! The failure it pins: Postgres restored to before the broadcast still says `processing`
//! with no `withdrawal_broadcasts` row, while the chain holds the send — the fresh-sign path
//! would pay the same withdrawal twice under a new nonce.

use std::sync::{
	Arc, Mutex,
	atomic::{AtomicUsize, Ordering},
};

use domain::money::{Network, Usdt, WalletAddress};
use evbanking_contracts::signer::v1::{
	GetKeyHealthRequest, GetKeyHealthResponse, MigrateAddressToCustodianRequest, MigrateAddressToCustodianResponse, ProvisionAddressRequest, ProvisionAddressResponse, RotateAddressRequest,
	SignErc20TransferRequest, SignErc20TransferResponse, SignJettonTransferRequest, SignNativeTransferRequest, SignNativeTransferResponse, SignTonTransferRequest, SignTrc20TransferRequest,
	SignTrxTransferRequest, SignedTonTxResponse, SignedTronTxResponse,
	signer_service_client::SignerServiceClient,
	signer_service_server::{SignerService, SignerServiceServer},
};
use piggybank_core::{
	config::EvmConfig,
	infrastructure::custody::ChainCustody,
	ports::custody::{BroadcastRequest, Custody},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpListener,
};
use tonic::transport::{Endpoint, Server};
use uuid::Uuid;

use crate::common;

const TREASURY: &str = "0x52908400098527886e0f7030069857d2e4169ee7";
const USDT: &str = "0x55d398326f99059ff775485246999027b3197955";
const HEAD: u64 = 10_000;
/// The mined send sits here; every block is `SECONDS_PER_BLOCK` after its parent, and the
/// head is "now".
const SENT_AT: u64 = 9_000;
const SECONDS_PER_BLOCK: u64 = 3;

struct CountingSigner(Arc<AtomicUsize>);

#[tonic::async_trait]
impl SignerService for CountingSigner {
	async fn provision_address(&self, _request: tonic::Request<ProvisionAddressRequest>) -> Result<tonic::Response<ProvisionAddressResponse>, tonic::Status> {
		Ok(tonic::Response::new(ProvisionAddressResponse {
			address: TREASURY.to_owned(),
			address_kind: "derived".to_owned(),
		}))
	}

	async fn sign_erc20_transfer(&self, _request: tonic::Request<SignErc20TransferRequest>) -> Result<tonic::Response<SignErc20TransferResponse>, tonic::Status> {
		let n = self.0.fetch_add(1, Ordering::SeqCst);
		Ok(tonic::Response::new(SignErc20TransferResponse {
			raw_tx: format!("0xfresh{n}"),
			tx_hash: format!("0x{:064x}", 0xf0 + n),
		}))
	}

	async fn sign_native_transfer(&self, _request: tonic::Request<SignNativeTransferRequest>) -> Result<tonic::Response<SignNativeTransferResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("not exercised"))
	}

	async fn sign_trc20_transfer(&self, _request: tonic::Request<SignTrc20TransferRequest>) -> Result<tonic::Response<SignedTronTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("not exercised"))
	}

	async fn sign_trx_transfer(&self, _request: tonic::Request<SignTrxTransferRequest>) -> Result<tonic::Response<SignedTronTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("not exercised"))
	}

	async fn sign_jetton_transfer(&self, _request: tonic::Request<SignJettonTransferRequest>) -> Result<tonic::Response<SignedTonTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("not exercised"))
	}

	async fn sign_ton_transfer(&self, _request: tonic::Request<SignTonTransferRequest>) -> Result<tonic::Response<SignedTonTxResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("not exercised"))
	}

	async fn get_key_health(&self, _request: tonic::Request<GetKeyHealthRequest>) -> Result<tonic::Response<GetKeyHealthResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("not exercised"))
	}

	async fn rotate_address(&self, _request: tonic::Request<RotateAddressRequest>) -> Result<tonic::Response<ProvisionAddressResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("not exercised"))
	}

	async fn migrate_address_to_custodian(&self, _request: tonic::Request<MigrateAddressToCustodianRequest>) -> Result<tonic::Response<MigrateAddressToCustodianResponse>, tonic::Status> {
		Err(tonic::Status::unimplemented("not exercised"))
	}
}

/// A node whose chain holds the mined treasury transfers in `sent` (tx hash → recipient,
/// raw amount), all at block [`SENT_AT`]. Records every method it is asked.
struct Chain {
	sent: Vec<(String, String, u128)>,
	calls: Mutex<Vec<String>>,
}

impl Chain {
	fn answer(&self, request: &Value) -> Value {
		let method = request["method"].as_str().expect("json-rpc method");
		self.calls.lock().unwrap().push(method.to_owned());
		let params = &request["params"];
		let hex = |n: u128| json!(format!("0x{n:x}"));
		let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
		match method {
			"eth_blockNumber" => hex(HEAD.into()),
			"eth_getBlockByNumber" => {
				let n = u64::from_str_radix(params[0].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
				json!({ "number": params[0], "timestamp": format!("0x{:x}", now - (HEAD - n) * SECONDS_PER_BLOCK) })
			}
			"eth_getLogs" => {
				let f = &params[0];
				let block = |k: &str| u64::from_str_radix(f[k].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
				let topic_address = |i: usize| f["topics"][i].as_str().map(|t| format!("0x{}", &t[26..]));
				let logs: Vec<Value> = self
					.sent
					.iter()
					.filter(|(_, to, _)| {
						(block("fromBlock")..=block("toBlock")).contains(&SENT_AT) && topic_address(1).as_deref() == Some(TREASURY) && topic_address(2).as_deref() == Some(to)
					})
					.map(|(hash, to, amount)| {
						json!({
							"address": USDT,
							"blockNumber": format!("0x{SENT_AT:x}"),
							"transactionHash": hash,
							"topics": [f["topics"][0], format!("0x000000000000000000000000{}", &TREASURY[2..]), format!("0x000000000000000000000000{}", &to[2..])],
							"data": format!("0x{amount:064x}"),
						})
					})
					.collect();
				json!(logs)
			}
			"eth_gasPrice" => hex(1_000_000_000),
			"eth_getBalance" => hex(10u128.pow(18)),
			"eth_call" => json!(format!("0x{:064x}", 10u128.pow(24))),
			"eth_getTransactionCount" => hex(7),
			"eth_sendRawTransaction" => json!(format!("0x{:064x}", 0xf0)),
			other => panic!("fake node asked {other}"),
		}
	}

	/// One request per connection (`Connection: close`), so the client's sequential calls
	/// arrive one at a time.
	async fn serve(&self, listener: TcpListener) {
		loop {
			let (mut stream, _) = listener.accept().await.expect("accept");
			let mut buf = Vec::new();
			let body = loop {
				let mut chunk = [0u8; 4096];
				let n = stream.read(&mut chunk).await.expect("read request");
				buf.extend_from_slice(&chunk[..n]);
				let text = String::from_utf8_lossy(&buf);
				if let Some(end) = text.find("\r\n\r\n") {
					let length: usize = text[..end]
						.lines()
						.find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
						.expect("content-length");
					if buf.len() >= end + 4 + length {
						break serde_json::from_slice::<Value>(&buf[end + 4..end + 4 + length]).expect("json body");
					}
				}
			};
			let reply = json!({ "jsonrpc": "2.0", "id": body["id"], "result": self.answer(&body) }).to_string();
			let response = format!(
				"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
				reply.len()
			);
			stream.write_all(response.as_bytes()).await.expect("write response");
		}
	}
}

async fn processing_withdrawal(pool: &PgPool, to: &str, net: Usdt) -> Uuid {
	let id = Uuid::new_v4();
	sqlx::query("INSERT INTO withdrawals (id, user_id, network, address, amount, fee, state, created_at) VALUES ($1, $2, 'bep20', $3, $4, '0', 'processing', now() - interval '1 hour')")
		.bind(id)
		.bind(Uuid::new_v4())
		.bind(to)
		.bind(net.base_units().to_string())
		.execute(pool)
		.await
		.unwrap();
	id
}

async fn broadcast_row(pool: &PgPool, id: Uuid) -> Option<String> {
	sqlx::query_scalar("SELECT tx_hash FROM withdrawal_broadcasts WHERE withdrawal_id = $1")
		.bind(id)
		.fetch_optional(pool)
		.await
		.unwrap()
}

#[tokio::test]
async fn a_restored_withdrawal_adopts_the_send_the_chain_already_has() {
	let Some(pool) = common::pool().await else { return };
	let net = Usdt::from_base_units(25 * 10u128.pow(18));
	let to = format!("0x{:040x}", Uuid::new_v4().as_u128());
	let other = format!("0x{:040x}", Uuid::new_v4().as_u128());
	let sent = format!("0x{:064x}", Uuid::new_v4().as_u128());
	let owned = format!("0x{:064x}", Uuid::new_v4().as_u128());

	let restored = processing_withdrawal(&pool, &to, net).await;
	// `other` was paid by a withdrawal whose row survived; a lookalike of it must not be
	// mistaken for a forgotten send.
	let settled = processing_withdrawal(&pool, &other, net).await;
	sqlx::query("INSERT INTO withdrawal_broadcasts (withdrawal_id, network, nonce, raw_tx, tx_hash) VALUES ($1, 'bep20', 3, '0xold', $2)")
		.bind(settled)
		.bind(&owned)
		.execute(&pool)
		.await
		.unwrap();
	let unpaid = processing_withdrawal(&pool, &other, net).await;

	let chain = Chain {
		sent: vec![(sent.clone(), to.clone(), net.base_units()), (owned.clone(), other.clone(), net.base_units())],
		calls: Mutex::new(Vec::new()),
	};
	let node = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let node_url = format!("http://{}", node.local_addr().unwrap());
	let signed = Arc::new(AtomicUsize::new(0));
	let signer_addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
	let signer = Server::builder().add_service(SignerServiceServer::new(CountingSigner(signed.clone()))).serve(signer_addr);
	let channel = Endpoint::from_shared(format!("http://{signer_addr}")).unwrap().connect_lazy();

	let evm = EvmConfig {
		network: Network::Bep20,
		rpc_url: node_url,
		usdt_contract: USDT.to_owned(),
		confirmations: 1,
		poll_secs: 1,
		start_block: None,
		max_block_range: 500,
		logs_rpc_url: None,
		chain_id: 56,
		gas_limit: 100_000,
		max_gas_price_gwei: 100,
	};
	let custody = ChainCustody::new(pool.clone(), &evm, SignerServiceClient::new(channel), None);
	let request = |id, to: &str| BroadcastRequest {
		withdrawal_id: id,
		network: Network::Bep20,
		address: WalletAddress::parse(Network::Bep20, to).unwrap(),
		amount: net,
	};

	tokio::select! {
		() = chain.serve(node) => unreachable!("the fake node serves forever"),
		served = signer => panic!("signer stopped: {served:?}"),
		() = async {
			custody.broadcast(&request(restored, &to)).await.expect("adopting is a successful broadcast");
			assert_eq!(signed.load(Ordering::SeqCst), 0, "nothing may be signed for a withdrawal the chain already paid");
			assert!(!chain.calls.lock().unwrap().iter().any(|m| m == "eth_sendRawTransaction"), "nothing may be sent");
			assert_eq!(broadcast_row(&pool, restored).await.as_deref(), Some(sent.as_str()), "the chain's send becomes this withdrawal's broadcast");
			custody.broadcast(&request(restored, &to)).await.expect("a re-delivery of an adopted withdrawal is a no-op");
			assert_eq!(signed.load(Ordering::SeqCst), 0);

			custody.broadcast(&request(unpaid, &other)).await.expect("an unpaid withdrawal is sent");
			assert_eq!(signed.load(Ordering::SeqCst), 1, "a send another withdrawal owns is not this one's");
			assert_ne!(broadcast_row(&pool, unpaid).await.as_deref(), Some(owned.as_str()));
		} => {}
	}
}
