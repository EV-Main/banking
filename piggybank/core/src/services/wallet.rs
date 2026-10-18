//! `wallet` context — a user's own crypto wallet (balances, deposit addresses,
//! withdrawals). Every RPC acts on the caller's own access-token `sub`.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use domain::{
	money::{Network, Usdt, WalletAddress},
	withdrawals::{Withdrawal, WithdrawalId},
};
use evbanking_contracts::banking::v1::{self as pb, wallet_service_server::WalletService};
use tonic::{Request, Response, Status};

use crate::{
	AppState,
	application::{wallet as wallet_app, withdrawals as withdrawal_app},
	ports::deposits::DepositRecord,
	services::support::{audit_of, caller_id, map_err, parse_withdrawal_id, rail_is_testnet, unfrozen_caller},
};

#[derive(Clone)]
pub struct WalletSvc {
	pub state: AppState,
}

impl WalletSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}
}

/// Configured minus operator-frozen: what a user is offered to deposit or withdraw on.
async fn live_rails(state: &AppState) -> Result<Vec<Network>, Status> {
	let frozen = state.outflow.frozen_rails().await.map_err(map_err)?;
	Ok(state.configured_networks.iter().copied().filter(|n| !frozen.contains(n)).collect())
}

#[tonic::async_trait]
impl WalletService for WalletSvc {
	async fn get_wallet(&self, request: Request<pb::GetWalletRequest>) -> Result<Response<pb::Wallet>, Status> {
		let user = caller_id(&request)?;
		let wallet = wallet_app::get_wallet(
			&wallet_app::WalletPorts {
				ledger: self.state.ledger.as_ref(),
				positions: self.state.positions.as_ref(),
				nav: self.state.nav.as_ref(),
				deposit_addresses: self.state.deposit_addresses.as_ref(),
				users: self.state.users.as_ref(),
				compartments: self.state.compartments.as_ref(),
			},
			&live_rails(&self.state).await?,
			self.state.kyc_gate,
			user,
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(pb::Wallet {
			balance: Some(pb::Balance {
				available: wallet.balance.available.to_decimal_string(),
				invested: wallet.balance.invested.to_decimal_string(),
				pending_withdrawal: wallet.balance.pending_withdrawal.to_decimal_string(),
				total: wallet.balance.total.to_decimal_string(),
				in_orders: wallet.balance.in_orders.to_decimal_string(),
				in_compartments: wallet.balance.in_compartments.to_decimal_string(),
			}),
			deposit_addresses: wallet
				.deposit_addresses
				.iter()
				.map(|rail| deposit_rail_to_proto(rail, rail_is_testnet(&self.state, rail.network)))
				.collect(),
			withdrawable: wallet.withdrawable.iter().map(withdrawable_to_proto).collect(),
		}))
	}

	async fn get_deposit_address(&self, request: Request<pb::GetDepositAddressRequest>) -> Result<Response<pb::DepositAddress>, Status> {
		let user = caller_id(&request)?;
		let network = Network::parse(&request.get_ref().network).map_err(map_err)?;
		let address = wallet_app::get_deposit_address(
			&wallet_app::DepositAddressPorts {
				deposit_addresses: self.state.deposit_addresses.as_ref(),
				users: self.state.users.as_ref(),
			},
			&live_rails(&self.state).await?,
			self.state.kyc_gate,
			user,
			network,
		)
		.await
		.map_err(map_err)?;
		// An empty `address` marks the rail unavailable: there is no fundable address yet
		// (the underlying address is still a placeholder — or the rail is unconfigured,
		// in which case no address is ever provisioned). An unverified caller does NOT
		// land here — that refusal is `permission_denied`, so the cabinet can tell
		// "this rail can't fund you" from "finish verification".
		Ok(Response::new(pb::DepositAddress {
			network: network.as_str().to_owned(),
			address: address.map(|a| a.as_str().to_owned()).unwrap_or_default(),
			min_confirmations: network.min_confirmations(),
			is_testnet: rail_is_testnet(&self.state, network),
		}))
	}

