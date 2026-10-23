//! Wallet query use cases — a user's unified balance, deposit rails, and per-rail
//! withdrawal options.
//!
//! The user has **one** network-agnostic claim; the wallet presents it segmented by
//! lifecycle. Read-First: `available`, `pending_withdrawal`, and the cash side of `total`
//! all come live from the **same** TigerBeetle claim balance (the authoritative data
//! plane) — `available = posted − reserved`, `pending_withdrawal = reserved`, so
//! `available + pending_withdrawal == posted` by construction and the figures cannot
//! drift. `in_orders` is the cash the book holds in escrow for the user's resting buy
//! orders (`BookCash`, a second claim account of theirs); `invested` is the sum of
//! active stakes valued at NAV, the units escrowed by resting sell orders included;
//! `in_compartments` is the user's money in scope compartments (D9), still theirs.
//! Network re-enters only as a transaction attribute: a per-rail deposit address and a
//! per-rail withdrawable view
//! (`instant = min(available, rail liquidity)`, the accept-and-queue degradation hint).
//!
//! Only **configured** rails (those with a running on-chain watcher) are presented or
//! provisioned at all: an unconfigured rail is omitted entirely — never provisioned,
//! not merely "address pending" — because a deposit sent to an address no watcher
//! scans is stranded, not credited. The **verification** gate sits in exactly the same
//! place and for the same reason: the first [`DepositAddresses::address`] call provisions
//! a signer keypair, so a key minted for an unverified user is an address the fund must
//! then watch, sweep and account for forever — money the platform is not allowed to take
//! yet. Both gates therefore run ABOVE the port, never after it.
//!
//! The verification gate is the one of the two that can be switched off — see
//! [`KycGate`], enforced unless the deployment says otherwise — because it guards a rule
//! the platform chose, while the rail gate guards a fact about the chain.

use domain::{
	balance::LedgerAccountKey,
	error::DomainError,
	money::{Network, Shares, Usdt, WalletAddress},
	users::UserId,
	withdrawals::WithdrawalPolicy,
};

use crate::{
	application::{compartments as compartments_app, funds as funds_app},
	config::KycGate,
	ports::{CompartmentStore, DepositAddresses, Deposits, FundPositionReader, NavMarks, UserRepository, deposit_addresses::MigratedAddress, deposits::DepositRecord, ledger::Ledger},
};

/// A user's single, network-agnostic balance, segmented by lifecycle. Every figure is
/// non-negative; `total = available + in_orders + invested + pending_withdrawal +
/// in_compartments` — the `main` wallet's terms plus what sits in scope compartments.
/// `available` and `pending_withdrawal` are two views of the same claim (`posted −
/// reserved` and `reserved`), so their sum is the claim's `posted` by construction —
/// never a moment where they diverge and `total` double-counts an in-flight withdrawal.
/// `in_orders` is a second account of the same user (`BookCash`), read in the same
/// pass: a buy order moves cash `Dr UserClaim / Cr BookCash`, so without it `available`
/// would simply drop by the reserve with nothing on the wallet explaining where it went.
pub struct WalletBalance {
	/// Free, spendable now (claim posted − reserved).
	pub available: Usdt,
	/// Escrowed by the user's resting buy orders on the book (`BookCash` posted): the
	/// worst-case notional plus taker fee of each, handed back as the orders fill or are
	/// cancelled. Still the user's money — it counts toward `total`.
	pub in_orders: Usdt,
	/// Held in fund units, valued at the current NAV (`Σ units × NAV`), counting the
	/// units a resting sell order has escrowed (`BookShares`) beside the free holding —
	/// they are still the holder's and still valued, exactly as a position reports them.
	pub invested: Usdt,
	/// Reserved by in-flight withdrawals (the claim's `reserved` = Σ gross the relay has
	/// locked). Read from the ledger, not the `withdrawals` projection, so it stays in
	/// lockstep with `available` off one balance read.
	pub pending_withdrawal: Usdt,
	/// The user's money in their scope compartments (Σ `scope_claim` posted) — theirs, just
	/// earmarked for a scope (D9).
	pub in_compartments: Usdt,
	/// `available + in_orders + invested + pending_withdrawal + in_compartments` — the
	/// user's whole position.
	pub total: Usdt,
}

