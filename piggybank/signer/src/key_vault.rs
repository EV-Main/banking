//! Minimal envelope-style key vault for an MVP custodial service.
//!
//! Stores chain private keys ENCRYPTED AT REST. Decryption happens only
//! transiently inside the signer; the plaintext is zeroized as soon as the
//! returned value is dropped.
//!
//! SCOPE / LIMIT (do not move this line):
//!   This protects a STOLEN DB or disk image. It does NOT protect against
//!   RCE on the running signer process (the key is in plaintext in memory at
//!   sign time). Use ONLY for a small hot-float you can afford to lose.
//!   Real balances belong behind MPC/HSM + an offline cold tier.
//!
//! Why XChaCha20-Poly1305: 24-byte nonce -> a random nonce per encryption is
//! safe, so there is zero nonce bookkeeping. That removes the classic footgun.
//!
//! Every random value here comes from `getrandom` (the OS CSPRNG) directly, so
//! the crypto crates do NOT need to agree on a `rand_core` version.

// `chacha20poly1305 0.10` re-exports `generic-array 0.14`, whose `from_slice` is
// deprecated in favour of the 1.x API the crate hasn't upgraded to yet. The calls
// are correct for this pinned version; silence the dependency's deprecation notice
// without altering the crypto core.
#![allow(deprecated)]

use chacha20poly1305::{
	Key, XChaCha20Poly1305, XNonce,
	aead::{Aead, KeyInit, Payload},
};
use zeroize::{Zeroize, Zeroizing};

/// The sentinel's fixed plaintext + AAD. Distinct from every wallet AAD (`<chain>:<uuid>`),
/// so a wallet blob can never masquerade as the sentinel or vice versa.
const SENTINEL_PLAINTEXT: &[u8] = b"evbanking-kek-sentinel:v1";
const SENTINEL_AAD: &[u8] = b"kek-sentinel";
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
	#[error("decrypt/encrypt failed (wrong KEK, tampered blob, or wrong chain/id)")]
	Crypto,
	#[error("KEK must be exactly 32 bytes (64 hex chars)")]
	KeyLen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Chain {
	BscBep20,   // EVM, secp256k1
	TronTrc20,  // secp256k1 (same curve as EVM)
	Ton,        // ed25519
	PolygonPos, // EVM, secp256k1 (Polygon PoS — same curve/address as BSC, its own sealing domain)
}

impl Chain {
	fn tag(self) -> &'static [u8] {
		match self {
			Chain::BscBep20 => b"bsc",
			Chain::TronTrc20 => b"tron",
			Chain::Ton => b"ton",
			Chain::PolygonPos => b"polygon",
		}
	}
}

/// The key-encrypting key (KEK).
///
/// It MUST be injected from OUTSIDE the app at runtime (secrets manager / KMS /
/// HSM-wrapped). It must NEVER live in the database, the repo, or a plaintext
/// config file next to the ciphertext. If the KEK and the encrypted blobs sit
/// in the same place, the encryption buys you almost nothing.
///
/// The KEK is wiped from memory on drop.
pub struct Vault {
	kek: Zeroizing<[u8; 32]>,
}
impl Vault {
	/// `kek_hex`: 64 hex chars (32 bytes). Read it from an env var that your
	/// host populates from a secrets store, e.g. `std::env::var("WALLET_KEK")`.
	pub fn from_hex(kek_hex: &str) -> Result<Self, VaultError> {
		let mut raw = hex::decode(kek_hex.trim()).map_err(|_| VaultError::KeyLen)?;
		if raw.len() != 32 {
			raw.zeroize();
			return Err(VaultError::KeyLen);
		}
		let mut k = [0u8; 32];
		k.copy_from_slice(&raw);
		raw.zeroize();
		Ok(Self { kek: Zeroizing::new(k) })
	}

	fn cipher(&self) -> XChaCha20Poly1305 {
		XChaCha20Poly1305::new(Key::from_slice(&*self.kek))
	}