	async fn request_withdrawal(&self, request: Request<pb::RequestWithdrawalRequest>) -> Result<Response<pb::Withdrawal>, Status> {
		let user = unfrozen_caller(&self.state, &request).await?;
		let req = request.into_inner();
		audit_of(req.audit.as_ref()).log("request_withdrawal", user);
		let network = Network::parse(&req.network).map_err(map_err)?;
		let address = WalletAddress::parse(network, &req.address).map_err(map_err)?;
		let amount = Usdt::parse_decimal(&req.amount).map_err(map_err)?;
		let ports = withdrawal_app::WithdrawalPorts {
			withdrawals: self.state.withdrawals.as_ref(),
			ledger: self.state.ledger.as_ref(),
			custody: self.state.custody.as_ref(),
			relay: &self.state.relay_notify,
		};
		let gates = withdrawal_app::AdmissionGates {
			policy: self.state.outflow.as_ref(),
			configured: &self.state.configured_networks,
			kyc: self.state.kyc_gate,
		};
		// An empty key is a client that predates the field: the earlier behaviour, no dedupe.
		let withdrawal = if req.idempotency_key.is_empty() {
			withdrawal_app::request_withdrawal(&ports, &gates, WithdrawalId::new(), user, network, address, amount).await
		} else {
			withdrawal_app::request_withdrawal_keyed(&ports, &gates, user, network, address, amount, &req.idempotency_key).await
		}
		.map_err(map_err)?;
		Ok(Response::new(withdrawal_to_proto(&withdrawal)))
	}

	async fn cancel_withdrawal(&self, request: Request<pb::CancelWithdrawalRequest>) -> Result<Response<pb::Withdrawal>, Status> {
		let user = caller_id(&request)?;
		let id = parse_withdrawal_id(&request.get_ref().withdrawal_id)?;
		let withdrawal = withdrawal_app::cancel_withdrawal(self.state.withdrawals.as_ref(), &self.state.relay_notify, id, user)
			.await
			.map_err(map_err)?;
		Ok(Response::new(withdrawal_to_proto(&withdrawal)))
	}

	async fn list_withdrawals(&self, request: Request<pb::ListWithdrawalsRequest>) -> Result<Response<pb::WithdrawalList>, Status> {
		let user = caller_id(&request)?;
		let withdrawals = withdrawal_app::list_withdrawals(self.state.withdrawals.as_ref(), user).await.map_err(map_err)?;
		Ok(Response::new(pb::WithdrawalList {
			withdrawals: withdrawals.iter().map(withdrawal_to_proto).collect(),
		}))
	}

	async fn list_deposits(&self, request: Request<pb::ListDepositsRequest>) -> Result<Response<pb::DepositList>, Status> {
		let user = caller_id(&request)?;
		let deposits = wallet_app::list_deposits(self.state.deposits.as_ref(), user).await.map_err(map_err)?;
		Ok(Response::new(pb::DepositList {
			deposits: deposits.iter().map(deposit_to_proto).collect(),
		}))
	}
}

fn deposit_rail_to_proto(rail: &wallet_app::DepositRail, is_testnet: bool) -> pb::DepositAddress {
	pb::DepositAddress {
		network: rail.network.as_str().to_owned(),
		address: rail.address.as_ref().map(|address| address.as_str().to_owned()).unwrap_or_default(),
		min_confirmations: rail.network.min_confirmations(),
		is_testnet,
	}
}

fn withdrawable_to_proto(rail: &wallet_app::NetworkWithdrawable) -> pb::NetworkWithdrawable {
	pb::NetworkWithdrawable {
		network: rail.network.as_str().to_owned(),
		withdrawable: rail.withdrawable.to_decimal_string(),
		instant: rail.instant.to_decimal_string(),
		min_withdrawal: rail.min_withdrawal.to_decimal_string(),
		withdrawal_fee: rail.withdrawal_fee.to_decimal_string(),
	}
}

fn deposit_to_proto(deposit: &DepositRecord) -> pb::Deposit {
	pb::Deposit {
		tx_ref: deposit.tx_ref.as_str().to_owned(),
		network: deposit.network.as_str().to_owned(),
		amount: deposit.amount.to_decimal_string(),
		created_at: deposit.created_at,
	}
}

pub(crate) fn withdrawal_to_proto(withdrawal: &Withdrawal) -> pb::Withdrawal {
	pb::Withdrawal {
		id: withdrawal.id().to_string(),
		network: withdrawal.network().as_str().to_owned(),
		address: withdrawal.address().as_str().to_owned(),
		amount: withdrawal.amount().to_decimal_string(),
		fee: withdrawal.fee().to_decimal_string(),
		net_amount: withdrawal.net_amount().to_decimal_string(),
		state: withdrawal.state().as_str().to_owned(),
		tx_ref: withdrawal.tx_ref().map(|tx_ref| tx_ref.as_str().to_owned()).unwrap_or_default(),
	}
}
