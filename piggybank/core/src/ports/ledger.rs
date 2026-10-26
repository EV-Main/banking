//! The TigerBeetle ledger gateway port.
//!
//! [`Ledger`] is a [`Gateway`] — the anti-corruption boundary to an external
//! transactional system that owns its own atomicity. The kernel gives `Gateway` no
//! `UnitOfWork` accessor, so "the ledger cannot join a Postgres transaction" is a
//! compile-discoverable fact: money is written **last**, in the relay, after the
//! control-plane commit.
//!
//! The port speaks the domain chart of accounts ([`LedgerAccountKey`],
//! [`TransferCode`]); the adapter resolves keys to `u128` TigerBeetle ids (via the
//! `tb_accounts` map) and creates accounts with the correct non-negative flag on
//! first touch. Transfer `id`s are **caller-assigned and deterministic** (derived
//! from the event id), so a retried submit returns `Exists` — idempotent by design.
//! Amounts are always **explicit** (never TB balancing flags), so a retry moves the
//! exact amount frozen into the event.

use async_trait::async_trait;
use domain::{
	architecture::Gateway,
	balance::{AccountCode, LedgerAccountKey, TransferCode},
	error::DomainError,
};
use thiserror::Error;

#[async_trait]
pub trait Ledger: Gateway {
	/// Ensure the account for `key` exists with the correct ledger, code, and
	/// non-negative flag. Idempotent; flags are set on first create and are
	/// immutable in TB thereafter, so the create must be right the first time.
	async fn ensure_account(&self, key: &LedgerAccountKey) -> Result<(), LedgerError>;

	/// Live balance for an account, normalized to its natural side (Read-First).
	async fn balance(&self, key: &LedgerAccountKey) -> Result<LedgerBalance, LedgerError>;

	/// Everything ever debited from `key` that still stands: posted debits plus pending
	/// ones (a voided pending leaves both). Raw base units. What reconciliation sets against
	/// the control plane's record of what was allowed to leave an account.
	async fn total_debits(&self, key: &LedgerAccountKey) -> Result<u128, LedgerError>;

	/// What a new spend out of `key` may take right now, in raw base units: the ledger's
	/// [`LedgerBalance::available`] less every debit of it the control plane has recorded
	/// and the relay not yet applied ([`LockedReads::unapplied_debits`]). THE ONE ANSWER
	/// every Read-First on a user's money asks — a withdrawal, a payment, a move — so a spend recorded a moment ago is never spent twice. Read on a
	/// pooled connection: for a first answer before any lock is taken. Under a claim's lock
	/// ask [`Self::spendable_locked`]. This default — for wrappers that only intercept
	/// other calls — knows of nothing unapplied.
	async fn spendable(&self, key: &LedgerAccountKey) -> Result<u128, LedgerError> {
		Ok(self.balance(key).await?.available())
	}

	/// [`Self::balance`], with the control plane's half of the read made through `reads`
	/// — on the connection that holds the claim's lock. This default, for wrappers that
	/// only intercept other calls, reads on a pooled connection instead.
	async fn balance_locked(&self, key: &LedgerAccountKey, reads: &mut dyn LockedReads) -> Result<LedgerBalance, LedgerError> {
		let _ = reads;
		self.balance(key).await
	}

	/// [`Self::spendable`] under the claim's lock, every read through `reads`. The
	/// unapplied debits are read BEFORE the ledger: the relay applies a debit to the ledger
	/// first and marks it dispatched after, so this order can only count one twice (a
	/// refusal that clears on retry), never miss it.
	async fn spendable_locked(&self, key: &LedgerAccountKey, reads: &mut dyn LockedReads) -> Result<u128, LedgerError> {
		let unapplied = reads.unapplied_debits(key).await?;
		Ok(self.balance_locked(key, reads).await?.available().saturating_sub(unapplied))
	}

	/// [`Self::balance`] for several accounts, in the order asked. The adapter answers in
	/// one id-map query and one ledger lookup per chunk; this default — one read per key —
	/// is for wrappers that only intercept other calls.
	async fn balances(&self, keys: &[LedgerAccountKey]) -> Result<Vec<LedgerBalance>, LedgerError> {
		let mut out = Vec::with_capacity(keys.len());
		for key in keys {
			out.push(self.balance(key).await?);
		}
		Ok(out)
	}

