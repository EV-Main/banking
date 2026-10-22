//! Driven adapter for the [`GovernanceMailer`] port — concierge's
//! `MailRelayService.SendGovernanceMail`.
//!
//! # THE PIN PROBLEM — READ BEFORE TOUCHING THIS FILE
//!
//! `governance.proto` (which declares `MailRelayService`) lives in the concierge repo and
//! has **not merged yet**. Banking pins `evconcierge_contracts` to a git rev that does not
//! contain it, so `evconcierge_contracts::concierge::v1::mail_relay_service_client` does
//! **not exist** in this build. Fabricating the wire types locally would compile and then
//! diverge silently the moment the real proto changed a field number, so this file does not
//! do that.
//!
//! The transport lives behind the `concierge_governance_mail` feature, which is now **on by
//! default**: the pin moved to the concierge rev that ships
//! `contracts/proto/concierge/v1/governance.proto`, so the seam compiles against the real
//! generated stubs rather than a hand-written guess.
//!
//! The feature survives as a build-off switch, not as a silent one. A build without it wires
//! no mailer, and every `Open*` consilium RPC then **refuses** rather than opening a consilium nobody
//! can be emailed about — a request that could never reach quorum is worse than an honest
//! refusal. Every mail is still written in the same transaction as the consilium fact it
//! announces, so a mailer outage queues rather than loses.

use domain::error::DomainError;

use crate::ports::governance_mail::GovernanceMail;

/// The outcome vocabulary the concierge templates switch on. Kept beside the mapping so the
/// two planes' understanding of "what kind of mail is this" stays in one place.
pub fn mail_kind_str(mail: &GovernanceMail) -> &'static str {
	match mail {
		GovernanceMail::PayoutOutcome(_) => "PAYOUT_OUTCOME",
		GovernanceMail::TokenBurned(_) => "APPROVAL_TOKEN_BURNED",
		GovernanceMail::PaymentConsent(_) => "PAYMENT_CONSENT",
		GovernanceMail::PaymentApproval(_) => "PAYMENT_APPROVAL",
		GovernanceMail::FeePolicyApproval(_) => "FEE_POLICY_APPROVAL",
		GovernanceMail::FeePolicyNotice(_) => "FEE_POLICY_NOTICE",
		GovernanceMail::PaymentOutcome(_) => "PAYMENT_OUTCOME",
	}
}

/// Whether the governance-mail seam is compiled in at all. `false` on the default build —
/// see the module docs.
pub const fn is_wired() -> bool {
	cfg!(feature = "concierge_governance_mail")
}

/// The gRPC adapter. Compiled only once the pin carries `governance.proto`.
///
/// The mapping below is written against the frozen contract in the concierge worktree
/// (`contracts/proto/concierge/v1/governance.proto`); it is deliberately explicit, field by
/// field, so that a contract change becomes a compile error here rather than a mail that
/// renders wrong.
#[cfg(feature = "concierge_governance_mail")]
pub mod wired {
	use async_trait::async_trait;
	use evconcierge_contracts::concierge::v1::{
		FeePolicyApprovalMail, FeePolicyNoticeMail, FeeTerms, GovernanceMailKind, PaymentApprovalMail, PaymentConsentMail, PaymentOutcomeMail, PayoutOutcomeMail, SendGovernanceMailRequest,
		mail_relay_service_client::MailRelayServiceClient,
	};
	use tonic::{Code, Request, metadata::MetadataValue, transport::Channel};
	use uuid::Uuid;

	use crate::ports::governance_mail::{FeePolicyTerms, GovernanceMail, GovernanceMailer, MailDeliveryError};

	/// One set of fee terms, queue shape to wire shape. Field by field on purpose: a renamed
	/// or renumbered wire field becomes a compile error here, not a mail rendering wrong.
	fn fee_terms(terms: &FeePolicyTerms) -> FeeTerms {
		FeeTerms {
			management_bps: terms.management_bps,
			performance_bps: terms.performance_bps,
			hurdle_bps: terms.hurdle_bps,
			basis: terms.basis.clone(),
			crystallization: terms.crystallization.clone(),
		}
	}

