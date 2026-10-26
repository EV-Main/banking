//! `balance` context — company-money RPCs (all admin-gated): treasury reads,
//! chain-proven arrival recording (a deposit), and the operator withdrawal lifecycle.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use concierge_domain::authz::bank;
use domain::{
	balance::Party,
	money::{Network, TxRef, Usdt},
};
use evbanking_contracts::banking::v1::{self as pb, balance_service_server::BalanceService};
use tonic::{Request, Response, Status};

use crate::{
	AppState,
	application::{
		balance::{self as balance_app, ClaimBalance},
		wallet as wallet_app, withdrawals as withdrawal_app,
	},
	services::support::{map_err, optional, parse_user_id, parse_withdrawal_id, rail_is_testnet, require_permission},
};

#[derive(Clone)]
pub struct BalanceSvc {
	pub state: AppState,
}

impl BalanceSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}

	async fn rails(&self) -> Result<Response<pb::RailList>, Status> {
		let frozen = crate::infrastructure::operations::frozen_rails(&self.state.pool)
			.await
			.map_err(|_| Status::unavailable("internal error"))?;
		Ok(Response::new(pb::RailList {
			rails: Network::ALL
				.into_iter()
				.map(|network| pb::Rail {
					network: network.as_str().to_owned(),
					configured: self.state.configured_networks.contains(&network),
					frozen: frozen.contains(&network),
					gas_coin: network.native_coin().to_owned(),
				})
				.collect(),
		}))
	}
}

#[tonic::async_trait]
impl BalanceService for BalanceSvc {
	/// The treasury: every claim on the custody is somebody's (#245) — a person's, or
	/// banking's own.
	async fn get_treasury(&self, request: Request<pb::GetTreasuryRequest>) -> Result<Response<pb::Treasury>, Status> {
		require_permission(&self.state, &request, bank::Treasury::Read).await?;
		let t = balance_app::treasury(self.state.ledger.as_ref(), self.state.custody.as_ref()).await.map_err(map_err)?;
		Ok(Response::new(pb::Treasury {
			rails: t
				.rails
				.into_iter()
				.map(|r| pb::RailLiquidity {
					is_testnet: rail_is_testnet(&self.state, r.network),
					network: r.network.as_str().to_owned(),
					custody: r.custody.to_decimal_string(),
					treasury_address: r.treasury_address.unwrap_or_default(),
					onchain_usdt: r.onchain_usdt.map(Usdt::to_decimal_string).unwrap_or_default(),
					onchain_gas: r.onchain_gas.unwrap_or_default(),
					gas_station_address: r.gas_station_address.unwrap_or_default(),
					gas_station_gas: r.gas_station_gas.unwrap_or_default(),
				})
				.collect(),
			bank: t.bank.to_decimal_string(),
			total_custody: t.total_custody.to_decimal_string(),
			reserved_for_withdrawals: t.reserved_for_withdrawals.to_decimal_string(),
			held_by_users: t.held_by_users.to_decimal_string(),
			banking_treasury: Some(claim_to_proto(&t.banking_treasury)),
			held_in_compartments: t.held_in_compartments.to_decimal_string(),
		}))
	}

	async fn record_deposit(&self, request: Request<pb::RecordDepositRequest>) -> Result<Response<pb::RecordDepositResponse>, Status> {
		require_permission(&self.state, &request, bank::Capital::Manage).await?;
		let req = request.into_inner();
		let tx_ref = TxRef::parse(&req.tx_ref).map_err(map_err)?;
		let network = Network::parse(&req.network).map_err(map_err)?;
		// Empty means "whatever the chain says"; a value is an assertion the chain must match.
		let expected_amount = optional(&req.expected_amount).map(Usdt::parse_decimal).transpose().map_err(map_err)?;
		let arrival = balance_app::record_verified_arrival(
			self.state.deposits.as_ref(),
			self.state.custody.as_ref(),
			self.state.deposit_addresses.as_ref(),
			&self.state.relay_notify,
			tx_ref,
			network,
			expected_amount,
		)
		.await
		.map_err(map_err)?;
		// Always a person: the chain names a deposit address's owner, and a treasury
		// arrival is refused upstream.
		let party = Party::User(arrival.user);
		Ok(Response::new(pb::RecordDepositResponse {
			recorded: arrival.recorded,
			amount: arrival.amount.to_decimal_string(),
			party_kind: party.kind_str().to_owned(),
			party_id: party.id_str(),
		}))
	}

