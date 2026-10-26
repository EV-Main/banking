//! Balance use cases — record chain-proven arrivals (a user's deposit), read the treasury.
//!
//! Commands validate and hand the fact to the [`Deposits`] port, whose adapter is
//! its own atomic unit (one Postgres transaction: the gate row + the outbox event),
//! then `notify` the relay to move money in TigerBeetle afterwards (Write-Last).
//! The query reads live, TigerBeetle-authoritative balances (Read-First).
//!
//! Every operator write here is a *verification*, never a statement: the caller names a
//! chain transaction and the amount and the credited party are read back from it. There
//! is no path that credits a claim from a number an operator typed.
//!
//! Nothing here credits a claim nobody holds (#245). A deposit is always a person's:
//! [`record_deposit`] takes a user, and the `deposits` CHECK says the same since migration
//! 0047. USDT that reaches a treasury hot wallet from outside is somebody's, but WHOSE the
//! chain cannot say, so it is refused rather than attributed by one administrator.

use domain::{
	balance::{LedgerAccountKey, Party},
	error::DomainError,
	money::{Network, TxRef, Usdt},
	users::UserId,
};
use tokio::sync::Notify;

use crate::ports::{
	Custody, Deposits,
	custody::InboundTransfer,
	deposit_addresses::DepositAddresses,
	ledger::{Ledger, LedgerBalance},
};

/// Per-rail on-chain liquidity (the treasury / Layer 2). `custody` is
/// TigerBeetle-authoritative; the funding fields are the operator's chain view,
/// enriched best-effort — `None` when the rail is unconfigured or the read failed.
pub struct RailLiquidity {
	pub network: Network,
	/// Liquid on-chain USDT the fund holds in this rail's custody wallet.
	pub custody: Usdt,
	/// The rail's treasury hot wallet — the operator funds USDT + gas here.
	pub treasury_address: Option<String>,
	/// USDT actually on-chain in the treasury hot wallet.
	pub onchain_usdt: Option<Usdt>,
	/// Native-coin gas balance (BNB/TRX/TON), pre-rendered by the adapter.
	pub onchain_gas: Option<String>,
	/// The rail's sweep gas-station wallet — fund the native coin here (never USDT).
	pub gas_station_address: Option<String>,
	/// The gas station's native-coin balance, pre-rendered by the adapter.
	pub gas_station_gas: Option<String>,
}

/// The treasury picture: per-rail liquidity (Layer 2) and who the claims on it belong
/// to (Layer 1). Under the unified-claim model the invariant is **global** —
/// `total_custody` (the asset side) equals the sum of all claims — and every claim is
/// somebody's (#245): a person's (`held_by_users`, `held_in_compartments`), or banking's
/// own (`banking_treasury`). Nothing here is a remainder: each figure is read
/// off its own accounts, and what they add up to against the custody is the
/// reconciliation's finding, not this view's arithmetic.
pub struct Treasury {
	/// Layer 2 — per-rail on-chain liquidity (USDT ledger).
	pub rails: Vec<RailLiquidity>,
	/// Mocked bank (USD) liquidity — a separate ledger, not 1:1 with USDT (off-ramp FX).
	pub bank: Usdt,
	/// Sum of per-rail custody — the asset side of the USDT ledger.
	pub total_custody: Usdt,
	/// Layer 1 — Σ `user:<id>` claims: what people hold in `main`.
	pub held_by_users: Usdt,
	/// Layer 1 — Σ users' scope compartments (`scope_claim`): still people's, earmarked for
	/// a scope (D9).
	pub held_in_compartments: Usdt,
	/// banking's own treasury (`scope_account:main:treasury`, D17).
	pub banking_treasury: ClaimBalance,
	/// The amount reserved by queued/in-flight withdrawals and approved payments (the
	/// clearing account's pending balance).
	pub reserved_for_withdrawals: Usdt,
}

/// A claim's cash, split the way every reader of it needs: what is settled, what an approved
/// payment out of it has spoken for, and what is left to spend. Read off ONE balance, so
/// the three can never disagree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaimBalance {
	pub posted: Usdt,
	/// `posted − reserved`.
	pub available: Usdt,
	/// Locked by approved payments out of the claim that have not settled.
	pub reserved: Usdt,
}

impl From<LedgerBalance> for ClaimBalance {
	fn from(balance: LedgerBalance) -> Self {
		Self {
			posted: Usdt::from_base_units(balance.posted),
			available: Usdt::from_base_units(balance.available()),
			reserved: Usdt::from_base_units(balance.locked),
		}
	}
}

