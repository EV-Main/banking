//! Balance use cases — record chain-proven arrivals (a user's deposit, the capital a
//! person seeds the platform with), read the treasury and the fund's revenue.
//!
//! Commands validate and hand the fact to the [`Deposits`] port, whose adapter is
//! its own atomic unit (one Postgres transaction: the gate row + the outbox event),
//! then `notify` the relay to move money in TigerBeetle afterwards (Write-Last).
//! The query reads live, TigerBeetle-authoritative balances (Read-First).
//!
//! Every operator write here is a *verification*, never a statement: the caller names a
//! chain transaction and the amount and the credited party are read back from it. There
//! is no path that credits a claim from a number an operator typed — the last one
//! (`SeedCapital` with a free amount and no dedup key) was removed in issue #234.
//!
//! Nothing here credits a claim nobody holds (#245). USDT that reaches a treasury hot
//! wallet from outside is *somebody's* — the person who sent it — and is booked as their
//! deposit, followed by their subscription into the `fund` allocation
//! ([`seed_fund_capital`]). A deposit is always a person's: [`record_deposit`] takes a
//! user, and the `deposits` CHECK says the same since migration 0047. WHOSE it
//! is, the chain cannot say, so the attribution is the owners' quorum's
//! (`consilium::open_seed_capital`), never one administrator's: the RPC opens the
//! consilium, and only its execution reaches [`seed_fund_capital`].

use domain::{
	allocations::{Allocation, AllocationAccess},
	balance::{LedgerAccountKey, Party, ServiceId},
	error::DomainError,
	money::{Network, Shares, TxRef, Usdt},
	subscriptions::{Subscription, SubscriptionId},
	users::UserId,
};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{
	application::{
		allocations as allocations_app,
		funds::{self as funds_app, FundPorts, NavQuote},
		issuance::UnitHolding,
		ownership::{self as ownership_app, AllocationClaim},
	},
	ports::{
		AllocationRegistry, Custody, Deposits, SubscriptionRepository,
		custody::InboundTransfer,
		deposit_addresses::DepositAddresses,
		ledger::{Ledger, NoSpendCheck},
		nav::NavMarks,
	},
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
/// arrived but not whose it is, and this path credits only whom the chain names. The
/// owner who knows the sender proposes the attribution with `SeedCapital` instead, and
/// the owners' quorum executes it.
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
				"{} is the {network} treasury, so the chain cannot say whose deposit this is — attribute it to its sender with SeedCapital",
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

/// The driven ports a seed borrows: the arrival's evidence (chain + address ownership),
/// the deposit gate it is recorded through, the dealing ports the subscription is priced
/// and gated with, and the store it is opened in.
pub struct SeedPorts<'a> {
	pub deposits: &'a dyn Deposits,
	pub custody: &'a dyn Custody,
	pub addresses: &'a dyn DepositAddresses,
	pub subscriptions: &'a dyn SubscriptionRepository,
	pub funds: FundPorts<'a>,
}

/// What a seed did: whether THIS call recorded the arrival (`false` is the idempotent
/// repeat), the amount the chain reported, and the subscription the depositor's units
/// come from — `None` on a repeat that found the mint already there, `Some` on the one
/// that had to open it (see [`seed_fund_capital`]).
#[derive(Debug)]
pub struct SeededCapital {
	pub recorded: bool,
	pub amount: Usdt,
	pub subscription: Option<Subscription>,
}

