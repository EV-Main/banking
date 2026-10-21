//! The BFF's one wallet key and the two things it signs: the CSRF token a host page sends
//! back, and the seal on a stored intent. Each use gets its own subkey, derived from the
//! key with a fixed label, so a value minted for one use can never pass as the other.
//!
//! HMAC-SHA-256 is written out over `sha2` (RFC 2104) rather than pulled in as another
//! crate; the RFC 4231 vectors in the tests pin it.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The length of one CSRF window. A token is good for the window it was minted in and the
/// next one, so a page that fetched one just before a boundary still has ten minutes.
pub const CSRF_WINDOW_SECS: u64 = 600;

const CSRF_LABEL: &[u8] = b"ev.cabinet.wallet.csrf.v1";
const SEAL_LABEL: &[u8] = b"ev.cabinet.wallet.intent-seal.v1";

pub struct IntentKeys {
	csrf: [u8; 32],
	seal: [u8; 32],
}

impl IntentKeys {
	pub fn derive(key: &[u8]) -> Self {
		Self {
			csrf: hmac_sha256(key, &[CSRF_LABEL]),
			seal: hmac_sha256(key, &[SEAL_LABEL]),
		}
	}

	/// A token for `sub` on the page `origin`, and the unix second after which it is refused.
	pub fn csrf_token(&self, sub: &str, origin: &str, now: u64) -> (String, u64) {
		let window = now / CSRF_WINDOW_SECS;
		let token = format!("{window}.{}", hex(&self.csrf_mac(sub, origin, window)));
		(token, (window + 2) * CSRF_WINDOW_SECS)
	}

	/// Whether `token` was minted by this key for exactly this `sub` and `origin`, in the
	/// current window or the one before it.
	pub fn csrf_valid(&self, token: &str, sub: &str, origin: &str, now: u64) -> bool {
		let Some((window, mac)) = token.split_once('.') else {
			return false;
		};
		let Ok(window) = window.parse::<u64>() else {
			return false;
		};
		let current = now / CSRF_WINDOW_SECS;
		if window != current && Some(window) != current.checked_sub(1) {
			return false;
		}
		let expected = hex(&self.csrf_mac(sub, origin, window));
		mac.len() == expected.len() && bool::from(mac.as_bytes().ct_eq(expected.as_bytes()))
	}

	fn csrf_mac(&self, sub: &str, origin: &str, window: u64) -> [u8; 32] {
		hmac_sha256(&self.csrf, &[sub.as_bytes(), origin.as_bytes(), &window.to_be_bytes()])
	}

	/// The seal over an intent's hash, in the hex form the store keeps.
	pub fn seal(&self, intent_hash: &[u8; 32]) -> String {
		hex(&hmac_sha256(&self.seal, &[intent_hash]))
	}

	pub fn seal_valid(&self, intent_hash: &[u8; 32], seal: &str) -> bool {
		let expected = self.seal(intent_hash);
		seal.len() == expected.len() && bool::from(seal.as_bytes().ct_eq(expected.as_bytes()))
	}
}

/// HMAC-SHA-256 over `parts`, each prefixed by its length, so that no two different lists
/// of parts feed the same bytes to the hash.
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
	hmac_over(key, |hasher| feed(hasher, parts))
}

/// SHA-256 over `parts`, framed the same way as [`hmac_sha256`].
pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
	let mut hasher = Sha256::new();
	feed(&mut hasher, parts);
	hasher.finalize().into()
}

fn hmac_over(key: &[u8], message: impl FnOnce(&mut Sha256)) -> [u8; 32] {
	const BLOCK: usize = 64;
	let mut block = [0u8; BLOCK];
	if key.len() > BLOCK {
		block[..32].copy_from_slice(&Sha256::digest(key));
	} else {
		block[..key.len()].copy_from_slice(key);
	}
	let mut inner = Sha256::new();
	inner.update(block.map(|b| b ^ 0x36));
	message(&mut inner);
	let mut outer = Sha256::new();
	outer.update(block.map(|b| b ^ 0x5c));
	outer.update(inner.finalize());
	outer.finalize().into()
}

