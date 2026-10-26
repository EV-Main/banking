//! `balance` bounded context — the platform's money, and where it sits.
//!
//! The platform ("piggybank") custodies all value; **every unit of it has a holder** — a
//! person, through their claim, or a scope, through its own accounts (banking's treasury
//! among them). Two views, both authoritative in TigerBeetle (the data plane); this
//! context is the **pure, wasm-safe** model of the chart of accounts and the parties — no
//! I/O, no `tigerbeetle` types leak in.
//!
//! - **Custody / treasury** (debit-normal): where value physically is — a crypto
//!   wallet per [`Network`] (the per-rail liquidity), plus a mocked bank account.
//!   Assets the platform holds. This is the **only** layer where network lives.
//! - **Claims** (credit-normal, **network-agnostic**): whose value it is — a user's
//!   claim (`UserClaim`, their `main` wallet) and their compartments in scopes
//!   (`ScopeClaim`), a scope's own accounts (`ScopeAccount`, banking's treasury among
//!   them, where the retained withdrawal fee lands — D17), and queued-withdrawal funds
//!   (`WithdrawalClearing`). USDT is one fungible pool, so a user has ONE claim per
//!   compartment, not one per chain.
//!
//! **Retired accounts.** Accounts no live key derives any more — the singleton `fund` and
//! `fee` claims, the allocations' `service:` claims, the Share ledger's unit accounts, the
//! book's cash escrow — stay in TigerBeetle and in `tb_accounts` for good, so their codes
//! are reserved forever ([`AccountCode::RETIRED`]).
//!
//! Two layers, one invariant: **`sum(custody) == sum(claims)`** globally on the USDT
//! ledger. Per-rail backing is a *treasury* concern (a withdrawal on a short rail is
//! queued, not refused), not a ledger one — which is why the invariant is global, not
//! per-network. A deposit is one balanced transfer `Dr WALLET:<net> / Cr <claim>`
//! (textbook Dr Cash / Cr customer-deposit); network rides on the transaction, never
//! on the claim. There is no "external world" account.

use ev::architecture::{DomainEvent, Id};
use serde::{Deserialize, Serialize};

use crate::{
	error::DomainError,
	money::{Network, Usdt},
	scopes::{ScopeAccountLabel, ScopeId},
	users::UserId,
};

/// A holder of value in (or a claim against) the fund — the party a deposit credits
/// and a claim belongs to. Tagged for a self-describing JSON shape in event payloads
/// and projections.
///
/// **Every variant must map to a [`LedgerAccountKey`]**, which is what keeps this the one
/// ubiquitous name for "an addressable end of a money move" rather than one vocabulary per
/// feature. Two deliberate absences follow from that rule:
///
/// - **`clearing` gets no variant.** [`LedgerAccountKey::WithdrawalClearing`] is an
///   *in-flight* account owned by a saga, never a party anyone pays or is paid. Giving it
///   a variant would make it addressable as one end of a payment, and a payment into
///   clearing is money parked behind a reservation nobody can complete.
/// - **an external address is not a party.** It has no claim account at all, so it lives
///   in `PaymentDestination` (`domain::payments`) beside this type rather than inside it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Party {
	User(UserId),
	/// banking's own treasury, `ScopeAccount(main, treasury)`, where the fees it charges
	/// for its own service land (D17). Money leaves it only by a payment the owners
	/// approved, so it is a party a payment may name as its SOURCE.
	Treasury,
}