/// Seed the platform's capital: a chain-proven arrival on the treasury, booked as
/// `depositor`'s deposit and at once subscribed into the `fund` allocation (#245).
///
/// [`record_verified_arrival`]'s evidence with the opposite verdict on where the money
/// landed: the transfer the reference names must have reached the rail's treasury from an
/// external sender. One that landed on a user's deposit address is that user's deposit —
/// booking it under someone else would hand the depositor a dollar owed to that user — so
/// it is refused and pointed at `RecordDeposit`. The sweep is refused by the shared
/// attribution, as everywhere.
///
/// Two ordinary facts, no new kind of money move: the deposit (`Dr wallet:<net> /
/// Cr user:<depositor>`) and a subscription into `fund` at its computed NAV, exactly the
/// legs an investor's subscription posts. The relay drains them in commit order, so the
/// cash leg finds the deposit already credited; the subscription is therefore opened
/// **without** the Read-First balance check an investor's subscribe runs (the deposit is
/// the balance, and it is not on the ledger yet) — TigerBeetle's non-negative flag stays
/// the backstop, and a parked cash leg leaves the money on the depositor's own claim,
/// where every dollar still has a holder.
///
/// Idempotent by `tx_ref`: the deposit gate admits a reference once, and the subscription's
/// id is derived from the same reference, so a double mint is impossible even under a
/// race. A repeat reports `recorded: false` and normally mints nothing — unless the first
/// call recorded the deposit and died before opening the subscription, which is the one
/// state a repeat repairs: the mint is missing under its deterministic id, the deposit is
/// this depositor's, so the subscription is opened now, priced at the current NAV and
/// through the same gates. A reference recorded under another person's name is refused
/// rather than minted to the caller.
///
/// The subscription is priced and gated **before** the deposit is recorded, so a `fund`
/// that is not dealing or a stale price refuses with nothing written. The one gate an
/// investor's subscribe runs that this does not is the catalog's access level: `fund` is
/// hidden from everyone by design, and the person who seeds it becomes its holder by the
/// act itself — the owners' quorum that executes the seed is the admission.
///
/// No RPC reaches this directly: `SeedCapital` opens a `seed_capital` consilium over the
/// same facts, and this is what its execution calls (`consilium::execute`). The one-off
/// data migration that seeds the first holders is the other caller.
pub async fn seed_fund_capital(
	ports: &SeedPorts<'_>,
	depositor: UserId,
	tx_ref: TxRef,
	network: Network,
	expected_amount: Option<Usdt>,
	now_unix: i64,
) -> Result<SeededCapital, DomainError> {
	let (arrival, transfer) = verify_arrival(ports.custody, ports.addresses, network, &tx_ref, expected_amount).await?;
	if let Arrival::User(_) = arrival {
		return Err(DomainError::Validation(format!(
			"{} is a user's deposit address, so this transfer is that user's deposit, not seed capital — record it with RecordDeposit",
			transfer.to
		)));
	}
	let subscription_id = seed_subscription_id(&tx_ref);
	let mut subscription = funds_app::price_fund_seed(&ports.funds, subscription_id, depositor, transfer.amount, now_unix).await?;
	let recorded = record_deposit(ports.deposits, ports.funds.relay, tx_ref.clone(), depositor, network, transfer.amount).await?;
	if !recorded {
		// The deposit and the subscription are two commits: a process that died between
		// them left the cash on the depositor's own claim with no units against it. A repeat
		// is where that gets fixed — the id is a function of the reference, so the mint the
		// first call meant to open is the one looked up here, and one exists at most once.
		if ports.subscriptions.find_by_id(subscription_id).await?.is_some() {
			return Ok(SeededCapital {
				recorded: false,
				amount: transfer.amount,
				subscription: None,
			});
		}
		// Only the person the deposit was booked to can be minted against it: opening the
		// subscription under another name would pull that person's own cash into `fund`
		// and leave the first depositor's on their claim.
		if !ports.deposits.list_by_user(depositor).await?.iter().any(|deposit| deposit.tx_ref == tx_ref) {
			return Err(DomainError::Conflict(format!(
				"{} was already recorded as another person's deposit — its fund subscription can only be opened for them",
				tx_ref.as_str()
			)));
		}
		tracing::warn!(tx_ref = %tx_ref.as_str(), %depositor, "seed capital: the deposit was recorded but its fund subscription was never opened — re-opening it");
	}
	// The cash is the seed's own deposit, booked in this same operation and not on the
	// ledger yet — there is nothing for a cover check to read.
	ports.subscriptions.open(&mut subscription, &NoSpendCheck).await?;
	ports.funds.relay.notify_one();
	Ok(SeededCapital {
		recorded,
		amount: transfer.amount,
		subscription: Some(subscription),
	})
}

/// The seed subscription's id, a function of the chain reference: one transfer, one mint.
/// Public so a seed consilium can name its effect without re-reading the row it opened.
pub fn seed_subscription_id(tx_ref: &TxRef) -> SubscriptionId {
	SubscriptionId::from_raw(Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("seed:{}", tx_ref.as_str()).as_bytes()))
}

/// The chain's account of a reference: the transfer it names and whose money it is.
///
/// One function for both operator write paths — and for the seed consilium's open gate —
/// so they can never disagree about what counts as proven: the lookup, the optional
/// assertion and the attribution are the whole of the evidence, and a path that skipped
/// any of them would be the very hole this closes.
pub(crate) async fn verify_arrival(
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