fn feed(hasher: &mut Sha256, parts: &[&[u8]]) {
	for part in parts {
		hasher.update((part.len() as u64).to_be_bytes());
		hasher.update(part);
	}
}

/// Lowercase hex, the one spelling concierge accepts for an `intent_hash`.
pub fn hex(bytes: &[u8]) -> String {
	const DIGITS: &[u8; 16] = b"0123456789abcdef";
	let mut out = String::with_capacity(bytes.len() * 2);
	for byte in bytes {
		out.push(DIGITS[(byte >> 4) as usize] as char);
		out.push(DIGITS[(byte & 0x0f) as usize] as char);
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	/// RFC 4231 test cases 2 and 6: a short key, and a key longer than the block that is
	/// hashed first.
	#[test]
	fn hmac_matches_rfc_4231() {
		let raw = |key: &[u8], message: &[u8]| hex(&hmac_over(key, |hasher| hasher.update(message)));
		assert_eq!(raw(b"Jefe", b"what do ya want for nothing?"), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
		assert_eq!(
			raw(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First"),
			"60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
		);
	}

	#[test]
	fn framing_keeps_part_boundaries() {
		assert_ne!(sha256(&[b"ab", b"c"]), sha256(&[b"a", b"bc"]));
		assert_ne!(sha256(&[b"abc"]), sha256(&[b"ab", b"c"]));
		assert_ne!(hmac_sha256(b"k", &[b"ab", b"c"]), hmac_sha256(b"k", &[b"a", b"bc"]));
	}

	#[test]
	fn a_csrf_token_is_bound_to_sub_origin_and_window() {
		let keys = IntentKeys::derive(&[7; 32]);
		let now = 1_790_000_123;
		let (token, expires_at) = keys.csrf_token("user-1", "https://invest.evinvest.ltd", now);
		assert!(keys.csrf_valid(&token, "user-1", "https://invest.evinvest.ltd", now));
		assert!(expires_at > now + CSRF_WINDOW_SECS - 1, "at least one whole window left");

		assert!(!keys.csrf_valid(&token, "user-2", "https://invest.evinvest.ltd", now), "another account");
		assert!(!keys.csrf_valid(&token, "user-1", "https://sa.evinvest.ltd", now), "another host");
		assert!(!IntentKeys::derive(&[8; 32]).csrf_valid(&token, "user-1", "https://invest.evinvest.ltd", now), "another key");

		assert!(
			keys.csrf_valid(&token, "user-1", "https://invest.evinvest.ltd", now + CSRF_WINDOW_SECS),
			"the next window still takes it"
		);
		assert!(
			!keys.csrf_valid(&token, "user-1", "https://invest.evinvest.ltd", now + 2 * CSRF_WINDOW_SECS),
			"two windows on, it is gone"
		);
		assert!(
			!keys.csrf_valid(&token, "user-1", "https://invest.evinvest.ltd", now - CSRF_WINDOW_SECS),
			"a token from the future"
		);

		for bad in ["", ".", "x.y", &token[..token.len() - 1], &format!("{token}0")] {
			assert!(!keys.csrf_valid(bad, "user-1", "https://invest.evinvest.ltd", now), "{bad:?}");
		}
	}

	/// One key, two uses: a seal never verifies as a CSRF token's MAC, nor the other way.
	#[test]
	fn the_two_subkeys_differ() {
		let keys = IntentKeys::derive(&[7; 32]);
		assert_ne!(keys.csrf, keys.seal);
		let hash = [1u8; 32];
		let seal = keys.seal(&hash);
		assert!(keys.seal_valid(&hash, &seal));
		assert!(!keys.seal_valid(&[2u8; 32], &seal));
	}
}
