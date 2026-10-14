//! `scopes` bounded context — a user's wallet split by scope, and the scope's own accounts.
//!
//! A **scope** is a concierge tenant (`invest`, `sa`, …); banking only holds money in the
//! scopes it knows. `main` is the implicit root: a user's `main` compartment is their
//! [`UserClaim`](LedgerAccountKey::UserClaim) — deposits land there and withdrawals leave
//! from there. Every other scope a user enters holds a **compartment**
//! ([`LedgerAccountKey::ScopeClaim`]), money earmarked for that scope's services. Only the
//! user moves money `main ⇄ scope` ([`CompartmentMove`]), instantly and without a fee, and
//! never between two non-main scopes directly (D9).
//!
//! A **scope account** ([`LedgerAccountKey::ScopeAccount`]) is the scope's own money, keyed
//! by a label its owner chose. `(main, treasury)` is banking's: the fees banking charges
//! for its own service land there (D17).
//!
//! Pure and wasm-safe: ids are minted by the application layer, no clock, no I/O. The
//! contract the wire speaks is `banking.v1.CompartmentService` (`wallet.proto`) and the
//! header of `scope/v1/ledger.proto`.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{balance::LedgerAccountKey, error::DomainError, money::Usdt, users::UserId};

/// The root scope's id. Never a row of the scope registry: it is the user's own wallet.
const MAIN: &str = "main";

/// A scope's id — concierge's tenant name, `[a-z][a-z0-9_]{0,31}`. `main` parses too:
/// it names the root in a compartment move and owns the treasury account, while the
/// registry of entered scopes never holds it ([`Compartment`] keeps the two apart).
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScopeId(String);

impl ScopeId {
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		let bytes = raw.as_bytes();
		let shaped = !bytes.is_empty() && bytes.len() <= 32 && bytes[0].is_ascii_lowercase() && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_');
		if !shaped {
			return Err(DomainError::Validation(format!("scope id must match [a-z][a-z0-9_]{{0,31}}: {raw:?}")));
		}
		Ok(Self(raw.to_owned()))
	}

	pub fn main() -> Self {
		Self(MAIN.to_owned())
	}

	pub fn is_main(&self) -> bool {
		self.0 == MAIN
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl TryFrom<String> for ScopeId {
	type Error = DomainError;

	fn try_from(raw: String) -> Result<Self, DomainError> {
		Self::parse(&raw)
	}
}

impl From<ScopeId> for String {
	fn from(id: ScopeId) -> Self {
		id.0
	}
}

impl core::fmt::Display for ScopeId {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(&self.0)
	}
}

/// The label of a scope account, `[a-z0-9_:.-]{1,64}`, chosen by the account's owner.
/// Banking does not know what a label means, except for its own two: `treasury` in
/// `main` and `fee` in every scope.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScopeAccountLabel(String);

impl ScopeAccountLabel {
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		let shaped = !raw.is_empty() && raw.len() <= 64 && raw.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b':' | b'.' | b'-'));
		if !shaped {
			return Err(DomainError::Validation(format!("scope account label must match [a-z0-9_:.-]{{1,64}}: {raw:?}")));
		}
		Ok(Self(raw.to_owned()))
	}

	/// banking's own account in `main`, where the fees it charges for its own service land
	/// (D17).
	pub fn treasury() -> Self {
		Self("treasury".to_owned())
	}

	/// banking's fee collector in a scope (`scope/v1/ledger.proto`, "the fee route").
	pub fn fee() -> Self {
		Self("fee".to_owned())
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl TryFrom<String> for ScopeAccountLabel {
	type Error = DomainError;

	fn try_from(raw: String) -> Result<Self, DomainError> {
		Self::parse(&raw)
	}
}

impl From<ScopeAccountLabel> for String {
	fn from(label: ScopeAccountLabel) -> Self {
		label.0
	}
}

impl core::fmt::Display for ScopeAccountLabel {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(&self.0)
	}
}

/// One end of a compartment move: the user's `main` wallet, or their compartment in a
/// scope. A `Scope` never holds `main` — [`Self::parse`] is the only way in from text,
/// so a `ScopeClaim` keyed on `main` cannot be addressed through a compartment.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub enum Compartment {
	Main,
	Scope(ScopeId),
}

