//! `withdrawals` bounded context — on-chain withdrawals out of the fund.
//!
//! The claim debited is an investor's own ([`WithdrawalSource::User`]) or, for an L1
//! payment the owners approved, banking's treasury ([`WithdrawalSource::Treasury`],
//! D17). The fund's
//! earnings used to leave by a second source — the revenue payout — which #245 retired:
//! an allocation fee is the `fee` allocation's, held by people, and cash leaves it only
//! through a holder's redemption onto their own claim and then their own withdrawal. The
//! withdrawal fee itself is banking's own and lands in its treasury (D17).
//!
//! A withdrawal moves that claim out of the fund and onto an external
//! address. It is the **dangerous direction** (value leaves the system), so it is a
//! saga with an explicit queue. The gross amount is *reserved* against the user's
//! claim the instant the request is recorded — into the network-agnostic
//! `WithdrawalClearing` account, so **acceptance never depends on a specific rail's
//! liquidity**. A request therefore starts **`Queued`**; it is **`Dispatched`** to
//! custody (→ `Processing`) as soon as the chosen rail has liquidity — immediately on
//! the happy path, or later (the treasury worker) when that rail was short. It then
//! **settles** (`Completed`, on N confirmations) or **fails** (`Failed`, voided +
//! refunded). A still-queued withdrawal can be **cancelled** (`Cancelled`, voided +
//! refunded) — always safe, since nothing was broadcast. The cardinal rule — *never
//! void once the broadcast may have reached the chain* — is why `fail` is only legal
//! from `Processing` and `cancel` only from `Queued`.
//!
//! Pure and wasm-safe: ids are minted by the application layer, no clock, no I/O.
//! The relay maps each event to ledger ops (and the custody broadcast); that
//! orchestration lives in the adapter, never on this aggregate.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{
	balance::{FeePayee, LedgerAccountKey},
	error::DomainError,
	money::{Network, TxRef, Usdt, WalletAddress},
	payments::PaymentId,
	users::UserId,
};

/// A unique withdrawal id (UUID). Minted by the application layer.
pub type WithdrawalId = Id<WithdrawalTag>;
/// Phantom tag making [`WithdrawalId`] a distinct, incompatible identity type.
pub struct WithdrawalTag;

/// **Whose money leaves** — the claim a withdrawal debits: an investor's, or banking's
/// treasury paying out what the owners approved (D17). The saga, the relay's
/// legs and the stored `source` column are all written against "a source", so the second
/// one is a variant here, not a parallel saga.
///
/// The wire form is the user's UUID as a string, so an `event_log`/`outbox` payload
/// written before the field was a source (it held a bare UUID) still reads back as
/// [`WithdrawalSource::User`]. Together with the `alias` on [`WithdrawalEvent`]'s `source`
/// field there is nothing to backfill and no undrained row that stops deserializing. The
/// treasury's is `treasury:<payment id>`, which no UUID can be.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(into = "String", try_from = "String")]
pub enum WithdrawalSource {
	/// An investor's own unified claim (`user:<uuid>`).
	User(UserId),
	/// banking's treasury, `ScopeAccount(main, treasury)` — only ever executing `payment`,
	/// an order the owners approved. The order rides on the source so no
	/// treasury withdrawal can exist without the approval it spends (a schema CHECK says
	/// the same of the row).
	Treasury { payment: PaymentId },
}

const TREASURY_WIRE: &str = "treasury";

impl WithdrawalSource {
	/// The stored/wire discriminant.
	pub fn as_wire(&self) -> String {
		match self {
			Self::User(user) => user.to_string(),
			Self::Treasury { payment } => format!("{TREASURY_WIRE}:{payment}"),
		}
	}