	/// Calls concierge's mail relay, authenticated with the shared banking↔concierge service
	/// secret — the same `BRIDGE_SERVICE_TOKEN` the one-way lifecycle bridge presents, on the
	/// same channel, because it is the same trust relationship.
	pub struct ConciergeGovernanceMailer {
		channel: Channel,
		service_token: String,
	}

	impl ConciergeGovernanceMailer {
		pub fn new(channel: Channel, service_token: String) -> Self {
			Self { channel, service_token }
		}
	}

	/// The queue row as the relay's request: the kind, and exactly the one payload field that
	/// kind reads.
	pub(crate) fn request_of(concierge_user_id: Uuid, dedupe_key: &str, mail: &GovernanceMail) -> SendGovernanceMailRequest {
		let mut payload = SendGovernanceMailRequest {
			kind: GovernanceMailKind::Unspecified as i32,
			user_id: concierge_user_id.to_string(),
			dedupe_key: dedupe_key.to_owned(),
			payout_approval: None,
			payout_outcome: None,
			payment_consent: None,
			payment_approval: None,
			fee_policy_approval: None,
			fee_policy_notice: None,
			payment_outcome: None,
		};
		match mail {
			// The burn notice shares `payout_outcome`: the contract declares a distinct
			// KIND for it but no payload message of its own, so the outcome shape (with
			// `outcome = TOKEN_BURNED`) is the only carrier available.
			GovernanceMail::PayoutOutcome(outcome) | GovernanceMail::TokenBurned(outcome) => {
				payload.kind = if matches!(mail, GovernanceMail::TokenBurned(_)) {
					GovernanceMailKind::ApprovalTokenBurned as i32
				} else {
					GovernanceMailKind::PayoutOutcome as i32
				};
				payload.payout_outcome = Some(PayoutOutcomeMail {
					consilium_id: outcome.consilium_id.clone(),
					outcome: outcome.outcome.clone(),
					network: outcome.network.clone(),
					address: outcome.address.clone(),
					amount: outcome.amount.clone(),
					detail: outcome.detail.clone(),
					tier: outcome.tier.clone(),
					source: outcome.source.clone(),
					destination: outcome.destination.clone(),
					reason: outcome.reason.clone(),
					fund: outcome.fund.clone(),
					current: outcome.current.as_ref().map(fee_terms),
					proposed: outcome.proposed.as_ref().map(fee_terms),
					mark: outcome.mark.clone(),
				});
			}
			GovernanceMail::PaymentConsent(consent) => {
				payload.kind = GovernanceMailKind::PaymentConsent as i32;
				payload.payment_consent = Some(PaymentConsentMail {
					payment_id: consent.payment_id.clone(),
					subject_user_id: consent.subject_user_id.clone(),
					initiator_email: consent.initiator_email.clone(),
					tier: consent.tier.clone(),
					source: consent.source.clone(),
					destination: consent.destination.clone(),
					amount: consent.amount.clone(),
					reason: consent.reason.clone(),
					payload_hash: consent.payload_hash.clone(),
					expires_at: consent.expires_at,
					approval_url: consent.approval_url.clone(),
					code: consent.code.clone(),
				});
			}
			GovernanceMail::PaymentApproval(approval) => {
				payload.kind = GovernanceMailKind::PaymentApproval as i32;
				payload.payment_approval = Some(PaymentApprovalMail {
					consilium_id: approval.consilium_id.clone(),
					payment_id: approval.payment_id.clone(),
					initiator_email: approval.initiator_email.clone(),
					tier: approval.tier.clone(),
					source: approval.source.clone(),
					destination: approval.destination.clone(),
					amount: approval.amount.clone(),
					reason: approval.reason.clone(),
					payload_hash: approval.payload_hash.clone(),
					threshold: approval.threshold,
					owner_count: approval.owner_count,
					expires_at: approval.expires_at,
					approval_url: approval.approval_url.clone(),
					code: approval.code.clone(),
				});
			}
			GovernanceMail::FeePolicyApproval(approval) => {
				payload.kind = GovernanceMailKind::FeePolicyApproval as i32;
				payload.fee_policy_approval = Some(FeePolicyApprovalMail {
					consilium_id: approval.consilium_id.clone(),
					initiator_email: approval.initiator_email.clone(),
					fund: approval.fund.clone(),
					// Absent when the fund charged nothing: the template says so in words,
					// which a zero-rate `FeeTerms` would not.
					current: approval.current.as_ref().map(fee_terms),
					proposed: Some(fee_terms(&approval.proposed)),
					reason: approval.reason.clone(),
					payload_hash: approval.payload_hash.clone(),
					threshold: approval.threshold,
					owner_count: approval.owner_count,
					expires_at: approval.expires_at,
					approval_url: approval.approval_url.clone(),
					code: approval.code.clone(),
				});
			}
			GovernanceMail::FeePolicyNotice(notice) => {
				payload.kind = GovernanceMailKind::FeePolicyNotice as i32;
				payload.fee_policy_notice = Some(FeePolicyNoticeMail {
					subject_user_id: notice.subject_user_id.clone(),
					fund: notice.fund.clone(),
					current: notice.current.as_ref().map(fee_terms),
					proposed: Some(fee_terms(&notice.proposed)),
					effective_at: notice.effective_at,
					link: notice.link.clone(),
					link_scope: crate::infrastructure::fee_policy_changes::PRODUCT_PAGE_SCOPE.to_owned(),
				});
			}
			GovernanceMail::PaymentOutcome(outcome) => {
				payload.kind = GovernanceMailKind::PaymentOutcome as i32;
				payload.payment_outcome = Some(PaymentOutcomeMail {
					subject_user_id: outcome.subject_user_id.clone(),
					outcome: outcome.outcome.clone(),
					reason: outcome.reason.clone(),
					tier: outcome.tier.clone(),
					source: outcome.source.clone(),
					destination: outcome.destination.clone(),
					amount: outcome.amount.clone(),
					payment_id: outcome.payment_id.clone(),
				});
			}
		}
		payload
	}