impl Compartment {
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		let id = ScopeId::parse(raw)?;
		Ok(if id.is_main() { Self::Main } else { Self::Scope(id) })
	}

	pub fn as_str(&self) -> &str {
		match self {
			Self::Main => MAIN,
			Self::Scope(id) => id.as_str(),
		}
	}

	/// The scope this end lies in, `None` for `main`.
	pub fn scope(&self) -> Option<&ScopeId> {
		match self {
			Self::Main => None,
			Self::Scope(id) => Some(id),
		}
	}

	/// The ledger account that holds `user`'s money at this end.
	pub fn claim_key(&self, user: UserId) -> LedgerAccountKey {
		match self {
			Self::Main => LedgerAccountKey::UserClaim(user),
			Self::Scope(scope) => LedgerAccountKey::ScopeClaim(user, scope.clone()),
		}
	}
}

impl TryFrom<String> for Compartment {
	type Error = DomainError;

	fn try_from(raw: String) -> Result<Self, DomainError> {
		Self::parse(&raw)
	}
}

impl From<Compartment> for String {
	fn from(compartment: Compartment) -> Self {
		compartment.as_str().to_owned()
	}
}

/// A scope's lifecycle, as concierge announces it. Credits and reads go on in every state;
/// what a state stops is new money going IN.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeState {
	Active,
	/// Takes no new money in (`scope_frozen`); users may still move their money out.
	Frozen,
	/// Wound down: takes no new money in (`scope_closed`), users may still move out what
	/// is left, and the scope is listed only to a user who still holds money there.
	Closed,
}

impl ScopeState {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Active => "active",
			Self::Frozen => "frozen",
			Self::Closed => "closed",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"active" => Ok(Self::Active),
			"frozen" => Ok(Self::Frozen),
			"closed" => Ok(Self::Closed),
			other => Err(DomainError::Validation(format!("unknown scope state: {other}"))),
		}
	}

	/// The refusal a move INTO a scope in this state meets, if any.
	pub const fn refuses_money_in(self) -> Option<MoveRefusal> {
		match self {
			Self::Active => None,
			Self::Frozen => Some(MoveRefusal::ScopeFrozen),
			Self::Closed => Some(MoveRefusal::ScopeClosed),
		}
	}
}

/// What it takes to move money into a scope, and how far its services may move money
/// without an approval. `None` limits mean none. The automatic ceilings are only stored
/// and read here; the scope ledger enforces them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopePolicy {
	/// The concierge KYC level a user needs to move money in.
	pub min_kyc_level: u32,
	/// Largest single move in.
	pub max_per_move: Option<Usdt>,
	/// Rolling 24 h total moved in, per user.
	pub max_moved_in_24h: Option<Usdt>,
	/// Ceiling on the compartment's total, applied ONLY to the user's own move in.
	pub max_balance: Option<Usdt>,
	pub auto_payout_per_op: Usdt,
	pub auto_payout_24h: Usdt,
	pub auto_transfer_24h: Usdt,
}

/// Where a recorded move stands. `Posted` and `Failed` are terminal: a failed move is
/// never executed later — not by a redelivery, not by an operator's unpark — because the
/// user's intent was refused, and acting on it later would be acting without it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveState {
	/// Recorded; the relay has not posted it yet.
	Pending,
	/// On the ledger.
	Posted,
	/// The ledger refused it; nothing moved.
	Failed,
}

impl MoveState {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Pending => "pending",
			Self::Posted => "posted",
			Self::Failed => "failed",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"pending" => Ok(Self::Pending),
			"posted" => Ok(Self::Posted),
			"failed" => Ok(Self::Failed),
			other => Err(DomainError::Validation(format!("unknown move state: {other}"))),
		}
	}
}

/// Why a compartment move was refused — each one the whole wire status message
/// (`FAILED_PRECONDITION`), which clients branch on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveRefusal {
	KycLevel,
	MaxPerMove,
	MaxMovedIn24h,
	MaxBalance,
	ScopeFrozen,
	ScopeClosed,
	UserFrozen,
	InsufficientFunds,
}

impl MoveRefusal {
	pub const fn reason(self) -> &'static str {
		match self {
			Self::KycLevel => "kyc_level",
			Self::MaxPerMove => "max_per_move",
			Self::MaxMovedIn24h => "max_moved_in_24h",
			Self::MaxBalance => "max_balance",
			Self::ScopeFrozen => "scope_frozen",
			Self::ScopeClosed => "scope_closed",
			Self::UserFrozen => "user_frozen",
			Self::InsufficientFunds => "insufficient_funds",
		}
	}
}