	/// The stored `source` column value (`user` | `treasury`); the user id rides beside it.
	pub fn column(&self) -> &'static str {
		match self {
			Self::User(_) => "user",
			Self::Treasury { .. } => TREASURY_WIRE,
		}
	}

	/// Parse the stored/wire form. The retired `revenue` literal is not UUID-shaped and
	/// is refused like any other junk.
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		if let Some(payment) = raw.strip_prefix(TREASURY_WIRE).and_then(|rest| rest.strip_prefix(':')) {
			return uuid::Uuid::parse_str(payment)
				.map(|id| Self::Treasury { payment: Id::from_raw(id) })
				.map_err(|_| DomainError::Validation(format!("unknown withdrawal source: {raw}")));
		}
		uuid::Uuid::parse_str(raw)
			.map(|id| Self::User(Id::from_raw(id)))
			.map_err(|_| DomainError::Validation(format!("unknown withdrawal source: {raw}")))
	}

	/// The claim account debited.
	pub fn claim_key(&self) -> LedgerAccountKey {
		match self {
			Self::User(user) => LedgerAccountKey::UserClaim(*user),
			Self::Treasury { .. } => crate::balance::FeePayee::treasury().claim_key(),
		}
	}

	/// The user this withdrawal belongs to; `None` for the treasury's.
	pub fn user(&self) -> Option<UserId> {
		match self {
			Self::User(user) => Some(*user),
			Self::Treasury { .. } => None,
		}
	}

	/// The approved payment order a treasury withdrawal executes; `None` for a user's.
	pub fn payment(&self) -> Option<PaymentId> {
		match self {
			Self::User(_) => None,
			Self::Treasury { payment } => Some(*payment),
		}
	}
}

impl From<WithdrawalSource> for String {
	fn from(source: WithdrawalSource) -> Self {
		source.as_wire()
	}
}

impl TryFrom<String> for WithdrawalSource {
	type Error = DomainError;

	fn try_from(raw: String) -> Result<Self, DomainError> {
		Self::parse(&raw)
	}
}

/// Per-network withdrawal policy — the flat network fee the fund retains and the
/// minimum gross a user may withdraw. **Placeholder constants** standing in for a
/// real fee oracle (gas/energy/bandwidth fluctuate per chain); kept here so the rule
/// stays pure and testable. Both are whole-USDT, hence representable at every chain's
/// precision, so the on-chain net is never sub-precision dust.
pub struct WithdrawalPolicy;

impl WithdrawalPolicy {
	/// The flat fee retained by the fund on a withdrawal (1 USDT).
	pub const fn fee(_network: Network) -> Usdt {
		Usdt::from_base_units(1_000_000_000_000_000_000)
	}

	/// The smallest gross withdrawal accepted (2 USDT) — necessarily above the fee,
	/// so the on-chain net is always positive.
	pub const fn minimum(_network: Network) -> Usdt {
		Usdt::from_base_units(2_000_000_000_000_000_000)
	}

	/// The fee retained for a withdrawal out of `source`.
	pub fn fee_for(source: WithdrawalSource, network: Network) -> Usdt {
		match source {
			WithdrawalSource::User(_) => Self::fee(network),
			// The fee would be paid to the treasury out of the treasury.
			WithdrawalSource::Treasury { .. } => Usdt::ZERO,
		}
	}
}

/// Lifecycle of a withdrawal. `Queued`/`Processing` are the in-flight states (the
/// gross is reserved as a TB pending against `WithdrawalClearing`); the rest are
/// terminal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WithdrawalState {
	/// Accepted and reserved against the user's claim (into clearing), awaiting a
	/// liquid rail to dispatch on. Nothing has been broadcast.
	Queued,
	/// Dispatched to custody — broadcast (or about to be), awaiting on-chain
	/// confirmation.
	Processing,
	/// Confirmed on-chain; the reservation was posted and the funds left custody
	/// (terminal).
	Completed,
	/// Broadcast attempted but the operator confirmed it never reached the chain; the
	/// reservation was voided and the user refunded (terminal).
	Failed,
	/// Cancelled while still queued (never broadcast); the reservation was voided and
	/// the user refunded (terminal).
	Cancelled,
}

