//! Shared money kernel — crypto value objects, all pure and wasm-safe.
//!
//! The fund accounts internally in a **single canonical unit**: 18-decimal USDT
//! ("base units"), matching BEP20's on-chain scale. TRC20, TON, and Polygon carry
//! USDT at 6 decimals on-chain, so amounts are scaled **up** by `10^12` when they enter the
//! ledger and **down** (rejecting non-representable dust) when they leave. Keeping
//! one canonical unit means the value ledger never has to reason about a token's
//! on-chain scale; only the custody edge ([`Usdt::from_onchain`] /
//! [`Usdt::to_onchain`]) does.
//!
//! Only USDT is accepted today; [`Token`] and [`CryptoAsset`] exist so a second
//! token is an enum variant, not a refactor.

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// Canonical internal scale: 18 decimals. Every [`Usdt`] is an integer count of
/// `10^-18` USDT, so amounts are exact (no floating point ever touches money).
pub const CANONICAL_DECIMALS: u32 = 18;
const BASE58_ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
/// The chains the fund custodies USDT on. The on-chain decimal scale differs per
/// chain, which is the whole reason [`Usdt`] normalizes to a canonical unit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Network {
	/// BNB Smart Chain — USDT has **18** decimals here (the canonical scale).
	Bep20,
	/// TRON — USDT has **6** decimals.
	Trc20,
	/// The Open Network — USDT (jetton) has **6** decimals.
	Ton,
	/// Polygon PoS — an EVM chain (secp256k1/keccak addresses, account nonces, the same
	/// JSON-RPC and legacy tx signing as BEP20), but USDT has **6** decimals here, not 18.
	Polygon,
}
impl Network {
	/// All supported networks, for boot-time account seeding and balance sweeps.
	pub const ALL: [Network; 4] = [Network::Bep20, Network::Trc20, Network::Ton, Network::Polygon];

	/// The token's decimal precision on this chain. The custody edge scales between
	/// this and [`CANONICAL_DECIMALS`].
	pub const fn onchain_decimals(self) -> u32 {
		match self {
			Self::Bep20 => 18,
			Self::Trc20 | Self::Ton | Self::Polygon => 6,
		}
	}

	/// `10^(CANONICAL_DECIMALS - onchain_decimals)` — the multiplier taking an
	/// on-chain raw amount up to canonical base units (and the divisor coming back).
	const fn scale_to_canonical(self) -> u128 {
		10u128.pow(CANONICAL_DECIMALS - self.onchain_decimals())
	}

	/// Confirmations a watcher waits for before crediting/settling on this network
	/// (reorg-safety): BEP20 ~15, TRC20 ~19 (SR rounds), TON a few, Polygon ~128 (its
	/// probabilistic-finality reorgs have historically run far deeper than BSC's — the
	/// widely-used exchange standard; overridable via `POLYGON_CONFIRMATIONS`). **Placeholder
	/// values** — a per-network ubiquitous-language fact, sibling to
	/// [`WithdrawalPolicy`](crate::withdrawals::WithdrawalPolicy), kept here so every
	/// `domain`-dependent consumer reuses one source rather than duplicating constants.
	pub const fn min_confirmations(self) -> u32 {
		match self {
			Self::Bep20 => 15,
			Self::Trc20 => 19,
			Self::Ton => 16,
			Self::Polygon => 128,
		}
	}

	/// The coin this chain charges gas in — what a rail treasury must hold to send anything.
	pub const fn native_coin(self) -> &'static str {
		match self {
			Self::Bep20 => "BNB",
			Self::Trc20 => "TRX",
			Self::Ton => "TON",
			Self::Polygon => "POL",
		}
	}

	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Bep20 => "bep20",
			Self::Trc20 => "trc20",
			Self::Ton => "ton",
			Self::Polygon => "polygon",
		}
	}

	/// Parse the stored/wire form back into the enum (persistence + gRPC boundary).
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"bep20" => Ok(Self::Bep20),
			"trc20" => Ok(Self::Trc20),
			"ton" => Ok(Self::Ton),
			"polygon" => Ok(Self::Polygon),
			other => Err(DomainError::Validation(format!("unknown network: {other}"))),
		}
	}
}