impl Party {
	/// The discriminator stored in an `*_kind` column.
	pub fn kind_str(&self) -> &'static str {
		match self {
			Self::User(_) => "user",
			Self::Treasury => "treasury",
		}
	}

	/// The identity stored in an `*_id` column.
	pub fn id_str(&self) -> String {
		match self {
			Self::User(id) => id.to_string(),
			// Non-null on purpose: the single-open-order-per-source index keys on the pair.
			Self::Treasury => TREASURY_ID.to_owned(),
		}
	}

	/// Reconstruct from the `(kind, id)` column pair (persistence adapter). A retired kind
	/// (`piggybank`, `revenue`, `service`) is refused like any unknown one.
	pub fn from_parts(kind: &str, id: Option<&str>) -> Result<Self, DomainError> {
		match (kind, id) {
			("user", Some(raw)) => {
				let uuid = uuid::Uuid::parse_str(raw).map_err(|_| DomainError::Validation("invalid user party id".into()))?;
				Ok(Self::User(Id::from_raw(uuid)))
			}
			("treasury", Some(TREASURY_ID)) => Ok(Self::Treasury),
			_ => Err(DomainError::Validation(format!("invalid party: {kind}"))),
		}
	}

	/// The network-agnostic, credit-normal claim account that holds this party's value.
	/// The relay credits/debits this when moving the party's money; network rides on the
	/// custody side of the transfer.
	pub fn claim_key(&self) -> LedgerAccountKey {
		match self {
			Self::User(user) => LedgerAccountKey::UserClaim(*user),
			Self::Treasury => LedgerAccountKey::ScopeAccount(ScopeId::main(), ScopeAccountLabel::treasury()),
		}
	}
}

/// The `*_id` column value of [`Party::Treasury`].
const TREASURY_ID: &str = "main:treasury";

/// Standalone ledger facts not tied to a Postgres aggregate — the fund's accounts
/// live in TigerBeetle, so a deposit is recorded straight to
/// the outbox for the relay to move. Internally tagged for a self-describing payload.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LedgerEvent {
	/// Value arrived from outside (an on-chain deposit) and was credited to a party.
	/// Ledger: `Dr WALLET:<net> / Cr <party claim>`.
	Deposited { party: Party, network: Network, amount: Usdt },
}

impl DomainEvent for LedgerEvent {
	const KIND: &'static str = "balance";
}

/// A unit of value in TigerBeetle (`ledger`). Only same-ledger accounts transact (a stray
/// cross-ledger pairing is a hard TB error, never a silent leak).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ledger {
	/// Canonical 18-dp USDT value ledger.
	Usdt,
	/// Mocked bank (USD) ledger — the real-money seam.
	UsdMock,
}

impl Ledger {
	pub const fn id(self) -> u32 {
		match self {
			Self::Usdt => 1,
			Self::UsdMock => 2,
		}
	}
}

/// Account type (TigerBeetle `code`) — only the kinds a live key derives. A retired kind
/// is a number in [`Self::RETIRED`], never a variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountCode {
	CryptoWallet,
	BankCustody,
	UserClaim,
	/// A user's compartment in a non-main scope.
	ScopeClaim,
	/// A scope's own account `(scope, label)`, `(main, treasury)` included.
	ScopeAccount,
	WithdrawalClearing,
	/// The credit side of every hold a scope's service places on a user's compartment.
	HoldClearing,
}

impl AccountCode {
	/// Codes of accounts no live key derives: `1`/`40` the singleton `fund`/`fee` claims,
	/// `30` an allocation's `service:` claim, `65` the book's cash escrow (USDT ledger);
	/// `60`/`61`/`62`/`63`/`64` the Share ledger's unit accounts. TigerBeetle cannot delete
	/// an account, so their rows stay and the numbers are reserved forever
	/// (`every_account_and_transfer_code_is_unique`).
	pub const RETIRED: [u16; 9] = [1, 30, 40, 60, 61, 62, 63, 64, 65];

	pub const fn code(self) -> u16 {
		match self {
			Self::CryptoWallet => 10,
			Self::BankCustody => 11,
			Self::UserClaim => 20,
			Self::ScopeClaim => 21,
			Self::ScopeAccount => 31,
			Self::WithdrawalClearing => 50,
			Self::HoldClearing => 51,
		}
	}

