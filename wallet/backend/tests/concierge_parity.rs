//! FB-24 / BANK-COMM-1, BANK-COMM-2: the cabinet identity surface is byte-straddled —
//! the browser TS is aliased to concierge's `UserProfile`/`Session` (the serving plane),
//! while the BFF `dto.rs` maps concierge's gRPC, and banking's own proto carries a
//! duplicate copy of the same messages for its admin/operator path. Those duplicates MUST
//! stay wire-compatible or the duplicated-shape skew the audit flagged becomes real.
//!
//! Pure wire test: a prost encode-as-concierge → decode-as-banking round trip (and back)
//! proves the duplicated `UserProfile`/`Session`/`UserSummary` messages share field
//! numbers AND field types. A field add/rename/retype on either side that drops or
//! mis-decodes a value fails this. No DB, no services.

use evbanking_contracts::banking::v1 as bk;
use evconcierge_contracts::concierge::v1 as cc;
use prost::Message;

#[test]
fn user_profile_is_wire_identical_across_planes() {
	let cc_profile = cc::UserProfile {
		user_id: "u-1".into(),
		email: "a@b.c".into(),
		email_verified: true,
		status: "active".into(),
		token_version: 7,
		legal_name: "Ada Lovelace".into(),
		preferred_name: "Ada".into(),
		phone: "+10000000000".into(),
		date_of_birth: "1815-12-10".into(),
		nationality: "GB".into(),
		tax_residence: "GB".into(),
		residential_address: "1 Analytical Engine Way".into(),
		language: "en".into(),
		base_currency: "USDT".into(),
		timezone: "Europe/London".into(),
		kyc_level: 2,
		role: "admin".into(),
		// Deliberately TRUE: `false` is the decode default, so a dropped field would still
		// compare equal and the parity assertion below would pass vacuously.
		role_is_break_glass: true,
		// Non-default for the same reason, and these two are why banking's duplicate had
		// to grow fields 19/20: prost discards unknown fields, so a concierge-only field
		// re-encodes SHORT from the banking side and the byte assertion below catches it.
		suspended_by: "admin_hold".into(),
		hold_expires_at: 1_700_086_400,
		// Empty on purpose, the one field banking's duplicate does NOT mirror: concierge
		// fills it on GetMe only, and the cabinet reads it from there. The money plane's
		// copy serves its admin/operator path, where it never travels.
		permissions: Vec::new(),
		username: "ada".into(),
	};

	let bk_profile = bk::UserProfile::decode(cc_profile.encode_to_vec().as_slice()).expect("concierge UserProfile decodes as banking UserProfile");

	assert_eq!(bk_profile.user_id, cc_profile.user_id);
	assert_eq!(bk_profile.email, cc_profile.email);
	assert_eq!(bk_profile.email_verified, cc_profile.email_verified);
	assert_eq!(bk_profile.status, cc_profile.status);
	assert_eq!(bk_profile.token_version, cc_profile.token_version);
	assert_eq!(bk_profile.legal_name, cc_profile.legal_name);
	assert_eq!(bk_profile.preferred_name, cc_profile.preferred_name);
	assert_eq!(bk_profile.phone, cc_profile.phone);
	assert_eq!(bk_profile.date_of_birth, cc_profile.date_of_birth);
	assert_eq!(bk_profile.nationality, cc_profile.nationality);
	assert_eq!(bk_profile.tax_residence, cc_profile.tax_residence);
	assert_eq!(bk_profile.residential_address, cc_profile.residential_address);
	assert_eq!(bk_profile.language, cc_profile.language);
	assert_eq!(bk_profile.base_currency, cc_profile.base_currency);
	assert_eq!(bk_profile.timezone, cc_profile.timezone);
	assert_eq!(bk_profile.kyc_level, cc_profile.kyc_level);
	assert_eq!(bk_profile.role, cc_profile.role);
	assert_eq!(bk_profile.role_is_break_glass, cc_profile.role_is_break_glass);
	assert_eq!(bk_profile.suspended_by, cc_profile.suspended_by);
	assert_eq!(bk_profile.hold_expires_at, cc_profile.hold_expires_at);
	assert_eq!(bk_profile.username, cc_profile.username);

	// Re-encoding from banking must reproduce concierge's exact bytes — no field added on
	// one side that the other silently drops.
	assert_eq!(bk_profile.encode_to_vec(), cc_profile.encode_to_vec());
}

#[test]
fn session_is_wire_identical_across_planes() {
	let cc_session = cc::Session {
		id: "s-1".into(),
		user_agent: "ua".into(),
		ip: "203.0.113.1".into(),
		created_at: 1_700_000_000,
		last_seen: 1_700_000_500,
		current: true,
	};

	let bk_session = bk::Session::decode(cc_session.encode_to_vec().as_slice()).expect("concierge Session decodes as banking Session");

	assert_eq!(bk_session.id, cc_session.id);
	assert_eq!(bk_session.user_agent, cc_session.user_agent);
	assert_eq!(bk_session.ip, cc_session.ip);
	assert_eq!(bk_session.created_at, cc_session.created_at);
	assert_eq!(bk_session.last_seen, cc_session.last_seen);
	assert_eq!(bk_session.current, cc_session.current);
	assert_eq!(bk_session.encode_to_vec(), cc_session.encode_to_vec());
}

#[test]
fn user_summary_is_wire_identical_across_planes() {
	let cc_summary = cc::UserSummary {
		user_id: "u-1".into(),
		email: "a@b.c".into(),
		status: "active".into(),
		token_version: 3,
		role: "operator".into(),
		// TRUE for the same reason as above — `false` would hide a dropped field.
		role_is_break_glass: true,
		email_verified: true,
		username: "ada".into(),
	};

	let bk_summary = bk::UserSummary::decode(cc_summary.encode_to_vec().as_slice()).expect("concierge UserSummary decodes as banking UserSummary");

	assert_eq!(bk_summary.user_id, cc_summary.user_id);
	assert_eq!(bk_summary.email, cc_summary.email);
	assert_eq!(bk_summary.status, cc_summary.status);
	assert_eq!(bk_summary.token_version, cc_summary.token_version);
	assert_eq!(bk_summary.role, cc_summary.role);
	assert_eq!(bk_summary.role_is_break_glass, cc_summary.role_is_break_glass);
	assert_eq!(bk_summary.email_verified, cc_summary.email_verified);
	assert_eq!(bk_summary.username, cc_summary.username);
	assert_eq!(bk_summary.encode_to_vec(), cc_summary.encode_to_vec());
}