impl core::fmt::Display for Network {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(self.as_str())
	}
}

/// Tokens the fund handles. USDT only for now — a stablecoin we treat 1:1 across
/// chains for *value* accounting (chain liquidity is tracked separately in custody).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Token {
	Usdt,
}

impl Token {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Usdt => "usdt",
		}
	}
}

/// A token on a specific chain — the unit of *custody* (a wallet holds one of
/// these). Distinct from [`Usdt`], which is canonical *value*.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CryptoAsset {
	pub token: Token,
	pub network: Network,
}

impl CryptoAsset {
	pub const fn usdt(network: Network) -> Self {
		Self { token: Token::Usdt, network }
	}
}

/// A USDT amount in canonical 18-decimal base units. Newtype over `u128` so it
/// can't be confused with a raw on-chain amount or an account id; arithmetic is
/// **checked** (a money type must never silently wrap). Serializes as a **string**
/// of base units — `serde_json` has no `u128` support, and a string is exact across
/// JSON consumers (no float/`2^53` loss) for the event log.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Usdt(u128);
impl Usdt {
	pub const ZERO: Usdt = Usdt(0);

	/// Wrap a raw count of canonical (18-dp) base units.
	pub const fn from_base_units(units: u128) -> Self {
		Self(units)
	}

	/// The raw canonical base-unit count (what TigerBeetle stores as a transfer
	/// `amount`).
	pub const fn base_units(self) -> u128 {
		self.0
	}

	pub const fn is_zero(self) -> bool {
		self.0 == 0
	}

	/// Checked add — `None` on overflow rather than wrapping.
	pub fn checked_add(self, other: Usdt) -> Option<Usdt> {
		self.0.checked_add(other.0).map(Usdt)
	}

	/// Checked sub — `None` if it would go negative.
	pub fn checked_sub(self, other: Usdt) -> Option<Usdt> {
		self.0.checked_sub(other.0).map(Usdt)
	}

	/// Lift an on-chain raw amount (in `network`'s native decimals) into canonical
	/// base units. Scaling **up** is always exact; overflow is the only failure.
	pub fn from_onchain(network: Network, raw: u128) -> Result<Usdt, DomainError> {
		raw.checked_mul(network.scale_to_canonical())
			.map(Usdt)
			.ok_or_else(|| DomainError::Validation("amount overflows canonical units".into()))
	}

	/// Lower this amount to an on-chain raw amount for `network`. Rejects any value
	/// that isn't representable at the chain's precision (sub-precision **dust**)
	/// rather than silently truncating — a truncating withdrawal is a slow leak.
	pub fn to_onchain(self, network: Network) -> Result<u128, DomainError> {
		let factor = network.scale_to_canonical();
		if !self.0.is_multiple_of(factor) {
			return Err(DomainError::Validation(format!("amount not representable on {network} (sub-precision dust)")));
		}
		Ok(self.0 / factor)
	}

