//! `consilium` bounded context — multi-owner authorization over the platform's OWN money:
//! a payment out of a reserved allocation, a NAV mark past the move guard, a change of fee
//! terms, a new holder of the `fee`/`fund` allocations, the attribution of a treasury
//! arrival as somebody's seed of `fund` ([`ConsiliumTerms`]).
//!
//! A consilium is an **authorization artifact**, not a money move. It reserves nothing,
//! queues nothing and refunds nothing; on approval it carries its subject through the
//! ordinary path that subject already has (an order's approval, an issuance, a mark). That
//! separation is deliberate — see `docs/CONSILIUM.md` — and it is why this aggregate adds
//! no state to any money aggregate and touches no claim.
//!
//! **Quorum.** `threshold = floor(N / 2) + 1` where `N` is the owner count at open. The
//! initiator is counted in the denominator but casts no vote: were opening a request to
//! remove you from the denominator, opening one would be a way to lower the bar you have
//! to clear. Below three owners the threshold is unreachable and [`Consilium::open`]
//! refuses rather than storing a request that can never pass.
//!
//! **Rejection closes early.** Once more owners have refused than the tally can afford,
//! the threshold is arithmetically out of reach, so the consilium is rejected at once
//! rather than left to run out its clock.
//!
//! Pure and wasm-safe: ids and the payload hash are supplied by the application layer,
//! no clock, no I/O, no crypto.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{
	balance::{LedgerAccountKey, ServiceId, ValuationId},
	error::DomainError,
	fees::{FeePolicyChangeId, FeePolicySubject},
	hex32,
	issuance::UnitIssuanceId,
	money::{Network, Shares, TxRef, Usdt},
	payments::{PaymentId, PaymentSubject},
	push_field,
	subscriptions::SubscriptionId,
	users::UserId,
	withdrawals::WithdrawalId,
};

/// A unique consilium id (UUID). Minted by the application layer.
pub type ConsiliumId = Id<ConsiliumTag>;
/// Phantom tag making [`ConsiliumId`] a distinct, incompatible identity type.
pub struct ConsiliumTag;

/// The smallest roster that can ever reach the threshold. With `N = 2` the threshold is
/// 2 and there is exactly one eligible voter, so no vote can carry it.
pub const MIN_OWNERS: u32 = 3;

/// How long an unanswered consilium stays open (72h). Past it the request can never
/// execute, however late a vote arrives.
pub const TTL_SECS: i64 = 72 * 60 * 60;

/// Owners needed to carry a consilium: strictly more than half of ALL of them.
pub fn threshold(owner_count: u32) -> u32 {
	owner_count / 2 + 1
}

/// What a consilium authorizes.
///
/// THE VOCABULARY HERE AND THE `consilium.kind` CHECK ARE ONE SIZE, ALWAYS. A value the
/// database admits but [`Self::parse`] refuses does not merely fail its own row: `kind`
/// decides how `terms` is read, so one such row fails EVERY read of the governance history.
/// That is why `0029_consilium_source_claim.sql` refused to widen the CHECK ahead of time
/// and why `0031_consilium_payment_kind.sql` widens it in the commit that adds this variant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsiliumKind {
	/// A [`crate::payments::PaymentOrder`] whose source is fund-owned money — §3's rule
	/// that the owners' money moves only on the owners' quorum, at every tier.
	Payment,
	/// A NAV mark the move guard refuses (banking#232). The price every redemption settles
	/// at is fund-owned money by another name, so a move past the cap is the owners' call.
	/// `0035_consilium_valuation_override.sql` widens the CHECK in this same commit.
	ValuationOverride,
	/// A change of a product's fee TERMS that tightens them beyond the house envelope
	/// (`docs/FEES.md` § "Changing the terms"). Not a money move: carrying it schedules the
	/// change; the sweeper promotes it once the holders' notice period has run.
	FeePolicy,
	/// Units of a reserved allocation (`fee`, `fund`) minted to a person — the owners
	/// seating a new holder of the platform's own money (#245). Not a cash move, but it
	/// dilutes every existing holder pro rata, so it is theirs to decide.
	/// `0046_consilium_holder_grant.sql` widens the CHECK in this same commit.
	HolderGrant,
	/// A chain-proven arrival on the treasury booked as a named person's deposit and their
	/// subscription into `fund` (#245). The chain proves the dollar arrived, not whose it
	/// is: attributing it seats a holder of the platform's capital, so it is the owners'
	/// call rather than the first administrator's to claim it. `0046` widens the CHECK.
	SeedCapital,
}

impl ConsiliumKind {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Payment => "payment",
			Self::ValuationOverride => "valuation_override",
			Self::FeePolicy => "fee_policy",
			Self::HolderGrant => "holder_grant",
			Self::SeedCapital => "seed_capital",
		}
	}

	/// The word a refusal message calls this kind by — "a payment consilium", "a fee-policy
	/// consilium". Kept apart from [`Self::as_str`] because that one is the wire and storage
	/// spelling and must never move, while this one is prose and may.
	pub fn noun(self) -> &'static str {
		match self {
			Self::Payment => "payment",
			Self::ValuationOverride => "valuation-override",
			Self::FeePolicy => "fee-policy",
			Self::HolderGrant => "holder-grant",
			Self::SeedCapital => "seed-capital",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"payment" => Ok(Self::Payment),
			"valuation_override" => Ok(Self::ValuationOverride),
			"fee_policy" => Ok(Self::FeePolicy),
			"holder_grant" => Ok(Self::HolderGrant),
			"seed_capital" => Ok(Self::SeedCapital),
			other => Err(DomainError::Validation(format!("unknown consilium kind: {other}"))),
		}
	}
}

/// Where a consilium stands. Terminal everywhere except [`Open`](ConsiliumState::Open).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsiliumState {
	/// Collecting votes.
	Open,
	/// Quorum reached; the payout has not been created yet.
	Approved,
	/// Enough owners refused that the threshold can no longer be reached.
	Rejected,
	/// The window closed with no verdict.
	Expired,
	/// The initiator withdrew it.
	Cancelled,
	/// The payout exists.
	Executed,
	/// Approved, but the payout could not be created. Nothing retries silently.
	ExecutionFailed,
}

impl ConsiliumState {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Open => "open",
			Self::Approved => "approved",
			Self::Rejected => "rejected",
			Self::Expired => "expired",
			Self::Cancelled => "cancelled",
			Self::Executed => "executed",
			Self::ExecutionFailed => "execution_failed",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"open" => Ok(Self::Open),
			"approved" => Ok(Self::Approved),
			"rejected" => Ok(Self::Rejected),
			"expired" => Ok(Self::Expired),
			"cancelled" => Ok(Self::Cancelled),
			"executed" => Ok(Self::Executed),
			"execution_failed" => Ok(Self::ExecutionFailed),
			other => Err(DomainError::Validation(format!("unknown consilium state: {other}"))),
		}
	}

	pub fn is_open(self) -> bool {
		matches!(self, Self::Open)
	}
}

/// How one owner answered.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoteDecision {
	#[default]
	Pending,
	Approve,
	Reject,
}

impl VoteDecision {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Pending => "pending",
			Self::Approve => "approve",
			Self::Reject => "reject",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"pending" => Ok(Self::Pending),
			"approve" => Ok(Self::Approve),
			"reject" => Ok(Self::Reject),
			other => Err(DomainError::Validation(format!("unknown vote decision: {other}"))),
		}
	}
}

/// The immutable subject of a valuation-override consilium: mark `service` at `aum`, past
/// the NAV-move guard. Only the AUM is frozen — the NAV itself is derived from the LIVE
/// unit supply at execution, because units can be minted or burned during the 72h vote and
/// a NAV frozen at open would then misprice the fund the owners approved marking.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValuationOverrideTerms {
	pub service: ServiceId,
	pub aum: Usdt,
}

impl ValuationOverrideTerms {
	/// The domain-separation prefix — see [`crate::payments::PaymentTerms::DOMAIN`]. FROZEN for the
	/// same reason.
	pub const DOMAIN: &'static [u8] = b"banking.v1.ValuationOverrideTerms\x00";

	/// The bytes the payload hash is taken over: the prefix, the length-prefixed service,
	/// the AUM's base units big-endian.
	pub fn canonical_bytes(&self) -> Vec<u8> {
		let mut out = Vec::with_capacity(Self::DOMAIN.len() + 96);
		out.extend_from_slice(Self::DOMAIN);
		push_field(&mut out, self.service.as_str().as_bytes());
		out.extend_from_slice(&self.aum.base_units().to_be_bytes());
		out
	}
}