	/// Domain-separated SHA-256 fingerprint of the KEK — identifies the KEK *epoch*
	/// without revealing anything about the key (256-bit random key ⇒ preimage-hard).
	/// Stamped on every sealed row and on the boot sentinel, so a blob sealed under a
	/// different KEK is PROVABLY dead by comparison, not just "fails to decrypt".
	pub fn fingerprint(&self) -> [u8; 32] {
		use sha2::{Digest, Sha256};
		let mut hasher = Sha256::new();
		hasher.update(b"evbanking-wallet-kek-fp:v1");
		hasher.update(*self.kek);
		hasher.finalize().into()
	}

	/// Seal the fixed sentinel plaintext under this KEK. Stored once at first boot;
	/// every later boot must [`verify_sentinel`](Self::verify_sentinel) it or the
	/// signer refuses to serve — the guard that catches a swapped/lost KEK at startup
	/// instead of at withdrawal time.
	pub fn seal_sentinel(&self) -> Result<Vec<u8>, VaultError> {
		let nonce_bytes = random_bytes::<24>();
		let nonce = XNonce::from_slice(&nonce_bytes);
		let ct = self
			.cipher()
			.encrypt(
				nonce,
				Payload {
					msg: SENTINEL_PLAINTEXT,
					aad: SENTINEL_AAD,
				},
			)
			.map_err(|_| VaultError::Crypto)?;
		let mut out = Vec::with_capacity(24 + ct.len());
		out.extend_from_slice(&nonce_bytes);
		out.extend_from_slice(&ct);
		Ok(out)
	}

	/// Open a stored sentinel blob and check it recovers the expected plaintext.
	/// Failure means the current KEK is not the KEK that sealed this database.
	pub fn verify_sentinel(&self, blob: &[u8]) -> Result<(), VaultError> {
		if blob.len() < 24 {
			return Err(VaultError::Crypto);
		}
		let (nonce_bytes, ct) = blob.split_at(24);
		let nonce = XNonce::from_slice(nonce_bytes);
		let pt = self.cipher().decrypt(nonce, Payload { msg: ct, aad: SENTINEL_AAD }).map_err(|_| VaultError::Crypto)?;
		if pt == SENTINEL_PLAINTEXT { Ok(()) } else { Err(VaultError::Crypto) }
	}

	/// Encrypt a private key. Stored blob layout: `nonce(24) || ciphertext+tag`.
	/// `chain` + `wallet_id` are bound as AAD, so a blob cannot be silently
	/// moved onto another wallet's row.
	pub fn seal(&self, chain: Chain, wallet_id: &str, secret: &[u8]) -> Result<Vec<u8>, VaultError> {
		let nonce_bytes = random_bytes::<24>();
		let nonce = XNonce::from_slice(&nonce_bytes);
		let aad = aad(chain, wallet_id);
		let ct = self.cipher().encrypt(nonce, Payload { msg: secret, aad: &aad }).map_err(|_| VaultError::Crypto)?;
		let mut out = Vec::with_capacity(24 + ct.len());
		out.extend_from_slice(&nonce_bytes);
		out.extend_from_slice(&ct);
		Ok(out)
	}

	/// Decrypt. The plaintext is zeroized when the returned value is dropped,
	/// so: use it for the signature, then let it go out of scope. Do not log it.
	pub fn open(&self, chain: Chain, wallet_id: &str, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
		if blob.len() < 24 {
			return Err(VaultError::Crypto);
		}
		let (nonce_bytes, ct) = blob.split_at(24);
		let nonce = XNonce::from_slice(nonce_bytes);
		let aad = aad(chain, wallet_id);
		let pt = self.cipher().decrypt(nonce, Payload { msg: ct, aad: &aad }).map_err(|_| VaultError::Crypto)?;
		Ok(Zeroizing::new(pt))
	}
}