/// A deposit rail — where to send USDT (on a given chain) to top up the unified
/// balance. Only configured rails appear; an unconfigured one is omitted entirely.
pub struct DepositRail {
	pub network: Network,
	/// `None` when the rail carries no fundable address for this caller: the derived
	/// address is still a placeholder, or the caller is not KYC-verified and so has no
	/// address provisioned at all. This listing deliberately does not distinguish the
	/// two — a wallet screen shows the rail as "not yet fundable" either way, and the
	/// caller learns *why* from [`get_deposit_address`], which refuses an unverified
	/// request outright rather than answering `None`.
	pub address: Option<WalletAddress>,
}

/// Per-rail withdrawal options (the accept-and-queue UX). `withdrawable` is the user's
/// whole available balance (a request beyond `instant` is accepted and queued until the
/// rail is topped up); `instant` is the portion that ships without queueing —
/// `min(available, rail liquidity)`. Note `instant` equals the rail's liquidity exactly
/// when the user's available exceeds it, so it discloses that rail's liquidity up to the
/// user's own balance — an inherent cost of the queue hint (bucket/round it below if that
/// must stay private).
pub struct NetworkWithdrawable {
	pub network: Network,
	pub withdrawable: Usdt,
	pub instant: Usdt,
	pub min_withdrawal: Usdt,
	pub withdrawal_fee: Usdt,
}

pub struct Wallet {
	pub balance: WalletBalance,
	pub deposit_addresses: Vec<DepositRail>,
	pub withdrawable: Vec<NetworkWithdrawable>,
}

/// The driven ports the wallet overview borrows: the ledger every figure is read from,
/// the position projection and marks that value the invested slice, the address gateway
/// each rail is presented from, and the user projection whose mirrored KYC tier decides
/// whether an address may be provisioned at all. Bundled so the use-case's own parameters
/// stay its *request* — whose wallet, on which rails. A plain borrow-holder: it owns
/// nothing and does nothing.
pub struct WalletPorts<'a> {
	/// The money gateway (TigerBeetle): the authoritative claim and rail balances.
	pub ledger: &'a dyn Ledger,
	/// The cost-basis/units projection behind the `invested` figure.
	pub positions: &'a dyn FundPositionReader,
	/// The valuation marks `invested` is priced at.
	pub nav: &'a dyn NavMarks,
	/// The signer/key-management seam a rail's address is derived from.
	pub deposit_addresses: &'a dyn DepositAddresses,
	/// The control-plane user row carrying the bridge-mirrored KYC tier.
	pub users: &'a dyn UserRepository,
	/// The scopes the user may hold compartments in — `in_compartments` sums them.
	pub compartments: &'a dyn CompartmentStore,
}

/// The driven ports the single-rail deposit-address read borrows. Kept separate from
/// [`WalletPorts`] rather than reusing it: this use case can reach neither the ledger nor
/// the marks, and a field it could never use would only invite one.
pub struct DepositAddressPorts<'a> {
	/// The signer/key-management seam — asked only once the gates below have passed.
	pub deposit_addresses: &'a dyn DepositAddresses,
	/// The control-plane user row carrying the bridge-mirrored KYC tier.
	pub users: &'a dyn UserRepository,
}

/// Whether `user` has cleared the verification tier that money movement requires. A
/// missing row fails CLOSED — an id with no local mirror is not a verified investor.
///
/// A lifted [`KycGate`] answers without reading the control plane at all: there is no tier
/// to compare, and the read would only be a round trip whose result is discarded.
async fn is_verified(gate: KycGate, users: &dyn UserRepository, user: UserId) -> Result<bool, DomainError> {
	if !gate.is_enforced() {
		return Ok(true);
	}
	Ok(users.find_by_id(user).await?.is_some_and(|account| gate.admits(account.kyc_level())))
}