/// The immutable subject of a holder-grant consilium: mint `units` of the reserved
/// `allocation` to `user`. The units are frozen, not a cash figure — what they are worth
/// is the allocation's NAV at execution, derived from what it holds over the live supply,
/// exactly as a subscription is priced at its own moment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HolderGrantTerms {
	pub allocation: ServiceId,
	pub user: UserId,
	pub units: Shares,
}

impl HolderGrantTerms {
	/// The domain-separation prefix — see [`crate::payments::PaymentTerms::DOMAIN`]. FROZEN for the
	/// same reason.
	pub const DOMAIN: &'static [u8] = b"banking.v1.HolderGrantTerms\x00";

	/// Only a reserved allocation is granted this way: a product's units are bought with
	/// cash or issued by its operator, and the owners' quorum has no say over them. Zero
	/// units is not a grant.
	pub fn new(allocation: ServiceId, user: UserId, units: Shares) -> Result<Self, DomainError> {
		if !allocation.is_reserved() {
			return Err(DomainError::Validation(format!(
				"'{allocation}' is not a reserved allocation: a holder grant seats holders of the fee and fund allocations only"
			)));
		}
		if units.is_zero() {
			return Err(DomainError::Validation("a holder grant must mint a positive number of units".into()));
		}
		Ok(Self { allocation, user, units })
	}

	/// The bytes the payload hash is taken over: the prefix, the length-prefixed
	/// allocation and user, the units' base units big-endian.
	pub fn canonical_bytes(&self) -> Vec<u8> {
		let mut out = Vec::with_capacity(Self::DOMAIN.len() + 96);
		out.extend_from_slice(Self::DOMAIN);
		push_field(&mut out, self.allocation.as_str().as_bytes());
		push_field(&mut out, self.user.to_string().as_bytes());
		out.extend_from_slice(&self.units.base_units().to_be_bytes());
		out
	}
}

/// The immutable subject of a seed-capital consilium: the transfer `tx_ref` names on
/// `network`, which the chain must report as exactly `amount` USDT landing on the treasury
/// from outside, is `depositor`'s — booked as their deposit and subscribed into `fund` at
/// its price when the quorum executes.
///
/// The amount is frozen INTO the terms, unlike a subscription's units: the owners are
/// approving "this dollar is this person's", and a reference that turns out to name a
/// different transfer must fail the signature rather than seat the person with whatever
/// the chain says. The units are not frozen — they are what the cash buys at execution,
/// as every subscription is priced at its own moment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SeedCapitalTerms {
	pub tx_ref: TxRef,
	pub network: Network,
	pub amount: Usdt,
	pub depositor: UserId,
}

impl SeedCapitalTerms {
	/// The domain-separation prefix — see [`crate::payments::PaymentTerms::DOMAIN`]. FROZEN for the
	/// same reason.
	pub const DOMAIN: &'static [u8] = b"banking.v1.SeedCapitalTerms\x00";

	/// Zero is not a seed: the chain never reports a zero transfer, and terms the chain can
	/// never match would sit open for 72 hours before failing.
	pub fn new(tx_ref: TxRef, network: Network, amount: Usdt, depositor: UserId) -> Result<Self, DomainError> {
		if amount.is_zero() {
			return Err(DomainError::Validation("a seed must name the positive amount the chain reports".into()));
		}
		Ok(Self { tx_ref, network, amount, depositor })
	}

	/// The bytes the payload hash is taken over: the prefix, the length-prefixed reference,
	/// network and depositor, the amount's base units big-endian.
	pub fn canonical_bytes(&self) -> Vec<u8> {
		let mut out = Vec::with_capacity(Self::DOMAIN.len() + 128);
		out.extend_from_slice(Self::DOMAIN);
		push_field(&mut out, self.tx_ref.as_str().as_bytes());
		push_field(&mut out, self.network.as_str().as_bytes());
		push_field(&mut out, self.depositor.to_string().as_bytes());
		out.extend_from_slice(&self.amount.base_units().to_be_bytes());
		out
	}
}

/// What a consilium is deciding, by value.
///
/// It exists so a second governance subject is a variant here rather than a parallel
/// aggregate, and it is an ENUM rather than a `Box<dyn Terms>` because
/// [`ConsiliumEvent::Opened`] carries the terms by value into the `event_log` and must
/// therefore round-trip through serde — which a trait object cannot do.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConsiliumTerms {
	/// The payment order this quorum authorizes, subject and all. The ORDER'S id is inside
	/// the hashed subject (see [`PaymentSubject`]), so an approval of one payment is not a
	/// valid signature over another with identical terms.
	Payment(PaymentSubject),
	/// A NAV mark past the move guard — see [`ValuationOverrideTerms`].
	ValuationOverride(ValuationOverrideTerms),
	/// The fee-policy change this quorum authorizes. The CHANGE'S id is inside the hashed
	/// subject for the same reason the payment's is.
	FeePolicy(FeePolicySubject),
	/// Units of a reserved allocation for a person — see [`HolderGrantTerms`].
	HolderGrant(HolderGrantTerms),
	/// A treasury arrival attributed to a person as their seed of `fund` — see
	/// [`SeedCapitalTerms`].
	SeedCapital(SeedCapitalTerms),
}

impl ConsiliumTerms {
	pub fn kind(&self) -> ConsiliumKind {
		match self {
			Self::Payment(_) => ConsiliumKind::Payment,
			Self::ValuationOverride(_) => ConsiliumKind::ValuationOverride,
			Self::FeePolicy(_) => ConsiliumKind::FeePolicy,
			Self::HolderGrant(_) => ConsiliumKind::HolderGrant,
			Self::SeedCapital(_) => ConsiliumKind::SeedCapital,
		}
	}

	/// The bytes the payload hash is taken over.
	///
	/// EVERY VARIANT MUST CARRY ITS OWN DOMAIN-SEPARATION PREFIX, and this method delegates
	/// precisely because each one does (see [`crate::payments::PaymentTerms::DOMAIN`]).
	/// Without a per-variant prefix the same field bytes could encode under two kinds, so the
	/// digest an owner signed for one kind would be a valid signature over the other — a
	/// mark's approval spendable as a payment. `terms_of_one_kind_cannot_be_hashed_as_another`
	/// pins it.
	pub fn canonical_bytes(&self) -> Vec<u8> {
		match self {
			Self::Payment(subject) => subject.canonical_bytes(),
			Self::ValuationOverride(terms) => terms.canonical_bytes(),
			Self::FeePolicy(subject) => subject.canonical_bytes(),
			Self::HolderGrant(terms) => terms.canonical_bytes(),
			Self::SeedCapital(terms) => terms.canonical_bytes(),
		}
	}

	/// The claim this consilium spends from. Drives both the per-source "one open request"
	/// index and the advisory lock the execution path takes, so two consilia over DIFFERENT
	/// claims no longer block each other while two over the SAME claim still do.
	pub fn source_claim(&self) -> LedgerAccountKey {
		match self {
			// READ OFF THE ORDER, never restated. The per-source "one open request" index and
			// `payments_single_open_per_source_idx` then key on the same claim, so the two
			// governance surfaces over one claim serialize against each other rather than
			// each holding its own idea of what is being spent.
			Self::Payment(subject) => subject.terms.source_claim(),
			// A mark moves no cash, but it reprices every redemption the fund's claim will pay,
			// so it "spends" that claim: one open override per fund, and it queues behind (or
			// blocks) a payment out of the same `service:<id>` claim rather than racing it —
			// the owners must not be voting on the price and on a drain of the pool at once.
			Self::ValuationOverride(terms) => LedgerAccountKey::ServiceClaim(terms.service.clone()),
			// The account the new terms will collect INTO. It spends nothing, but it is the
			// one claim a fee-policy change is unambiguously about, so keying the per-source
			// "one open request" index on it yields exactly one open fee-policy consilium per
			// product — without blocking a payout or a payment over some other claim.
			Self::FeePolicy(subject) => LedgerAccountKey::FeeShares(subject.service.clone()),
			// A grant moves no cash either, but it reprices the allocation's every unit — the
			// same claim a payment out of `service:fee` spends and a redemption is paid from.
			// Keying on it yields one open grant per reserved allocation, and it queues behind
			// (or blocks) a payment out of that claim rather than racing it: the owners must
			// not be voting on who holds the money and on a drain of it at once.
			Self::HolderGrant(terms) => LedgerAccountKey::ServiceClaim(terms.allocation.clone()),
			// A seed is a subscription into `fund`: it reprices nothing, but it seats a holder
			// of that claim and adds cash to it, so it takes the same slot a grant of `fund`
			// units or a payment out of `service:fund` takes — one open seed at a time, never
			// while the owners are also deciding who holds the fund or what leaves it.
			Self::SeedCapital(_) => LedgerAccountKey::ServiceClaim(ServiceId::fund()),
		}
	}
}