	/// Whether a transfer with this (deterministic, caller-assigned) id has already
	/// applied. The relay's settle pre-check uses it to stay idempotent under
	/// at-least-once delivery: a redelivered event's already-applied leg re-submits as
	/// `Exists`, and the live balance already reflects its outflow — so its liquidity
	/// guard must be skipped, not re-checked against the post-outflow balance.
	async fn transfer_exists(&self, id: u128) -> Result<bool, LedgerError>;

	/// Apply a posted transfer with an explicit amount. Ensures both accounts exist
	/// first. Idempotent on the transfer `id` (a re-submit returns `Exists` ⇒ ok).
	async fn post(&self, transfer: &LedgerTransfer) -> Result<(), LedgerError>;

	/// Apply several posted transfers as ONE TigerBeetle linked chain: every leg lands or
	/// none does, and each leg's non-negative check sees the legs before it — so a chain
	/// can credit an account and debit it again in the same breath. Empty is a no-op.
	/// Idempotent on the FIRST leg's id: a chain applies atomically, so its first id
	/// existing means the whole chain already did, and a re-submit is `Ok(())` without
	/// touching the ledger.
	async fn post_linked(&self, transfers: &[LedgerTransfer]) -> Result<(), LedgerError>;

	/// Apply a pending (two-phase) transfer with `timeout = 0` — the saga owns the
	/// lifecycle, never TB's clock (so a pending can't auto-void out from under it).
	async fn reserve(&self, transfer: &LedgerTransfer) -> Result<(), LedgerError>;

	/// Post or void a pending transfer. Already-posted/already-voided ⇒ success
	/// (idempotent); pending-not-found ⇒ [`LedgerError::Retryable`].
	async fn complete(&self, completion: &PendingCompletion) -> Result<(), LedgerError>;

	/// Apply posted, pending and completing legs as ONE TigerBeetle linked chain — every
	/// leg lands or none does, each leg seeing the ones before it. What a hold's partial
	/// capture needs: void the reservation, post the capture, reserve the rest, with the
	/// user's money never free in between. Idempotent on the FIRST leg's id, like
	/// [`Self::post_linked`]. Empty is a no-op.
	async fn apply_linked(&self, legs: &[LinkedLeg]) -> Result<(), LedgerError>;

	/// The cash plane's global posted invariant, summed straight from TigerBeetle (the
	/// authoritative store): total custody (`wallet:<net>` debit-normal assets) vs total
	/// claims (every credit-normal account on the USDT ledger), the claims broken down by
	/// [`CashSide`] so a reader can say WHOSE the custody is. By construction `custody ==
	/// claims` always holds; reconciliation asserts it and alerts if TB and the design
	/// ever diverge. Returns raw 18-dp USDT base units.
	async fn cash_invariant(&self) -> Result<CashInvariant, LedgerError>;
}

/// The control plane's reads a spend makes under a claim's write lock, on the very
/// connection holding that lock.
///
/// Every waiter on the lock holds a connection of the shared pool while it waits; a check
/// that asked the pool for another connection while holding the lock could find none left
/// and stall every RPC of the hub until the pool's acquire timeout (#507). So nothing
/// decided under the lock reads the control plane any other way.
#[async_trait]
pub trait LockedReads: Send {
	/// Σ the debits of `key` the control plane recorded and the ledger has not applied yet:
	/// compartment moves still `pending`, and every outbox event neither dispatched nor
	/// parked whose plan debits `key` — a withdrawal's reserve, a payment's reserve, a
	/// scope's debit. Raw base units.
	async fn unapplied_debits(&mut self, key: &LedgerAccountKey) -> Result<u128, LedgerError>;

	/// The ledger's id for `key` from the adapter's id map; `None` when the account was
	/// never touched.
	async fn account_id(&mut self, key: &LedgerAccountKey) -> Result<Option<u128>, LedgerError>;
}

/// A spend's last check, run by the repository that records the spend AFTER it takes the
/// claim's write lock and before it writes — so two spends of one claim (a withdrawal and a
/// move) cannot both pass on the same money. The application
/// says what to check ([`ClaimCovers`]); the repository says when, and hands in the reads
/// of its own locked connection.
#[async_trait]
pub trait SpendGuard: Send + Sync {
	async fn check(&self, reads: &mut dyn LockedReads) -> Result<(), DomainError>;
}

