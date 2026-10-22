//! The security notices of the wallet (`bank:wallet-security`): what the owner of the money
//! is told when a withdrawal, a new withdrawal address or a mandate appears on their account.
//!
//! Plain text only: concierge renders the inbox entry and the mail from it, and the "this
//! wasn't me" path is the `link` — the wallet page, where the withdrawal is cancelled and the
//! sessions are ended. The words say what to do there, because a link alone is not an
//! instruction.

use domain::{money::Usdt, scope_operations::WALLET_SECURITY_TOPIC, users::UserId, withdrawals::Withdrawal};

use crate::ports::scope_notifications::ScopeNotice;

/// The wallet page, relative to the cabinet root: concierge joins it onto `CABINET_URL`, or onto
/// the base of the notice's `link_scope`, and each base already ends in the cabinet's own
/// prefix, so a `/cabinet` here would double it. Cancelling a queued withdrawal and signing out
/// everywhere both start there.
const WALLET_PAGE: &str = "/wallet";

const WASNT_ME: &str = "If this was not you, open your wallet now, cancel it and sign out of every device.";

/// A withdrawal of `withdrawal` was requested. `hold_secs` is `Some` when the destination is
/// new and the money will not move before that delay has passed.
pub fn withdrawal_requested(user: UserId, withdrawal: &Withdrawal, hold_secs: Option<u64>) -> ScopeNotice {
	let what = format!(
		"{} USDT to {} on {}",
		withdrawal.amount().to_decimal_string(),
		withdrawal.address().as_str(),
		withdrawal.network().as_str()
	);
	let (kind, title, body) = match hold_secs {
		Some(secs) => (
			"withdrawal_new_address",
			"Withdrawal to a new address",
			format!(
				"A withdrawal of {what} was requested from your account. This address is new, or still in its waiting period, so the money is held for up to {} before it is sent, and you can cancel it until then. {WASNT_ME}",
				hours(secs)
			),
		),
		None => (
			"withdrawal_requested",
			"Withdrawal requested",
			format!("A withdrawal of {what} was requested from your account. {WASNT_ME}"),
		),
	};
	ScopeNotice {
		user,
		topic: WALLET_SECURITY_TOPIC.to_owned(),
		kind: kind.to_owned(),
		title: title.to_owned(),
		body,
		link: WALLET_PAGE.to_owned(),
		// A withdrawal leaves the person's own wallet (`main`), which belongs to no scope, and
		// banking records no scope a person belongs to: the platform cabinet is the only one
		// known to be theirs.
		link_scope: String::new(),
		dedupe_key: format!("withdrawal:{}:requested", withdrawal.id()),
	}
}

/// A mandate was granted to a service (`granted`) or ended (`!granted`).
pub fn mandate_changed(user: UserId, mandate_id: &str, granted: bool, client: &str, scope: &str, cap_per_op: Usdt, cap_24h: Usdt) -> ScopeNotice {
	let (kind, title, body) = if granted {
		(
			"mandate_granted",
			"A service can now draw on your wallet",
			format!(
				"You granted {client} a mandate on {scope}: up to {} USDT per operation and {} USDT per 24 hours. {WASNT_ME}",
				cap_per_op.to_decimal_string(),
				cap_24h.to_decimal_string()
			),
		)
	} else {
		(
			"mandate_revoked",
			"A mandate on your wallet was revoked",
			format!("The mandate of {client} on {scope} was revoked. {WASNT_ME}"),
		)
	};
	ScopeNotice {
		user,
		topic: WALLET_SECURITY_TOPIC.to_owned(),
		kind: kind.to_owned(),
		title: title.to_owned(),
		body,
		link: WALLET_PAGE.to_owned(),
		// The mandate is the person's tie to this scope, so its cabinet is where they read it.
		link_scope: scope.to_owned(),
		dedupe_key: format!("mandate:{mandate_id}:{}", if granted { "granted" } else { "revoked" }),
	}
}

fn hours(secs: u64) -> String {
	match secs {
		s if s >= 3600 && s % 3600 == 0 => format!("{} h", s / 3600),
		s if s >= 3600 => format!("{} h {} min", s / 3600, s % 3600 / 60),
		s => format!("{} min", s.div_ceil(60).max(1)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_hold_reads_in_hours_and_minutes() {
		assert_eq!(hours(21_600), "6 h");
		assert_eq!(hours(5_400), "1 h 30 min");
		assert_eq!(hours(30), "1 min");
	}

	#[test]
	fn a_mandate_notice_names_the_service_and_points_at_the_wallet() {
		let notice = mandate_changed(UserId::new(), "m-1", true, "panel", "sa", Usdt::parse_decimal("10").unwrap(), Usdt::parse_decimal("50").unwrap());
		assert_eq!(notice.topic, "bank:wallet-security");
		assert_eq!((notice.link.as_str(), notice.link_scope.as_str()), ("/wallet", "sa"), "read in the mandate's scope's cabinet");
		assert!(notice.body.contains("panel") && notice.body.contains("10 USDT per operation"));
		assert!(!notice.body.contains("://"), "plain text, no links in the body");
		assert!(notice.title.chars().count() <= 200 && notice.dedupe_key.len() <= 128);
	}
}