	/// The inverse of [`Self::code`]; `None` for a retired number and for one no kind has
	/// ever carried — a scan tells the two apart with [`Self::is_retired`].
	pub const fn from_code(code: u16) -> Option<Self> {
		Some(match code {
			10 => Self::CryptoWallet,
			11 => Self::BankCustody,
			20 => Self::UserClaim,
			21 => Self::ScopeClaim,
			31 => Self::ScopeAccount,
			50 => Self::WithdrawalClearing,
			51 => Self::HoldClearing,
			_ => return None,
		})
	}

	pub fn is_retired(code: u16) -> bool {
		Self::RETIRED.contains(&code)
	}
}

/// Which side an account's balance is normal on — drives the non-negative flag and
/// how a posted/pending balance is computed from `debits`/`credits`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Normal {
	/// Assets (custody): balance = `debits − credits`; guard with
	/// `CreditsMustNotExceedDebits`.
	Debit,
	/// Liabilities/equity (claims): balance = `credits − debits`; guard with
	/// `DebitsMustNotExceedCredits`.
	Credit,
}

/// Transfer type (TigerBeetle `code`) — classifies a money movement for audit and
/// reconciliation. Never load-bearing for correctness, only for forensics. Only the kinds
/// the hub still posts; a retired kind is a number in [`Self::RETIRED`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferCode {
	Deposit,
	Withdraw,
	WithdrawFee,
	/// A [`crate::payments::PaymentOrder`] moving value between two claims — both the
	/// reservation into `clearing` and its settlement out of it.
	///
	/// ONE CODE, NOT ONE PER TIER. The code is forensic, and the tier is already derivable
	/// from the claim pair a transfer names; a code per tier would encode the same fact
	/// twice and let the two disagree.
	PaymentTransfer,
	/// A user moving their own money between `main` and a scope compartment, either way.
	/// One code for both directions: the claim pair says which way it went.
	CompartmentMove,
	/// Money between two scope accounts — a client's automatic transfer into its scope's
	/// `fee` collector (`scope.v1.ScopeLedgerService.Transfer`).
	ScopeTransfer,
	/// A hold on a user's compartment (`ScopeLedgerService.Reserve`): its pending
	/// reservation, the void that releases it, and a capture's posted leg out of the
	/// compartment. One code for all three: the flags and the account pair say which.
	ScopeHold,
	/// A payout out of a scope account into users' compartments in the scope
	/// (`ScopeLedgerService.Payout`, or an approved `RequestPayout`): one leg per line, posted
	/// as one linked chain.
	ScopePayout,
	/// A user paying a scope's invoice (`banking.v1.InvoicePaymentService.PayInvoice`):
	/// `Dr <main or the compartment in the scope> / Cr scope_account:<scope>:<label>`, posted
	/// at once.
	InvoicePayment,
	/// A scope's one-off charge of a user's compartment under their mandate
	/// (`ScopeLedgerService.Charge`): `Dr scope_claim:<scope>:<user> / Cr
	/// scope_account:<scope>:<label>`, posted at once.
	ScopeCharge,
}

impl TransferCode {
	/// Codes of kinds the hub no longer posts: `1` the seed onto the singleton `fund` claim,
	/// `10`/`11` user allocate/revoke, `20`–`23` the allocations' service-claim sagas, `30`
	/// the bridge, `40`–`45` subscriptions, redemptions, share mint/burn and the unit fees,
	/// `47`/`53` in-kind issue/retire, `48`–`51` the book, `52` a hand-over out of the
	/// company stake, `54` the ownership data migration, `61` the migration window's moves.
	/// TigerBeetle keeps those transfers forever, so the numbers are reserved and never
	/// handed to a new kind (`every_account_and_transfer_code_is_unique`).
	pub const RETIRED: [u16; 23] = [1, 10, 11, 20, 21, 22, 23, 30, 40, 41, 42, 43, 44, 45, 47, 48, 49, 50, 51, 52, 53, 54, 61];