/// secp256k1 secret key. BSC/BEP20 and Tron/TRC20 share this curve.
pub fn gen_secp256k1() -> Zeroizing<[u8; 32]> {
	use k256::ecdsa::SigningKey;
	// Rejection sampling: a random 32-byte value exceeds secp256k1's order with
	// probability ~2^-128, so this all but always succeeds on the first iteration;
	// retrying is the correct, unbiased fix and there is no meaningful bound to enforce.
	loop {
		let bytes = random_bytes::<32>();
		// Rejects the astronomically rare out-of-range scalar.
		if SigningKey::from_slice(&bytes).is_ok() {
			return Zeroizing::new(bytes);
		}
	}
}
/// ed25519 secret seed (TON). Any 32 bytes are a valid seed.
pub fn gen_ed25519() -> Zeroizing<[u8; 32]> {
	Zeroizing::new(random_bytes::<32>())
}
/// Compressed secp256k1 public key (33 bytes). BSC/BEP20 and Tron/TRC20 share this curve, so
/// the same stored key backs both an EVM and a Tron address — they differ only in the encoding
/// of the same `keccak256(pubkey)[12..]` 20-byte image ([`evm_address`] / [`tron_address`]).
pub fn secp256k1_pubkey(seed: &[u8; 32]) -> Vec<u8> {
	use k256::ecdsa::SigningKey;
	let sk = SigningKey::from_slice(seed).expect("valid secp256k1 scalar");
	sk.verifying_key().to_sec1_bytes().to_vec()
}
/// The EVM (BSC/BEP20) address for a stored compressed secp256k1 public key:
/// `keccak256(uncompressed_pubkey[1..])[12..]`, EIP-55 mixed-case checksummed.
/// `None` only if the bytes are not a valid curve point (never for keys we minted).
/// Address derivation is network-agnostic — the same on BSC mainnet and testnet.
pub fn evm_address(compressed_pubkey: &[u8]) -> Option<String> {
	Some(eip55_checksum(&keccak_address_bytes(compressed_pubkey)?))
}
/// The Tron (TRC20) address for the same stored secp256k1 key: the EVM 20-byte image with a
/// `0x41` mainnet prefix, Base58Check-encoded (double-SHA-256 checksum) into the `T…` string.
/// Tron shares EVM's curve + keccak step; only the envelope differs. `None` only for a non-curve
/// point. Network-agnostic — the same `T…` form on Tron mainnet and the Nile/Shasta testnets.
pub fn tron_address(compressed_pubkey: &[u8]) -> Option<String> {
	Some(base58check(&tron_raw_address(compressed_pubkey)?))
}
/// The 21-byte raw Tron address (`0x41 || keccak_image`) for a stored secp256k1 key — the form
/// that goes on the wire as `owner_address` in a signed transaction. The `T…` string is its
/// Base58Check rendering ([`tron_address`]).
pub fn tron_raw_address(compressed_pubkey: &[u8]) -> Option<[u8; 21]> {
	let image = keccak_address_bytes(compressed_pubkey)?;
	let mut raw = [0u8; 21];
	raw[0] = 0x41;
	raw[1..].copy_from_slice(&image);
	Some(raw)
}
/// Decode a `T…` Base58Check Tron address back to its 21-byte raw form (`0x41 || account`),
/// verifying the 4-byte double-SHA-256 checksum. `None` on a bad alphabet, length, or checksum —
/// so a malformed destination is rejected before it can reach a signed transaction.
pub fn tron_base58_to_raw(address: &str) -> Option<[u8; 21]> {
	use sha2::{Digest, Sha256};

	let decoded = base58_decode(address)?;
	if decoded.len() != 25 {
		return None;
	}
	let (payload, checksum) = decoded.split_at(21);
	if Sha256::digest(Sha256::digest(payload))[..4] != *checksum {
		return None;
	}
	payload.try_into().ok()
}
/// ed25519 public key (32 bytes). The TON wallet ADDRESS additionally needs a
/// wallet contract (v4/v5) + workchain; see [`ton_address`].
pub fn ed25519_pubkey(seed: &[u8; 32]) -> [u8; 32] {
	use ed25519_dalek::SigningKey;
	SigningKey::from_bytes(seed).verifying_key().to_bytes()
}
/// The TON v4R2 wallet-contract address for a stored 32-byte Ed25519 public key.
///
/// Unlike EVM (`addr = keccak(pubkey)[12..]`), a TON address is the contract's
/// **StateInit hash** — `sha256(code ++ data)` where `data` embeds the pubkey and the
/// v4R2 `wallet_id` (`0x29a9a317` = 698983191). So the same pubkey on a different
/// wallet version is a different address; we standardize on v4R2 (simplest `seqno`
/// nonce, universally indexed). We return the RAW canonical `0:<64hex>` form, which is
/// bounceability- and network-agnostic and accepted by
/// [`domain::money::WalletAddress::parse`]`(Ton, …)`; the user-facing `UQ…`/`0Q…`
/// (mainnet/testnet, non-bounceable) form is a display-time concern derived from this.
/// `None` only if the bytes are not a 32-byte key (never for keys we minted).
pub fn ton_address(pubkey: &[u8]) -> Option<String> {
	use tonlib_core::wallet::{mnemonic::KeyPair, ton_wallet::TonWallet, wallet_version::WalletVersion};

	if pubkey.len() != 32 {
		return None;
	}
	// Only the public key participates in address derivation (the StateInit data cell);
	// the secret half is irrelevant here (signing lives in `ton_tx`), so leave it empty.
	let key_pair = KeyPair {
		public_key: pubkey.to_vec(),
		secret_key: Vec::new(),
	};
	let wallet = TonWallet::new(WalletVersion::V4R2, key_pair).ok()?;
	Some(wallet.address.to_hex())
}
/// The 20-byte account image shared by EVM and Tron: `keccak256(uncompressed_pubkey[1..])[12..]`.
/// `None` only if the bytes are not a valid curve point (never for keys we minted).
fn keccak_address_bytes(compressed_pubkey: &[u8]) -> Option<[u8; 20]> {
	use k256::{PublicKey, elliptic_curve::sec1::ToEncodedPoint};
	use sha3::{Digest, Keccak256};

	let pubkey = PublicKey::from_sec1_bytes(compressed_pubkey).ok()?;
	let point = pubkey.to_encoded_point(false); // 0x04 || X(32) || Y(32)
	let hash = Keccak256::digest(&point.as_bytes()[1..]);
	let mut out = [0u8; 20];
	out.copy_from_slice(&hash[12..32]);
	Some(out)
}
/// Base58Check: append the first 4 bytes of `sha256(sha256(payload))` and Base58-encode. This is
/// the Tron (and Bitcoin) address envelope; hand-rolled here rather than pulling a chain SDK,
/// matching the repo's "hand-roll the encoding, not a heavyweight dep" stance (cf. EVM RLP).
fn base58check(payload: &[u8]) -> String {
	use sha2::{Digest, Sha256};

	let checksum = Sha256::digest(Sha256::digest(payload));
	let mut full = Vec::with_capacity(payload.len() + 4);
	full.extend_from_slice(payload);
	full.extend_from_slice(&checksum[..4]);
	base58_encode(&full)
}
/// Base58 (Bitcoin/Tron alphabet) of an arbitrary big-endian byte string. Leading zero bytes map
/// to leading `1`s; the rest is base-256 → base-58 by repeated division over the digit buffer.
fn base58_encode(input: &[u8]) -> String {
	const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
	let zeros = input.iter().take_while(|&&b| b == 0).count();
	let mut digits: Vec<u8> = Vec::new();
	for &byte in input {
		let mut carry = byte as u32;
		for digit in digits.iter_mut() {
			carry += (*digit as u32) << 8;
			*digit = (carry % 58) as u8;
			carry /= 58;
		}
		while carry > 0 {
			digits.push((carry % 58) as u8);
			carry /= 58;
		}
	}
	let mut out = String::with_capacity(zeros + digits.len());
	out.extend(std::iter::repeat_n('1', zeros));
	out.extend(digits.iter().rev().map(|&d| ALPHABET[d as usize] as char));
	out
}
/// Inverse of [`base58_encode`]: decode a Base58 string to its big-endian bytes (leading `1`s
/// become leading zero bytes). `None` on any character outside the alphabet.
fn base58_decode(input: &str) -> Option<Vec<u8>> {
	const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
	let zeros = input.bytes().take_while(|&b| b == b'1').count();
	let mut bytes: Vec<u8> = Vec::new();
	for ch in input.bytes() {
		let mut carry = ALPHABET.iter().position(|&a| a == ch)? as u32;
		for byte in bytes.iter_mut() {
			carry += (*byte as u32) * 58;
			*byte = (carry & 0xff) as u8;
			carry >>= 8;
		}
		while carry > 0 {
			bytes.push((carry & 0xff) as u8);
			carry >>= 8;
		}
	}
	let mut out = vec![0u8; zeros];
	out.extend(bytes.iter().rev());
	Some(out)
}

