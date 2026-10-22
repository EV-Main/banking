//! The one seam over which the money plane asks the identity plane to send a governance
//! mail.
//!
//! WHY NOT SEND IT HERE. Concierge owns the only mailer on the platform — the transport,
//! the delivery queue, the backoff and the daily budget — and the house rule forbids
//! hand-wiring a second vendor SDK. So banking hands over a **typed** payload and concierge
//! renders it. Never rendered HTML: a compromised money plane must not become a way to put
//! arbitrary markup into an owner's mailbox.
//!
//! WHY THE PAYLOADS ARE OUR OWN TYPES. These structs are the shape of a queue row, not the
//! wire. The adapter maps them onto the generated `concierge.v1` messages at the boundary,
//! so the queue's JSON is stable across a contract regeneration and nothing here pretends
//! to be a protobuf.

use async_trait::async_trait;
use domain::error::DomainError;
use serde::{Deserialize, Serialize};

/// One governance mail, addressed to a single person.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GovernanceMail {
	/// Telling the audience how a consilium ended, whatever its kind.
	PayoutOutcome(PayoutOutcome),
	/// Warning every owner that a token burned on failed code attempts.
	TokenBurned(PayoutOutcome),
	/// Asking ONE investor to consent to a payment out of their own claim. Carries secrets,
	/// and is the one kind addressed by identity rather than by seat.
	PaymentConsent(PaymentConsent),
	/// Asking an owner to approve a payment out of fund-owned money. Carries secrets.
	PaymentApproval(PaymentApproval),
	/// Asking an owner to approve a change of a product's fee terms. Carries secrets.
	FeePolicyApproval(FeePolicyApproval),
	/// Telling ONE unit holder that the terms of a product they hold will change, and when.
	/// Addressed by identity, like [`Self::PaymentConsent`]; carries no secret.
	FeePolicyNotice(FeePolicyNotice),
	/// Telling a payment's subject, and the staff member who opened it, that the order died
	/// because its consent could no longer be given — burned or invalidated. Carries no secret.
	PaymentOutcome(PaymentOutcome),
}

impl GovernanceMail {
	/// The stored discriminant, matching the `consilium_mail.kind` CHECK.
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::PayoutOutcome(_) => "payout_outcome",
			Self::TokenBurned(_) => "token_burned",
			Self::PaymentConsent(_) => "payment_consent",
			Self::PaymentApproval(_) => "payment_approval",
			Self::FeePolicyApproval(_) => "fee_policy_approval",
			Self::FeePolicyNotice(_) => "fee_policy_notice",
			Self::PaymentOutcome(_) => "payment_outcome",
		}
	}

	/// Whether this mail hands its recipient a token to answer with — the kinds whose
	/// delivery is what a seat's `notified` flag reports. An outcome, burn or fee notice
	/// tells the recipient nothing about whether they can vote, so it flips nothing.
	pub fn carries_a_token(&self) -> bool {
		match self {
			Self::PaymentConsent(_) | Self::PaymentApproval(_) | Self::FeePolicyApproval(_) => true,
			Self::PayoutOutcome(_) | Self::TokenBurned(_) | Self::FeePolicyNotice(_) | Self::PaymentOutcome(_) => false,
		}
	}

	/// The same mail with its secrets removed, written back over the queue row once the
	/// message has been handed to concierge. The plaintext code exists for exactly as long
	/// as it takes to deliver it and no longer.
	pub fn redacted(&self) -> Self {
		match self {
			Self::PaymentConsent(mail) => Self::PaymentConsent(PaymentConsent {
				approval_url: String::new(),
				code: String::new(),
				..mail.clone()
			}),
			Self::PaymentApproval(mail) => Self::PaymentApproval(PaymentApproval {
				approval_url: String::new(),
				code: String::new(),
				..mail.clone()
			}),
			Self::FeePolicyApproval(mail) => Self::FeePolicyApproval(FeePolicyApproval {
				approval_url: String::new(),
				code: String::new(),
				..mail.clone()
			}),
			Self::PayoutOutcome(_) | Self::TokenBurned(_) | Self::FeePolicyNotice(_) | Self::PaymentOutcome(_) => self.clone(),
		}
	}
}