/// Record an on-chain deposit, **idempotent by `tx_ref`** (see [`Deposits::record`]).
/// Returns `true` if newly recorded, `false` for a duplicate; the relay is nudged
/// only when a new event was committed.
///
/// The depositor is a person, by type: a claim credited with new custody on anyone else's
/// say-so would have nobody behind it (#245).
pub async fn record_deposit(deposits: &dyn Deposits, relay: &Notify, tx_ref: TxRef, user: UserId, network: Network, amount: Usdt) -> Result<bool, DomainError> {
	if amount.is_zero() {
		return Err(DomainError::Validation("deposit amount must be positive".into()));
	}
	let recorded = deposits.record(tx_ref, user, network, amount).await?;
	if recorded {
		relay.notify_one();
	}
	Ok(recorded)
}

/// Whose money a confirmed transfer is, decided by the address it landed on.
///
/// An application-layer answer rather than a [`Party`]: the treasury is not a party
/// anyone is credited as — its arrivals are refused (see [`record_verified_arrival`]) — so the type that names it cannot be the type a deposit
/// is recorded against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arrival {
	/// Landed on this user's derived deposit address: their deposit.
	User(UserId),
	/// Landed on the rail's treasury hot wallet from outside every wallet we control.
	Treasury,
}

/// What a verified arrival turned out to be, once the chain had its say.
#[derive(Debug)]
pub struct VerifiedArrival {
	pub recorded: bool,
	/// The person whose claim the deposit credits.
	pub user: UserId,
	pub amount: Usdt,
}

/// Record an out-of-band arrival, taking every material fact from the CHAIN.
///
/// This is the operator's general way to write a deposit by hand, and it is deliberately
/// not a way to *state* one. The caller supplies a reference; the amount and the credited
/// party are read back from the transfer that reference names. So the operator surface
/// cannot mint a balance — the worst a bad reference achieves is a refusal.
///
/// Read-First against the chain, then the ordinary idempotent `record_deposit`, so a
/// hand-verified arrival and a scanned one collapse onto the same `tx_ref` and one transfer
/// can never be booked twice.
///
/// A transfer that landed on the TREASURY is refused here: the chain proves the dollar
/// arrived but not whose it is, and this path credits only whom the chain names.
pub async fn record_verified_arrival(
	deposits: &dyn Deposits,
	custody: &dyn Custody,
	addresses: &dyn DepositAddresses,
	relay: &Notify,
	tx_ref: TxRef,
	network: Network,
	expected_amount: Option<Usdt>,
) -> Result<VerifiedArrival, DomainError> {
	let (arrival, transfer) = verify_arrival(custody, addresses, network, &tx_ref, expected_amount).await?;
	let user = match arrival {
		Arrival::User(user) => user,
		Arrival::Treasury => {
			return Err(DomainError::Validation(format!(
				"{} is the {network} treasury, so the chain cannot say whose deposit this is",
				transfer.to
			)));
		}
	};
	let recorded = record_deposit(deposits, relay, tx_ref, user, network, transfer.amount).await?;
	Ok(VerifiedArrival {
		recorded,
		user,
		amount: transfer.amount,
	})
}

/// The chain's account of a reference: the transfer it names and whose money it is.
///
/// The lookup, the optional assertion and the attribution are the whole of the evidence, and
/// a path that skipped any of them would be the very hole this closes.
async fn verify_arrival(
	custody: &dyn Custody,
	addresses: &dyn DepositAddresses,
	network: Network,
	tx_ref: &TxRef,
	expected_amount: Option<Usdt>,
) -> Result<(Arrival, InboundTransfer), DomainError> {
	let transfer = custody
		.inbound_transfer(network, tx_ref)
		.await
		.map_err(|e| DomainError::Repository(format!("chain lookup failed: {e}")))?
		.ok_or_else(|| {
			DomainError::Validation(format!(
				"no confirmed {network} USDT transfer matches {} — check the reference, the rail, and that it has enough confirmations",
				tx_ref.as_str()
			))
		})?;
	// An assertion, never an input: it can only cause a refusal. Its job is to turn a
	// reference that points at some OTHER real transfer — a copy-paste from the wrong row —
	// into a loud error instead of a silent credit of the wrong amount.
	if let Some(expected) = expected_amount
		&& expected != transfer.amount
	{
		return Err(DomainError::Validation(format!(
			"the chain reports {} USDT for this reference, not {}",
			transfer.amount.to_decimal_string(),
			expected.to_decimal_string()
		)));
	}
	let arrival = attribute(custody, addresses, network, &transfer).await?;
	Ok((arrival, transfer))
}