	/// Push a queued withdrawal onto its rail by operator command.
	///
	/// The permission is necessary and not sufficient: the command re-runs the outflow
	/// policy — read-only kill-switch, owner freeze, tier-1 floor — so this handle cannot
	/// ship a payout the user path and the sweep would both refuse. Deliberately NO
	/// per-call `force` override: the operator override for a pause already exists and is
	/// `SetOperationsMode`, which is itself permissioned and leaves one auditable record of
	/// who reopened outflows and when, rather than a flag on an individual payout that
	/// looks identical to an ordinary dispatch in the log.
	async fn dispatch_withdrawal(&self, request: Request<pb::DispatchWithdrawalRequest>) -> Result<Response<pb::DispatchWithdrawalResponse>, Status> {
		require_permission(&self.state, &request, bank::Withdrawal::Dispatch).await?;
		let id = parse_withdrawal_id(&request.get_ref().withdrawal_id)?;
		withdrawal_app::dispatch_withdrawal(
			self.state.withdrawals.as_ref(),
			self.state.custody.as_ref(),
			self.state.outflow.as_ref(),
			self.state.kyc_gate,
			&self.state.relay_notify,
			id,
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(pb::DispatchWithdrawalResponse {}))
	}

	async fn settle_withdrawal(&self, request: Request<pb::SettleWithdrawalRequest>) -> Result<Response<pb::SettleWithdrawalResponse>, Status> {
		require_permission(&self.state, &request, bank::Withdrawal::Settle).await?;
		let req = request.into_inner();
		let id = parse_withdrawal_id(&req.withdrawal_id)?;
		let tx_ref = TxRef::parse(&req.tx_ref).map_err(map_err)?;
		withdrawal_app::settle_withdrawal(self.state.withdrawals.as_ref(), &self.state.relay_notify, id, tx_ref)
			.await
			.map_err(map_err)?;
		Ok(Response::new(pb::SettleWithdrawalResponse {}))
	}

	async fn fail_withdrawal(&self, request: Request<pb::FailWithdrawalRequest>) -> Result<Response<pb::FailWithdrawalResponse>, Status> {
		require_permission(&self.state, &request, bank::Withdrawal::Fail).await?;
		let id = parse_withdrawal_id(&request.get_ref().withdrawal_id)?;
		withdrawal_app::fail_withdrawal(self.state.withdrawals.as_ref(), &self.state.relay_notify, id)
			.await
			.map_err(map_err)?;
		Ok(Response::new(pb::FailWithdrawalResponse {}))
	}

	async fn get_operations_mode(&self, request: Request<pb::GetOperationsModeRequest>) -> Result<Response<pb::OperationsMode>, Status> {
		require_permission(&self.state, &request, bank::Treasury::Read).await?;
		let read_only = crate::infrastructure::operations::is_read_only(&self.state.pool)
			.await
			.map_err(|_| Status::unavailable("internal error"))?;
		Ok(Response::new(pb::OperationsMode { read_only }))
	}

	async fn set_operations_mode(&self, request: Request<pb::SetOperationsModeRequest>) -> Result<Response<pb::OperationsMode>, Status> {
		require_permission(&self.state, &request, bank::Operations::Manage).await?;
		let read_only = crate::infrastructure::operations::set_read_only(&self.state.pool, request.get_ref().read_only)
			.await
			.map_err(|_| Status::unavailable("internal error"))?;
		Ok(Response::new(pb::OperationsMode { read_only }))
	}

	async fn list_rails(&self, request: Request<pb::ListRailsRequest>) -> Result<Response<pb::RailList>, Status> {
		require_permission(&self.state, &request, bank::Treasury::Read).await?;
		self.rails().await
	}

	async fn set_rail_frozen(&self, request: Request<pb::SetRailFrozenRequest>) -> Result<Response<pb::RailList>, Status> {
		require_permission(&self.state, &request, bank::Operations::Manage).await?;
		let req = request.get_ref();
		let network = Network::parse(&req.network).map_err(map_err)?;
		crate::infrastructure::operations::set_rail_frozen(&self.state.pool, network, req.frozen)
			.await
			.map_err(|_| Status::unavailable("internal error"))?;
		tracing::warn!(%network, frozen = req.frozen, "rail freeze changed by an operator");
		self.rails().await
	}

	async fn list_parked_events(&self, request: Request<pb::ListParkedEventsRequest>) -> Result<Response<pb::ParkedEventList>, Status> {
		require_permission(&self.state, &request, bank::Treasury::Read).await?;
		let rows = crate::infrastructure::outbox::parked_rows(&self.state.pool)
			.await
			.map_err(|_| Status::unavailable("internal error"))?;
		Ok(Response::new(pb::ParkedEventList {
			events: rows
				.into_iter()
				.map(|r| pb::ParkedEvent {
					seq: r.seq,
					event_id: r.event_id.to_string(),
					aggregate: r.aggregate,
					aggregate_id: r.aggregate_id.to_string(),
					kind: r.kind,
					reason: r.last_error.unwrap_or_default(),
					parked_at: r.parked_at_unix,
					compensated: r.compensated,
				})
				.collect(),
		}))
	}