/// One set of fee terms as a mail states them — the five fields, in the vocabulary the
/// policy stores them in (`invested_capital` / `market_value`, `monthly` … `annual`).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FeePolicyTerms {
	pub management_bps: u32,
	pub performance_bps: u32,
	pub hurdle_bps: u32,
	pub basis: String,
	pub crystallization: String,
}

/// The owner-facing approval invitation over a change of fee terms: which fund, what the
/// terms are now, what they would become, and the initiator's stated reason.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FeePolicyApproval {
	pub consilium_id: String,
	pub initiator_email: String,
	/// The product's DISPLAY name, never its slug — the owner is judging a fund, not a key.
	pub fund: String,
	/// The terms in force; `None` when the fund charged nothing, which the template states
	/// differently from a policy whose rates happen to be zero.
	pub current: Option<FeePolicyTerms>,
	pub proposed: FeePolicyTerms,
	/// Why, in the initiator's words. Required and non-empty — concierge refuses the mail
	/// without one.
	pub reason: String,
	pub payload_hash: String,
	pub threshold: u32,
	pub owner_count: u32,
	pub expires_at: i64,
	/// Absolute URL of the approval page, carrying the opaque token.
	pub approval_url: String,
	/// The secret code. Cleared from the queue row on success.
	pub code: String,
}

/// The notice to one holder that the terms of a product they hold will change.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FeePolicyNotice {
	/// The holder's id IN THE IDENTITY PLANE — the same rule as a consent: concierge refuses
	/// the mail unless this is the addressee, so a notice cannot be redirected. Filled in by
	/// the worker from the recipient's mirror at send time (the queue row may predate it);
	/// the value written at enqueue is informational.
	pub subject_user_id: String,
	/// The product's display name.
	pub fund: String,
	pub current: Option<FeePolicyTerms>,
	pub proposed: FeePolicyTerms,
	/// Unix seconds at which the proposed terms bind.
	pub effective_at: i64,
	/// Cabinet-relative path of the product page (`/invest/<service>`), single leading `/`.
	/// Concierge joins it onto the invest cabinet (the mail names the scope), so this plane
	/// never mints a host.
	pub link: String,
}

/// How a consilium ended, or why a token burned.
///
/// ONE shape for four subjects, additively — the wire's `PayoutOutcomeMail` is the same:
/// `network` + `address` describe a payout, `tier` + `source` + `destination` + `reason`
/// describe a payment, `fund` + `proposed` (+ `current`, `reason`) describe a change of fee
/// terms, `fund` + `mark` (+ `reason`) describe a NAV mark, and the renderer switches on
/// which description is filled — and refuses two. The payment, the fee and the mark fields
/// default to empty so queue rows written before they existed still deserialize.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PayoutOutcome {
	pub consilium_id: String,
	/// APPROVED / REJECTED / EXPIRED / CANCELLED / EXECUTED / EXECUTION_FAILED, or
	/// TOKEN_BURNED for the burn notice.
	pub outcome: String,
	pub network: String,
	pub address: String,
	pub amount: String,
	pub detail: String,
	#[serde(default)]
	pub tier: String,
	#[serde(default)]
	pub source: String,
	#[serde(default)]
	pub destination: String,
	#[serde(default)]
	pub reason: String,
	/// The product's display name, as [`FeePolicyApproval::fund`] spells it — for a change
	/// of fee terms and for a NAV mark. Empty for a payout and for a payment.
	#[serde(default)]
	pub fund: String,
	/// The terms in force when the change was proposed; `None` when the fund charged
	/// nothing — and `None` for the other three subjects.
	#[serde(default)]
	pub current: Option<FeePolicyTerms>,
	/// The terms the consilium was over. Concierge refuses a `fund` without it — unless
	/// `mark` is filled instead.
	#[serde(default)]
	pub proposed: Option<FeePolicyTerms>,
	/// The value a valuation override records, spelled as an amount is (`AUM 1000 USDT`).
	/// Concierge refuses it without a `fund`. Empty for the other three subjects.
	#[serde(default)]
	pub mark: String,
}