	pub const fn code(self) -> u16 {
		match self {
			Self::Deposit => 2,
			Self::Withdraw => 3,
			Self::WithdrawFee => 4,
			Self::PaymentTransfer => 46,
			Self::CompartmentMove => 55,
			Self::ScopeTransfer => 56,
			Self::ScopeHold => 57,
			Self::InvoicePayment => 58,
			Self::ScopePayout => 59,
			Self::ScopeCharge => 60,
		}
	}
}

/// The logical identity of a ledger account — resolved to a concrete `u128`
/// TigerBeetle id (minted once, stored in the `tb_accounts` map) by the adapter.
/// Carries everything the adapter needs to *create* the account correctly the first
/// time: `ledger`, `code`, and the non-negative flag (all immutable in TB once set).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LedgerAccountKey {
	/// The fund's on-chain custody wallet for a rail (debit-normal asset). The only
	/// network-bearing account — per-rail liquidity (the treasury layer).
	CryptoWallet(Network),
	/// A user's single, network-agnostic claim (credit-normal). One account per user.
	UserClaim(UserId),
	/// Funds reserved for queued/in-flight withdrawals, not yet sent on-chain
	/// (credit-normal, network-agnostic). Decoupled from any rail, so a withdrawal can
	/// be accepted and queued even when the chosen rail is short on liquidity.
	WithdrawalClearing,
	/// The mocked bank custody account (debit-normal, USD ledger).
	BankCustody,
	/// A user's compartment in a scope other than `main` (credit-normal, USDT ledger, one
	/// per `(user, scope)`): their money earmarked for that scope's services. Their `main`
	/// compartment is [`Self::UserClaim`]; a `ScopeClaim` keyed on `main` is never built
	/// ([`crate::scopes::Compartment`] is the only path) and never parses.
	ScopeClaim(UserId, ScopeId),
	/// A scope's own money under a label its owner chose (credit-normal, USDT ledger):
	/// `(main, treasury)` holds the fees banking charges for its own service (D17).
	ScopeAccount(ScopeId, ScopeAccountLabel),
	/// The credit side of the pending reservations that hold users' compartments
	/// (credit-normal, USDT ledger, one for every hold). Pending only: a capture voids the
	/// reservation and posts out of the compartment directly, so nothing is ever posted
	/// here and the cash invariant never sees it.
	HoldClearing,
}

impl LedgerAccountKey {
	/// Stable string key for the `tb_accounts` id-map (and the idempotent create).
	pub fn logical_key(&self) -> String {
		match self {
			Self::CryptoWallet(net) => format!("wallet:{net}"),
			Self::UserClaim(user) => format!("user:{user}"),
			Self::WithdrawalClearing => "clearing".to_owned(),
			Self::BankCustody => "bank".to_owned(),
			Self::ScopeClaim(user, scope) => format!("scope_claim:{scope}:{user}"),
			Self::ScopeAccount(scope, label) => format!("scope_account:{scope}:{label}"),
			Self::HoldClearing => "hold_clearing".to_owned(),
		}
	}

	pub fn ledger(&self) -> Ledger {
		match self {
			Self::BankCustody => Ledger::UsdMock,
			_ => Ledger::Usdt,
		}
	}

	pub fn account_code(&self) -> AccountCode {
		match self {
			Self::CryptoWallet(_) => AccountCode::CryptoWallet,
			Self::BankCustody => AccountCode::BankCustody,
			Self::UserClaim(_) => AccountCode::UserClaim,
			Self::WithdrawalClearing => AccountCode::WithdrawalClearing,
			Self::ScopeClaim(..) => AccountCode::ScopeClaim,
			Self::ScopeAccount(..) => AccountCode::ScopeAccount,
			Self::HoldClearing => AccountCode::HoldClearing,
		}
	}

	/// Custody (wallet/bank) is debit-normal; every claim is credit-normal.
	pub fn normal(&self) -> Normal {
		match self {
			Self::CryptoWallet(_) | Self::BankCustody => Normal::Debit,
			Self::UserClaim(_) | Self::WithdrawalClearing | Self::ScopeClaim(..) | Self::ScopeAccount(..) | Self::HoldClearing => Normal::Credit,
		}
	}

