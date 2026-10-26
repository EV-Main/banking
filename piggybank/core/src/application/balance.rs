//! Balance use cases — record chain-proven arrivals (a user's deposit), read the treasury and
//! the fund's revenue.
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
	allocations::{Allocation, AllocationAccess},
	balance::{LedgerAccountKey, Party, ServiceId},
	error::DomainError,
	money::{Network, Shares, TxRef, Usdt},
	users::UserId,
};
use tokio::sync::Notify;

use crate::{
	application::{
		allocations as allocations_app,
		funds::{self as funds_app, NavQuote},
		issuance::UnitHolding,
		ownership::{self as ownership_app, AllocationClaim},
	},
	ports::{AllocationRegistry, Custody, Deposits, custody::InboundTransfer, deposit_addresses::DepositAddresses, ledger::Ledger, nav::NavMarks},
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
/// somebody's (#245): a person's directly (`held_by_users`), or an allocation's, whose
/// units people hold (`allocations`). Nothing here is a remainder: each figure is read
/// off its own accounts, and what they add up to against the custody is the
/// reconciliation's finding, not this view's arithmetic.
pub struct Treasury {
	/// Layer 2 — per-rail on-chain liquidity (USDT ledger).
	pub rails: Vec<RailLiquidity>,
	/// Mocked bank (USD) liquidity — a separate ledger, not 1:1 with USDT (off-ramp FX).
	pub bank: Usdt,
	/// Sum of per-rail custody — the asset side of the USDT ledger.
	pub total_custody: Usdt,
	/// Layer 1 — Σ `user:<id>` claims: what people hold directly, outside any allocation.
	pub held_by_users: Usdt,
	/// Layer 1 — Σ users' scope compartments (`scope_claim`): still people's, earmarked for
	/// a scope (D9).
	pub held_in_compartments: Usdt,
	/// banking's own treasury (`scope_account:main:treasury`, D17) — not an allocation, so
	/// a line of its own.
	pub banking_treasury: AllocationClaim,
	/// Layer 1 — every allocation the registry knows, the hidden `fee` and `fund` included,
	/// each with its claim, its supply and who holds it.
	pub allocations: Vec<AllocationTreasury>,
	/// The amount reserved by queued/in-flight withdrawals and approved payments (the
	/// clearing account's pending balance).
	pub reserved_for_withdrawals: Usdt,
}

/// One allocation as the treasury shows it: the registry's name for it, its ownership
/// picture read off the ledger ([`ownership_app::allocation_ownership`]) and its price
/// ([`funds_app::nav_of`]). The same shape for a product and for the reserved `fee` /
/// `fund` allocations — the admin revenue screen is this struct for `fee`
/// ([`fee_allocation`]), not a second computation of the same accounts.
pub struct AllocationTreasury {
	pub service: ServiceId,
	pub title: String,
	pub access: AllocationAccess,
	/// The allocation's cash: settled, reserved by approved payments out of it, and the
	/// difference. Grows by every subscription, every settled fee class and (for `fee`)
	/// every taker fee — the retained withdrawal fee goes to banking's treasury instead
	/// (D17); falls by a holder's redemption or a payment the owners approved out of it.
	pub claim: AllocationClaim,
	/// Units outstanding — what the cash and the held fee classes are divided over.
	pub units_outstanding: Shares,
	/// The allocation's price and the value it prices — a product's posted mark, or the
	/// computed value of a reserved allocation (cash plus every fee class it holds at
	/// that product's NAV).
	pub nav: NavQuote,
	/// Who holds it, largest first — people, or the `fee` allocation holding a product's
	/// fee class.
	pub holders: Vec<UnitHolding>,
}

/// The driven ports the treasury reads through: the ledger and the chain view for the
/// rails, the registry for the list of allocations, and the marks their units are
/// priced at.
pub struct TreasuryPorts<'a> {
	pub ledger: &'a dyn Ledger,
	pub custody: &'a dyn Custody,
	pub allocations: &'a dyn AllocationRegistry,
	pub nav: &'a dyn NavMarks,
}