/// EIP-55 mixed-case checksum of 20 address bytes: a hex char `a-f` is uppercased
/// when the corresponding nibble of `keccak256(lowercase_hex)` is ≥ 8.
fn eip55_checksum(addr: &[u8]) -> String {
	use sha3::{Digest, Keccak256};

	let lower: String = addr.iter().map(|b| format!("{b:02x}")).collect();
	let hash = Keccak256::digest(lower.as_bytes());
	let mut out = String::with_capacity(2 + lower.len());
	out.push_str("0x");
	for (i, c) in lower.bytes().enumerate() {
		let nibble = if i % 2 == 0 { hash[i.div_euclid(2)] >> 4 } else { hash[i.div_euclid(2)] & 0x0f };
		if c.is_ascii_alphabetic() && nibble >= 8 {
			out.push(c.to_ascii_uppercase() as char);
		} else {
			out.push(c as char);
		}
	}
	out
}

/// 32 fresh bytes from the OS CSPRNG.
fn random_bytes<const N: usize>() -> [u8; N] {
	let mut b = [0u8; N];
	getrandom::fill(&mut b).expect("OS RNG unavailable");
	b
}

fn aad(chain: Chain, wallet_id: &str) -> Vec<u8> {
	let mut v = Vec::with_capacity(chain.tag().len() + 1 + wallet_id.len());
	v.extend_from_slice(chain.tag());
	v.push(b':');
	v.extend_from_slice(wallet_id.as_bytes());
	v
}