/// What the policy is asked about one move in, every figure read just before it.
#[derive(Clone, Copy, Debug)]
pub struct MoveInFacts {
	pub kyc_level: u32,
	pub amount: Usdt,
	/// Moved into this scope by this user over the last 24 h, this move excluded.
	pub moved_in_24h: Usdt,
	/// The compartment's total right now, this move excluded.
	pub balance: Usdt,
}

impl ScopePolicy {
	/// Whether a user meeting `kyc_level` may move money in at all.
	pub fn admits_kyc(&self, kyc_level: u32) -> bool {
		kyc_level >= self.min_kyc_level
	}

	/// The first rule a move in breaks, in the order the wire documents them.
	pub fn admit_move_in(&self, facts: MoveInFacts) -> Result<(), MoveRefusal> {
		if !self.admits_kyc(facts.kyc_level) {
			return Err(MoveRefusal::KycLevel);
		}
		if self.max_per_move.is_some_and(|cap| facts.amount > cap) {
			return Err(MoveRefusal::MaxPerMove);
		}
		// An overflowing sum is past any cap a u128 can express.
		let within = |cap: Option<Usdt>, base: Usdt| cap.is_none_or(|cap| base.checked_add(facts.amount).is_some_and(|total| total <= cap));
		if !within(self.max_moved_in_24h, facts.moved_in_24h) {
			return Err(MoveRefusal::MaxMovedIn24h);
		}
		if !within(self.max_balance, facts.balance) {
			return Err(MoveRefusal::MaxBalance);
		}
		Ok(())
	}
}

/// A unique compartment move id (UUID). Minted by the application layer.
pub type CompartmentMoveId = Id<CompartmentMoveTag>;
/// Phantom tag making [`CompartmentMoveId`] a distinct, incompatible identity type.
pub struct CompartmentMoveTag;

/// A user moving their own money between `main` and one scope compartment. One posted
/// ledger transfer, applied by the relay: `Dr <from claim> / Cr <to claim>`. Recorded once
/// per `(user, idempotency key)`; it has no later states.
#[derive(Clone, Debug)]
pub struct CompartmentMove {
	id: CompartmentMoveId,
	user: UserId,
	from: Compartment,
	to: Compartment,
	/// The non-main side, resolved once so no reader has to re-prove one exists.
	scope: ScopeId,
	amount: Usdt,
	pending: Vec<CompartmentEvent>,
}

impl CompartmentMove {
	/// Exactly one side is `main`, so the other names the scope; the amount is positive.
	pub fn request(id: CompartmentMoveId, user: UserId, from: Compartment, to: Compartment, amount: Usdt) -> Result<Self, DomainError> {
		let mut requested = Self::restore(id, user, from, to, amount)?;
		requested.pending.push(CompartmentEvent::Moved {
			move_id: id,
			user,
			from: requested.from.clone(),
			to: requested.to.clone(),
			amount,
		});
		Ok(requested)
	}

	/// Rebuild from storage (no events).
	pub fn restore(id: CompartmentMoveId, user: UserId, from: Compartment, to: Compartment, amount: Usdt) -> Result<Self, DomainError> {
		if amount.is_zero() {
			return Err(DomainError::Validation("amount must be positive".into()));
		}
		let scope = match (&from, &to) {
			(Compartment::Main, Compartment::Scope(scope)) | (Compartment::Scope(scope), Compartment::Main) => scope.clone(),
			(Compartment::Main, Compartment::Main) | (Compartment::Scope(_), Compartment::Scope(_)) => return Err(DomainError::Validation("exactly one side of a move must be main".into())),
		};
		Ok(Self {
			id,
			user,
			from,
			to,
			scope,
			amount,
			pending: Vec::new(),
		})
	}

	pub fn id(&self) -> CompartmentMoveId {
		self.id
	}

	pub fn user(&self) -> UserId {
		self.user
	}

	pub fn from(&self) -> &Compartment {
		&self.from
	}

	pub fn to(&self) -> &Compartment {
		&self.to
	}

	pub fn amount(&self) -> Usdt {
		self.amount
	}