impl From<PaymentSubject> for ConsiliumTerms {
	fn from(subject: PaymentSubject) -> Self {
		Self::Payment(subject)
	}
}

impl From<ValuationOverrideTerms> for ConsiliumTerms {
	fn from(terms: ValuationOverrideTerms) -> Self {
		Self::ValuationOverride(terms)
	}
}

impl From<FeePolicySubject> for ConsiliumTerms {
	fn from(subject: FeePolicySubject) -> Self {
		Self::FeePolicy(subject)
	}
}

impl From<HolderGrantTerms> for ConsiliumTerms {
	fn from(terms: HolderGrantTerms) -> Self {
		Self::HolderGrant(terms)
	}
}

impl From<SeedCapitalTerms> for ConsiliumTerms {
	fn from(terms: SeedCapitalTerms) -> Self {
		Self::SeedCapital(terms)
	}
}

/// What an executed consilium produced — an identity, never the machinery behind it.
///
/// The aggregate records WHICH artifact its approval was spent on and nothing more; how one
/// is created stays in `application::consilium::execute`, which is the only place that is
/// allowed to know that a payout is a withdrawal saga.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "effect", content = "id", rename_all = "snake_case")]
pub enum ConsiliumEffect {
	Withdrawal(WithdrawalId),
	/// The payment order this quorum carried. The order, not its money: what the payment
	/// then settles as (a withdrawal, or one posted transfer) is the order's own business.
	Payment(PaymentId),
	/// The `fund_valuations` mark a valuation-override quorum recorded.
	Valuation(ValuationId),
	/// The fee-policy change this quorum scheduled. Promoting it into the live terms once
	/// the notice period has run is the fee sweeper's business.
	FeePolicy(FeePolicyChangeId),
	/// The in-kind issuance a holder-grant quorum minted. The record, not the units: the
	/// relay posts them, as it posts every issuance.
	Issuance(UnitIssuanceId),
	/// The `fund` subscription a seed-capital quorum opened for the depositor. The
	/// subscription and not the deposit, because the deposit is keyed by the chain
	/// reference and has no id of its own, while the subscription is derived from that
	/// same reference and is the fact that seats the holder.
	Subscription(SubscriptionId),
}

impl From<WithdrawalId> for ConsiliumEffect {
	fn from(id: WithdrawalId) -> Self {
		Self::Withdrawal(id)
	}
}

impl From<PaymentId> for ConsiliumEffect {
	fn from(id: PaymentId) -> Self {
		Self::Payment(id)
	}
}

impl From<ValuationId> for ConsiliumEffect {
	fn from(id: ValuationId) -> Self {
		Self::Valuation(id)
	}
}

impl From<FeePolicyChangeId> for ConsiliumEffect {
	fn from(id: FeePolicyChangeId) -> Self {
		Self::FeePolicy(id)
	}
}

impl From<UnitIssuanceId> for ConsiliumEffect {
	fn from(id: UnitIssuanceId) -> Self {
		Self::Issuance(id)
	}
}

impl From<SubscriptionId> for ConsiliumEffect {
	fn from(id: SubscriptionId) -> Self {
		Self::Subscription(id)
	}
}

/// One recorded answer.
///
/// `counts` is the roster re-check: a vote cast by someone who has since lost their seat
/// stays on the record and stays visible, but is not counted toward quorum. Changing the
/// roster can therefore only make approval harder.
#[derive(Clone, Copy, Debug)]
pub struct ConsiliumVote {
	pub voter: UserId,
	pub decision: VoteDecision,
	pub decided_at: i64,
	pub counts: bool,
}

/// The consilium aggregate. Construct via [`Consilium::open`] (raises
/// [`ConsiliumEvent::Opened`]) or [`Consilium::rehydrate`] (load from the store, no
/// events).
#[derive(Clone, Debug)]
pub struct Consilium {
	id: ConsiliumId,
	terms: ConsiliumTerms,
	payload_hash: [u8; 32],
	initiator: UserId,
	owner_count: u32,
	threshold: u32,
	eligible: Vec<UserId>,
	votes: Vec<ConsiliumVote>,
	state: ConsiliumState,
	created_at: i64,
	expires_at: i64,
	decided_at: Option<i64>,
	executed: Option<ConsiliumEffect>,
	failure_reason: Option<String>,
	version: u64,
	pending: Vec<ConsiliumEvent>,
}

impl Consilium {
	/// Open a consilium over `terms`, snapshotting `owners` as the roster it is judged
	/// against. `payload_hash` is the SHA-256 of [`ConsiliumTerms::canonical_bytes`],
	/// taken by the application layer (the domain stays free of crypto).
	///
	/// The kind is READ OFF the terms rather than supplied: the two cannot then disagree,
	/// which is the whole reason the terms are one enum instead of a kind tag beside a
	/// struct.
	///
	/// Refuses a roster below [`MIN_OWNERS`]: with the initiator barred from voting the
	/// threshold would be unreachable, and a request that can never pass should not be
	/// stored as though it might.
	pub fn open(id: ConsiliumId, terms: ConsiliumTerms, payload_hash: [u8; 32], initiator: UserId, owners: &[UserId], created_at: i64) -> Result<Self, DomainError> {
		let mut roster: Vec<UserId> = Vec::with_capacity(owners.len());
		for owner in owners {
			if !roster.contains(owner) {
				roster.push(*owner);
			}
		}
		if !roster.contains(&initiator) {
			return Err(DomainError::Forbidden("only a fund owner may open a consilium".into()));
		}
		let owner_count = roster.len() as u32;
		if owner_count < MIN_OWNERS {
			// "needs at least", "owners" and "this fund has N" are what the wallet frontend's
			// `consilium-refusal.ts` classifies this refusal by; only the noun varies.
			let noun = terms.kind().noun();
			return Err(DomainError::Validation(format!(
				"a {noun} consilium needs at least {MIN_OWNERS} owners; this fund has {owner_count}, so the threshold can never be reached"
			)));
		}
		let eligible: Vec<UserId> = roster.into_iter().filter(|owner| *owner != initiator).collect();
		let mut consilium = Self {
			id,
			terms,
			payload_hash,
			initiator,
			owner_count,
			threshold: threshold(owner_count),
			eligible,
			votes: Vec::new(),
			state: ConsiliumState::Open,
			created_at,
			expires_at: created_at.saturating_add(TTL_SECS),
			decided_at: None,
			executed: None,
			failure_reason: None,
			version: 1,
			pending: Vec::new(),
		};
		consilium.pending.push(ConsiliumEvent::Opened {
			consilium_id: id,
			kind: consilium.kind(),
			terms: consilium.terms.clone(),
			payload_hash: hex32(&payload_hash),
			initiator,
			owner_count,
			threshold: consilium.threshold,
			expires_at: consilium.expires_at,
		});
		Ok(consilium)
	}

	/// Reconstitute from the store. Raises no events.
	#[allow(clippy::too_many_arguments)]
	pub fn rehydrate(
		id: ConsiliumId,
		terms: ConsiliumTerms,
		payload_hash: [u8; 32],
		initiator: UserId,
		owner_count: u32,
		threshold: u32,
		eligible: Vec<UserId>,
		votes: Vec<ConsiliumVote>,
		state: ConsiliumState,
		created_at: i64,
		expires_at: i64,
		decided_at: Option<i64>,
		executed: Option<ConsiliumEffect>,
		failure_reason: Option<String>,
		version: u64,
	) -> Self {
		Self {
			id,
			terms,
			payload_hash,
			initiator,
			owner_count,
			threshold,
			eligible,
			votes,
			state,
			created_at,
			expires_at,
			decided_at,
			executed,
			failure_reason,
			version,
			pending: Vec::new(),
		}
	}

	/// Record `voter`'s answer, then settle the tally — which may carry the consilium
	/// straight to its verdict.
	///
	/// One-shot: repeating the SAME decision is an idempotent no-op (a retried request
	/// must not error), while a DIFFERENT second decision is refused. A vote from someone
	/// with no seat is forbidden, which is the second line of defence behind never having
	/// minted them a token at all.
	pub fn record_vote(&mut self, voter: UserId, decision: VoteDecision, at: i64) -> Result<(), DomainError> {
		if decision == VoteDecision::Pending {
			return Err(DomainError::Validation("a vote must be approve or reject".into()));
		}
		if !self.eligible.contains(&voter) {
			return Err(DomainError::Forbidden("not an eligible voter on this consilium".into()));
		}
		if let Some(existing) = self.votes.iter().find(|vote| vote.voter == voter) {
			return if existing.decision == decision {
				Ok(())
			} else {
				Err(DomainError::Conflict("this seat has already answered; a decision cannot be changed".into()))
			};
		}
		if !self.state.is_open() {
			return Err(DomainError::Conflict(format!("consilium is {}, no longer accepting votes", self.state.as_str())));
		}
		self.votes.push(ConsiliumVote {
			voter,
			decision,
			decided_at: at,
			counts: true,
		});
		self.raise(ConsiliumEvent::VoteRecorded {
			consilium_id: self.id,
			voter,
			decision,
			decided_at: at,
		});
		self.settle_tally(at);
		Ok(())
	}

