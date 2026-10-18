//! The withdrawal limits the owner set (WALLET-INPAGE-SPEC, "Owner decisions"): two profiles,
//! picked by the origin the BFF saw the request come from, all tunable from the environment.
//!
//! | profile | per operation | per 24 h | per hour |
//! | --- | --- | --- | --- |
//! | first-party (evinvest.ltd, admin) | 4000 USDT | 10000 USDT | unlimited |
//! | host (a scope's page) | 1000 USDT | 2000 USDT | 6 |
//!
//! An origin that is not a first-party origin is a host: a typo or a new origin lands on the
//! tighter profile, so the profile can only ever fail closed. The 24 h and hourly figures are
//! counted over ALL of the user's withdrawals whichever origin made them.

use domain::{error::DomainError, money::Usdt};

use crate::ports::withdrawals::LimitProfile;

/// The origin the cabinet itself runs on; the BFF sends it as `AuditContext.origin`.
const DEFAULT_FIRST_PARTY_ORIGIN: &str = "https://evinvest.ltd";
/// Six hours: how long a first-seen address waits before money may go to it, and how long
/// after a sign-in change a new address stays shut.
const SIX_HOURS: u64 = 21_600;
/// The reaper auto-cancels a queued withdrawal after 24 h; a hold must end well inside that.
const MAX_HOLD_SECS: u64 = 23 * 3600;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithdrawalLimits {
	pub first_party: LimitProfile,
	pub host: LimitProfile,
	first_party_origins: Vec<String>,
	/// Seconds a first-seen destination is held before the dispatcher may send to it; 0 = no hold.
	pub new_address_hold_secs: u64,
	/// Seconds after a sign-in change during which a first-seen destination is refused; 0 = off.
	pub credential_cooloff_secs: u64,
}

fn usdt(raw: &str) -> Usdt {
	// Only ever called on the literals below and on operator input already validated by
	// `Usdt::parse_decimal`'s caller; a literal that fails is caught by `defaults_are_the_owners_numbers`.
	Usdt::parse_decimal(raw).unwrap_or(Usdt::ZERO)
}

impl Default for WithdrawalLimits {
	fn default() -> Self {
		Self {
			first_party: LimitProfile {
				per_op: usdt("4000"),
				per_24h: usdt("10000"),
				max_per_hour: None,
			},
			host: LimitProfile {
				per_op: usdt("1000"),
				per_24h: usdt("2000"),
				max_per_hour: Some(6),
			},
			first_party_origins: vec![DEFAULT_FIRST_PARTY_ORIGIN.to_owned()],
			new_address_hold_secs: SIX_HOURS,
			credential_cooloff_secs: SIX_HOURS,
		}
	}
}

impl WithdrawalLimits {
	/// The profile for the origin the request came from. Only an origin on the first-party list
	/// gets the first-party profile; an empty one (a caller that sends no audit block, or an
	/// older BFF) gets the host profile, so a missing origin fails closed.
	pub fn profile_for(&self, origin: &str) -> &LimitProfile {
		if self.first_party_origins.iter().any(|known| known == origin) {
			&self.first_party
		} else {
			&self.host
		}
	}

	pub fn from_env() -> Result<Self, String> {
		Self::from_source(|var| std::env::var(var).ok())
	}