	/// The scope on the non-main side.
	pub fn scope(&self) -> &ScopeId {
		&self.scope
	}

	/// Whether the money goes into the scope (the policy applies) or back to `main`.
	pub fn is_move_in(&self) -> bool {
		self.from == Compartment::Main
	}

	/// Whether `other` asks for the same move — what an idempotent repeat must match.
	pub fn same_terms(&self, from: &Compartment, to: &Compartment, amount: Usdt) -> bool {
		self.from == *from && self.to == *to && self.amount == amount
	}
}

impl Entity for CompartmentMove {
	type Id = CompartmentMoveId;

	fn id(&self) -> CompartmentMoveId {
		self.id
	}
}

impl AggregateRoot for CompartmentMove {
	const NAME: &'static str = "compartment_move";
}

/// Facts raised by [`CompartmentMove`]; internally tagged so the stored JSON describes
/// itself.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompartmentEvent {
	/// Relay: post `Dr <from claim> / Cr <to claim>` for `amount`.
	Moved {
		move_id: CompartmentMoveId,
		user: UserId,
		from: Compartment,
		to: Compartment,
		amount: Usdt,
	},
}

impl DomainEvent for CompartmentEvent {
	const KIND: &'static str = "compartments";
}

impl EmitsEvents for CompartmentMove {
	type Event = CompartmentEvent;