	async fn unpark_event(&self, request: Request<pb::UnparkEventRequest>) -> Result<Response<pb::UnparkEventResponse>, Status> {
		require_permission(&self.state, &request, bank::Outbox::Manage).await?;
		let seq = request.get_ref().seq;
		let unparked = crate::infrastructure::outbox::unpark(&self.state.pool, seq)
			.await
			.map_err(|_| Status::unavailable("internal error"))?;
		if unparked {
			self.state.relay_notify.notify_one();
			return Ok(Response::new(pb::UnparkEventResponse {}));
		}
		// Refused — answer precisely: a compensated park is half-applied with compensation
		// OWED (the relay stamps it at park time, before any recovery runs), so re-driving
		// would re-apply the legs that already posted; a dispatched row has nothing to
		// re-drive.
		match crate::infrastructure::outbox::unpark_refusal(&self.state.pool, seq)
			.await
			.map_err(|_| Status::unavailable("internal error"))?
		{
			Some((_, true)) => Err(Status::failed_precondition("event parked half-applied — compensation is owed; unparking would double-apply")),
			Some((true, _)) => Err(Status::failed_precondition("event already dispatched")),
			_ => Err(Status::not_found("parked event")),
		}
	}

	async fn list_withdrawal_queue(&self, request: Request<pb::ListWithdrawalQueueRequest>) -> Result<Response<pb::WithdrawalQueue>, Status> {
		require_permission(&self.state, &request, bank::Withdrawal::Settle).await?;
		let queued = self.state.withdrawals.list_actionable().await.map_err(map_err)?;
		Ok(Response::new(pb::WithdrawalQueue {
			items: queued
				.into_iter()
				.map(|w| pb::WithdrawalQueueItem {
					withdrawal_id: w.id.to_string(),
					source: w.source.column().to_owned(),
					// Empty for a treasury payout, which belongs to no user.
					user_id: w.source.user().map(|user| user.to_string()).unwrap_or_default(),
					email: w.email,
					network: w.network.as_str().to_owned(),
					address: w.address,
					amount: w.amount.to_decimal_string(),
					net_amount: w.net_amount.to_decimal_string(),
					state: w.state,
					created_at: w.created_at,
				})
				.collect(),
		}))
	}

	async fn rotate_deposit_address(&self, request: Request<pb::RotateDepositAddressRequest>) -> Result<Response<pb::RotateDepositAddressResponse>, Status> {
		require_permission(&self.state, &request, bank::DepositAddress::Rotate).await?;
		let req = request.get_ref();
		let user = parse_user_id(&req.user_id)?;
		let network = Network::parse(&req.network).map_err(map_err)?;
		let address = self.state.deposit_addresses.rotate(user, network).await.map_err(map_err)?;
		// WARN on success on purpose: a rotation is a recovery event worth an audit trail —
		// the old address is permanently unspendable and users may still hold it.
		tracing::warn!(user_id = %req.user_id, network = %req.network, new_address = %address.as_str(), "rotated a dead deposit address");
		Ok(Response::new(pb::RotateDepositAddressResponse {
			address: address.as_str().to_owned(),
		}))
	}

	/// Phase 4 of the Turnkey migration, one `(user, network)` at a time.
	///
	/// Its own permission, not `DepositAddressRotate`: rotation is emergency recovery for a key
	/// that already cannot sign, this deliberately retires one that can. They share a shape and
	/// nothing else, and whoever may do the first should not thereby be able to do the second.
	async fn migrate_deposit_address_to_custodian(
		&self,
		request: Request<pb::MigrateDepositAddressToCustodianRequest>,
	) -> Result<Response<pb::MigrateDepositAddressToCustodianResponse>, Status> {
		require_permission(&self.state, &request, bank::DepositAddress::Migrate).await?;
		let req = request.get_ref();
		let user = parse_user_id(&req.user_id)?;
		let network = Network::parse(&req.network).map_err(map_err)?;
		let migrated = wallet_app::migrate_deposit_address_to_custodian(
			self.state.deposits.as_ref(),
			self.state.deposit_addresses.as_ref(),
			&self.state.configured_networks,
			user,
			network,
		)
		.await
		.map_err(map_err)?;
		// WARN on success, like a rotation: an address changing hands is an audit event. Both
		// addresses go in the line — the old one is still worth watching, because its key is
		// archived rather than destroyed and a late arrival on it stays recoverable.
		tracing::warn!(
			user_id = %req.user_id,
			network = %req.network,
			old_address = %migrated.old_address,
			new_address = %migrated.new_address.as_str(),
			"migrated a deposit address onto the key custodian — the old address is no longer served"
		);
		Ok(Response::new(pb::MigrateDepositAddressToCustodianResponse {
			old_address: migrated.old_address,
			new_address: migrated.new_address.as_str().to_owned(),
		}))
	}
}

fn claim_to_proto(claim: &ClaimBalance) -> pb::ClaimBalance {
	pb::ClaimBalance {
		posted: claim.posted.to_decimal_string(),
		available: claim.available.to_decimal_string(),
		reserved: claim.reserved.to_decimal_string(),
	}
}