/// The consent invitation to the ONE investor whose claim a payment spends.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PaymentConsent {
	pub payment_id: String,
	/// The subject's id IN THE IDENTITY PLANE. Concierge refuses the mail unless the user it
	/// is being asked to write to is this person — a caller that fans one consent out to a
	/// second mailbox has to contradict itself in the same message to do it.
	pub subject_user_id: String,
	pub initiator_email: String,
	pub tier: String,
	pub source: String,
	pub destination: String,
	pub amount: String,
	pub reason: String,
	pub payload_hash: String,
	pub expires_at: i64,
	/// Absolute URL of the consent page, carrying the opaque token.
	pub approval_url: String,
	/// The secret code. Cleared from the queue row on success.
	pub code: String,
}

/// The owner-facing approval invitation over a PAYMENT — the consilium counterpart of
/// [`PaymentConsent`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PaymentApproval {
	pub consilium_id: String,
	pub payment_id: String,
	pub initiator_email: String,
	pub tier: String,
	pub source: String,
	pub destination: String,
	pub amount: String,
	pub reason: String,
	pub payload_hash: String,
	pub threshold: u32,
	pub owner_count: u32,
	pub expires_at: i64,
	/// Absolute URL of the approval page, carrying the opaque token.
	pub approval_url: String,
	/// The secret code. Cleared from the queue row on success.
	pub code: String,
}

/// A payment cancelled because its subject's consent could no longer be given (#238).
///
/// Its own kind rather than a [`PayoutOutcome`]: that one is the consilium's record and
/// concierge addresses it to seated owners only, while the people this one is FOR — the
/// investor whose money it was, and the admin who opened the order — hold no seat. Nothing
/// here is a secret and nothing may be a link.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PaymentOutcome {
	/// The SUBJECT's id in the identity plane, on every copy — the initiator's included.
	/// Concierge sends a copy only to this person or to an admin/owner, and decides by it
	/// which of the two audiences a copy is written for.
	pub subject_user_id: String,
	/// `TOKEN_BURNED` or `INVALIDATED` — a closed word concierge phrases itself.
	pub outcome: String,
	/// `WRONG_CODES` for a burn; `SESSIONS_REVOKED` or `EMAIL_CHANGED` for an invalidation.
	/// Concierge refuses any other pairing.
	pub reason: String,
	pub tier: String,
	pub source: String,
	pub destination: String,
	/// Money and nothing else — a number, one space, a currency code (`1200.5 USDT`): it is
	/// the one money-plane string in the subject line, and concierge refuses any other shape.
	pub amount: String,
	pub payment_id: String,
}

/// Why a mail was not taken — split by what the worker should do about it.
///
/// The identity plane rate-limits governance mail per recipient and answers
/// `RESOURCE_EXHAUSTED`; it can also simply be unreachable. Neither says anything about
/// the message, so neither may spend one of the message's attempts: a recipient who is
/// throttled ten times in five minutes would otherwise lose their approval token for good
/// while the mechanism reported a delivery failure that never happened.
#[derive(Debug)]
pub enum MailDeliveryError {
	/// Try again later, charging nothing: the relay is throttling this recipient or is down.
	Deferred(String),
	/// The relay refused this message, or the transport failed in a way a retry may fix;
	/// each such answer costs an attempt.
	Failed(String),
}

impl core::fmt::Display for MailDeliveryError {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		match self {
			Self::Deferred(why) | Self::Failed(why) => f.write_str(why),
		}
	}
}

impl From<MailDeliveryError> for DomainError {
	fn from(err: MailDeliveryError) -> Self {
		DomainError::Repository(err.to_string())
	}
}