impl WithdrawalState {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Queued => "queued",
			Self::Processing => "processing",
			Self::Completed => "completed",
			Self::Failed => "failed",
			Self::Cancelled => "cancelled",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"queued" => Ok(Self::Queued),
			"processing" => Ok(Self::Processing),
			"completed" => Ok(Self::Completed),
			"failed" => Ok(Self::Failed),
			"cancelled" => Ok(Self::Cancelled),
			other => Err(DomainError::Validation(format!("unknown withdrawal state: {other}"))),
		}
	}
}

/// The withdrawal aggregate — the saga's coordinator. Construct via
/// [`Withdrawal::request`] (raises [`WithdrawalEvent::Requested`]) or
/// [`Withdrawal::rehydrate`] (load from the store, no events). The settle/fail
/// commands each accumulate one event, drained by the repository into the event log
/// + outbox in the same unit of work.
#[derive(Clone, Debug)]
pub struct Withdrawal {
	id: WithdrawalId,
	source: WithdrawalSource,
	network: Network,
	address: WalletAddress,
	amount: Usdt,
	fee: Usdt,
	state: WithdrawalState,
	tx_ref: Option<TxRef>,
	pending: Vec<WithdrawalEvent>,
}

impl Withdrawal {
	/// Open a withdrawal of `amount` (gross) to `address`, debiting `source`. `amount`
	/// must clear the per-network minimum and exceed `fee` (so the net is positive),
	/// and the net must be representable at the chain's precision (no dust leak).
	/// Raises `Requested`; the relay reserves the gross against the source's claim into
	/// `WithdrawalClearing`. Starts `Queued` — the application dispatches it to custody
	/// once the chosen rail is liquid.
	pub fn request(id: WithdrawalId, source: WithdrawalSource, network: Network, address: WalletAddress, amount: Usdt, fee: Usdt) -> Result<Self, DomainError> {
		if address.network() != network {
			return Err(DomainError::Validation("withdrawal address is for a different network".into()));
		}
		if amount < WithdrawalPolicy::minimum(network) {
			return Err(DomainError::Validation("amount is below the minimum withdrawal".into()));
		}
		let net = amount
			.checked_sub(fee)
			.filter(|net| !net.is_zero())
			.ok_or_else(|| DomainError::Validation("amount does not cover the network fee".into()))?;
		// Reject a net that can't be expressed at the chain's precision — a truncating
		// withdrawal is a slow leak (the same guard as the custody edge).
		net.to_onchain(network)?;
		let mut withdrawal = Self {
			id,
			source,
			network,
			address: address.clone(),
			amount,
			fee,
			state: WithdrawalState::Queued,
			tx_ref: None,
			pending: Vec::new(),
		};
		withdrawal.pending.push(WithdrawalEvent::Requested {
			withdrawal_id: id,
			source,
			network,
			address,
			amount,
			fee,
		});
		Ok(withdrawal)
	}

	/// Reconstitute from the store. Raises no events.
	#[allow(clippy::too_many_arguments)]
	pub fn rehydrate(id: WithdrawalId, source: WithdrawalSource, network: Network, address: WalletAddress, amount: Usdt, fee: Usdt, state: WithdrawalState, tx_ref: Option<TxRef>) -> Self {
		Self {
			id,
			source,
			network,
			address,
			amount,
			fee,
			state,
			tx_ref,
			pending: Vec::new(),
		}
	}

	/// Dispatch a queued withdrawal to custody: the chosen rail has the liquidity, so
	/// the saga broadcasts. Idempotent if already processing; only a queued withdrawal
	/// can be dispatched.
	pub fn dispatch(&mut self) -> Result<(), DomainError> {
		if self.state == WithdrawalState::Processing {
			return Ok(());
		}
		if self.state != WithdrawalState::Queued {
			return Err(DomainError::Conflict(format!("withdrawal is {}, not dispatchable", self.state.as_str())));
		}
		self.state = WithdrawalState::Processing;
		self.pending.push(WithdrawalEvent::Dispatched {
			withdrawal_id: self.id,
			source: self.source,
			network: self.network,
			address: self.address.clone(),
			amount: self.amount,
			fee: self.fee,
		});
		Ok(())
	}

