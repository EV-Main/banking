//! Shared domain crate.
//!
//! The single source of truth for domain types across the platform. The hub
//! server (`piggybank-core`) depends on it, and so do other service repos and
//! their wasm frontends (it stays wasm-safe). It never depends on the hub server
//! or any adapter.
//!
//! It carries the cross-cutting [`error::DomainError`], re-exports the `ev`
//! architecture building blocks, and holds the hub's bounded contexts — `auth` /
//! `authz` (the mirrored seat), `balance` / `money` (the chart of accounts
//! and the 18-dp USDT unit), `allocations` (the registry of investable products), `fees`
//! (the management + performance fee policy), `issuance` (an operator minting units in
//! kind, to an investor or the company), `book` (the secondary market in a product's
//! units — orders, the matching engine, the escrow facts), `scopes` (a user's wallet split
//! by scope, and the scope's own accounts), `scope_ledger` (what a scope's own service may
//! do with money there — its grants, its accounts, its transfers), `scope_invoices` (a scope
//! asking a user to pay into one of its accounts), `scope_mandates` (a user's consent to a
//! service drawing on their compartment), and
//! the `users` / `subscriptions` / `redemptions` / `withdrawals` aggregates.

pub mod error;

pub mod allocations;
pub mod auth;
pub mod authz;
pub mod balance;
pub mod book;
pub mod consilium;
pub mod fees;
pub mod issuance;
pub mod money;
pub mod payments;
pub mod redemptions;
pub mod scope_invoices;
pub mod scope_ledger;
pub mod scope_mandates;
pub mod scopes;
pub mod subscriptions;
pub mod users;
pub mod withdrawals;

/// Append one length-prefixed field to a canonical encoding.
///
/// Shared by every hashed subject in this crate ([`payments::PaymentTerms`],
/// [`consilium::HolderGrantTerms`] and the rest) because the rule it encodes is the same one in each: a
/// variable-length part is prefixed with its length so no two distinct values can
/// concatenate to the same bytes. Duplicating four lines would be cheap; duplicating the
/// *rule* is how two subjects end up with subtly different framings and one of them
/// collides.
pub(crate) fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
	out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
	out.extend_from_slice(bytes);
}

/// Lowercase hex of a 32-byte digest. Hand-rolled so `domain` keeps its dependency set
/// (and its wasm-safety) unchanged for four lines of work.
pub(crate) fn hex32(bytes: &[u8; 32]) -> String {
	const DIGITS: &[u8; 16] = b"0123456789abcdef";
	let mut out = String::with_capacity(64);
	for byte in bytes {
		out.push(DIGITS[(byte >> 4) as usize] as char);
		out.push(DIGITS[(byte & 0x0f) as usize] as char);
	}
	out
}

/// Re-export of the `architecture` feature of the external `ev` crate — the
/// shared DDD tactical building blocks (`Id`, `Entity`, `AggregateRoot`,
/// `Repository`, `Gateway`, …) — so consumers reach them via
/// `domain::architecture::…` without depending on `ev` directly.
pub use ev::architecture;