	/// Carry the consilium to its verdict if the counted votes now decide it. Approval
	/// wins the moment the threshold is met; rejection wins the moment the threshold has
	/// become arithmetically unreachable, so the owners are not left waiting out a clock
	/// on a request that has already failed.
	fn settle_tally(&mut self, at: i64) {
		if !self.state.is_open() {
			return;
		}
		if self.approvals() >= self.threshold {
			self.decide(ConsiliumState::Approved, at);
		} else if self.rejections() > (self.eligible.len() as u32).saturating_sub(self.threshold) {
			self.decide(ConsiliumState::Rejected, at);
		}
	}

	fn decide(&mut self, state: ConsiliumState, at: i64) {
		self.state = state;
		self.decided_at = Some(at);
		let event = match state {
			ConsiliumState::Approved => ConsiliumEvent::Approved {
				consilium_id: self.id,
				approvals: self.approvals(),
				threshold: self.threshold,
				decided_at: at,
			},
			ConsiliumState::Rejected => ConsiliumEvent::Rejected {
				consilium_id: self.id,
				rejections: self.rejections(),
				decided_at: at,
			},
			ConsiliumState::Cancelled => ConsiliumEvent::Cancelled {
				consilium_id: self.id,
				decided_at: at,
			},
			_ => ConsiliumEvent::Expired {
				consilium_id: self.id,
				decided_at: at,
			},
		};
		self.raise(event);
	}

	/// Quorum has been reached. Normally driven by [`Self::record_vote`]; exposed so the
	/// transition is nameable. Idempotent.
	pub fn approve(&mut self, at: i64) -> Result<(), DomainError> {
		self.close_open(ConsiliumState::Approved, at)
	}

	/// The threshold is out of reach. Idempotent.
	pub fn reject(&mut self, at: i64) -> Result<(), DomainError> {
		self.close_open(ConsiliumState::Rejected, at)
	}

	/// The initiator withdrew the request. Ownership of the request is checked by the
	/// caller; the aggregate only guarantees it is still open. Idempotent.
	pub fn cancel(&mut self, at: i64) -> Result<(), DomainError> {
		self.close_open(ConsiliumState::Cancelled, at)
	}

	/// The window closed with no verdict. Refuses to expire a consilium early, so a
	/// clock skew cannot cut a vote short. Idempotent.
	pub fn expire(&mut self, at: i64) -> Result<(), DomainError> {
		if self.state.is_open() && at < self.expires_at {
			return Err(DomainError::Conflict("consilium has not expired yet".into()));
		}
		self.close_open(ConsiliumState::Expired, at)
	}

	/// The shared body of the four verdicts reachable from `open`: idempotent on repeat,
	/// a conflict from any other state.
	fn close_open(&mut self, target: ConsiliumState, at: i64) -> Result<(), DomainError> {
		if self.state == target {
			return Ok(());
		}
		if !self.state.is_open() {
			return Err(DomainError::Conflict(format!("consilium is {}, not {}", self.state.as_str(), target.as_str())));
		}
		self.decide(target, at);
		Ok(())
	}

	/// The approved request's effect now exists. Written exactly once — a repeat naming the
	/// SAME effect is the idempotent retry an at-least-once execution path depends on, and
	/// one naming a different effect is a conflict rather than a silent overwrite.
	pub fn mark_executed(&mut self, effect: ConsiliumEffect, at: i64) -> Result<(), DomainError> {
		if self.state == ConsiliumState::Executed {
			return if self.executed == Some(effect) {
				Ok(())
			} else {
				Err(DomainError::Conflict("consilium already executed a different payout".into()))
			};
		}
		if self.state != ConsiliumState::Approved {
			return Err(DomainError::Conflict(format!("consilium is {}, not executable", self.state.as_str())));
		}
		self.state = ConsiliumState::Executed;
		self.executed = Some(effect);
		self.raise(ConsiliumEvent::Executed { consilium_id: self.id, effect, at });
		Ok(())
	}

	/// The approved payout could not be created. Terminal: nothing retries silently, and
	/// the reason is what an owner reads to understand why.
	pub fn mark_execution_failed(&mut self, reason: String, at: i64) -> Result<(), DomainError> {
		if self.state == ConsiliumState::ExecutionFailed {
			return Ok(());
		}
		if self.state != ConsiliumState::Approved {
			return Err(DomainError::Conflict(format!("consilium is {}, not executable", self.state.as_str())));
		}
		self.state = ConsiliumState::ExecutionFailed;
		self.failure_reason = Some(reason.clone());
		self.raise(ConsiliumEvent::ExecutionFailed { consilium_id: self.id, reason, at });
		Ok(())
	}

	fn raise(&mut self, event: ConsiliumEvent) {
		self.version = self.version.saturating_add(1);
		self.pending.push(event);
	}

	/// Approvals that still count — cast by someone who is still an owner.
	pub fn approvals(&self) -> u32 {
		self.counted(VoteDecision::Approve)
	}

	/// Rejections that still count.
	pub fn rejections(&self) -> u32 {
		self.counted(VoteDecision::Reject)
	}

	fn counted(&self, decision: VoteDecision) -> u32 {
		self.votes.iter().filter(|vote| vote.counts && vote.decision == decision).count() as u32
	}

	pub fn id(&self) -> ConsiliumId {
		self.id
	}

	pub fn kind(&self) -> ConsiliumKind {
		self.terms.kind()
	}

	pub fn terms(&self) -> &ConsiliumTerms {
		&self.terms
	}

	/// The claim this request spends from — see [`ConsiliumTerms::source_claim`].
	pub fn source_claim(&self) -> LedgerAccountKey {
		self.terms.source_claim()
	}

	pub fn payload_hash(&self) -> [u8; 32] {
		self.payload_hash
	}

	/// The hash as the mail and the wire show it.
	pub fn payload_hash_hex(&self) -> String {
		hex32(&self.payload_hash)
	}

	pub fn initiator(&self) -> UserId {
		self.initiator
	}

	pub fn owner_count(&self) -> u32 {
		self.owner_count
	}

	pub fn threshold(&self) -> u32 {
		self.threshold
	}

	/// The seats entitled to vote — the roster at open, minus the initiator.
	pub fn eligible(&self) -> &[UserId] {
		&self.eligible
	}

	pub fn votes(&self) -> &[ConsiliumVote] {
		&self.votes
	}

	pub fn state(&self) -> ConsiliumState {
		self.state
	}

	pub fn created_at(&self) -> i64 {
		self.created_at
	}

	pub fn expires_at(&self) -> i64 {
		self.expires_at
	}

	pub fn decided_at(&self) -> Option<i64> {
		self.decided_at
	}

	/// What this consilium's approval was spent on, if it has executed.
	pub fn effect(&self) -> Option<ConsiliumEffect> {
		self.executed
	}

	/// The executed effect NARROWED to a withdrawal — the projection the `consilium`
	/// table's `executed_withdrawal_id` column and the read model both want. A future
	/// non-withdrawal effect leaves that column NULL, which is why this returns an
	/// `Option` over the variant rather than over the effect.
	pub fn executed_withdrawal_id(&self) -> Option<WithdrawalId> {
		let effect = self.executed?;
		// Exhaustive on purpose: a second effect must not silently fall through to `None` and
		// leave the column blank on a row that did execute.
		match effect {
			ConsiliumEffect::Withdrawal(id) => Some(id),
			ConsiliumEffect::Payment(_) | ConsiliumEffect::Valuation(_) | ConsiliumEffect::FeePolicy(_) | ConsiliumEffect::Issuance(_) | ConsiliumEffect::Subscription(_) => None,
		}
	}

	/// The executed effect NARROWED to a payment order — the `executed_payment_id` column's
	/// projection, and another leg of `consilium_execution_is_recorded`'s
	/// `num_nonnulls(...) = 1`: exactly one of the six accessors answers on an executed row.
	pub fn executed_payment_id(&self) -> Option<PaymentId> {
		match self.executed? {
			ConsiliumEffect::Payment(id) => Some(id),
			ConsiliumEffect::Withdrawal(_) | ConsiliumEffect::Valuation(_) | ConsiliumEffect::FeePolicy(_) | ConsiliumEffect::Issuance(_) | ConsiliumEffect::Subscription(_) => None,
		}
	}