	/// Settle a confirmed withdrawal: it has the required on-chain confirmations, so
	/// the saga posts the reserved transfer and moves the net out of custody. Records
	/// the chain `tx_ref`. Idempotent if already completed; only a processing
	/// withdrawal can be settled.
	pub fn settle(&mut self, tx_ref: TxRef) -> Result<(), DomainError> {
		if self.state == WithdrawalState::Completed {
			return Ok(());
		}
		if self.state != WithdrawalState::Processing {
			return Err(DomainError::Conflict(format!("withdrawal is {}, not settleable", self.state.as_str())));
		}
		self.state = WithdrawalState::Completed;
		self.tx_ref = Some(tx_ref.clone());
		self.pending.push(WithdrawalEvent::Settled {
			withdrawal_id: self.id,
			source: self.source,
			network: self.network,
			amount: self.amount,
			fee: self.fee,
			tx_ref,
			payee: FeePayee::treasury(),
		});
		Ok(())
	}

	/// Fail a processing withdrawal: the saga voids the reservation, refunding the
	/// user in full. **Only safe when the broadcast certainly did not reach the
	/// chain** — voiding a landed withdrawal double-pays. Idempotent if already
	/// failed; only a processing withdrawal can be failed (a queued one is cancelled).
	pub fn fail(&mut self) -> Result<(), DomainError> {
		if self.state == WithdrawalState::Failed {
			return Ok(());
		}
		if self.state != WithdrawalState::Processing {
			return Err(DomainError::Conflict(format!("withdrawal is {}, not failable", self.state.as_str())));
		}
		self.state = WithdrawalState::Failed;
		self.pending.push(WithdrawalEvent::Failed {
			withdrawal_id: self.id,
			source: self.source,
			network: self.network,
			amount: self.amount,
			fee: self.fee,
		});
		Ok(())
	}

	/// Cancel a still-queued withdrawal (the user changed their mind, or the rail
	/// stayed short): the saga voids the reservation, refunding the user in full.
	/// Always safe — nothing was broadcast. Idempotent if already cancelled; only a
	/// queued withdrawal can be cancelled.
	pub fn cancel(&mut self) -> Result<(), DomainError> {
		if self.state == WithdrawalState::Cancelled {
			return Ok(());
		}
		if self.state != WithdrawalState::Queued {
			return Err(DomainError::Conflict(format!("withdrawal is {}, not cancellable", self.state.as_str())));
		}
		self.state = WithdrawalState::Cancelled;
		self.pending.push(WithdrawalEvent::Cancelled {
			withdrawal_id: self.id,
			source: self.source,
			network: self.network,
			amount: self.amount,
			fee: self.fee,
		});
		Ok(())
	}

	pub fn id(&self) -> WithdrawalId {
		self.id
	}

	/// The claim this withdrawal debits.
	pub fn source(&self) -> WithdrawalSource {
		self.source
	}

	/// The owning investor; `None` for a treasury payout.
	pub fn user(&self) -> Option<UserId> {
		self.source.user()
	}

	pub fn network(&self) -> Network {
		self.network
	}

	pub fn address(&self) -> &WalletAddress {
		&self.address
	}

	pub fn amount(&self) -> Usdt {
		self.amount
	}

	pub fn fee(&self) -> Usdt {
		self.fee
	}

	/// The amount actually sent on-chain — gross minus the retained fee.
	pub fn net_amount(&self) -> Usdt {
		self.amount.checked_sub(self.fee).unwrap_or(Usdt::ZERO)
	}

	pub fn state(&self) -> WithdrawalState {
		self.state
	}

	pub fn tx_ref(&self) -> Option<&TxRef> {
		self.tx_ref.as_ref()
	}
}

impl Entity for Withdrawal {
	type Id = WithdrawalId;

	fn id(&self) -> WithdrawalId {
		self.id
	}
}