	fn drain_events(&mut self) -> Vec<CompartmentEvent> {
		core::mem::take(&mut self.pending)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn usdt(raw: &str) -> Usdt {
		Usdt::parse_decimal(raw).unwrap()
	}

	fn scope(raw: &str) -> Compartment {
		Compartment::parse(raw).unwrap()
	}

	#[test]
	fn scope_ids_follow_the_tenant_name_form() {
		for ok in ["a", "sa", "invest", "service_arb", "a1_", "abcdefghijklmnopqrstuvwxyz012345"] {
			assert!(ScopeId::parse(ok).is_ok(), "{ok:?} refused");
		}
		for bad in ["", "1a", "_a", "Sa", "s-a", "s:a", "abcdefghijklmnopqrstuvwxyz0123456", " sa"] {
			assert!(ScopeId::parse(bad).is_err(), "{bad:?} accepted");
		}
		assert!(ScopeId::main().is_main());
		assert!(!ScopeId::parse("sa").unwrap().is_main());
	}

	#[test]
	fn labels_allow_the_owner_namespacing_characters() {
		for ok in ["treasury", "fee", "alloc:service_arb", "a.b-c_1"] {
			assert!(ScopeAccountLabel::parse(ok).is_ok(), "{ok:?} refused");
		}
		for bad in ["", "Fee", "a b", "a/b", &"x".repeat(65)] {
			assert!(ScopeAccountLabel::parse(bad).is_err(), "{bad:?} accepted");
		}
	}

	#[test]
	fn main_is_the_users_own_claim_and_a_scope_its_compartment() {
		let user = UserId::new();
		assert_eq!(scope("main"), Compartment::Main);
		assert_eq!(scope("main").claim_key(user), LedgerAccountKey::UserClaim(user));
		assert_eq!(scope("sa").claim_key(user), LedgerAccountKey::ScopeClaim(user, ScopeId::parse("sa").unwrap()));
		assert_eq!(serde_json::to_string(&scope("sa")).unwrap(), r#""sa""#);
		assert_eq!(serde_json::from_str::<Compartment>(r#""main""#).unwrap(), Compartment::Main);
	}

	#[test]
	fn a_move_has_exactly_one_main_side_and_a_positive_amount() {
		let user = UserId::new();
		let id = CompartmentMoveId::new;
		assert!(CompartmentMove::request(id(), user, scope("main"), scope("sa"), usdt("1")).is_ok());
		assert!(CompartmentMove::request(id(), user, scope("sa"), scope("main"), usdt("1")).is_ok());
		assert!(CompartmentMove::request(id(), user, scope("main"), scope("main"), usdt("1")).is_err());
		assert!(CompartmentMove::request(id(), user, scope("sa"), scope("invest"), usdt("1")).is_err(), "never scope to scope");
		assert!(CompartmentMove::request(id(), user, scope("sa"), scope("sa"), usdt("1")).is_err());
		assert!(CompartmentMove::request(id(), user, scope("main"), scope("sa"), Usdt::ZERO).is_err());
	}

	#[test]
	fn a_move_raises_one_fact_naming_both_ends() {
		let user = UserId::new();
		let mut m = CompartmentMove::request(CompartmentMoveId::new(), user, scope("sa"), scope("main"), usdt("2.5")).unwrap();
		assert!(!m.is_move_in());
		assert_eq!(m.scope().as_str(), "sa");
		let events = m.drain_events();
		assert_eq!(events.len(), 1);
		let json = serde_json::to_value(&events[0]).unwrap();
		assert_eq!(json["type"], "moved");
		assert_eq!(json["from"], "sa");
		assert_eq!(json["to"], "main");
		assert!(m.drain_events().is_empty());
	}

	fn policy() -> ScopePolicy {
		ScopePolicy {
			min_kyc_level: 1,
			max_per_move: Some(usdt("100")),
			max_moved_in_24h: Some(usdt("150")),
			max_balance: Some(usdt("200")),
			auto_payout_per_op: usdt("1000"),
			auto_payout_24h: usdt("5000"),
			auto_transfer_24h: usdt("5000"),
		}
	}

	fn facts(kyc_level: u32, amount: &str, moved_in_24h: &str, balance: &str) -> MoveInFacts {
		MoveInFacts {
			kyc_level,
			amount: usdt(amount),
			moved_in_24h: usdt(moved_in_24h),
			balance: usdt(balance),
		}
	}

	#[test]
	fn the_policy_refuses_with_the_first_rule_broken() {
		let p = policy();
		assert_eq!(p.admit_move_in(facts(1, "100", "50", "100")), Ok(()), "every limit is inclusive");
		assert_eq!(p.admit_move_in(facts(0, "1", "0", "0")), Err(MoveRefusal::KycLevel));
		assert_eq!(p.admit_move_in(facts(0, "1000", "1000", "1000")), Err(MoveRefusal::KycLevel), "kyc first");
		assert_eq!(p.admit_move_in(facts(1, "100.000000000000000001", "0", "0")), Err(MoveRefusal::MaxPerMove));
		assert_eq!(p.admit_move_in(facts(1, "60", "100", "0")), Err(MoveRefusal::MaxMovedIn24h));
		assert_eq!(p.admit_move_in(facts(1, "60", "0", "150")), Err(MoveRefusal::MaxBalance));
		let open = ScopePolicy {
			min_kyc_level: 0,
			max_per_move: None,
			max_moved_in_24h: None,
			max_balance: None,
			..policy()
		};
		assert_eq!(open.admit_move_in(facts(0, "1000000", "1000000", "1000000")), Ok(()), "no limit is no limit");
	}

	#[test]
	fn move_states_round_trip() {
		for state in [MoveState::Pending, MoveState::Posted, MoveState::Failed] {
			assert_eq!(MoveState::parse(state.as_str()).unwrap(), state);
		}
		assert!(MoveState::parse("parked").is_err());
	}

	#[test]
	fn a_frozen_or_closed_scope_takes_no_money_in() {
		assert_eq!(ScopeState::Active.refuses_money_in(), None);
		assert_eq!(ScopeState::Frozen.refuses_money_in(), Some(MoveRefusal::ScopeFrozen));
		assert_eq!(ScopeState::Closed.refuses_money_in(), Some(MoveRefusal::ScopeClosed));
		for state in [ScopeState::Active, ScopeState::Frozen, ScopeState::Closed] {
			assert_eq!(ScopeState::parse(state.as_str()).unwrap(), state);
		}
	}

	#[test]
	fn refusal_reasons_are_the_wire_vocabulary() {
		let reasons = [
			MoveRefusal::KycLevel,
			MoveRefusal::MaxPerMove,
			MoveRefusal::MaxMovedIn24h,
			MoveRefusal::MaxBalance,
			MoveRefusal::ScopeFrozen,
			MoveRefusal::ScopeClosed,
			MoveRefusal::UserFrozen,
			MoveRefusal::InsufficientFunds,
		]
		.map(MoveRefusal::reason);
		assert_eq!(
			reasons,
			[
				"kyc_level", "max_per_move", "max_moved_in_24h", "max_balance", "scope_frozen", "scope_closed", "user_frozen", "insufficient_funds"
			]
		);
	}
}