/// `key` still covers `amount` — [`Ledger::spendable_locked`], net of every debit recorded
/// and not yet on the ledger. A shortfall is `Validation(refusal)`, the wording each flow
/// has always answered with.
pub struct ClaimCovers<'a> {
	pub ledger: &'a dyn Ledger,
	pub key: LedgerAccountKey,
	pub amount: u128,
	pub refusal: &'static str,
}

impl ClaimCovers<'_> {
	/// The same question before any lock, to refuse early: [`Ledger::spendable`].
	pub async fn check_unlocked(&self) -> Result<(), DomainError> {
		self.answer(self.ledger.spendable(&self.key).await?)
	}

	fn answer(&self, spendable: u128) -> Result<(), DomainError> {
		if spendable < self.amount {
			return Err(DomainError::Validation(self.refusal.into()));
		}
		Ok(())
	}
}

#[async_trait]
impl SpendGuard for ClaimCovers<'_> {
	async fn check(&self, reads: &mut dyn LockedReads) -> Result<(), DomainError> {
		self.answer(self.ledger.spendable_locked(&self.key, reads).await?)
	}
}

/// Which side of the cash invariant a USDT-ledger account counts on, by its kind — the
/// domain's chart of accounts read as a conservation statement. The claims sides are
/// the answer to "who is owed the custody": people directly (their `main` claim and their
/// scope compartments), allocations (whose holders are people), scopes' own accounts, the
/// withdrawal transit, the book's cash escrow, and the retired singleton claims (#245),
/// which the data migration emptied but which still exist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CashSide {
	/// `wallet:<net>` — the asset side.
	Custody,
	/// `user:<id>` — a person's own claim.
	UserClaims,
	/// `service:<svc>` — an allocation's claim, a product's or a reserved one's.
	ServiceClaims,
	/// `clearing` — in flight between a claim and a rail.
	Clearing,
	/// `book_cash:<user>` — a person's cash resting in a buy order.
	BookCash,
	/// `scope_claim:<scope>:<user>` — a person's money in one scope's compartment.
	ScopeClaims,
	/// `scope_account:<scope>:<label>` — a scope's own money, banking's treasury included.
	ScopeAccounts,
	/// The retired `fund` (code 1) and `fee` (code 40) claims (#245). Zero since the
	/// ownership data migration moved them onto the reserved allocations; still a side,
	/// because TigerBeetle cannot delete an account and the conservation check must count
	/// whatever is on every one of them.
	RetiredClaims,
}

impl CashSide {
	/// The side an account of kind `code` counts on; `None` for a kind that is not on
	/// the USDT ledger at all (custody in the mocked bank, the retired unit accounts).
	pub fn of(code: AccountCode) -> Option<Self> {
		match code {
			AccountCode::CryptoWallet => Some(Self::Custody),
			AccountCode::UserClaim => Some(Self::UserClaims),
			AccountCode::ServiceClaim => Some(Self::ServiceClaims),
			// Holds are pending only, so nothing posted ever sits on it; should something
			// ever, it is in-flight value like the withdrawal transit, not anybody's claim.
			AccountCode::WithdrawalClearing | AccountCode::HoldClearing => Some(Self::Clearing),
			AccountCode::BookCash => Some(Self::BookCash),
			AccountCode::ScopeClaim => Some(Self::ScopeClaims),
			AccountCode::ScopeAccount => Some(Self::ScopeAccounts),
			AccountCode::RetiredFundClaim | AccountCode::RetiredFeeClaim => Some(Self::RetiredClaims),
			AccountCode::BankCustody | AccountCode::UserShares | AccountCode::SharesOutstanding | AccountCode::FeeShares | AccountCode::RetiredCompanyStake | AccountCode::BookShares => None,
		}
	}
}
/// The reconciliation read of the cash plane's global double-entry invariant: the summed
/// posted custody side and claims side. They must be equal (`balanced()`).
///
/// `claims` is the whole credit side — every non-custody account on the USDT ledger,
/// whatever its kind — so the conservation check cannot be fooled by an account the
/// breakdown does not know. The named parts say whose the custody is; what they leave
/// over ([`Self::unclassified`]) is value on an account of no known kind, which is its
/// own finding.
#[derive(Clone, Copy, Debug, Default)]
pub struct CashInvariant {
	pub custody: u128,
	pub claims: u128,
	/// Σ `user:<id>` — held by people directly.
	pub user_claims: u128,
	/// Σ `service:<svc>` — held by allocations, whose units people hold.
	pub service_claims: u128,
	/// `clearing` — the posted withdrawal transit.
	pub clearing: u128,
	/// Σ `book_cash:<user>` — resting in buy orders.
	pub book_cash: u128,
	/// Σ `scope_claim:<scope>:<user>` — people's money in scope compartments.
	pub scope_claims: u128,
	/// Σ `scope_account:<scope>:<label>` — scopes' own money, banking's treasury included.
	pub scope_accounts: u128,
	/// The retired `fund` + `fee` singleton claims (#245).
	pub retired_claims: u128,
}