impl AggregateRoot for Withdrawal {
	const NAME: &'static str = "withdrawal";
}

/// Facts raised by the [`Withdrawal`] aggregate. Each carries the saga-relevant data
/// (source, network, amount, fee, address) so the relay maps an event to its ledger
/// ops and the custody broadcast with no extra read. Internally tagged so the stored
/// JSON is self-describing.
///
/// `source` is read from either key: events written before the field was a source
/// spell it `user` and hold a bare UUID, which [`WithdrawalSource`]'s string form
/// parses unchanged. An outbox row parked before the upgrade therefore still drains
/// after it — no backfill, no wedged queue.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WithdrawalEvent {
	/// Accepted + reserved against the source's claim into clearing (relay: pending
	/// `Dr <source> / Cr clearing` for the gross). No rail touched yet.
	Requested {
		withdrawal_id: WithdrawalId,
		#[serde(alias = "user")]
		source: WithdrawalSource,
		network: Network,
		address: WalletAddress,
		amount: Usdt,
		fee: Usdt,
	},
	/// The rail is liquid — broadcast the net to custody (relay: custody broadcast).
	Dispatched {
		withdrawal_id: WithdrawalId,
		#[serde(alias = "user")]
		source: WithdrawalSource,
		network: Network,
		address: WalletAddress,
		amount: Usdt,
		fee: Usdt,
	},
	/// Confirmed on-chain (relay: post the clearing pending, then move net→`wallet:<net>`
	/// and the retained fee→`payee`'s account).
	///
	/// `payee` is banking's treasury (D17); an event raised before names the `fee`
	/// allocation and still pays it.
	Settled {
		withdrawal_id: WithdrawalId,
		#[serde(alias = "user")]
		source: WithdrawalSource,
		network: Network,
		amount: Usdt,
		fee: Usdt,
		tx_ref: TxRef,
		payee: FeePayee,
	},
	/// Broadcast confirmed not to have landed — void the clearing reservation (refund).
	Failed {
		withdrawal_id: WithdrawalId,
		#[serde(alias = "user")]
		source: WithdrawalSource,
		network: Network,
		amount: Usdt,
		fee: Usdt,
	},
	/// Cancelled while queued — void the clearing reservation (refund).
	Cancelled {
		withdrawal_id: WithdrawalId,
		#[serde(alias = "user")]
		source: WithdrawalSource,
		network: Network,
		amount: Usdt,
		fee: Usdt,
	},
}

impl DomainEvent for WithdrawalEvent {
	const KIND: &'static str = "withdrawals";
}

impl EmitsEvents for Withdrawal {
	type Event = WithdrawalEvent;