	/// Read the knobs through `get` (empty counts as unset). A malformed value is an error,
	/// not a silent default: a limit that quietly stays at its default after an operator
	/// typed something else is worse than a hub that refuses to boot.
	///
	/// `WITHDRAWAL_LIMIT_PER_OP_USDT` / `WITHDRAWAL_LIMIT_PER_24H_USDT` are the first-party
	/// profile, `WITHDRAWAL_HOST_LIMIT_PER_OP_USDT` / `WITHDRAWAL_HOST_LIMIT_PER_24H_USDT` the
	/// host one, `WITHDRAWAL_MAX_PER_HOUR` the host's hourly cap (0 = none) and
	/// `WITHDRAWAL_FIRST_PARTY_ORIGINS` a comma list of origins judged first-party.
	pub fn from_source(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
		let get = |var: &str| get(var).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
		let amount = |var: &str, default: Usdt| -> Result<Usdt, String> {
			match get(var) {
				None => Ok(default),
				Some(raw) => Usdt::parse_decimal(&raw).map_err(|err: DomainError| format!("{var}={raw:?}: {err}")),
			}
		};
		let secs = |var: &str, default: u64| -> Result<u64, String> {
			match get(var) {
				None => Ok(default),
				Some(raw) => raw.parse::<u64>().map_err(|_| format!("{var}={raw:?} is not a whole number of seconds")),
			}
		};
		let defaults = Self::default();
		let max_per_hour = match get("WITHDRAWAL_MAX_PER_HOUR") {
			None => defaults.host.max_per_hour,
			Some(raw) => match raw.parse::<u32>() {
				Ok(0) => None,
				Ok(n) => Some(n),
				Err(_) => return Err(format!("WITHDRAWAL_MAX_PER_HOUR={raw:?} is not a whole number")),
			},
		};
		let new_address_hold_secs = secs("WITHDRAWAL_NEW_ADDRESS_HOLD_SECS", defaults.new_address_hold_secs)?;
		let first_party_origins = match get("WITHDRAWAL_FIRST_PARTY_ORIGINS") {
			None => defaults.first_party_origins,
			Some(raw) => raw.split(',').map(|o| o.trim().to_owned()).filter(|o| !o.is_empty()).collect(),
		};
		if new_address_hold_secs > MAX_HOLD_SECS {
			return Err(format!(
				"WITHDRAWAL_NEW_ADDRESS_HOLD_SECS={new_address_hold_secs} is over {MAX_HOLD_SECS}: the reaper cancels queued withdrawals after 24 h, so a longer hold would be cancelled before it ends"
			));
		}
		Ok(Self {
			first_party: LimitProfile {
				per_op: amount("WITHDRAWAL_LIMIT_PER_OP_USDT", defaults.first_party.per_op)?,
				per_24h: amount("WITHDRAWAL_LIMIT_PER_24H_USDT", defaults.first_party.per_24h)?,
				max_per_hour: None,
			},
			host: LimitProfile {
				per_op: amount("WITHDRAWAL_HOST_LIMIT_PER_OP_USDT", defaults.host.per_op)?,
				per_24h: amount("WITHDRAWAL_HOST_LIMIT_PER_24H_USDT", defaults.host.per_24h)?,
				max_per_hour,
			},
			first_party_origins,
			new_address_hold_secs,
			credential_cooloff_secs: secs("WITHDRAWAL_CREDENTIAL_COOLOFF_SECS", defaults.credential_cooloff_secs)?,
		})
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::*;
	use crate::ports::withdrawals::Usage;

	fn from(pairs: &[(&str, &str)]) -> Result<WithdrawalLimits, String> {
		let env: HashMap<String, String> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
		WithdrawalLimits::from_source(|var| env.get(var).cloned())
	}

	#[test]
	fn defaults_are_the_owners_numbers() {
		let limits = from(&[]).unwrap();
		assert_eq!(limits, WithdrawalLimits::default());
		assert_eq!(limits.first_party.per_op.to_decimal_string(), "4000");
		assert_eq!(limits.first_party.per_24h.to_decimal_string(), "10000");
		assert_eq!(limits.host.per_op.to_decimal_string(), "1000");
		assert_eq!(limits.host.per_24h.to_decimal_string(), "2000");
		assert_eq!(limits.host.max_per_hour, Some(6));
		assert_eq!(limits.new_address_hold_secs, 21_600);
	}

	#[test]
	fn the_environment_overrides_and_junk_refuses() {
		let limits = from(&[
			("WITHDRAWAL_LIMIT_PER_OP_USDT", "50.5"),
			("WITHDRAWAL_MAX_PER_HOUR", "0"),
			("WITHDRAWAL_NEW_ADDRESS_HOLD_SECS", "60"),
		])
		.unwrap();
		assert_eq!(limits.first_party.per_op.to_decimal_string(), "50.5");
		assert_eq!(limits.host.max_per_hour, None);
		assert_eq!(limits.new_address_hold_secs, 60);
		for (var, bad) in [
			("WITHDRAWAL_LIMIT_PER_OP_USDT", "lots"),
			("WITHDRAWAL_HOST_LIMIT_PER_24H_USDT", "-1"),
			("WITHDRAWAL_MAX_PER_HOUR", "-3"),
			("WITHDRAWAL_NEW_ADDRESS_HOLD_SECS", "6h"),
			("WITHDRAWAL_NEW_ADDRESS_HOLD_SECS", "90000"),
		] {
			assert!(from(&[(var, bad)]).is_err(), "{var}={bad}");
		}
	}

	#[test]
	fn only_a_listed_origin_is_first_party() {
		let limits = WithdrawalLimits::default();
		assert_eq!(limits.profile_for(""), &limits.host, "no origin fails closed");
		assert_eq!(limits.profile_for("https://evinvest.ltd"), &limits.first_party);
		for host in [
			"https://sa.evinvest.ltd",
			"https://evinvest.ltd.evil.example",
			"null",
			"https://evinvest.ltd/",
			"https://evinvest.ltd:8443",
		] {
			assert_eq!(limits.profile_for(host), &limits.host, "{host}");
		}
	}

	#[test]
	fn a_profile_refuses_past_each_limit() {
		let host = WithdrawalLimits::default().host;
		let none = Usage { gross_24h: Usdt::ZERO, count_1h: 0 };
		assert!(host.check(usdt("1000"), none).is_ok());
		assert!(host.check(usdt("1000.000000000000000001"), none).is_err(), "per operation");
		let day = Usage {
			gross_24h: usdt("1500"),
			count_1h: 1,
		};
		assert!(host.check(usdt("500"), day).is_ok());
		assert!(host.check(usdt("500.01"), day).is_err(), "per 24 hours");
		let busy = Usage { gross_24h: usdt("60"), count_1h: 6 };
		assert!(host.check(usdt("10"), busy).is_err(), "per hour");
		assert!(WithdrawalLimits::default().first_party.check(usdt("10"), busy).is_ok(), "first-party has no hourly cap");
	}
}
