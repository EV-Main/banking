//! One intent: what a host page asked for, the terms the BFF computed for it, and the hash a
//! step-up proof is bound to.
//!
//! The record is stored as the exact JSON bytes it was hashed over, so the hash is a
//! property of the stored bytes and never of a re-serialisation. The store holds no key:
//! the seal (`keys::IntentKeys::seal`) is what stops anyone with access to Redis from
//! editing the terms of a pending intent.

use domain::money::Usdt;
use evconcierge_auth::stepup::StepupMethod;
use serde::{Deserialize, Serialize};

use super::{IntentKind, RiskPolicy, keys};

/// How long a host has to confirm. The step-up proof lives at most as long, so a person who
/// takes longer starts again with fresh terms.
pub const INTENT_TTL_SECS: u64 = 120;

const HASH_LABEL: &[u8] = b"ev.cabinet.wallet.intent.v1";

/// What the host asked for, exactly as the body named it. Unknown fields are refused, so a
/// page cannot hide a parameter it hopes a later version reads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
	Move(MoveParams),
	Withdraw(WithdrawParams),
	Pay(PayParams),
	Mandate(MandateParams),
}

impl Action {
	pub fn kind(&self) -> IntentKind {
		match self {
			Self::Move(_) => IntentKind::Move,
			Self::Withdraw(_) => IntentKind::Withdraw,
			Self::Pay(_) => IntentKind::Pay,
			Self::Mandate(_) => IntentKind::Mandate,
		}
	}
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MoveParams {
	pub from_scope: String,
	pub to_scope: String,
	pub amount: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WithdrawParams {
	pub network: String,
	pub address: String,
	pub amount: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PayParams {
	pub invoice_id: String,
	pub pay_from: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MandateParams {
	pub request_id: String,
}

/// What the person is shown and confirms, computed by the BFF from the hub's own records.
/// `None` is "the hub does not say", never a guess.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Summary {
	pub amount: Option<String>,
	pub fee: Option<String>,
	pub net: Option<String>,
	pub recipient_full: String,
	pub network: Option<String>,
	pub scope: String,
	pub host_origin: String,
	pub mandate: Option<MandateTerms>,
}

/// A mandate's caps as the service's stored request names them (decimal USDT).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MandateTerms {
	pub client_id: String,
	pub client_display_name: String,
	pub cap_per_op: String,
	pub cap_24h: String,
	pub cap_outstanding: String,
	pub cap_book_fill_24h: String,
	pub operations: Vec<String>,
	/// Unix seconds; 0 = until revoked.
	pub mandate_expires_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Stepup {
	pub required: bool,
	pub methods: Vec<StepupMethod>,
}

/// The session that created the intent, as its access token names it. Concierge's access
/// token carries no session id; `auth_time` is the last interactive sign-in of the session
/// and survives a refresh, and `token_version` moves when every session is revoked.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionBinding {
	pub token_version: u64,
	pub auth_time: Option<u64>,
}

/// The stored intent. Field order is the hashed byte order; do not reorder.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IntentRecord {
	/// 128 random bits, lowercase hex. Also the hub's `idempotency_key` and the audit
	/// `intent_id`, so the one value that names the act names it everywhere.
	pub intent_id: String,
	pub sub: String,
	pub origin: String,
	pub scope: String,
	pub action: Action,
	pub summary: Summary,
	pub stepup: Stepup,
	pub created_at: u64,
	pub expires_at: u64,
	pub session: SessionBinding,
}

impl IntentRecord {
	/// The bytes stored and hashed.
	pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
		serde_json::to_vec(self)
	}
}

/// The hash a proof binds to: over a label and the stored bytes, which hold the random
/// `intent_id` and every term the person confirms.
pub fn intent_hash(record_bytes: &[u8]) -> [u8; 32] {
	keys::sha256(&[HASH_LABEL, record_bytes])
}

/// A random intent id: 16 bytes from the OS, in hex.
pub fn new_intent_id() -> Result<String, getrandom::Error> {
	let mut bytes = [0u8; 16];
	getrandom::fill(&mut bytes)?;
	Ok(keys::hex(&bytes))
}