/// The caller's wallet: the unified lifecycle balance, a deposit address per
/// configured rail, and the per-rail withdrawable view.
///
/// An unverified caller still gets their whole balance — nothing here is hidden from
/// them — but every rail comes back with no address, because provisioning one would mint
/// the signer keypair the verification gate exists to withhold. With `gate` lifted there
/// is nothing to withhold and every configured rail carries its address.
pub async fn get_wallet(ports: &WalletPorts<'_>, configured: &[Network], gate: KycGate, user: UserId) -> Result<Wallet, DomainError> {
	let (ledger, positions, nav, deposit_addresses) = (ports.ledger, ports.positions, ports.nav, ports.deposit_addresses);
	// Layer 1 — the single unified claim. `available` and `pending_withdrawal` are the two
	// sides of this one balance (`posted − reserved` and `reserved`), so they can never
	// disagree about an in-flight withdrawal — the reserve (`Dr user / Cr clearing`
	// pending) is the only thing that locks a claim, and it moves both fields together.
	// The compartments are read in the same ledger lookup as the claim, so `total` adds
	// two figures of one instant.
	let (held, book_cash, held_positions) = tokio::try_join!(
		compartments_app::holdings(ports.compartments, ledger, user),
		async { ledger.balance(&LedgerAccountKey::BookCash(user)).await.map_err(DomainError::from) },
		positions.list(user),
	)?;
	let claim = held.main;
	let available = claim.available();
	// Sourced from `reserved`, NOT the `withdrawals` table: the projection counted a row
	// the instant it was created, while the ledger reserve applies asynchronously — so the
	// two summed as if simultaneously consistent and `total` transiently (or permanently,
	// if a reserve parked) overstated the claim by the gross.
	let pending_withdrawal = claim.locked;
	// The book's cash escrow is a separate account; posted, because a lock and a release
	// are both posted transfers (nothing pends on it).
	let in_orders = Usdt::from_base_units(book_cash.posted);

	// invested = the value of the user's fund positions: live units × current NAV, the
	// units a resting sell escrowed included — a sell order moves them `Dr BookShares /
	// Cr UserShares`, and a wallet that read only the holding would show the stake
	// shrinking the moment an order rests.
	let mut invested = Usdt::ZERO;
	for position in held_positions {
		let held = Shares::from_base_units(ledger.balance(&LedgerAccountKey::UserShares(position.service.clone(), user)).await?.posted);
		let escrowed = Shares::from_base_units(ledger.balance(&LedgerAccountKey::BookShares(position.service.clone(), user)).await?.posted);
		let owned = held.checked_add(escrowed).ok_or_else(|| DomainError::Repository("position units overflow".into()))?;
		if owned.is_zero() {
			continue;
		}
		let price = funds_app::nav_of(nav, ledger, &position.service).await?.nav;
		let value = price.value(owned)?;
		invested = invested.checked_add(value).ok_or_else(|| DomainError::Repository("invested total overflow".into()))?;
	}

	// total is the whole claim (its settled `posted`, which still carries the reserved
	// gross as a pending debit until the withdrawal settles) plus the book's cash escrow
	// plus invested plus the compartments — so `total == available + pending_withdrawal +
	// in_orders + invested + in_compartments` by construction, and a buy order or a move
	// between `main` and a compartment shifts cash between terms without moving the sum.
	let in_compartments = held.compartments.posted;
	let total = claim
		.posted
		.checked_add(in_orders)
		.and_then(|sum| sum.checked_add(invested))
		.and_then(|sum| sum.checked_add(in_compartments))
		.ok_or_else(|| DomainError::Repository("wallet total overflow".into()))?;

	let balance = WalletBalance {
		available,
		in_orders,
		invested,
		pending_withdrawal,
		in_compartments,
		total,
	};

	// Layer 2 — per-rail deposit addresses and withdrawable view, configured rails only.
	// Resolved once, outside the loop: an unverified caller must not reach the address
	// gateway on ANY rail, and one user read is enough to decide that for all of them.
	let verified = is_verified(gate, ports.users, user).await?;
	let mut deposit_addresses_out = Vec::with_capacity(configured.len());
	let mut withdrawable = Vec::with_capacity(configured.len());
	for network in configured.iter().copied() {
		// `None` ⇒ no fundable address yet — the derived address is still a placeholder,
		// or the caller is unverified so none was ever provisioned. Either way the rail is
		// presented as unavailable, never with an address that cannot receive funds.
		deposit_addresses_out.push(DepositRail {
			network,
			address: if verified { deposit_addresses.address(user, network).await? } else { None },
		});
		// `instant` = min(available, rail liquidity) — "this much ships without queueing".
		// It reveals the rail's liquidity only up to the user's own balance (see the
		// NetworkWithdrawable doc); bucket/round here if that disclosure must be avoided.
		let rail_liquidity = Usdt::from_base_units(ledger.balance(&LedgerAccountKey::CryptoWallet(network)).await?.posted);
		withdrawable.push(NetworkWithdrawable {
			network,
			withdrawable: available,
			instant: available.min(rail_liquidity),
			min_withdrawal: WithdrawalPolicy::minimum(network),
			withdrawal_fee: WithdrawalPolicy::fee(network),
		});
	}

	Ok(Wallet {
		balance,
		deposit_addresses: deposit_addresses_out,
		withdrawable,
	})
}