impl CashInvariant {
	pub fn balanced(self) -> bool {
		self.custody == self.claims
	}

	/// Claims on accounts of a kind the breakdown does not name — zero on a ledger that
	/// only the chart of accounts has ever written to.
	pub fn unclassified(self) -> u128 {
		self.claims
			.saturating_sub(self.user_claims)
			.saturating_sub(self.service_claims)
			.saturating_sub(self.clearing)
			.saturating_sub(self.book_cash)
			.saturating_sub(self.scope_claims)
			.saturating_sub(self.scope_accounts)
			.saturating_sub(self.retired_claims)
	}
}
/// Failure modes the relay and query handlers must distinguish — most importantly
/// `InsufficientFunds` (a real domain outcome from a non-negative-flag violation)
/// from `Retryable`/`Unavailable` (transient) and `Conflict` (a should-not-happen
/// the saga must surface, never silently absorb).
#[derive(Debug, Error)]
pub enum LedgerError {
	/// A non-negative invariant would be violated (TB `ExceedsCredits`/`ExceedsDebits`):
	/// the source account can't cover the transfer.
	#[error("insufficient funds")]
	InsufficientFunds,
	/// Transient — the ledger was unreachable or closed. Retry.
	#[error("ledger unavailable: {0}")]
	Unavailable(String),
	/// A two-phase post raced its pending create (`PendingTransferNotFound`). Retry
	/// after the pending lands.
	#[error("ledger retryable: {0}")]
	Retryable(String),
	/// A genuine inconsistency (overflow, expired pending, ledger mismatch). Park +
	/// alert; never treat as success.
	#[error("ledger conflict: {0}")]
	Conflict(String),
}

impl From<LedgerError> for DomainError {
	/// For a *query* read or a Read-First check: `InsufficientFunds` is a client-
	/// facing validation outcome; the rest are infrastructure faults (never leaked
	/// verbatim to clients — they map to `unavailable` at the gRPC boundary).
	fn from(err: LedgerError) -> Self {
		match err {
			LedgerError::InsufficientFunds => DomainError::Validation("insufficient funds".into()),
			LedgerError::Unavailable(detail) | LedgerError::Retryable(detail) | LedgerError::Conflict(detail) => DomainError::Repository(detail),
		}
	}
}

/// An account's live balance in **raw base units**, normalized to its natural side
/// (every field `>= 0`). The unit is the account's ledger's — the gateway is
/// currency-agnostic, so callers wrap into the typed `Usdt` at the boundary. Zero when the account
/// doesn't exist yet.
#[derive(Clone, Copy, Debug)]
pub struct LedgerBalance {
	/// The settled balance on the natural side (`credits − debits` for a claim).
	pub posted: u128,
	/// In-flight INFLOW on the natural side (pending credits for a claim) awaiting
	/// settlement — zero for the common one-sided pending.
	pub pending: u128,
	/// In-flight OUTFLOW reserved against this account (pending debits on a claim):
	/// the amount locked by an unsettled withdrawal or reservation. Subtract from
	/// `posted` for the spendable balance ([`LedgerBalance::available`]).
	pub locked: u128,
}

impl LedgerBalance {
	/// The settled balance not already reserved by an in-flight pending — what a new
	/// command may actually spend (Read-First). Saturating, so never negative.
	pub fn available(self) -> u128 {
		self.posted.saturating_sub(self.locked)
	}
}