	pub fn network(&self) -> Option<Network> {
		match self {
			Self::CryptoWallet(net) => Some(*net),
			_ => None,
		}
	}

	/// The inverse of [`Self::logical_key`]: read a `tb_accounts` row's key back into
	/// the account it names. Total over every string `logical_key` can produce, and an
	/// error for anything else, so a foreign or corrupt row is reported rather than
	/// silently attributed to an account it is not.
	///
	/// A retired row (`fund`, `fee`, `service:<id>`, a unit account) is an error too: no
	/// live account is named by it, and a scan skips such rows by their code
	/// ([`AccountCode::is_retired`]) before it parses.
	pub fn parse_logical_key(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"clearing" => Ok(Self::WithdrawalClearing),
			"hold_clearing" => Ok(Self::HoldClearing),
			"bank" => Ok(Self::BankCustody),
			_ => {
				let (prefix, rest) = raw.split_once(':').ok_or_else(|| DomainError::Validation(format!("unknown logical key: {raw}")))?;
				match prefix {
					"wallet" => Network::parse(rest).map(Self::CryptoWallet),
					"user" => user_id(rest).map(Self::UserClaim),
					// A scope id holds no ':', so the first ':' splits it off — a label may hold
					// ':' of its own (`alloc:<slug>`).
					"scope_claim" => {
						let (scope, user) = rest.split_once(':').ok_or_else(|| DomainError::Validation(format!("malformed scope claim key: {raw}")))?;
						let scope = ScopeId::parse(scope)?;
						if scope.is_main() {
							return Err(DomainError::Validation(format!("main has no scope claim: {raw}")));
						}
						Ok(Self::ScopeClaim(user_id(user)?, scope))
					}
					"scope_account" => {
						let (scope, label) = rest.split_once(':').ok_or_else(|| DomainError::Validation(format!("malformed scope account key: {raw}")))?;
						Ok(Self::ScopeAccount(ScopeId::parse(scope)?, ScopeAccountLabel::parse(label)?))
					}
					_ => Err(DomainError::Validation(format!("unknown logical key: {raw}"))),
				}
			}
		}
	}
}