/// The caller's deposit address on `network` (stable; derived once and reused).
///
/// Two refusals, deliberately told apart. `Ok(None)` means **the rail cannot fund this
/// account**: it is unconfigured, or the derived address is still a placeholder — nothing
/// the caller can act on, so the screen offers another rail. [`DomainError::Forbidden`]
/// means **the caller is not verified yet** — a state they can leave, and the one screen
/// worth showing them is "finish verification". Folding the second into the first would
/// leave the wallet host unable to tell "try another chain" from "verify your identity", so
/// the verification failure is an error rather than an absent value.
///
/// Both gates sit ABOVE the port, in that order: the rail check is free and answers
/// without touching the control plane, and the first `DepositAddresses::address` call
/// provisions a signer keypair — a key minted here for an unverified user is an address
/// the fund must watch and sweep forever. A lifted `gate` removes the second refusal
/// entirely (the rail one always stands): every caller is served an address.
pub async fn get_deposit_address(ports: &DepositAddressPorts<'_>, configured: &[Network], gate: KycGate, user: UserId, network: Network) -> Result<Option<WalletAddress>, DomainError> {
	if !configured.contains(&network) {
		return Ok(None);
	}
	if !is_verified(gate, ports.users, user).await? {
		return Err(DomainError::Forbidden("identity verification required before a deposit address is issued".into()));
	}
	ports.deposit_addresses.address(user, network).await
}

/// The caller's credited on-chain deposits (projection), newest first.
pub async fn list_deposits(deposits: &dyn Deposits, user: UserId) -> Result<Vec<DepositRecord>, DomainError> {
	deposits.list_by_user(user).await
}

/// Retire `user`'s healthy KEK-sealed deposit address on `network` in favour of a
/// custody-held one (phase 4 of `docs/MIGRATION-turnkey.md`).
///
/// **The funds gate lives here**, above the port, for the same reason
/// [`get_deposit_address`]'s does: only the hub can answer it. The signer holds keys and
/// produces signatures and has no chain view at all, so the question "is anything still on
/// that address" is not one it can be asked. This hub owns the deposit scanner and the
/// `deposits` table, so it asks the one question that has an authoritative local answer —
/// does the sweeper still consider this address capable of holding funds — and refuses while
/// the answer is yes.
///
/// Order matters and is not interchangeable. The gate runs BEFORE the address is read, and the
/// address that was cleared is the one handed to the port, so nothing can be cleared for one
/// address and retired for another. That value travels all the way to the signer, which
/// refuses unless it names the row it is about to archive.
pub async fn migrate_deposit_address_to_custodian(
	deposits: &dyn Deposits,
	deposit_addresses: &dyn DepositAddresses,
	configured: &[Network],
	user: UserId,
	network: Network,
) -> Result<MigratedAddress, DomainError> {
	if !configured.contains(&network) {
		return Err(DomainError::Validation(format!("{network} is not a configured rail")));
	}
	// THE gate. `has_unswept` is the sweeper's own predicate, so "safe to retire" here means
	// exactly "the sweeper has stopped scanning this address" — never a second, weaker opinion.
	if deposits.has_unswept(user, network).await? {
		return Err(DomainError::Validation(format!(
			"{user} still has an unswept credited deposit on {network} — sweep the address to the treasury before retiring it"
		)));
	}
	// Read the address only after the gate passes, and pass THAT value on: the address proved
	// drained and the address about to be retired must be one and the same.
	let drained = deposit_addresses
		.address(user, network)
		.await?
		.ok_or_else(|| DomainError::Validation(format!("{user} has no fundable {network} address to migrate")))?;
	deposit_addresses.migrate_to_custodian(user, network, drained.as_str()).await
}
