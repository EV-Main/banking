//! The custody/signing-service port — the narrow "broadcast this withdrawal" seam.
//!
//! Custody is a **separate trust domain** — our signer (`piggybank/signer`). It holds
//! the key handles, applies its own spend policy (`SignerPolicy`: a per-transfer USDT
//! cap plus a destination allowlist), and is the second gate even if the hub is
//! compromised. The key custodian behind it (Turnkey, `sign_raw_payload` over a 32-byte
//! digest) sees neither amount nor destination and enforces nothing about them. The hub
//! never signs. This port is all the hub asks of it — submit an *already-reserved*
//! withdrawal for on-chain broadcast, **idempotently by `withdrawal_id`** (a retried
//! relay delivery must not double-send). A stub adapter stands in for rails without a
//! configured signer (an operator settles manually).

use async_trait::async_trait;
use domain::{
	architecture::Gateway,
	money::{Network, TxRef, Usdt, WalletAddress},
};
use thiserror::Error;
use uuid::Uuid;

/// A [`Gateway`]: an external transactional system that owns its own atomicity —
/// by construction it can never enrol in a Postgres transaction.
#[async_trait]
pub trait Custody: Gateway {
	/// Submit the withdrawal's on-chain leg for signing + broadcast. MUST be
	/// idempotent by `request.withdrawal_id` so an at-least-once relay never
	/// double-spends.
	async fn broadcast(&self, request: &BroadcastRequest) -> Result<(), CustodyError>;

	/// The rail treasury's spendable **on-chain** USDT, in 18-dp base units — the
	/// withdrawal **dispatch gate**'s source. The ledger's `wallet:<net>` balance is
	/// accounting (it counts un-swept deposit-address funds the treasury cannot spend),
	/// so dispatchability is `min(TB rail, this)`. `None` means the adapter has no chain
	/// view (the stub / an unwired rail) — callers fall back to the TB-only behaviour.
	/// Read-only: never signs, never allocates a nonce/seqno; the adapter's broadcast-time
	/// `ensure_treasury_funded` remains the last-line backstop.
	async fn treasury_liquidity(&self, network: Network) -> Result<Option<Usdt>, CustodyError> {
		let _ = network;
		Ok(None)
	}

	/// The USDT sitting on this rail's **derived deposit addresses** — credited deposits the
	/// sweep has not consolidated yet, in 18-dp base units.
	///
	/// `treasury_liquidity` plus this is every wallet we control on the rail, which is exactly
	/// what `wallet:<net>` claims to count. The pair is the only way to check the ledger against
	/// the chain: the treasury balance alone under-counts by whatever is still sitting on users'
	/// addresses, which is why no per-rail assertion existed before. Costs one balance read per
	/// derived address, so it belongs on a slow cadence, never on a request path. `None` means
	/// the adapter has no chain view.
	async fn deposit_address_liquidity(&self, network: Network) -> Result<Option<Usdt>, CustodyError> {
		let _ = network;
		Ok(None)
	}

	/// Read one **inbound** USDT transfer back off the chain by its reference.
	///
	/// This is the seam that makes an operator-recorded arrival a *verification* rather than
	/// an assertion: the amount and the recipient come from here, never from the caller, so
	/// the operator surface cannot mint a balance. Implementations MUST prove all of:
	/// the transfer exists, it moved **this rail's USDT contract** (not some worthless token
	/// the caller deployed), it was not aborted/reverted, and it is buried under this rail's
	/// confirmation depth. Anything unproven is `Ok(None)` — a refusal, never a credit.
	///
	/// `None` is also what an adapter with no chain view returns, which is the safe default:
	/// a rail we cannot read is a rail we will not credit by hand.
	async fn inbound_transfer(&self, network: Network, tx_ref: &TxRef) -> Result<Option<InboundTransfer>, CustodyError> {
		let (_, _) = (network, tx_ref);
		Ok(None)
	}

	/// How many more withdrawals this rail's treasury can pay **gas** for, at the chain's
	/// current price.
	///
	/// Counted in withdrawals rather than coins on purpose: the gas model is the adapter's
	/// business (an EVM rail multiplies a live `gas_price` by its gas limit, TON attaches a
	/// fixed `msg_value`), and the caller only wants to know how close the rail is to the
	/// cliff. `0` is not "nearly empty" but "the next withdrawal parks" — it is the same
	/// comparison `ensure_treasury_funded` makes at broadcast time, so the two can never
	/// disagree about when a rail stops paying. `None` means the adapter has no chain view.
	async fn treasury_gas_runway(&self, network: Network) -> Result<Option<GasRunway>, CustodyError> {
		let _ = network;
		Ok(None)
	}