	#[async_trait]
	impl GovernanceMailer for ConciergeGovernanceMailer {
		async fn send(&self, concierge_user_id: Uuid, dedupe_key: &str, mail: &GovernanceMail) -> Result<(), MailDeliveryError> {
			let mut request = Request::new(request_of(concierge_user_id, dedupe_key, mail));
			let token: MetadataValue<_> = format!("Bearer {}", self.service_token)
				.parse()
				.map_err(|_| MailDeliveryError::Failed("malformed governance mail service token".into()))?;
			request.metadata_mut().insert("authorization", token);
			MailRelayServiceClient::new(self.channel.clone())
				.send_governance_mail(request)
				.await
				.map(|_| ())
				.map_err(|status| match status.code() {
					// The recipient is being rate-limited, or the relay is down: nothing about
					// THIS message was refused, so nothing about it is charged.
					Code::ResourceExhausted | Code::Unavailable => MailDeliveryError::Deferred(format!("governance mail relay: {status}")),
					_ => MailDeliveryError::Failed(format!("governance mail relay: {status}")),
				})
		}
	}

	#[cfg(test)]
	mod tests {
		use super::*;
		use crate::ports::governance_mail::{FeePolicyNotice, PaymentOutcome};

		/// The fee notice opens the product page in the invest cabinet, which serves it.
		#[test]
		fn a_fee_policy_notice_links_into_the_invest_cabinet() {
			let recipient = Uuid::new_v4();
			let terms = FeePolicyTerms {
				management_bps: 200,
				performance_bps: 2000,
				hurdle_bps: 0,
				basis: "invested_capital".to_owned(),
				crystallization: "annual".to_owned(),
			};
			let mail = GovernanceMail::FeePolicyNotice(FeePolicyNotice {
				subject_user_id: recipient.to_string(),
				fund: "Quy Nhon (quy_nhon)".to_owned(),
				current: None,
				proposed: terms,
				effective_at: 1_800_000_000,
				link: "/invest/quy_nhon".to_owned(),
			});
			let request = request_of(recipient, "fee-policy-notice:c:u", &mail);
			assert_eq!(request.kind, GovernanceMailKind::FeePolicyNotice as i32);
			let payload = request.fee_policy_notice.as_ref().expect("the fee_policy_notice field is set");
			assert_eq!((payload.link.as_str(), payload.link_scope.as_str()), ("/invest/quy_nhon", "invest"));
		}