/// The driven port: hand one mail to the identity plane's mailer.
///
/// `dedupe_key` makes the call idempotent on concierge's side, so the retrying worker
/// behind this port may redeliver freely without sending twice. `concierge_user_id` is the
/// recipient's id **in the plane that owns identities** — the address is resolved there,
/// which is what stops the money plane from redirecting a governance mail.
#[async_trait]
pub trait GovernanceMailer: Send + Sync {
	async fn send(&self, concierge_user_id: uuid::Uuid, dedupe_key: &str, mail: &GovernanceMail) -> Result<(), MailDeliveryError>;
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The two JSON keys the queue blanks in SQL when an invitation is withdrawn
	/// (`consilium_mailer::withdraw_undelivered_invitations`) are exactly the ones
	/// [`GovernanceMail::redacted`] blanks, on every kind that carries a token; a kind that
	/// renamed either field would have its secret survive the withdrawal.
	#[test]
	fn redacting_a_token_bearing_mail_blanks_approval_url_and_code_and_nothing_else() {
		let secret = |mail: GovernanceMail| {
			assert!(mail.carries_a_token());
			let before = serde_json::to_value(&mail).unwrap();
			assert_eq!(before["approval_url"], "https://example.test/a/t0k3n");
			assert_eq!(before["code"], "ABCDEFGHJK");
			let after = serde_json::to_value(mail.redacted()).unwrap();
			assert_eq!(after["approval_url"], "");
			assert_eq!(after["code"], "");
			let (before, after) = (before.as_object().unwrap(), after.as_object().unwrap());
			assert_eq!(before.len(), after.len(), "redaction removes no key");
			for (key, value) in before {
				if key != "approval_url" && key != "code" {
					assert_eq!(&after[key], value, "{key} is not a secret and must survive");
				}
			}
		};
		let url = || "https://example.test/a/t0k3n".to_owned();
		let code = || "ABCDEFGHJK".to_owned();
		let terms = || FeePolicyTerms {
			management_bps: 200,
			performance_bps: 2_000,
			hurdle_bps: 0,
			basis: "invested_capital".to_owned(),
			crystallization: "annual".to_owned(),
		};
		secret(GovernanceMail::PaymentConsent(PaymentConsent {
			payment_id: "p".to_owned(),
			subject_user_id: "u".to_owned(),
			initiator_email: "o@example.test".to_owned(),
			tier: "user".to_owned(),
			source: "s".to_owned(),
			destination: "d".to_owned(),
			amount: "1".to_owned(),
			reason: String::new(),
			payload_hash: "h".to_owned(),
			expires_at: 1,
			approval_url: url(),
			code: code(),
		}));
		secret(GovernanceMail::PaymentApproval(PaymentApproval {
			consilium_id: "c".to_owned(),
			payment_id: "p".to_owned(),
			initiator_email: "o@example.test".to_owned(),
			tier: "service".to_owned(),
			source: "s".to_owned(),
			destination: "d".to_owned(),
			amount: "1".to_owned(),
			reason: String::new(),
			payload_hash: "h".to_owned(),
			threshold: 2,
			owner_count: 3,
			expires_at: 1,
			approval_url: url(),
			code: code(),
		}));
		secret(GovernanceMail::FeePolicyApproval(FeePolicyApproval {
			consilium_id: "c".to_owned(),
			initiator_email: "o@example.test".to_owned(),
			fund: "EV Trading".to_owned(),
			current: None,
			proposed: terms(),
			reason: String::new(),
			payload_hash: "h".to_owned(),
			threshold: 2,
			owner_count: 3,
			expires_at: 1,
			approval_url: url(),
			code: code(),
		}));
	}

	/// The consent-outcome notice is queued under the kind the 0048 CHECK admits, hands its
	/// recipient nothing to answer with — so delivering it flips no seat's `notified` — and
	/// holds nothing redaction would have to strip.
	#[test]
	fn a_payment_outcome_is_its_own_kind_and_carries_no_secret() {
		let mail = GovernanceMail::PaymentOutcome(PaymentOutcome {
			subject_user_id: "u".to_owned(),
			outcome: "INVALIDATED".to_owned(),
			reason: "EMAIL_CHANGED".to_owned(),
			tier: "external".to_owned(),
			source: "s".to_owned(),
			destination: "d".to_owned(),
			amount: "1 USDT".to_owned(),
			payment_id: "p".to_owned(),
		});
		assert_eq!(mail.as_str(), "payment_outcome");
		assert!(!mail.carries_a_token());
		let json = serde_json::to_value(&mail).unwrap();
		assert_eq!(json["kind"], "payment_outcome", "the queue row's tag is the stored kind");
		assert!(json.get("approval_url").is_none() && json.get("code").is_none());
		assert_eq!(serde_json::to_value(mail.redacted()).unwrap(), json);
		let back: GovernanceMail = serde_json::from_value(json).unwrap();
		assert!(matches!(back, GovernanceMail::PaymentOutcome(ref outcome) if outcome.reason == "EMAIL_CHANGED"));
	}
}
