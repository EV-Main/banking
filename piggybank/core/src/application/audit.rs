//! Who asked for a money act, and from where — the BFF's reading of the request, carried on
//! the wire as `banking.v1.AuditContext`. Recorded beside the act and never an authorization
//! input, except that [`RequestAudit::origin`] picks a limit profile (see
//! [`withdrawal_limits`](super::withdrawal_limits)), which can only ever tighten a limit.

use domain::users::UserId;
use tracing::info;

const MAX_ORIGIN: usize = 128;
const MAX_IP: usize = 64;
const MAX_USER_AGENT: usize = 256;
const MAX_INTENT: usize = 64;
const MAX_METHOD: usize = 32;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RequestAudit {
	pub origin: String,
	pub client_ip: String,
	pub user_agent: String,
	pub intent_id: String,
	pub stepup_method: String,
}

impl RequestAudit {
	/// Every field clamped on a character boundary: the BFF bounds them too, but the hub's
	/// table CHECKs must never be the first line that refuses a long user agent.
	pub fn new(origin: &str, client_ip: &str, user_agent: &str, intent_id: &str, stepup_method: &str) -> Self {
		let clamp = |value: &str, max: usize| value.chars().take(max).collect::<String>();
		Self {
			origin: clamp(origin, MAX_ORIGIN),
			client_ip: clamp(client_ip, MAX_IP),
			user_agent: clamp(user_agent, MAX_USER_AGENT),
			intent_id: clamp(intent_id, MAX_INTENT),
			stepup_method: clamp(stepup_method, MAX_METHOD),
		}
	}

	/// The structured audit line for acts that keep no audit row of their own (a move, an
	/// invoice payment, a mandate grant or revoke).
	pub fn log(&self, act: &'static str, user: UserId) {
		info!(
			target: "audit",
			act,
			%user,
			origin = %self.origin,
			client_ip = %self.client_ip,
			user_agent = %self.user_agent,
			intent_id = %self.intent_id,
			stepup_method = %self.stepup_method,
			"money act requested"
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fields_are_clamped_on_character_boundaries() {
		let audit = RequestAudit::new(&"é".repeat(500), "1.2.3.4", &"ж".repeat(1000), "", "fresh_login");
		assert_eq!(audit.origin.chars().count(), MAX_ORIGIN);
		assert_eq!(audit.user_agent.chars().count(), MAX_USER_AGENT);
		assert_eq!(audit.client_ip, "1.2.3.4");
	}
}