/// Record an on-chain deposit, **idempotent by `tx_ref`** (see [`Deposits::record`]).
/// Returns `true` if newly recorded, `false` for a duplicate; the relay is nudged
/// only when a new event was committed.
///
/// The depositor is a person, by type: an allocation's claim grows by a subscription or a
/// settled fee, both of which move a dollar already on the ledger, never by new custody
/// booked straight onto it — a claim credited that way would have nobody behind it (#245).
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
/// anyone is credited as — its arrivals are attributed to a person by the operator who
/// knows who sent them (see [`seed_fund_capital`]) or refused (see
/// [`record_verified_arrival`]) — so the type that names it cannot be the type a deposit
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
/// The allocations are the registry's, every state and level: the treasury is the
/// platform's own picture of what it runs, and the hidden `fee` and `fund` allocations
/// are exactly the ones whose holders an operator has to be able to see. What people
/// hold directly is the cash plane's own sum of `user:<id>` claims — read through the
/// same scan the reconciliation asserts conservation with, so the two can never name
/// different figures.
pub async fn treasury(ports: &TreasuryPorts<'_>) -> Result<Treasury, DomainError> {
	let mut rails = Vec::with_capacity(Network::ALL.len());
	let mut total_custody = Usdt::ZERO;
	for network in Network::ALL {
		let rail_custody = Usdt::from_base_units(ports.ledger.balance(&LedgerAccountKey::CryptoWallet(network)).await?.posted);
		total_custody = total_custody.checked_add(rail_custody).ok_or_else(|| DomainError::Repository("custody total overflow".into()))?;
		let funding = ports.custody.treasury_funding(network).await.unwrap_or_else(|err| {
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
	let bank = Usdt::from_base_units(ports.ledger.balance(&LedgerAccountKey::BankCustody).await?.posted);
	let claims = ports.ledger.cash_invariant().await?;
	let held_by_users = Usdt::from_base_units(claims.user_claims);
	let held_in_compartments = Usdt::from_base_units(claims.scope_claims);
	let banking_treasury = banking_treasury(ports.ledger).await?;
	let mut allocations = Vec::new();
	for allocation in ports.allocations.list_all().await? {
		allocations.push(allocation_treasury(ports.ledger, ports.nav, &allocation).await?);
	}
	let reserved_for_withdrawals = Usdt::from_base_units(ports.ledger.balance(&LedgerAccountKey::WithdrawalClearing).await?.pending);
	Ok(Treasury {
		rails,
		bank,
		total_custody,
		held_by_users,
		held_in_compartments,
		banking_treasury,
		allocations,
		reserved_for_withdrawals,
	})
}

/// banking's own treasury (D17): settled, reserved by approved payments out of it, and what
/// is left — the same three-way split an allocation's claim is read in.
pub async fn banking_treasury(ledger: &dyn Ledger) -> Result<AllocationClaim, DomainError> {
	Ok(ledger.balance(&Party::Treasury.claim_key()).await?.into())
}

/// One allocation's treasury line: the registry row the caller has in hand, its
/// ownership picture and its price, each read once.
///
/// The claim is read on its own rather than taken from the quote's AUM: the AUM of a
/// reserved allocation sums the held fee classes in, and the treasury has to tell cash
/// from units.
pub async fn allocation_treasury(ledger: &dyn Ledger, nav: &dyn NavMarks, allocation: &Allocation) -> Result<AllocationTreasury, DomainError> {
	let service = allocation.service().clone();
	let ownership = ownership_app::allocation_ownership(ledger, service.clone()).await?;
	let quote = funds_app::nav_of(nav, ledger, &service).await?;
	Ok(AllocationTreasury {
		service,
		title: allocation.title().to_owned(),
		access: allocation.access(),
		claim: ownership.claim,
		units_outstanding: ownership.units_outstanding,
		nav: quote,
		holders: ownership.holders,
	})
}

/// The `fee` allocation as its owners read it — the view behind the admin revenue screen
/// (#245). What the platform has earned is not a claim it may pay itself out of: it is an
/// allocation people hold through units, priced at what it holds, and cash leaves it only
/// by a holder's redemption. So the screen shows the allocation — the same line the
/// treasury lists it as, for the one reserved slug. `NotFound` if the registry has no
/// `fee` row (a database that predates migration `0045`).
pub async fn fee_allocation(allocations: &dyn AllocationRegistry, ledger: &dyn Ledger, nav: &dyn NavMarks) -> Result<AllocationTreasury, DomainError> {
	let fee = allocations_app::get(allocations, &ServiceId::fee()).await?;
	allocation_treasury(ledger, nav, &fee).await
}