	/// The rail treasury's operator **funding view** — the hot-wallet address plus its
	/// real on-chain USDT and native-coin gas balances, for the treasury screen. Same
	/// read-only rules as [`treasury_liquidity`](Custody::treasury_liquidity); `None`
	/// means the adapter has no chain view (the stub / an unwired rail).
	async fn treasury_funding(&self, network: Network) -> Result<Option<TreasuryFunding>, CustodyError> {
		let _ = network;
		Ok(None)
	}
}
/// A rail treasury's native-coin gas against what one withdrawal costs, both in the chain's
/// smallest unit — enough for an alert to say where to send how much.
#[derive(Clone, Debug)]
pub struct GasRunway {
	pub treasury: String,
	pub balance: u128,
	/// Never zero: an adapter reading a zero price reports no view instead.
	pub per_withdrawal: u128,
	pub decimals: u32,
}
impl GasRunway {
	pub fn withdrawals(&self) -> u64 {
		u64::try_from(self.balance / self.per_withdrawal).unwrap_or(u64::MAX) // saturates: past u64::MAX withdrawals is "plenty"
	}
}

/// One confirmed inbound USDT transfer, as the chain reports it — the evidence behind a
/// hand-recorded arrival. Addresses are in the rail's own canonical form (lowercase `0x…`
/// on EVM, raw `0:<hex>` on TON), the same form `user_deposit_addresses` stores, so the
/// caller compares them without decoding anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboundTransfer {
	/// Who sent it. The only thing separating an outside arrival on the treasury from the
	/// sweep consolidating money already on the ledger.
	pub from: String,
	/// Who received it — the address that decides which party gets credited.
	pub to: String,
	/// Canonical 18-dp USDT, already scaled from the rail's on-chain precision.
	pub amount: Usdt,
}

/// A rail treasury's funding view, read live from the chain: where the operator funds
/// (`address`) and what is actually there. The balance fields degrade to `None` when
/// their chain read fails — the address alone is still useful.
#[derive(Clone, Debug)]
pub struct TreasuryFunding {
	/// The treasury hot wallet — fund USDT (liquidity) + native coin (gas) here.
	pub address: String,
	/// On-chain USDT held by the treasury wallet (canonical 18-dp).
	pub onchain_usdt: Option<Usdt>,
	/// Native-coin gas balance, pre-rendered in whole units (BNB/TRX/TON differ in
	/// decimals, so the adapter formats it — see [`format_native_units`]).
	pub onchain_gas: Option<String>,
	/// The rail's sweep **gas-station** wallet — a SEPARATE account that pays the gas
	/// drops consolidating user deposits into the treasury. Fund it with the native
	/// coin ONLY (never USDT). `None` when the rail has no gas-station view — before
	/// this was surfaced, an operator once funded the wrong wallet mid-incident.
	pub gas_station_address: Option<String>,
	/// The gas station's native-coin balance, formatted like `onchain_gas`.
	pub gas_station_gas: Option<String>,
}
/// Render a native-coin amount (`units` at `decimals`) as a decimal string with the
/// fraction's trailing zeros trimmed — the gas-balance display format.
/// [`Usdt::to_decimal_string`] is fixed at the 18-dp canonical scale; gas is 18 (wei) /
/// 6 (SUN) / 9 (nanoton) dp per rail.
pub fn format_native_units(units: u128, decimals: u32) -> String {
	let scale = 10u128.pow(decimals);
	let int = units / scale;
	let frac = units % scale;
	if frac == 0 {
		return int.to_string();
	}
	let frac = format!("{frac:0width$}", width = decimals as usize);
	let frac = frac.trim_end_matches('0');
	format!("{int}.{frac}")
}
/// A request to broadcast the on-chain leg of a withdrawal. `withdrawal_id` is the
/// idempotency key the custodian MUST dedupe on.
#[derive(Clone, Debug)]
pub struct BroadcastRequest {
	pub withdrawal_id: Uuid,
	pub network: Network,
	pub address: WalletAddress,
	/// The net amount to send on-chain (gross minus the retained fee).
	pub amount: Usdt,
}

/// Failure modes the relay distinguishes — transient (retry; nothing was sent) vs a
/// policy/liquidity refusal (park for intervention; the reservation stays pending).
#[derive(Debug, Error)]
pub enum CustodyError {
	#[error("custody unavailable: {0}")]
	Unavailable(String),
	#[error("custody rejected: {0}")]
	Rejected(String),
}

#[cfg(test)]
mod tests {
	use super::format_native_units;

	#[test]
	fn formats_native_units_across_gas_scales() {
		assert_eq!(format_native_units(0, 18), "0");
		assert_eq!(format_native_units(1_500_000_000_000_000_000, 18), "1.5");
		assert_eq!(format_native_units(2_000_000, 6), "2");
		assert_eq!(format_native_units(123_456, 6), "0.123456");
		assert_eq!(format_native_units(1_000_000_001, 9), "1.000000001");
	}
}
