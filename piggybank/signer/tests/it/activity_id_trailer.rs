//! The `turnkey-activity-id` trailer is the #195 discriminator for an activity parked for
//! human consensus (banking#196). These pin the wire contract from outside the crate:
//! `classify` itself is private and covered next to it in `turnkey.rs`.

use piggybank_signer::backend::{ACTIVITY_ID_METADATA_KEY, BackendError};
use tonic::Status;
use uuid::Uuid;

/// `FailedPrecondition` is shared with the signer's own gates, so the trailer — not the
/// code — tells a parked activity apart. Every other variant must leave it off, not only
/// the one `FailedPrecondition` sibling.
#[test]
fn no_other_backend_error_carries_the_activity_id_trailer() {
	for err in [
		BackendError::NotProvisioned,
		BackendError::KeyUnusable,
		BackendError::Signing,
		BackendError::Unavailable("outage".into()),
		BackendError::Rejected("policy".into()),
		BackendError::Protocol("drift".into()),
		BackendError::WrongBackend { stored: "local".into() },
	] {
		let label = format!("{err:?}");
		let status = Status::from(err);
		assert!(status.metadata().get(ACTIVITY_ID_METADATA_KEY).is_none(), "{label} must not carry the trailer");
	}
}

/// `Uuid::parse_str` accepts simple, braced, urn and uppercase spellings; whatever the
/// vendor sends, the message and the trailer carry one canonical form an operator can
/// paste into the custodian's console as-is.
#[test]
fn the_trailer_is_the_canonical_hyphenated_lowercase_form_whatever_the_vendor_spelling() {
	const CANONICAL: &str = "0f6a2b3c-4d5e-4f70-8a9b-0c1d2e3f4a5b";
	for spelling in [
		"0f6a2b3c4d5e4f708a9b0c1d2e3f4a5b",
		"{0f6a2b3c-4d5e-4f70-8a9b-0c1d2e3f4a5b}",
		"urn:uuid:0f6a2b3c-4d5e-4f70-8a9b-0c1d2e3f4a5b",
		"0F6A2B3C-4D5E-4F70-8A9B-0C1D2E3F4A5B",
	] {
		let activity_id = Uuid::parse_str(spelling).expect("parse_str accepts this spelling");
		let status = Status::from(BackendError::RequiresApproval { activity_id });
		assert_eq!(
			status.metadata().get(ACTIVITY_ID_METADATA_KEY).and_then(|v| v.to_str().ok()),
			Some(CANONICAL),
			"spelling {spelling}"
		);
		assert_eq!(status.message(), format!("key custodian requires approval for activity {CANONICAL}"), "spelling {spelling}");
	}
}
