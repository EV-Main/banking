//! The deposit-address port — a user's per-network address to receive USDT at.
//!
//! On account-model chains (BEP20/TRC20) a per-user deposit address is the only way
//! to attribute an incoming transfer — a USDT transfer carries no memo — so addresses
//! are HD-derived from banking's xpub. This port hands the application a **stable**
//! address per (user, network); a stub deterministically derives and caches one until
//! the real key-management/derivation service exists.

use async_trait::async_trait;
use domain::{
	architecture::Gateway,
	error::DomainError,
	money::{Network, WalletAddress},
	users::UserId,
};

/// A [`Gateway`]: the signer/key-management seam is a separate trust domain — it
/// owns its own store and can never enrol in a hub Postgres transaction.
#[async_trait]
pub trait DepositAddresses: Gateway {
	/// The user's **fundable** deposit address on `network`, derived once and reused
	/// (stable across calls so a user always sees the same address). `None` means no
	/// fundable address exists yet — the underlying address is still a placeholder (not
	/// the on-chain image of the key), so the rail is presented as unavailable rather than
	/// surfacing an address that cannot receive funds.
	async fn address(&self, user: UserId, network: Network) -> Result<Option<WalletAddress>, DomainError>;

	/// The reverse lookup: which user, if any, owns `address` on `network`.
	///
	/// Attribution runs this way round whenever the CHAIN is the input rather than the user —
	/// verifying an arrival starts from a recipient and has to discover whose it is. `None`
	/// means the address is not one of ours, which is a refusal: crediting a transfer that
	/// landed somewhere we do not control would book money we cannot spend.
	///
	/// `address` is matched in the rail's canonical stored form (lowercase `0x…` on EVM, raw
	/// `0:<hex>` on TON).
	async fn owner_of(&self, network: Network, address: &str) -> Result<Option<UserId>, DomainError> {
		let _ = (network, address);
		Ok(None)
	}

	/// Supersede the user's key-backed address on `network` with a freshly minted
	/// keypair — recovery for a PROVABLY DEAD key (the signer can no longer unseal
	/// it). Returns the NEW fundable address; the backend refuses to rotate a key
	/// that is still healthy. Backends without a rotating key store reject.
	async fn rotate(&self, user: UserId, network: Network) -> Result<WalletAddress, DomainError> {
		let _ = (user, network);
		Err(DomainError::Validation("deposit-address rotation is not supported by this backend".into()))
	}

	/// Retire the user's HEALTHY key-backed address on `network` in favour of one held by
	/// the key custodian, and return both halves.
	///
	/// `drained_address` is what the caller proved empty, passed on so the backend can refuse
	/// unless it is the very address being retired — a funds check made against a stale or
	/// mistaken address is the failure this parameter exists to catch. Deliberately NOT
	/// defaulted to the caller re-reading the address itself: the address that was cleared and
	/// the address that gets retired have to be the same value travelling together.
	///
	/// Distinct from [`rotate`](Self::rotate) all the way down. Rotation replaces a key that
	/// can no longer sign; this replaces one that still can, so the safety argument is the
	/// opposite way round and neither backend gate may be reused for the other. Backends with
	/// no custodian reject.
	async fn migrate_to_custodian(&self, user: UserId, network: Network, drained_address: &str) -> Result<MigratedAddress, DomainError> {
		let _ = (user, network, drained_address);
		Err(DomainError::Validation("deposit-address custody migration is not supported by this backend".into()))
	}
}

/// Both sides of a completed custody migration. The old address is returned, not discarded:
/// its key is archived rather than destroyed, so an operator watching for a late arrival on it
/// needs to be told which address to watch.
/// `Debug` is safe here: both fields are public on-chain addresses, and neither the retired
/// key nor the custodian's handle is reachable from this type.
#[derive(Debug)]
pub struct MigratedAddress {
	pub old_address: String,
	pub new_address: WalletAddress,
}