		/// The consent-outcome kind reaches the relay as `PAYMENT_OUTCOME` (8) with its own
		/// payload (field 10) and no other: concierge reads the payload the kind names, so a
		/// mail filed under another field would be refused as missing its payload.
		#[test]
		fn a_payment_outcome_is_sent_as_its_own_kind_with_only_its_own_payload() {
			let recipient = Uuid::new_v4();
			let mail = GovernanceMail::PaymentOutcome(PaymentOutcome {
				subject_user_id: recipient.to_string(),
				outcome: "TOKEN_BURNED".to_owned(),
				reason: "WRONG_CODES".to_owned(),
				tier: "internal".to_owned(),
				source: "investor 8f3e…".to_owned(),
				destination: "fee".to_owned(),
				amount: "12.5 USDT".to_owned(),
				payment_id: Uuid::nil().to_string(),
			});
			let request = request_of(recipient, "payment:p:outcome:u", &mail);
			assert_eq!(request.kind, GovernanceMailKind::PaymentOutcome as i32);
			assert_eq!(request.kind, 8, "the wire number concierge#95 froze");
			assert_eq!(request.user_id, recipient.to_string());
			assert_eq!(request.dedupe_key, "payment:p:outcome:u");
			let payload = request.payment_outcome.as_ref().expect("the payment_outcome field is set");
			assert_eq!((payload.outcome.as_str(), payload.reason.as_str()), ("TOKEN_BURNED", "WRONG_CODES"));
			assert_eq!(payload.subject_user_id, recipient.to_string());
			assert_eq!(payload.amount, "12.5 USDT");
			assert_eq!(payload.payment_id, Uuid::nil().to_string());
			assert!(
				request.payout_approval.is_none()
					&& request.payout_outcome.is_none()
					&& request.payment_consent.is_none()
					&& request.payment_approval.is_none()
					&& request.fee_policy_approval.is_none()
					&& request.fee_policy_notice.is_none(),
				"exactly one payload rides a request"
			);
		}
	}
}

/// The reason a default build has no mailer, in one place so the boot log and the worker
/// agree on the wording.
pub fn unwired_reason() -> DomainError {
	DomainError::Repository("the concierge governance-mail seam is not compiled in (evconcierge_contracts predates governance.proto)".into())
}

/// Log the seam's state once at boot. An operator must never discover from an owner that no
/// approval mail went out, so an unwired build says so loudly and says what fixes it.
pub fn announce_wiring(pending: i64) {
	if is_wired() {
		tracing::info!("consilium: governance mail relay wired");
	} else {
		tracing::error!(
			pending,
			"consilium: governance mail is NOT wired — approval mails are being QUEUED BUT NOT SENT. \
			 No owner can receive an approval link, so no payout consilium can reach quorum. \
			 Fix: bump evconcierge_contracts to the rev shipping concierge/v1/governance.proto and \
			 enable the piggybank-core `concierge_governance_mail` feature."
		);
	}
}