	/// The executed effect NARROWED to a valuation mark — the `executed_valuation_id`
	/// column's projection, the third leg of the same `num_nonnulls(...) = 1`.
	pub fn executed_valuation_id(&self) -> Option<ValuationId> {
		match self.executed? {
			ConsiliumEffect::Valuation(id) => Some(id),
			ConsiliumEffect::Withdrawal(_) | ConsiliumEffect::Payment(_) | ConsiliumEffect::FeePolicy(_) | ConsiliumEffect::Issuance(_) | ConsiliumEffect::Subscription(_) => None,
		}
	}

	/// The executed effect NARROWED to a fee-policy change — the
	/// `executed_fee_policy_change_id` column's projection, the fourth leg.
	pub fn executed_fee_policy_change_id(&self) -> Option<FeePolicyChangeId> {
		match self.executed? {
			ConsiliumEffect::FeePolicy(id) => Some(id),
			ConsiliumEffect::Withdrawal(_) | ConsiliumEffect::Payment(_) | ConsiliumEffect::Valuation(_) | ConsiliumEffect::Issuance(_) | ConsiliumEffect::Subscription(_) => None,
		}
	}

	/// The executed effect NARROWED to an in-kind issuance — the `executed_issuance_id`
	/// column's projection, the fifth leg.
	pub fn executed_issuance_id(&self) -> Option<UnitIssuanceId> {
		match self.executed? {
			ConsiliumEffect::Issuance(id) => Some(id),
			ConsiliumEffect::Withdrawal(_) | ConsiliumEffect::Payment(_) | ConsiliumEffect::Valuation(_) | ConsiliumEffect::FeePolicy(_) | ConsiliumEffect::Subscription(_) => None,
		}
	}

	/// The executed effect NARROWED to a `fund` subscription — the `executed_subscription_id`
	/// column's projection, the sixth leg.
	pub fn executed_subscription_id(&self) -> Option<SubscriptionId> {
		match self.executed? {
			ConsiliumEffect::Subscription(id) => Some(id),
			ConsiliumEffect::Withdrawal(_) | ConsiliumEffect::Payment(_) | ConsiliumEffect::Valuation(_) | ConsiliumEffect::FeePolicy(_) | ConsiliumEffect::Issuance(_) => None,
		}
	}

	pub fn failure_reason(&self) -> Option<&str> {
		self.failure_reason.as_deref()
	}

	pub fn version(&self) -> u64 {
		self.version
	}
}

/// Facts raised by the [`Consilium`] aggregate. Audit-only — a consilium moves no money,
/// so these land in `event_log` and never in the `outbox` (the relay has no ledger op for
/// the kind and would park the row).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConsiliumEvent {
	Opened {
		consilium_id: ConsiliumId,
		kind: ConsiliumKind,
		terms: ConsiliumTerms,
		payload_hash: String,
		initiator: UserId,
		owner_count: u32,
		threshold: u32,
		expires_at: i64,
	},
	VoteRecorded {
		consilium_id: ConsiliumId,
		voter: UserId,
		decision: VoteDecision,
		decided_at: i64,
	},
	Approved {
		consilium_id: ConsiliumId,
		approvals: u32,
		threshold: u32,
		decided_at: i64,
	},
	Rejected {
		consilium_id: ConsiliumId,
		rejections: u32,
		decided_at: i64,
	},
	Expired {
		consilium_id: ConsiliumId,
		decided_at: i64,
	},
	Cancelled {
		consilium_id: ConsiliumId,
		decided_at: i64,
	},
	Executed {
		consilium_id: ConsiliumId,
		effect: ConsiliumEffect,
		at: i64,
	},
	ExecutionFailed {
		consilium_id: ConsiliumId,
		reason: String,
		at: i64,
	},
}

impl DomainEvent for ConsiliumEvent {
	const KIND: &'static str = "consilium";
}

impl Entity for Consilium {
	type Id = ConsiliumId;

	fn id(&self) -> ConsiliumId {
		self.id
	}
}

impl AggregateRoot for Consilium {
	const NAME: &'static str = "consilium";
}

impl EmitsEvents for Consilium {
	type Event = ConsiliumEvent;