/// Whether `raw` is the shape [`new_intent_id`] mints, so a path segment of any other shape
/// never reaches the store.
pub fn is_intent_id(raw: &str) -> bool {
	raw.len() == 32 && raw.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The risk class of spec (h), as the step-up it demands. `amount` is what leaves the
/// caller's hands (`None` for a mandate, which moves nothing now); `moved_24h` is what the
/// caller already moved through this host in the last 24 h.
pub fn stepup_for(kind: IntentKind, amount: Option<Usdt>, moved_24h: Usdt, policy: &RiskPolicy) -> Stepup {
	let passkey_only = || Stepup {
		required: true,
		methods: vec![StepupMethod::Passkey],
	};
	// The email method rests on a mailbox and on a summary the page writes itself, so it
	// authorises only small amounts; above the cap, and for anything unreadable, a passkey.
	let by_amount = |amount: Option<Usdt>| match amount {
		Some(amount) if amount <= policy.email_cap => Stepup {
			required: true,
			methods: vec![StepupMethod::Passkey, StepupMethod::EmailCode],
		},
		_ => passkey_only(),
	};
	match kind {
		IntentKind::Withdraw | IntentKind::Mandate => passkey_only(),
		IntentKind::Pay => by_amount(amount),
		IntentKind::Move => {
			let within = amount.is_some_and(|amount| amount <= policy.no_stepup_per_op && moved_24h.checked_add(amount).is_some_and(|total| total <= policy.no_stepup_24h));
			if within {
				Stepup {
					required: false,
					methods: Vec::new(),
				}
			} else {
				by_amount(amount)
			}
		}
	}
}

/// `amount` in whole micro-USDT, rounded up: the unit the shared move budget counts in, small
/// enough for Redis's Lua numbers to stay exact.
pub fn micro_ceil(amount: Usdt) -> u64 {
	const PER_MICRO: u128 = 1_000_000_000_000;
	u64::try_from(amount.base_units().div_ceil(PER_MICRO)).unwrap_or(u64::MAX)
}

/// `amount` in whole micro-USDT, rounded down: a cap is never read as more than it says.
pub fn micro_floor(amount: Usdt) -> u64 {
	const PER_MICRO: u128 = 1_000_000_000_000;
	u64::try_from(amount.base_units() / PER_MICRO).unwrap_or(u64::MAX)
}

pub fn from_micro(micro: u64) -> Usdt {
	Usdt::from_base_units(u128::from(micro) * 1_000_000_000_000)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn usdt(raw: &str) -> Usdt {
		Usdt::parse_decimal(raw).expect("a test amount")
	}

	fn policy() -> RiskPolicy {
		RiskPolicy {
			no_stepup_per_op: usdt("100"),
			no_stepup_24h: usdt("300"),
			email_cap: usdt("250"),
		}
	}

	#[test]
	fn the_risk_classes_of_the_spec() {
		let p = policy();
		let none = Stepup { required: false, methods: vec![] };
		let either = Stepup {
			required: true,
			methods: vec![StepupMethod::Passkey, StepupMethod::EmailCode],
		};
		let passkey = Stepup {
			required: true,
			methods: vec![StepupMethod::Passkey],
		};
		assert_eq!(stepup_for(IntentKind::Move, Some(usdt("100")), usdt("200"), &p), none, "exactly at both lines");
		assert_eq!(stepup_for(IntentKind::Move, Some(usdt("100.000001")), Usdt::ZERO, &p), either, "over the per-op line");
		assert_eq!(stepup_for(IntentKind::Move, Some(usdt("1")), usdt("299.5"), &p), either, "over the 24 h line");
		assert_eq!(stepup_for(IntentKind::Move, Some(usdt("250.01")), Usdt::ZERO, &p), passkey, "over the email cap");
		assert_eq!(stepup_for(IntentKind::Move, None, Usdt::ZERO, &p), passkey, "an amount nobody could read");
		assert_eq!(stepup_for(IntentKind::Pay, Some(usdt("1")), Usdt::ZERO, &p), either, "pay always steps up");
		assert_eq!(stepup_for(IntentKind::Pay, Some(usdt("250")), Usdt::ZERO, &p), either);
		assert_eq!(stepup_for(IntentKind::Pay, Some(usdt("251")), Usdt::ZERO, &p), passkey);
		assert_eq!(stepup_for(IntentKind::Withdraw, Some(usdt("1")), Usdt::ZERO, &p), passkey);
		assert_eq!(stepup_for(IntentKind::Mandate, None, Usdt::ZERO, &p), passkey);
	}

	#[test]
	fn micro_units_round_toward_the_safe_side() {
		assert_eq!(micro_ceil(usdt("1")), 1_000_000);
		assert_eq!(micro_ceil(usdt("0.0000001")), 1, "a dust amount still counts");
		assert_eq!(micro_floor(usdt("0.0000019")), 1);
		assert_eq!(from_micro(micro_ceil(usdt("12.5"))), usdt("12.5"));
	}

	#[test]
	fn an_unknown_field_or_kind_does_not_parse() {
		let parse = |raw: &str| serde_json::from_str::<Action>(raw);
		assert!(parse(r#"{"kind":"move","params":{"from_scope":"main","to_scope":"sa","amount":"1"}}"#).is_ok());
		assert!(parse(r#"{"kind":"move","params":{"from_scope":"main","to_scope":"sa","amount":"1","scope":"invest"}}"#).is_err());
		assert!(parse(r#"{"kind":"move","params":{"from_scope":"main","to_scope":"sa","amount":"1"},"scope":"invest"}"#).is_err());
		assert!(parse(r#"{"kind":"transfer","params":{}}"#).is_err());
		assert!(parse(r#"{"kind":"pay","params":{"invoice_id":"i","pay_from":"main","amount":"0.01"}}"#).is_err());
	}

	#[test]
	fn intent_ids_are_random_and_well_formed() {
		let (a, b) = (new_intent_id().unwrap(), new_intent_id().unwrap());
		assert_ne!(a, b);
		assert!(is_intent_id(&a), "{a}");
		for bad in ["", "ABCDEF0123456789abcdef0123456789", "../../etc", &format!("{a}0")] {
			assert!(!is_intent_id(bad), "{bad}");
		}
	}
}