	/// Parse a human/wire decimal string (e.g. `"100"`, `"100.5"`, `"0.000001"`)
	/// into canonical base units. Rejects more than 18 fractional digits and any
	/// non-digit. This is the inbound wire format (amounts cross gRPC as strings to
	/// stay exact in JS BFFs).
	pub fn parse_decimal(raw: &str) -> Result<Usdt, DomainError> {
		let trimmed = raw.trim();
		if trimmed.is_empty() {
			return Err(DomainError::Validation("amount must not be empty".into()));
		}
		let (int_part, frac_part) = match trimmed.split_once('.') {
			Some((i, f)) => (i, f),
			None => (trimmed, ""),
		};
		if int_part.is_empty() && frac_part.is_empty() {
			return Err(DomainError::Validation("amount must have digits".into()));
		}
		if frac_part.len() as u32 > CANONICAL_DECIMALS {
			return Err(DomainError::Validation("amount has more than 18 decimal places".into()));
		}
		if !int_part.bytes().all(|b| b.is_ascii_digit()) || !frac_part.bytes().all(|b| b.is_ascii_digit()) {
			return Err(DomainError::Validation("amount must be a decimal number".into()));
		}
		let int_units = int_part.parse::<u128>().map_err(|_| DomainError::Validation("amount integer part too large".into()))?;
		let scaled_int = int_units
			.checked_mul(10u128.pow(CANONICAL_DECIMALS))
			.ok_or_else(|| DomainError::Validation("amount too large".into()))?;
		// Right-pad the fraction to 18 digits, then it is already a base-unit count.
		let mut frac_units = 0u128;
		if !frac_part.is_empty() {
			let padded = format!("{frac_part:0<18}");
			frac_units = padded.parse::<u128>().map_err(|_| DomainError::Validation("invalid amount fraction".into()))?;
		}
		scaled_int.checked_add(frac_units).map(Usdt).ok_or_else(|| DomainError::Validation("amount too large".into()))
	}

	/// Render as a fixed-point decimal string with trailing zeros trimmed (the
	/// outbound wire format). `"0"` for zero.
	pub fn to_decimal_string(self) -> String {
		let scale = 10u128.pow(CANONICAL_DECIMALS);
		let int = self.0 / scale;
		let frac = self.0 % scale;
		if frac == 0 {
			return int.to_string();
		}
		let frac = format!("{frac:018}");
		let frac = frac.trim_end_matches('0');
		format!("{int}.{frac}")
	}
}

impl Serialize for Usdt {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.serialize_str(&self.0.to_string())
	}
}

impl<'de> Deserialize<'de> for Usdt {
	fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		let raw = String::deserialize(deserializer)?;
		raw.parse::<u128>().map(Usdt).map_err(serde::de::Error::custom)
	}
}

impl core::fmt::Display for Usdt {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(&self.to_decimal_string())
	}
}

/// A validated on-chain wallet address, tagged by its [`Network`]. Parse-don't-
/// validate: structural per-chain checks on construction (format + alphabet), so a
/// malformed address can't reach the custody layer. Full checksum verification is
/// the custody/signing service's job (out of this scope) — this guards shape.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WalletAddress {
	network: Network,
	value: String,
}
impl WalletAddress {
	pub fn parse(network: Network, raw: &str) -> Result<Self, DomainError> {
		let value = raw.trim();
		let ok = match network {
			// EVM: 0x + 40 hex. BEP20 (BSC) and Polygon are both EVM chains sharing this form.
			Network::Bep20 | Network::Polygon => value.len() == 42 && value.starts_with("0x") && value[2..].bytes().all(|b| b.is_ascii_hexdigit()),
			// TRON base58check: 'T' + 33 base58 chars (34 total).
			Network::Trc20 => value.len() == 34 && value.starts_with('T') && value.bytes().all(|b| BASE58_ALPHABET.contains(&b)),
			// TON: 48-char user-friendly base64url, or raw `<wc>:<64 hex>`.
			Network::Ton => is_ton_friendly(value) || is_ton_raw(value),
		};
		if !ok {
			return Err(DomainError::Validation(format!("invalid {network} wallet address")));
		}
		Ok(Self { network, value: value.to_owned() })
	}

	pub fn network(&self) -> Network {
		self.network
	}

	pub fn as_str(&self) -> &str {
		&self.value
	}
}