fn user_id(raw: &str) -> Result<UserId, DomainError> {
	uuid::Uuid::parse_str(raw)
		.map(Id::from_raw)
		.map_err(|_| DomainError::Validation(format!("invalid user id in logical key: {raw}")))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::users::UserId;

	#[test]
	fn party_round_trips_through_columns() {
		for party in [Party::User(UserId::new()), Party::Treasury] {
			let back = Party::from_parts(party.kind_str(), Some(&party.id_str())).unwrap();
			assert_eq!(party, back);
		}
	}

	#[test]
	fn the_retired_party_kinds_no_longer_read() {
		// `piggybank` / `revenue` were the fund's own singleton claims (#245), `service` an
		// allocation's pooled claim: none of them is a party any more, so a stray one is
		// corrupt data and must say so rather than be mapped onto some live claim.
		for kind in ["piggybank", "revenue", "service"] {
			assert!(Party::from_parts(kind, None).is_err(), "{kind} read as a party");
			assert!(Party::from_parts(kind, Some("fee")).is_err(), "{kind} read as a party");
		}
		assert!(serde_json::from_str::<Party>(r#"{"kind":"piggybank"}"#).is_err());
		assert!(serde_json::from_str::<Party>(r#"{"kind":"revenue"}"#).is_err());
		assert!(serde_json::from_str::<Party>(r#"{"kind":"service","id":"fee"}"#).is_err());
	}

	#[test]
	fn account_keys_and_sides() {
		let uid = UserId::from_raw(uuid::Uuid::nil());
		assert_eq!(LedgerAccountKey::UserClaim(uid).logical_key(), "user:00000000-0000-0000-0000-000000000000");
		assert_eq!(LedgerAccountKey::CryptoWallet(Network::Bep20).logical_key(), "wallet:bep20");
		assert_eq!(LedgerAccountKey::WithdrawalClearing.logical_key(), "clearing");
		assert_eq!(LedgerAccountKey::UserClaim(uid).normal(), Normal::Credit);
		assert_eq!(LedgerAccountKey::CryptoWallet(Network::Ton).normal(), Normal::Debit);
		assert_eq!(LedgerAccountKey::UserClaim(uid).network(), None);
		assert_eq!(LedgerAccountKey::CryptoWallet(Network::Ton).network(), Some(Network::Ton));
		assert_eq!(LedgerAccountKey::BankCustody.ledger(), Ledger::UsdMock);
		assert_eq!(LedgerAccountKey::UserClaim(uid).ledger(), Ledger::Usdt);
	}

	#[test]
	fn retired_codes_are_never_derived_by_a_live_key() {
		// Reusing a retired code would let a new account wear a retired account's forensic
		// identity in TigerBeetle's history.
		for code in AccountCode::RETIRED {
			assert!(AccountCode::is_retired(code));
			assert_eq!(AccountCode::from_code(code), None, "retired code {code} reads as a live kind");
		}
		let uid = UserId::from_raw(uuid::Uuid::nil());
		let live = [
			LedgerAccountKey::CryptoWallet(Network::Bep20),
			LedgerAccountKey::BankCustody,
			LedgerAccountKey::UserClaim(uid),
			LedgerAccountKey::WithdrawalClearing,
			LedgerAccountKey::ScopeClaim(uid, ScopeId::parse("sa").unwrap()),
			LedgerAccountKey::ScopeAccount(ScopeId::main(), ScopeAccountLabel::treasury()),
			LedgerAccountKey::HoldClearing,
		];
		for key in live {
			assert!(!AccountCode::is_retired(key.account_code().code()), "{key:?} derives a retired code");
			assert!(!matches!(key.logical_key().as_str(), "fund" | "fee"), "{key:?} derives a retired logical key");
			assert!(!key.logical_key().starts_with("shares_company:"), "{key:?} derives a retired logical key");
		}
	}

	#[test]
	fn the_scope_keys_are_claims_on_the_cash_ledger() {
		let uid = UserId::from_raw(uuid::Uuid::nil());
		let compartment = LedgerAccountKey::ScopeClaim(uid, ScopeId::parse("sa").unwrap());
		assert_eq!(compartment.logical_key(), "scope_claim:sa:00000000-0000-0000-0000-000000000000");
		assert_eq!((compartment.ledger(), compartment.normal(), compartment.network()), (Ledger::Usdt, Normal::Credit, None));
		assert_eq!(compartment.account_code().code(), 21);
		assert_ne!(compartment.logical_key(), LedgerAccountKey::UserClaim(uid).logical_key());
		let treasury = Party::Treasury.claim_key();
		assert_eq!(treasury.logical_key(), "scope_account:main:treasury");
		assert_eq!((treasury.ledger(), treasury.normal()), (Ledger::Usdt, Normal::Credit));
		assert_eq!(treasury.account_code().code(), 31);
	}

	#[test]
	fn the_treasury_party_is_banking_s_scope_account() {
		assert_eq!(Party::Treasury.claim_key(), LedgerAccountKey::ScopeAccount(ScopeId::main(), ScopeAccountLabel::treasury()));
		assert_eq!(serde_json::to_string(&Party::Treasury).unwrap(), r#"{"kind":"treasury"}"#);
		assert_eq!(serde_json::from_str::<Party>(r#"{"kind":"treasury"}"#).unwrap(), Party::Treasury);
		assert!(Party::from_parts("treasury", None).is_err(), "the id column is never null for it");
		assert!(Party::from_parts("treasury", Some("fee")).is_err());
	}

	#[test]
	fn every_logical_key_parses_back_to_the_account_it_names() {
		let uid = UserId::new();
		let keys = [
			LedgerAccountKey::CryptoWallet(Network::Bep20),
			LedgerAccountKey::CryptoWallet(Network::Ton),
			LedgerAccountKey::UserClaim(uid),
			LedgerAccountKey::WithdrawalClearing,
			LedgerAccountKey::BankCustody,
			LedgerAccountKey::ScopeClaim(uid, ScopeId::parse("invest").unwrap()),
			LedgerAccountKey::ScopeAccount(ScopeId::main(), ScopeAccountLabel::treasury()),
			LedgerAccountKey::ScopeAccount(ScopeId::parse("sa").unwrap(), ScopeAccountLabel::parse("alloc:service_arb").unwrap()),
			LedgerAccountKey::HoldClearing,
		];
		for key in keys {
			let raw = key.logical_key();
			assert_eq!(LedgerAccountKey::parse_logical_key(&raw).unwrap(), key, "{raw} does not round-trip");
		}
		// A row the chart of accounts does not know is an error, never a guess.
		for foreign in [
			"",
			"fees",
			"user:",
			"vault:bep20",
			"wallet:btc",
			"scope_claim:sa",
			"scope_claim:sa:not-a-uuid",
			"scope_claim:Sa:00000000-0000-0000-0000-000000000000",
			"scope_claim:main:00000000-0000-0000-0000-000000000000",
			"scope_account:main",
			"scope_account:main:",
			"scope_account:main:Treasury",
		] {
			assert!(LedgerAccountKey::parse_logical_key(foreign).is_err(), "{foreign:?} parsed");
		}
		// Retired rows name no live account: parsing one is an error, never a live key. A
		// scan skips them by their code first.
		for retired in [
			"fund",
			"fee",
			"service:fee",
			"shares_company:service_arb",
			"shares:service_arb:00000000-0000-0000-0000-000000000000",
			"shares_outstanding:service_arb",
			"shares_fee:service_arb",
			"book_shares:service_arb:00000000-0000-0000-0000-000000000000",
			"book_cash:00000000-0000-0000-0000-000000000000",
		] {
			assert!(LedgerAccountKey::parse_logical_key(retired).is_err(), "{retired:?} parsed as a live key");
		}
	}

	#[test]
	// The retired codes are IN the sets on purpose: reserved forever, so a newcomer that
	// picks one collides here.
	fn every_account_and_transfer_code_is_unique() {
		let live_account_codes = [
			AccountCode::CryptoWallet,
			AccountCode::BankCustody,
			AccountCode::UserClaim,
			AccountCode::ScopeClaim,
			AccountCode::ScopeAccount,
			AccountCode::WithdrawalClearing,
			AccountCode::HoldClearing,
		]
		.map(AccountCode::code);
		for code in live_account_codes {
			assert_eq!(AccountCode::from_code(code).map(AccountCode::code), Some(code), "code {code} does not read back as itself");
		}
		assert_eq!(AccountCode::from_code(2), None, "a number no kind carries reads back as nothing");
		assert_unique("an account", live_account_codes.iter().copied().chain(AccountCode::RETIRED).collect());

		let live_transfer_codes = [
			TransferCode::Deposit,
			TransferCode::Withdraw,
			TransferCode::WithdrawFee,
			TransferCode::PaymentTransfer,
			TransferCode::CompartmentMove,
			TransferCode::ScopeTransfer,
			TransferCode::ScopeHold,
			TransferCode::InvoicePayment,
			TransferCode::ScopePayout,
			TransferCode::ScopeCharge,
		]
		.map(TransferCode::code);
		assert_unique("a transfer", live_transfer_codes.iter().copied().chain(TransferCode::RETIRED).collect());
	}

	fn assert_unique(kind: &str, codes: Vec<u16>) {
		let mut deduped = codes.clone();
		deduped.sort_unstable();
		deduped.dedup();
		assert_eq!(deduped.len(), codes.len(), "{kind} code is reused: {codes:?}");
	}
}