/// Decide whose money a confirmed transfer is, from its recipient — and refuse anything that
/// is not ours to credit.
///
/// The treasury case carries the one subtlety: the sweep also lands there, moving USDT from a
/// user's own deposit address, and that dollar is already in `wallet:<net>` behind a claim.
/// Crediting it again would invent custody and break `sum(custody) == sum(claims)`, so a
/// treasury arrival only counts when it came from outside every wallet we control.
async fn attribute(custody: &dyn Custody, addresses: &dyn DepositAddresses, network: Network, transfer: &InboundTransfer) -> Result<Arrival, DomainError> {
	if let Some(user) = addresses.owner_of(network, &transfer.to).await? {
		return Ok(Arrival::User(user));
	}
	let funding = custody
		.treasury_funding(network)
		.await
		.map_err(|e| DomainError::Repository(format!("treasury address unavailable: {e}")))?;
	let is_treasury = funding.as_ref().is_some_and(|f| f.address.eq_ignore_ascii_case(&transfer.to));
	if !is_treasury {
		return Err(DomainError::Validation(format!(
			"{} received this transfer and it is not one of our {network} addresses",
			transfer.to
		)));
	}
	let gas_station = funding.as_ref().and_then(|f| f.gas_station_address.clone());
	let internal = addresses.owner_of(network, &transfer.from).await?.is_some()
		|| gas_station.is_some_and(|g| g.eq_ignore_ascii_case(&transfer.from))
		|| funding.as_ref().is_some_and(|f| f.address.eq_ignore_ascii_case(&transfer.from));
	if internal {
		return Err(DomainError::Validation(
			"this transfer is the sweep consolidating funds already on the ledger, not new capital".into(),
		));
	}
	Ok(Arrival::Treasury)
}

/// The treasury, read live from TigerBeetle (Read-First): per-rail liquidity plus the
/// claims it backs, each claim at its holders. Each rail is enriched with the custody
/// adapter's funding view (hot-wallet address + real on-chain USDT/gas) **best-effort**
/// — an unwired rail or a chain-RPC failure leaves those fields `None`; the ledger read
/// must never fail because a chain node is down.
///
/// What people hold is the cash plane's own sum of their claims — read through the same
/// scan the reconciliation asserts conservation with, so the two can never name different
/// figures.
pub async fn treasury(ledger: &dyn Ledger, custody: &dyn Custody) -> Result<Treasury, DomainError> {
	let mut rails = Vec::with_capacity(Network::ALL.len());
	let mut total_custody = Usdt::ZERO;
	for network in Network::ALL {
		let rail_custody = Usdt::from_base_units(ledger.balance(&LedgerAccountKey::CryptoWallet(network)).await?.posted);
		total_custody = total_custody.checked_add(rail_custody).ok_or_else(|| DomainError::Repository("custody total overflow".into()))?;
		let funding = custody.treasury_funding(network).await.unwrap_or_else(|err| {
			tracing::debug!(%network, "treasury funding view unavailable: {err}");
			None
		});
		let (treasury_address, onchain_usdt, onchain_gas, gas_station_address, gas_station_gas) = match funding {
			Some(f) => (Some(f.address), f.onchain_usdt, f.onchain_gas, f.gas_station_address, f.gas_station_gas),
			None => (None, None, None, None, None),
		};
		rails.push(RailLiquidity {
			network,
			custody: rail_custody,
			treasury_address,
			onchain_usdt,
			onchain_gas,
			gas_station_address,
			gas_station_gas,
		});
	}
	let bank = Usdt::from_base_units(ledger.balance(&LedgerAccountKey::BankCustody).await?.posted);
	let claims = ledger.cash_invariant().await?;
	let held_by_users = Usdt::from_base_units(claims.user_claims);
	let held_in_compartments = Usdt::from_base_units(claims.scope_claims);
	let banking_treasury = ledger.balance(&Party::Treasury.claim_key()).await?.into();
	let reserved_for_withdrawals = Usdt::from_base_units(ledger.balance(&LedgerAccountKey::WithdrawalClearing).await?.pending);
	Ok(Treasury {
		rails,
		bank,
		total_custody,
		held_by_users,
		held_in_compartments,
		banking_treasury,
		reserved_for_withdrawals,
	})
}