	fn drain_events(&mut self) -> Vec<ConsiliumEvent> {
		core::mem::take(&mut self.pending)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const NOW: i64 = 1_700_000_000;

	fn owners(n: usize) -> Vec<UserId> {
		(0..n).map(|_| UserId::new()).collect()
	}

	fn terms() -> ConsiliumTerms {
		ConsiliumTerms::Payment(payment_subject())
	}

	fn payout(id: WithdrawalId) -> ConsiliumEffect {
		ConsiliumEffect::Withdrawal(id)
	}

	fn opened(roster: &[UserId]) -> Consilium {
		let mut c = Consilium::open(ConsiliumId::new(), terms(), [7u8; 32], roster[0], roster, NOW).unwrap();
		c.drain_events();
		c
	}

	#[test]
	fn threshold_is_strictly_more_than_half_for_every_roster() {
		// The quorum table in docs/CONSILIUM.md, asserted rather than described. `voters`
		// is N-1 because the initiator is in the denominator but casts no vote.
		let expected: [(u32, u32, u32); 7] = [
			// (N, threshold, voters)
			(2, 2, 1),
			(3, 2, 2),
			(4, 3, 3),
			(5, 3, 4),
			(6, 4, 5),
			(7, 4, 6),
			(8, 5, 7),
		];
		for (n, want_threshold, want_voters) in expected {
			assert_eq!(threshold(n), want_threshold, "threshold for N={n}");
			assert_eq!(n - 1, want_voters, "voters for N={n}");
			assert!(2 * want_threshold > n, "N={n}: the threshold must be strictly more than half");
		}
		// N=2 is the row that cannot work: one voter, two needed.
		assert!(threshold(2) > 2 - 1, "with two owners the single voter can never reach the threshold");
	}

	#[test]
	fn a_roster_below_three_is_refused_at_open() {
		for n in [1usize, 2] {
			let roster = owners(n);
			let err = Consilium::open(ConsiliumId::new(), terms(), [0u8; 32], roster[0], &roster, NOW).unwrap_err();
			assert!(matches!(err, DomainError::Validation(_)), "N={n} must be refused, not stored");
		}
		// One wording is pinned byte-for-byte, and the classifier's substrings must survive
		// every kind (banking#250).
		let roster = owners(2);
		let refusal = |kind_terms: ConsiliumTerms| -> String {
			match Consilium::open(ConsiliumId::new(), kind_terms, [0u8; 32], roster[0], &roster, NOW).unwrap_err() {
				DomainError::Validation(message) => message,
				other => panic!("expected Validation, got {other:?}"),
			}
		};
		assert_eq!(
			refusal(terms()),
			"a payment consilium needs at least 3 owners; this fund has 2, so the threshold can never be reached"
		);
		let by_kind: [(ConsiliumTerms, &str); 5] = [
			(ConsiliumTerms::Payment(payment_subject()), "a payment consilium"),
			(ConsiliumTerms::ValuationOverride(valuation_override("16250")), "a valuation-override consilium"),
			(ConsiliumTerms::FeePolicy(fee_policy_subject()), "a fee-policy consilium"),
			(ConsiliumTerms::HolderGrant(holder_grant("1000")), "a holder-grant consilium"),
			(ConsiliumTerms::SeedCapital(seed_capital("250")), "a seed-capital consilium"),
		];
		for (kind_terms, want_noun) in by_kind {
			let kind = kind_terms.kind();
			let message = refusal(kind_terms);
			assert!(message.contains(want_noun), "{kind:?}: {message}");
			assert!(message.contains("needs at least") && message.contains("owners"), "{kind:?}: {message}");
			assert!(message.contains("this fund has 2"), "{kind:?}: {message}");
		}
		// N=3 is the smallest roster that CAN open — pinned here so the refusal above is
		// known to be about the roster size and not about some unrelated gate. (This line
		// previously called `owners(3)` twice, so the initiator was not in the roster it was
		// checked against; it passed on `Forbidden` while claiming to prove N=3 was refused,
		// which is the opposite of the policy.)
		let roster = owners(MIN_OWNERS as usize);
		assert!(
			Consilium::open(ConsiliumId::new(), terms(), [0u8; 32], roster[0], &roster, NOW).is_ok(),
			"N={MIN_OWNERS} is the smallest roster that can reach a threshold"
		);
	}

	#[test]
	fn only_an_owner_may_open() {
		let roster = owners(4);
		let outsider = UserId::new();
		let err = Consilium::open(ConsiliumId::new(), terms(), [0u8; 32], outsider, &roster, NOW).unwrap_err();
		assert!(matches!(err, DomainError::Forbidden(_)));
	}

	#[test]
	fn the_initiator_is_never_an_eligible_voter() {
		let roster = owners(5);
		let c = opened(&roster);
		assert_eq!(c.owner_count(), 5);
		assert_eq!(c.threshold(), 3);
		assert_eq!(c.eligible().len(), 4);
		assert!(!c.eligible().contains(&roster[0]), "the initiator holds no seat");
	}

	#[test]
	fn five_owners_need_three_of_four_voters() {
		let roster = owners(5);
		let mut c = opened(&roster);
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		c.record_vote(roster[2], VoteDecision::Approve, NOW).unwrap();
		assert_eq!(c.state(), ConsiliumState::Open, "two of four is short of the threshold");
		c.record_vote(roster[3], VoteDecision::Approve, NOW).unwrap();
		assert_eq!(c.state(), ConsiliumState::Approved);
		assert_eq!(c.approvals(), 3);
	}

	#[test]
	fn four_owners_need_all_three_voters() {
		let roster = owners(4);
		let mut c = opened(&roster);
		assert_eq!(c.threshold(), 3);
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		c.record_vote(roster[2], VoteDecision::Approve, NOW).unwrap();
		assert_eq!(c.state(), ConsiliumState::Open);
		c.record_vote(roster[3], VoteDecision::Approve, NOW).unwrap();
		assert_eq!(c.state(), ConsiliumState::Approved, "unanimity among the three peers");
	}

	#[test]
	fn three_owners_need_both_peers() {
		let roster = owners(3);
		let mut c = opened(&roster);
		assert_eq!(c.threshold(), 2);
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		assert_eq!(c.state(), ConsiliumState::Open);
		c.record_vote(roster[2], VoteDecision::Approve, NOW).unwrap();
		assert_eq!(c.state(), ConsiliumState::Approved);
	}

	#[test]
	fn rejection_closes_the_moment_the_threshold_is_unreachable() {
		// N=5: 4 voters, threshold 3, so the tally can afford exactly one refusal.
		let roster = owners(5);
		let mut c = opened(&roster);
		c.record_vote(roster[1], VoteDecision::Reject, NOW).unwrap();
		assert_eq!(c.state(), ConsiliumState::Open, "three of the remaining four could still carry it");
		c.record_vote(roster[2], VoteDecision::Reject, NOW).unwrap();
		assert_eq!(c.state(), ConsiliumState::Rejected, "only two voters remain and three are needed");

		// N=3 and N=4 need unanimity, so a single refusal ends it immediately.
		for n in [3usize, 4] {
			let roster = owners(n);
			let mut c = opened(&roster);
			c.record_vote(roster[1], VoteDecision::Reject, NOW).unwrap();
			assert_eq!(c.state(), ConsiliumState::Rejected, "N={n} needs every peer");
		}
	}

	#[test]
	fn a_vote_is_one_shot_and_idempotent_on_repeat() {
		let roster = owners(5);
		let mut c = opened(&roster);
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		c.drain_events();
		// The same answer again changes nothing and raises nothing.
		c.record_vote(roster[1], VoteDecision::Approve, NOW + 1).unwrap();
		assert!(c.drain_events().is_empty());
		assert_eq!(c.approvals(), 1);
		// A different answer is refused outright.
		let err = c.record_vote(roster[1], VoteDecision::Reject, NOW + 2).unwrap_err();
		assert!(matches!(err, DomainError::Conflict(_)));
		assert_eq!(c.rejections(), 0);
	}

	#[test]
	fn a_non_voter_is_refused_and_pending_is_not_a_vote() {
		let roster = owners(4);
		let mut c = opened(&roster);
		// The initiator holds no seat, so their own vote has nowhere to land.
		assert!(matches!(c.record_vote(roster[0], VoteDecision::Approve, NOW), Err(DomainError::Forbidden(_))));
		assert!(matches!(c.record_vote(UserId::new(), VoteDecision::Approve, NOW), Err(DomainError::Forbidden(_))));
		assert!(matches!(c.record_vote(roster[1], VoteDecision::Pending, NOW), Err(DomainError::Validation(_))));
	}

	#[test]
	fn a_voter_who_lost_their_seat_stops_counting() {
		let roster = owners(5);
		let mut c = opened(&roster);
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		c.record_vote(roster[2], VoteDecision::Approve, NOW).unwrap();
		assert_eq!(c.approvals(), 2);
		// Rehydrating with that seat voided — what the repository does when the live
		// roster no longer lists them — takes the tally back below the threshold.
		let votes = c
			.votes()
			.iter()
			.map(|vote| ConsiliumVote {
				counts: vote.voter != roster[1],
				..*vote
			})
			.collect();
		let reloaded = Consilium::rehydrate(
			c.id(),
			c.terms().clone(),
			c.payload_hash(),
			c.initiator(),
			c.owner_count(),
			c.threshold(),
			c.eligible().to_vec(),
			votes,
			c.state(),
			c.created_at(),
			c.expires_at(),
			c.decided_at(),
			c.effect(),
			None,
			c.version(),
		);
		assert_eq!(reloaded.approvals(), 1, "a vote from someone with no seat does not count");
	}

	#[test]
	fn cancel_expire_and_the_verdicts_are_idempotent_and_mutually_exclusive() {
		let roster = owners(4);

		let mut cancelled = opened(&roster);
		cancelled.cancel(NOW).unwrap();
		assert_eq!(cancelled.state(), ConsiliumState::Cancelled);
		cancelled.drain_events();
		cancelled.cancel(NOW + 1).unwrap();
		assert!(cancelled.drain_events().is_empty(), "a repeat cancel raises nothing");
		assert!(cancelled.expire(NOW + TTL_SECS + 1).is_err());
		assert!(cancelled.record_vote(roster[1], VoteDecision::Approve, NOW).is_err());

		let mut expired = opened(&roster);
		// Refuses to cut the window short.
		assert!(matches!(expired.expire(NOW + 10), Err(DomainError::Conflict(_))));
		expired.expire(NOW + TTL_SECS).unwrap();
		assert_eq!(expired.state(), ConsiliumState::Expired);
		expired.expire(NOW + TTL_SECS + 99).unwrap();
		assert!(expired.cancel(NOW).is_err());
	}

	#[test]
	fn an_expired_consilium_can_never_execute() {
		let roster = owners(3);
		let mut c = opened(&roster);
		c.expire(NOW + TTL_SECS).unwrap();
		// Execution is reachable only from `approved`, and expiry is reachable only from
		// `open` — so no ordering of the two can produce an executed expired consilium.
		assert!(matches!(c.mark_executed(payout(WithdrawalId::new()), NOW + TTL_SECS + 1), Err(DomainError::Conflict(_))));
		assert!(c.mark_execution_failed("late".into(), NOW).is_err());
	}

	#[test]
	fn execution_is_recorded_once_and_retries_are_no_ops() {
		let roster = owners(3);
		let mut c = opened(&roster);
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		c.record_vote(roster[2], VoteDecision::Approve, NOW).unwrap();
		assert_eq!(c.state(), ConsiliumState::Approved);
		let withdrawal = WithdrawalId::new();
		c.mark_executed(payout(withdrawal), NOW).unwrap();
		c.drain_events();
		// The deterministic id makes a retry name the same payout, so it is a no-op.
		c.mark_executed(payout(withdrawal), NOW + 5).unwrap();
		assert!(c.drain_events().is_empty());
		assert_eq!(c.effect(), Some(ConsiliumEffect::Withdrawal(withdrawal)));
		assert_eq!(c.executed_withdrawal_id(), Some(withdrawal));
		// A different payout is a conflict, never a silent overwrite.
		assert!(matches!(c.mark_executed(payout(WithdrawalId::new()), NOW + 6), Err(DomainError::Conflict(_))));
		// And a failure can no longer be recorded over a completed execution.
		assert!(c.mark_execution_failed("too late".into(), NOW + 7).is_err());
	}

	#[test]
	fn execution_failure_is_terminal_and_states_why() {
		let roster = owners(3);
		let mut c = opened(&roster);
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		c.record_vote(roster[2], VoteDecision::Approve, NOW).unwrap();
		c.mark_execution_failed("payout exceeds the fund's available revenue".into(), NOW).unwrap();
		assert_eq!(c.state(), ConsiliumState::ExecutionFailed);
		assert_eq!(c.failure_reason(), Some("payout exceeds the fund's available revenue"));
		c.drain_events();
		c.mark_execution_failed("again".into(), NOW + 1).unwrap();
		assert!(c.drain_events().is_empty());
		// Nothing retries silently: an execution failure cannot become an execution.
		assert!(c.mark_executed(payout(WithdrawalId::new()), NOW + 2).is_err());
	}

	#[test]
	fn the_version_moves_on_every_fact() {
		let roster = owners(3);
		let mut c = opened(&roster);
		let opened_at = c.version();
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		assert!(c.version() > opened_at);
		let after_vote = c.version();
		c.record_vote(roster[2], VoteDecision::Approve, NOW).unwrap();
		// The carrying vote raises two facts: the vote and the verdict.
		assert_eq!(c.version(), after_vote + 2);
	}

	#[test]
	fn terms_of_one_kind_cannot_be_hashed_as_another() {
		// THE PREFIX IS THE WHOLE OF THE SEPARATION. Without one, a second kind whose fields
		// happened to encode to the same bytes would produce the same digest — and an owner's
		// approval of a payment would be a valid signature over that other request.
		let subject = payment_subject();
		assert!(
			subject.canonical_bytes().starts_with(crate::payments::PaymentSubject::DOMAIN),
			"every variant's encoding must open with its own domain prefix"
		);
		let payment = ConsiliumTerms::Payment(subject).canonical_bytes();
		// FROZEN BYTES. Live rows carry a `payload_hash` taken over an encoding that starts
		// exactly here; a changed prefix invalidates every approval in flight at once.
		// A mark's encoding opens with its own frozen prefix and cannot collide with a
		// payment's however their fields line up.
		let mark = valuation_override("250");
		assert!(mark.canonical_bytes().starts_with(ValuationOverrideTerms::DOMAIN));
		assert_eq!(ValuationOverrideTerms::DOMAIN, b"banking.v1.ValuationOverrideTerms\x00");
		assert_ne!(ConsiliumTerms::ValuationOverride(mark).canonical_bytes(), payment);
		// And a grant of units opens with its own frozen prefix too.
		let grant = holder_grant("1000");
		assert!(grant.canonical_bytes().starts_with(HolderGrantTerms::DOMAIN));
		assert_eq!(HolderGrantTerms::DOMAIN, b"banking.v1.HolderGrantTerms\x00");
		assert_ne!(ConsiliumTerms::HolderGrant(grant.clone()).canonical_bytes(), payment);
		assert_ne!(
			ConsiliumTerms::HolderGrant(grant).canonical_bytes(),
			ConsiliumTerms::ValuationOverride(valuation_override("250")).canonical_bytes()
		);
	}

	/// Units of the `fee` allocation for one person.
	fn holder_grant(units: &str) -> HolderGrantTerms {
		HolderGrantTerms::new(ServiceId::fee(), UserId::from_raw(uuid::Uuid::from_u128(7)), Shares::parse_decimal(units).unwrap()).unwrap()
	}

	fn seed_capital(amount: &str) -> SeedCapitalTerms {
		SeedCapitalTerms::new(
			TxRef::parse("0xabc").unwrap(),
			Network::Bep20,
			Usdt::parse_decimal(amount).unwrap(),
			UserId::from_raw(uuid::Uuid::from_u128(7)),
		)
		.unwrap()
	}

	#[test]
	fn a_seed_binds_the_reference_the_amount_and_the_person_and_spends_the_funds_claim() {
		let terms = ConsiliumTerms::SeedCapital(seed_capital("250"));
		assert_eq!(terms.kind(), ConsiliumKind::SeedCapital);
		assert_eq!(ConsiliumKind::SeedCapital.as_str(), "seed_capital");
		assert_eq!(ConsiliumKind::parse("seed_capital").unwrap(), ConsiliumKind::SeedCapital);
		// Its own frozen prefix, so a seed approval is a signature over nothing else.
		assert!(terms.canonical_bytes().starts_with(SeedCapitalTerms::DOMAIN));
		assert_eq!(SeedCapitalTerms::DOMAIN, b"banking.v1.SeedCapitalTerms\x00");
		assert_ne!(terms.canonical_bytes(), ConsiliumTerms::HolderGrant(holder_grant("250")).canonical_bytes());
		// Every field is under the signature: another reference, rail, person or amount is
		// another request.
		let base = seed_capital("250");
		let variants = [
			SeedCapitalTerms {
				tx_ref: TxRef::parse("0xabd").unwrap(),
				..base.clone()
			},
			SeedCapitalTerms {
				network: Network::Trc20,
				..base.clone()
			},
			SeedCapitalTerms {
				depositor: UserId::from_raw(uuid::Uuid::from_u128(8)),
				..base.clone()
			},
			seed_capital("250.000001"),
		];
		for other in variants {
			assert_ne!(base.canonical_bytes(), other.canonical_bytes(), "{other:?}");
		}
		assert_eq!(base.canonical_bytes(), seed_capital("250").canonical_bytes());
		// Zero is refused at construction, not after a vote.
		assert!(SeedCapitalTerms::new(TxRef::parse("0xabc").unwrap(), Network::Bep20, Usdt::ZERO, UserId::from_raw(uuid::Uuid::from_u128(7))).is_err());
		// One open seed per platform, in the fund allocation's slot.
		assert_eq!(terms.source_claim(), LedgerAccountKey::ServiceClaim(ServiceId::fund()));
		// The effect narrows to the subscription column and to no other.
		let roster = owners(3);
		let mut c = Consilium::open(ConsiliumId::new(), terms, [2u8; 32], roster[0], &roster, NOW).unwrap();
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		c.record_vote(roster[2], VoteDecision::Approve, NOW).unwrap();
		let subscription = SubscriptionId::new();
		c.mark_executed(ConsiliumEffect::Subscription(subscription), NOW).unwrap();
		assert_eq!(c.executed_subscription_id(), Some(subscription));
		assert!(c.executed_issuance_id().is_none() && c.executed_withdrawal_id().is_none() && c.executed_payment_id().is_none());
		assert!(c.executed_valuation_id().is_none() && c.executed_fee_policy_change_id().is_none());
	}

	#[test]
	fn a_holder_grant_names_a_reserved_allocation_only_and_spends_its_claim() {
		let terms = ConsiliumTerms::HolderGrant(holder_grant("1000"));
		assert_eq!(terms.kind(), ConsiliumKind::HolderGrant);
		assert_eq!(ConsiliumKind::HolderGrant.as_str(), "holder_grant");
		assert_eq!(ConsiliumKind::parse("holder_grant").unwrap(), ConsiliumKind::HolderGrant);
		// One open grant per reserved allocation, serialized against a payment out of it.
		assert_eq!(terms.source_claim(), LedgerAccountKey::ServiceClaim(ServiceId::fee()));
		// Every field is in the digest: a different grantee, allocation or size is a
		// different signature.
		let other_user = HolderGrantTerms::new(ServiceId::fee(), UserId::from_raw(uuid::Uuid::from_u128(8)), Shares::parse_decimal("1000").unwrap()).unwrap();
		let other_allocation = HolderGrantTerms::new(ServiceId::fund(), UserId::from_raw(uuid::Uuid::from_u128(7)), Shares::parse_decimal("1000").unwrap()).unwrap();
		assert_ne!(terms.canonical_bytes(), ConsiliumTerms::HolderGrant(other_user).canonical_bytes());
		assert_ne!(terms.canonical_bytes(), ConsiliumTerms::HolderGrant(other_allocation).canonical_bytes());
		assert_ne!(terms.canonical_bytes(), ConsiliumTerms::HolderGrant(holder_grant("1001")).canonical_bytes());
		// A product is not granted; nor is nothing.
		let user = UserId::from_raw(uuid::Uuid::from_u128(7));
		assert!(HolderGrantTerms::new(ServiceId::parse("svc-arb").unwrap(), user, Shares::parse_decimal("1").unwrap()).is_err());
		assert!(HolderGrantTerms::new(ServiceId::fee(), user, Shares::ZERO).is_err());

		// Its effect is an issuance, and only the issuance column answers for it.
		let roster = owners(3);
		let mut c = Consilium::open(ConsiliumId::new(), terms, [9u8; 32], roster[0], &roster, NOW).unwrap();
		c.record_vote(roster[1], VoteDecision::Approve, NOW + 1).unwrap();
		c.record_vote(roster[2], VoteDecision::Approve, NOW + 2).unwrap();
		let issuance = UnitIssuanceId::new();
		c.mark_executed(ConsiliumEffect::Issuance(issuance), NOW + 3).unwrap();
		assert_eq!(c.executed_issuance_id(), Some(issuance));
		assert_eq!(c.executed_withdrawal_id(), None);
		assert_eq!(c.executed_payment_id(), None);
		assert_eq!(c.executed_valuation_id(), None);
		assert_eq!(c.executed_fee_policy_change_id(), None);
	}

	/// A NAV mark past the move guard on one fund.
	fn valuation_override(aum: &str) -> ValuationOverrideTerms {
		ValuationOverrideTerms {
			service: ServiceId::parse("svc-arb").unwrap(),
			aum: Usdt::parse_decimal(aum).unwrap(),
		}
	}

	#[test]
	fn a_valuation_override_hashes_both_fields_and_spends_the_funds_claim() {
		let terms = ConsiliumTerms::ValuationOverride(valuation_override("16250"));
		assert_eq!(terms.kind(), ConsiliumKind::ValuationOverride);
		// The AUM is what the owners approve, so editing it invalidates every approval; so
		// does pointing the same AUM at another fund.
		assert_ne!(valuation_override("16250").canonical_bytes(), valuation_override("16251").canonical_bytes());
		let other_fund = ValuationOverrideTerms {
			service: ServiceId::parse("svc-other").unwrap(),
			aum: Usdt::parse_decimal("16250").unwrap(),
		};
		assert_ne!(valuation_override("16250").canonical_bytes(), other_fund.canonical_bytes());
		assert_eq!(valuation_override("16250").canonical_bytes(), valuation_override("16250").canonical_bytes());
		// One open override per fund, serialized against a payment out of that fund's claim.
		assert_eq!(terms.source_claim(), LedgerAccountKey::ServiceClaim(ServiceId::parse("svc-arb").unwrap()));
		// The effect narrows to the valuation column and to no other.
		let roster = owners(3);
		let mut c = Consilium::open(ConsiliumId::new(), terms, [1u8; 32], roster[0], &roster, NOW).unwrap();
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		c.record_vote(roster[2], VoteDecision::Approve, NOW).unwrap();
		let mark = ValuationId::new();
		c.mark_executed(ConsiliumEffect::Valuation(mark), NOW).unwrap();
		assert_eq!(c.executed_valuation_id(), Some(mark));
		assert!(c.executed_withdrawal_id().is_none() && c.executed_payment_id().is_none());
	}

	/// A payment subject over the fund allocation's capital — the §3 case that needs a quorum.
	fn payment_subject() -> crate::payments::PaymentSubject {
		use crate::payments::{PaymentDestination, PaymentReason, PaymentSubject, PaymentTerms};
		PaymentSubject {
			payment_id: crate::payments::PaymentId::from_raw(uuid::Uuid::from_u128(0x9e17)),
			terms: PaymentTerms::new(
				crate::balance::Party::Service(crate::balance::ServiceId::fund()),
				PaymentDestination::Internal(crate::balance::Party::Service(crate::balance::ServiceId::fee())),
				Usdt::parse_decimal("250").unwrap(),
				PaymentReason::new("settle the quarterly management fee").unwrap(),
			)
			.unwrap(),
		}
	}

	#[test]
	fn a_payment_consilium_spends_the_orders_claim_and_names_its_own_kind() {
		let terms = ConsiliumTerms::Payment(payment_subject());
		assert_eq!(terms.kind(), ConsiliumKind::Payment);
		// The per-source "one open request" index keys on this, so a payment out of the fund's
		// pooled capital queues behind another payment that spends the same claim and behind
		// nothing else.
		assert_eq!(terms.source_claim(), LedgerAccountKey::ServiceClaim(crate::balance::ServiceId::fund()));
	}

	/// A fee-policy subject tightening the house terms past the envelope — the case that
	/// needs a quorum.
	fn fee_policy_subject() -> crate::fees::FeePolicySubject {
		use crate::fees::{CrystallizationPeriod, FeePolicy, FeePolicyChangeId, ManagementBasis};
		crate::fees::FeePolicySubject {
			change_id: FeePolicyChangeId::from_raw(uuid::Uuid::from_u128(0x233)),
			service: crate::balance::ServiceId::parse("trading").unwrap(),
			from: Some(FeePolicy::HOUSE),
			to: FeePolicy::new(300, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual).unwrap(),
			reason: "the new mandate costs more to run".to_owned(),
			requested_effective_from: 0,
		}
	}

	#[test]
	fn a_fee_policy_consilium_is_keyed_on_the_products_fee_shares_and_names_its_own_kind() {
		let subject = fee_policy_subject();
		let terms = ConsiliumTerms::FeePolicy(subject.clone());
		assert_eq!(terms.kind(), ConsiliumKind::FeePolicy);
		// One open fee-policy consilium PER PRODUCT, and no contention with a payout (`fee`) or
		// a payment over the fund's pooled capital.
		assert_eq!(terms.source_claim(), LedgerAccountKey::FeeShares(crate::balance::ServiceId::parse("trading").unwrap()));
		assert!(terms.canonical_bytes().starts_with(crate::fees::FeePolicySubject::DOMAIN));
		assert_ne!(terms.canonical_bytes(), ConsiliumTerms::Payment(payment_subject()).canonical_bytes());
		// The effect narrows to its own column and to no other.
		let roster = owners(3);
		let mut c = opened(&roster);
		c.record_vote(roster[1], VoteDecision::Approve, NOW).unwrap();
		c.record_vote(roster[2], VoteDecision::Approve, NOW).unwrap();
		c.mark_executed(ConsiliumEffect::FeePolicy(subject.change_id), NOW).unwrap();
		assert_eq!(c.executed_fee_policy_change_id(), Some(subject.change_id));
		assert_eq!(c.executed_withdrawal_id(), None);
		assert_eq!(c.executed_payment_id(), None);
	}

	#[test]
	fn wrapping_terms_leaves_the_hashed_bytes_untouched() {
		// The enum is a container, not a second encoding layer: `payload_hash` for every
		// consilium that already exists was taken over the inner encoding, so the wrapper
		// must add nothing at all.
		let subject = payment_subject();
		assert_eq!(ConsiliumTerms::Payment(subject.clone()).canonical_bytes(), subject.canonical_bytes());
	}

	#[test]
	fn states_and_decisions_round_trip_and_reject_junk() {
		for state in [
			ConsiliumState::Open,
			ConsiliumState::Approved,
			ConsiliumState::Rejected,
			ConsiliumState::Expired,
			ConsiliumState::Cancelled,
			ConsiliumState::Executed,
			ConsiliumState::ExecutionFailed,
		] {
			assert_eq!(ConsiliumState::parse(state.as_str()).unwrap(), state);
		}
		for decision in [VoteDecision::Pending, VoteDecision::Approve, VoteDecision::Reject] {
			assert_eq!(VoteDecision::parse(decision.as_str()).unwrap(), decision);
		}
		for kind in [
			ConsiliumKind::Payment,
			ConsiliumKind::ValuationOverride,
			ConsiliumKind::FeePolicy,
			ConsiliumKind::HolderGrant,
			ConsiliumKind::SeedCapital,
		] {
			// THE DATABASE ADMITS EXACTLY THESE. `0031_consilium_payment_kind.sql`,
			// `0035_consilium_valuation_override.sql` and `0036_fee_policy_changes.sql` widen the
			// CHECK to the same four strings, and a value on one side only is a row that fails
			// EVERY read of the governance history rather than just its own.
			assert_eq!(ConsiliumKind::parse(kind.as_str()).unwrap(), kind);
		}
		assert_eq!(ConsiliumKind::Payment.as_str(), "payment");
		assert_eq!(ConsiliumKind::ValuationOverride.as_str(), "valuation_override");
		assert_eq!(ConsiliumKind::FeePolicy.as_str(), "fee_policy");
		assert!(ConsiliumState::parse("done").is_err());
		assert!(VoteDecision::parse("maybe").is_err());
		assert!(ConsiliumKind::parse("owner_removal").is_err());
		// Retired by #245 and narrowed out of the CHECK by 0047.
		assert!(ConsiliumKind::parse("revenue_payout").is_err());
	}

	#[test]
	fn events_round_trip_through_json() {
		let roster = owners(3);
		let mut c = Consilium::open(ConsiliumId::new(), terms(), [0xab; 32], roster[0], &roster, NOW).unwrap();
		let event = c.drain_events().pop().unwrap();
		let json = serde_json::to_string(&event).unwrap();
		let back: ConsiliumEvent = serde_json::from_str(&json).unwrap();
		let ConsiliumEvent::Opened { payload_hash, threshold: t, .. } = back else {
			panic!("expected Opened")
		};
		assert_eq!(t, 2);
		assert_eq!(payload_hash, "ab".repeat(32));
	}
}