/// A posted or to-be-pending transfer. `id` is caller-assigned and deterministic;
/// `amount` is in the ledger's base units (`Usdt` converted via `base_units()` by the
/// relay); `reference` is stamped into `user_data_128` (the aggregate id) for
/// reconciliation.
#[derive(Clone, Debug)]
pub struct LedgerTransfer {
	pub id: u128,
	pub debit: LedgerAccountKey,
	pub credit: LedgerAccountKey,
	pub amount: u128,
	pub code: TransferCode,
	pub reference: u128,
}

/// One leg of an [`Ledger::apply_linked`] chain.
#[derive(Clone, Debug)]
pub enum LinkedLeg {
	Post(LedgerTransfer),
	Reserve(LedgerTransfer),
	Complete(PendingCompletion),
}

impl LinkedLeg {
	/// The leg's own transfer id.
	pub fn id(&self) -> u128 {
		match self {
			Self::Post(transfer) | Self::Reserve(transfer) => transfer.id,
			Self::Complete(completion) => completion.id,
		}
	}
}

/// Whether a [`PendingCompletion`] posts (commits) or voids (releases) the pending.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionKind {
	Post,
	Void,
}

/// Completes a previously-created pending transfer. Carries the original accounts
/// and amount so the adapter can re-issue the completion idempotently on retry.
#[derive(Clone, Debug)]
pub struct PendingCompletion {
	/// The completion transfer's own deterministic id.
	pub id: u128,
	/// The original pending transfer's id (TB `pending_id`).
	pub pending_id: u128,
	pub kind: CompletionKind,
	pub debit: LedgerAccountKey,
	pub credit: LedgerAccountKey,
	pub amount: u128,
	pub code: TransferCode,
	pub reference: u128,
}

#[cfg(test)]
mod tests {
	use super::*;

	// Every kind on the USDT ledger has a side, and the retired singletons count as
	// CLAIMS: the accounts `fund` (1) and `fee` (40) exist for good, and whatever were on
	// them would be owed to somebody. A kind on another ledger has none.
	#[test]
	fn every_usdt_ledger_kind_has_a_side_and_the_retired_claims_are_claims() {
		use domain::{balance::Ledger as Plane, money::Network, users::UserId};
		let user = UserId::new();
		let keys = [
			LedgerAccountKey::CryptoWallet(Network::Bep20),
			LedgerAccountKey::BankCustody,
			LedgerAccountKey::UserClaim(user),
			LedgerAccountKey::WithdrawalClearing,
			LedgerAccountKey::HoldClearing,
			LedgerAccountKey::ScopeClaim(user, domain::scopes::ScopeId::parse("sa").unwrap()),
			LedgerAccountKey::ScopeAccount(domain::scopes::ScopeId::main(), domain::scopes::ScopeAccountLabel::treasury()),
		];
		for key in keys {
			let side = CashSide::of(key.account_code());
			assert_eq!(side.is_some(), key.ledger() == Plane::Usdt, "{}: a side iff on the USDT ledger", key.logical_key());
		}
		assert_eq!(CashSide::of(AccountCode::RetiredFundClaim), Some(CashSide::RetiredClaims));
		assert_eq!(CashSide::of(AccountCode::RetiredFeeClaim), Some(CashSide::RetiredClaims));
		assert_eq!(CashSide::of(AccountCode::RetiredCompanyStake), None, "a unit account, off the cash plane");
		assert_eq!(CashSide::of(AccountCode::CryptoWallet), Some(CashSide::Custody));
	}

	// The named parts never exceed the whole, and what they leave over is the value on
	// accounts the chart of accounts does not know — zero on a ledger only it wrote to.
	#[test]
	fn the_unclassified_remainder_is_what_the_named_parts_leave_over() {
		let inv = CashInvariant {
			custody: 100,
			claims: 100,
			user_claims: 50,
			service_claims: 20,
			clearing: 5,
			book_cash: 3,
			scope_claims: 10,
			scope_accounts: 5,
			retired_claims: 7,
		};
		assert!(inv.balanced());
		assert_eq!(inv.unclassified(), 0);
		assert_eq!(CashInvariant { retired_claims: 0, ..inv }.unclassified(), 7, "7 on an account of no known kind");
		assert_eq!(
			CashInvariant {
				scope_claims: 0,
				scope_accounts: 0,
				..inv
			}
			.unclassified(),
			15,
			"the scope sides are named parts"
		);
	}
}