	fn drain_events(&mut self) -> Vec<WithdrawalEvent> {
		core::mem::take(&mut self.pending)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn user() -> WithdrawalSource {
		WithdrawalSource::User(UserId::new())
	}

	fn addr(network: Network) -> WalletAddress {
		let raw = match network {
			Network::Bep20 | Network::Polygon => "0x52908400098527886E0F7030069857D2E4169EE7",
			Network::Trc20 => "TJRabPrwbZy45sbavfcjinPJC18kjpRTv8",
			Network::Ton => "EQCD39VS5jcptHL8vMjEXrzGaRcCVYto7HUn4bpAOg8xqB2N",
		};
		WalletAddress::parse(network, raw).unwrap()
	}

	fn gross(units: &str) -> Usdt {
		Usdt::parse_decimal(units).unwrap()
	}

	fn fee(network: Network) -> Usdt {
		WithdrawalPolicy::fee(network)
	}

	#[test]
	fn request_sets_queued_and_emits_requested() {
		let mut w = Withdrawal::request(WithdrawalId::new(), user(), Network::Trc20, addr(Network::Trc20), gross("100"), fee(Network::Trc20)).unwrap();
		assert_eq!(w.state(), WithdrawalState::Queued);
		assert_eq!(w.net_amount(), gross("99"));
		let events = w.drain_events();
		assert_eq!(events.len(), 1);
		assert!(matches!(events[0], WithdrawalEvent::Requested { .. }));
		assert!(w.drain_events().is_empty());
	}

	#[test]
	fn dispatch_moves_to_processing_and_is_idempotent() {
		let mut w = Withdrawal::request(WithdrawalId::new(), user(), Network::Bep20, addr(Network::Bep20), gross("50"), fee(Network::Bep20)).unwrap();
		w.drain_events();
		w.dispatch().unwrap();
		assert_eq!(w.state(), WithdrawalState::Processing);
		assert!(matches!(w.drain_events()[0], WithdrawalEvent::Dispatched { .. }));
		// Idempotent: second dispatch is a no-op.
		w.dispatch().unwrap();
		assert!(w.drain_events().is_empty());
	}

	#[test]
	fn settle_and_fail_require_dispatch_first() {
		let mut w = Withdrawal::request(WithdrawalId::new(), user(), Network::Bep20, addr(Network::Bep20), gross("50"), fee(Network::Bep20)).unwrap();
		// Still queued — cannot settle or fail until dispatched.
		assert!(w.settle(TxRef::parse("0xhash").unwrap()).is_err());
		assert!(w.fail().is_err());
	}

	#[test]
	fn address_network_must_match() {
		let err = Withdrawal::request(WithdrawalId::new(), user(), Network::Trc20, addr(Network::Bep20), gross("100"), fee(Network::Trc20)).unwrap_err();
		assert!(matches!(err, DomainError::Validation(_)));
	}

	#[test]
	fn amount_below_minimum_is_rejected() {
		let err = Withdrawal::request(WithdrawalId::new(), user(), Network::Ton, addr(Network::Ton), gross("1"), fee(Network::Ton)).unwrap_err();
		assert!(matches!(err, DomainError::Validation(_)));
	}

	#[test]
	fn amount_must_cover_the_fee() {
		// A bespoke fee larger than a (minimum-clearing) amount leaves a non-positive net.
		let err = Withdrawal::request(WithdrawalId::new(), user(), Network::Bep20, addr(Network::Bep20), gross("10"), gross("10")).unwrap_err();
		assert!(matches!(err, DomainError::Validation(_)));
	}

	#[test]
	fn settle_posts_and_is_idempotent() {
		let mut w = Withdrawal::request(WithdrawalId::new(), user(), Network::Bep20, addr(Network::Bep20), gross("50"), fee(Network::Bep20)).unwrap();
		w.dispatch().unwrap();
		w.drain_events();
		w.settle(TxRef::parse("0xhash").unwrap()).unwrap();
		assert_eq!(w.state(), WithdrawalState::Completed);
		assert!(matches!(w.drain_events()[0], WithdrawalEvent::Settled { .. }));
		// Second settle: no error, no event.
		w.settle(TxRef::parse("0xhash").unwrap()).unwrap();
		assert!(w.drain_events().is_empty());
		// A completed withdrawal can't be failed.
		assert!(w.fail().is_err());
	}

	#[test]
	fn fail_voids_and_is_idempotent() {
		let mut w = Withdrawal::request(WithdrawalId::new(), user(), Network::Trc20, addr(Network::Trc20), gross("50"), fee(Network::Trc20)).unwrap();
		w.dispatch().unwrap();
		w.drain_events();
		w.fail().unwrap();
		assert_eq!(w.state(), WithdrawalState::Failed);
		assert!(matches!(w.drain_events()[0], WithdrawalEvent::Failed { .. }));
		w.fail().unwrap();
		assert!(w.drain_events().is_empty());
		// A failed withdrawal can't be settled.
		assert!(w.settle(TxRef::parse("0xhash").unwrap()).is_err());
	}

	#[test]
	fn queued_cancels_but_processing_cannot() {
		let mut w = Withdrawal::request(WithdrawalId::new(), user(), Network::Ton, addr(Network::Ton), gross("50"), fee(Network::Ton)).unwrap();
		w.drain_events();
		w.cancel().unwrap();
		assert_eq!(w.state(), WithdrawalState::Cancelled);
		assert!(matches!(w.drain_events()[0], WithdrawalEvent::Cancelled { .. }));
		// Idempotent; and a cancelled withdrawal can't be dispatched.
		w.cancel().unwrap();
		assert!(w.drain_events().is_empty());
		assert!(w.dispatch().is_err());

		// Once dispatched (processing), cancel is rejected — only fail is legal.
		let mut p = Withdrawal::request(WithdrawalId::new(), user(), Network::Ton, addr(Network::Ton), gross("50"), fee(Network::Ton)).unwrap();
		p.dispatch().unwrap();
		assert!(p.cancel().is_err());
	}

	#[test]
	fn event_round_trips_through_json() {
		let mut w = Withdrawal::request(WithdrawalId::new(), user(), Network::Trc20, addr(Network::Trc20), gross("100"), fee(Network::Trc20)).unwrap();
		let event = w.drain_events().pop().unwrap();
		let json = serde_json::to_string(&event).unwrap();
		let back: WithdrawalEvent = serde_json::from_str(&json).unwrap();
		assert!(matches!(back, WithdrawalEvent::Requested { network: Network::Trc20, .. }));
	}

	#[test]
	fn a_withdrawal_debits_its_users_claim_and_pays_the_policy_fee() {
		let uid = UserId::new();
		assert_eq!(WithdrawalSource::User(uid).claim_key(), LedgerAccountKey::UserClaim(uid));
		assert_eq!(WithdrawalPolicy::fee_for(WithdrawalSource::User(uid), Network::Bep20), WithdrawalPolicy::fee(Network::Bep20));
	}

	#[test]
	fn source_round_trips_and_rejects_junk() {
		let uid = UserId::new();
		let source = WithdrawalSource::User(uid);
		assert_eq!(WithdrawalSource::parse(&source.as_wire()).unwrap(), source);
		// Not UUID-shaped ⇒ refused, never a silent default — the retired `revenue` literal
		// included (#245).
		assert!(WithdrawalSource::parse("revenue").is_err());
		assert!(WithdrawalSource::parse("fund").is_err());
		assert!(WithdrawalSource::parse("").is_err());
		let payment = crate::payments::PaymentId::new();
		let treasury = WithdrawalSource::Treasury { payment };
		assert_eq!(WithdrawalSource::parse(&treasury.as_wire()).unwrap(), treasury);
		assert_eq!(treasury.as_wire(), format!("treasury:{payment}"));
		assert!(WithdrawalSource::parse("treasury").is_err(), "a treasury withdrawal names its payment");
		assert!(WithdrawalSource::parse("treasury:nope").is_err());
		assert_eq!((treasury.user(), treasury.payment()), (None, Some(payment)));
		assert_eq!(WithdrawalPolicy::fee_for(treasury, Network::Bep20), Usdt::ZERO);
	}

	#[test]
	fn a_pre_payout_event_payload_still_deserializes() {
		// An outbox/event_log row written before revenue payouts existed spells the field
		// `user` and holds a bare UUID. If this regressed, a parked row from before the
		// upgrade could never be unparked — it would fail to deserialize forever.
		let uid = UserId::new();
		let mut w = Withdrawal::request(
			WithdrawalId::new(),
			WithdrawalSource::User(uid),
			Network::Trc20,
			addr(Network::Trc20),
			gross("100"),
			fee(Network::Trc20),
		)
		.unwrap();
		// The only difference from a current payload is the key: `"source"` → `"user"`.
		let legacy = serde_json::to_string(&w.drain_events().pop().unwrap()).unwrap().replace(r#""source""#, r#""user""#);
		assert!(legacy.contains(r#""user""#), "the legacy key must actually be present");
		let back: WithdrawalEvent = serde_json::from_str(&legacy).unwrap();
		let WithdrawalEvent::Requested { source, .. } = back else { panic!("expected Requested") };
		assert_eq!(source, WithdrawalSource::User(uid));
	}
}