/// An on-chain transaction reference (the deposit/withdrawal idempotency key). Opaque
/// and trimmed; a deposit is recorded at most once per [`TxRef`].
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TxRef(String);
impl TxRef {
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		let trimmed = raw.trim();
		if trimmed.is_empty() {
			return Err(DomainError::Validation("tx ref must not be empty".into()));
		}
		if trimmed.len() > 128 {
			return Err(DomainError::Validation("tx ref too long".into()));
		}
		// No chain's hash, nor the `hash:recipient` form, contains a link, so one is a typo
		// or a paste of something else.
		if contains_url(trimmed) {
			return Err(DomainError::Validation("tx ref must not contain a link".into()));
		}
		Ok(Self(trimmed.to_owned()))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// Whether `value` carries a URL by the needles concierge's governance relay refuses
/// (`no_url`): a scheme separator or a `www.` host, case-insensitive. Text that ends up in
/// a payment label has to clear exactly these, or its mail is never sent.
pub(crate) fn contains_url(value: &str) -> bool {
	let lower = value.to_ascii_lowercase();
	lower.contains("://") || lower.contains("www.")
}

fn is_ton_friendly(value: &str) -> bool {
	value.len() == 48 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_ton_raw(value: &str) -> bool {
	match value.split_once(':') {
		Some((wc, hash)) => {
			let wc_ok = wc == "0" || wc == "-1";
			wc_ok && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
		}
		None => false,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tx_ref_refuses_a_link_but_keeps_every_chain_form() {
		for raw in ["https://evil.example", "0xabc HTTP://x", "www.evil.example"] {
			assert!(TxRef::parse(raw).is_err(), "{raw} must be refused");
		}
		for raw in [
			"0x5c504ed432cb51138bcf09aa5e8a410dd4a1e204ef84bfed1be16dfba1b22060:0",
			"94f5a2c8e1b0d3f6a7c9e2b4d6f8a0c1e3b5d7f9a2c4e6b8d0f1a3c5e7b9d2f4:TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf",
			"Qm2a3B+c/d==:EQDtFpEwcFAEcRe5mLVh2N6C0x-_hJEM7W61_JLnSF74p4q2",
			"itest-6f1d0c7e-8b4a-4c2e-9f3d-2a1b0c9d8e7f",
		] {
			assert!(TxRef::parse(raw).is_ok(), "{raw} must be accepted");
		}
	}

	#[test]
	fn onchain_scaling_round_trips_and_rejects_dust() {
		// 1 USDT on TRC20 (6dp) == 1e6 raw == 1e18 canonical.
		let one = Usdt::from_onchain(Network::Trc20, 1_000_000).unwrap();
		assert_eq!(one.base_units(), 1_000_000_000_000_000_000);
		assert_eq!(one.to_onchain(Network::Trc20).unwrap(), 1_000_000);

		// BEP20 is 1:1 with canonical.
		let raw_bep = 5_000_000_000_000_000_000; // 5 USDT @18dp
		assert_eq!(Usdt::from_onchain(Network::Bep20, raw_bep).unwrap().base_units(), raw_bep);

		// Polygon USDT is 6dp (an EVM chain, unlike BEP20's 18dp) — scales like TRC20/TON.
		let one_pol = Usdt::from_onchain(Network::Polygon, 1_000_000).unwrap();
		assert_eq!(one_pol.base_units(), 1_000_000_000_000_000_000);
		assert_eq!(one_pol.to_onchain(Network::Polygon).unwrap(), 1_000_000);

		// 1 canonical base unit cannot be expressed on a 6dp chain → dust rejected.
		let dust = Usdt::from_base_units(1);
		assert!(dust.to_onchain(Network::Trc20).is_err());
		assert!(dust.to_onchain(Network::Polygon).is_err()); // Polygon is 6dp too
		assert!(dust.to_onchain(Network::Bep20).is_ok()); // representable at 18dp
	}

	#[test]
	fn checked_arithmetic_does_not_wrap() {
		assert_eq!(Usdt::from_base_units(3).checked_sub(Usdt::from_base_units(5)), None);
		assert_eq!(Usdt::from_base_units(u128::MAX).checked_add(Usdt::from_base_units(1)), None);
		assert_eq!(Usdt::from_base_units(5).checked_sub(Usdt::from_base_units(2)), Some(Usdt::from_base_units(3)));
	}

	#[test]
	fn decimal_parsing_and_rendering() {
		assert_eq!(Usdt::parse_decimal("100").unwrap().base_units(), 100_000_000_000_000_000_000);
		assert_eq!(Usdt::parse_decimal("100.5").unwrap().base_units(), 100_500_000_000_000_000_000);
		assert_eq!(Usdt::parse_decimal("0.000000000000000001").unwrap().base_units(), 1);
		assert_eq!(Usdt::from_base_units(100_500_000_000_000_000_000).to_decimal_string(), "100.5");
		assert_eq!(Usdt::from_base_units(0).to_decimal_string(), "0");
		assert_eq!(Usdt::from_base_units(1).to_decimal_string(), "0.000000000000000001");
		// More than 18 fractional digits is rejected, not truncated.
		assert!(Usdt::parse_decimal("0.0000000000000000001").is_err());
		assert!(Usdt::parse_decimal("abc").is_err());
		assert!(Usdt::parse_decimal("").is_err());
	}

	#[test]
	fn wallet_address_validates_per_network() {
		assert!(WalletAddress::parse(Network::Bep20, "0x52908400098527886E0F7030069857D2E4169EE7").is_ok());
		assert!(WalletAddress::parse(Network::Bep20, "0xnothex").is_err());
		assert!(WalletAddress::parse(Network::Trc20, "TJRabPrwbZy45sbavfcjinPJC18kjpRTv8").is_ok());
		assert!(WalletAddress::parse(Network::Trc20, "0xabc").is_err());
		assert!(WalletAddress::parse(Network::Ton, "0:8d8c9d8a8e8b8c8d8e8f808182838485868788898a8b8c8d8e8f80818283848f").is_ok());
		assert!(WalletAddress::parse(Network::Ton, "EQCD39VS5jcptHL8vMjEXrzGaRcCVYto7HUn4bpAOg8xqB2N").is_ok());
		assert!(WalletAddress::parse(Network::Ton, "nope").is_err());
		// Polygon shares the EVM 0x-hex form with BEP20.
		assert!(WalletAddress::parse(Network::Polygon, "0x52908400098527886E0F7030069857D2E4169EE7").is_ok());
		assert!(WalletAddress::parse(Network::Polygon, "0xnothex").is_err());
		assert!(WalletAddress::parse(Network::Polygon, "TJRabPrwbZy45sbavfcjinPJC18kjpRTv8").is_err());
		// A BEP20-shaped address is rejected for the wrong network.
		assert!(WalletAddress::parse(Network::Trc20, "0x52908400098527886E0F7030069857D2E4169EE7").is_err());
	}

	#[test]
	fn usdt_serializes_as_base_unit_string() {
		let json = serde_json::to_string(&Usdt::from_base_units(42)).unwrap();
		assert_eq!(json, r#""42""#);
		let back: Usdt = serde_json::from_str(r#""42""#).unwrap();
		assert_eq!(back, Usdt::from_base_units(42));
	}

	#[test]
	fn network_parse_round_trips() {
		for net in Network::ALL {
			assert_eq!(Network::parse(net.as_str()).unwrap(), net);
		}
		assert!(Network::parse("eth").is_err());
	}

	#[test]
	fn min_confirmations_are_defined_per_network() {
		assert_eq!(Network::Bep20.min_confirmations(), 15);
		assert_eq!(Network::Trc20.min_confirmations(), 19);
		assert_eq!(Network::Ton.min_confirmations(), 16);
		assert_eq!(Network::Polygon.min_confirmations(), 128);
		// Every supported rail has a positive reorg-safety threshold.
		assert!(Network::ALL.iter().all(|net| net.min_confirmations() > 0));
	}
}