// --- per-chain key generation -------------------------------------------
// Generate the secret in the signer, immediately `seal` it, store the blob,
// and store the PUBLIC key/address for watch-only indexing.

// Suggested Postgres row:
//   id           uuid primary key
//   chain        text  not null         -- 'bsc' | 'tron' | 'ton'
//   public_key   bytea not null         -- for watch-only indexing
//   address      text  not null
//   enc_key      bytea not null         -- output of Vault::seal (nonce||ct)
//   created_at   timestamptz default now()

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn round_trip_and_tamper_resistance() {
		// 32-byte KEK as hex. In prod this comes from your secrets store.
		let kek_hex = hex::encode([7u8; 32]);
		let vault = Vault::from_hex(&kek_hex).unwrap();

		let sk = gen_secp256k1();
		let blob = vault.seal(Chain::BscBep20, "wallet-42", &*sk).unwrap();

		// correct chain + id -> recovers the exact secret
		let opened = vault.open(Chain::BscBep20, "wallet-42", &blob).unwrap();
		assert_eq!(&*opened, &sk[..]);

		// wrong wallet_id (AAD mismatch) -> fails, no plaintext leaks
		assert!(vault.open(Chain::BscBep20, "wallet-99", &blob).is_err());

		// wrong chain -> fails
		assert!(vault.open(Chain::TronTrc20, "wallet-42", &blob).is_err());

		// flipped byte -> auth tag rejects it
		let mut bad = blob.clone();
		*bad.last_mut().unwrap() ^= 0x01;
		assert!(vault.open(Chain::BscBep20, "wallet-42", &bad).is_err());

		// ed25519 path works too
		let ton = gen_ed25519();
		let ton_blob = vault.seal(Chain::Ton, "wallet-42", &*ton).unwrap();
		assert_eq!(&*vault.open(Chain::Ton, "wallet-42", &ton_blob).unwrap(), &ton[..]);
	}

	#[test]
	fn sentinel_and_fingerprint_pin_the_kek_epoch() {
		let vault = Vault::from_hex(&hex::encode([7u8; 32])).unwrap();
		let other = Vault::from_hex(&hex::encode([8u8; 32])).unwrap();

		// Fingerprint identifies the epoch: stable per KEK, different across KEKs.
		assert_eq!(vault.fingerprint(), vault.fingerprint());
		assert_ne!(vault.fingerprint(), other.fingerprint());

		// The sentinel verifies only under the KEK that sealed it — the boot guard.
		let blob = vault.seal_sentinel().unwrap();
		assert!(vault.verify_sentinel(&blob).is_ok());
		assert!(other.verify_sentinel(&blob).is_err());

		// A wallet blob cannot masquerade as the sentinel (distinct AAD).
		let sk = gen_secp256k1();
		let wallet_blob = vault.seal(Chain::BscBep20, "wallet-42", &*sk).unwrap();
		assert!(vault.verify_sentinel(&wallet_blob).is_err());
	}

	#[test]
	fn evm_address_matches_canonical_vector() {
		// secp256k1 private key = 1 → public key = G → the canonical EVM address.
		// This pins both the keccak/last-20-bytes derivation and the EIP-55 casing.
		let mut seed = [0u8; 32];
		seed[31] = 1;
		let pubkey = secp256k1_pubkey(&seed);
		assert_eq!(evm_address(&pubkey).unwrap(), "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf");
	}

	#[test]
	fn evm_address_is_deterministic_and_well_formed() {
		let pubkey = secp256k1_pubkey(&gen_secp256k1());
		let a = evm_address(&pubkey).unwrap();
		assert_eq!(a, evm_address(&pubkey).unwrap());
		assert!(a.starts_with("0x") && a.len() == 42);
	}

	#[test]
	fn tron_address_matches_canonical_vector() {
		// secp256k1 private key = 1 → the same key the EVM vector uses. EVM address
		// 0x7E5F…395Bdf re-encodes to this Tron T-address (0x41 prefix + Base58Check),
		// pinning the shared keccak image + the Base58Check envelope.
		let mut seed = [0u8; 32];
		seed[31] = 1;
		let pubkey = secp256k1_pubkey(&seed);
		assert_eq!(tron_address(&pubkey).unwrap(), "TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC");
	}

	#[test]
	fn base58check_matches_the_usdt_contract_address() {
		// Independently pins Base58Check against external truth: the mainnet USDT (TRC20)
		// contract's 21-byte hex 0x41a614…d13c encodes to its canonical T-address.
		let payload = hex::decode("41a614f803b6fd780986a42c78ec9c7f77e6ded13c").unwrap();
		assert_eq!(base58check(&payload), "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t");
	}

	#[test]
	fn tron_address_is_deterministic_and_well_formed() {
		let pubkey = secp256k1_pubkey(&gen_secp256k1());
		let a = tron_address(&pubkey).unwrap();
		assert_eq!(a, tron_address(&pubkey).unwrap());
		assert!(a.starts_with('T') && a.len() == 34);
	}

	#[test]
	fn ton_address_matches_a_known_v4r2_vector() {
		// A real v4R2 wallet's pubkey → its on-chain (deploy-time, seqno 0) StateInit
		// address. Pins the StateInit-hash derivation + the v4R2 wallet_id/code. The
		// non-bounceable mainnet (UQ) rendering of the stored raw `0:hex` is the wallet's
		// user-friendly address as seen on tonviewer.
		let pubkey = hex::decode("cbf377c9b73604c70bf73488ddceba14f763baef2ac70f68d1d6032a120149f4").unwrap();
		let raw = ton_address(&pubkey).unwrap();
		let parsed = tonlib_core::TonAddress::from_hex_str(&raw).unwrap();
		assert_eq!(parsed.to_base64_url_flags(true, false), "UQCS65EGyiApUTLOYXDs4jOLoQNCE0o8oNnkmfIcm0iX5FRT");
	}

	#[test]
	fn ton_address_is_deterministic_and_domain_parseable() {
		use domain::money::{Network, WalletAddress};

		let pubkey = ed25519_pubkey(&gen_ed25519());
		let a = ton_address(&pubkey).unwrap();
		// Deterministic for a fixed key.
		assert_eq!(a, ton_address(&pubkey).unwrap());
		// Raw canonical `0:<64hex>`, and the domain's parser accepts it (so the hub can
		// serve it as a fundable deposit address). A real testnet deposit is the final
		// external proof of correctness; here we pin shape + determinism + parse-acceptance.
		assert!(a.starts_with("0:") && a.len() == 66);
		assert!(WalletAddress::parse(Network::Ton, &a).is_ok());
		// A short/garbage key is refused, never panics.
		assert!(ton_address(&[0u8; 4]).is_none());
	}
}
