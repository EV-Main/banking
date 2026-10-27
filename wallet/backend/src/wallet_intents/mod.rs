//! Wallet actions as intents (spec controls (a), (b), (d), (h)): a page asks for an intent,
//! the BFF computes and stores its terms, and the same session on the same page confirms it —
//! with a concierge step-up proof when the act asks for one. The pages are the wallet itself
//! and the trusted scope hosts. The routes are `routes::intents`; the contract is
//! `docs/WALLET-INTENTS.md`.

pub mod intent;
pub mod keys;
pub mod proof;
pub mod store;

use std::{fmt, str::FromStr, sync::Arc};

use domain::money::Usdt;
use evconcierge_auth::stepup::Aal;
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, UsdtThreshold};

/// What a page may ask for.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
	Move,
	Withdraw,
	Pay,
	Mandate,
	ApprovePayment,
}

/// The env names host kinds only: [`IntentKind::ApprovePayment`] is the wallet's own, so no
/// list can hand it to a host.
impl FromStr for IntentKind {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"move" => Ok(Self::Move),
			"withdraw" => Ok(Self::Withdraw),
			"pay" => Ok(Self::Pay),
			"mandate" => Ok(Self::Mandate),
			other => Err(format!("{other:?} is not a host wallet action (move, withdraw, pay, mandate)")),
		}
	}
}
ev::settings_via_from_str!(IntentKind);

/// One host the bank lets act in place, reviewed here in code. `WALLET_ACTION_ORIGINS` can
/// switch an entry on and `WALLET_ACTION_KINDS` can narrow its kinds; neither can add a host
/// or a kind, because a host that is not here has no scope to check its requests against.
pub struct TrustedHost {
	pub origin: &'static str,
	pub scope: &'static str,
	pub kinds: &'static [IntentKind],
}

/// The matrix. Mirrors `wallet/frontend/shared/config/wallet-scopes.ts`. The SA panel has no
/// withdrawal: the spec ships it there only once the panel's strict CSP is live, and that is
/// a reviewed change of this line, not an env flip that would also reach invest.
pub const TRUSTED_HOSTS: &[TrustedHost] = &[
	TrustedHost {
		origin: "https://invest.evinvest.ltd",
		scope: "invest",
		kinds: &[IntentKind::Move, IntentKind::Pay, IntentKind::Mandate, IntentKind::Withdraw],
	},
	TrustedHost {
		origin: "https://sa.evinvest.ltd",
		scope: "sa",
		kinds: &[IntentKind::Move, IntentKind::Pay, IntentKind::Mandate],
	},
];

/// The wallet's own money-out acts. Fixed here: the env narrows hosts, never the wallet.
pub const WALLET_KINDS: &[IntentKind] = &[IntentKind::Withdraw, IntentKind::Pay, IntentKind::Mandate, IntentKind::ApprovePayment];

pub fn trusted_host(origin: &str) -> Option<&'static TrustedHost> {
	TRUSTED_HOSTS.iter().find(|host| host.origin == origin)
}

/// What production demands of a host origin on top of the matrix: `https`, no port, a
/// subdomain of evinvest.ltd. The matrix already satisfies it (a test pins that); this is
/// the second line against a matrix edit that does not.
pub fn production_host_rule(origin: &str) -> Result<(), &'static str> {
	let host = origin.strip_prefix("https://").ok_or("not https")?;
	if host.contains(':') {
		return Err("has a port");
	}
	if host.contains(['/', '*']) || host.contains("..") {
		return Err("is not a bare host");
	}
	match host.strip_suffix(".evinvest.ltd") {
		Some(label) if !label.is_empty() => Ok(()),
		_ => Err("is not a subdomain of evinvest.ltd"),
	}
}

/// The scopes a page may act in, beside `main`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reach {
	/// The wallet: every scope of the caller.
	Wallet,
	/// A scope host: its own scope only.
	Scope(&'static str),
}

impl Reach {
	pub fn covers(self, scope: &str) -> bool {
		match self {
			Self::Wallet => true,
			Self::Scope(own) => own == scope,
		}
	}
}

/// A page that may run intents: its exact origin, its reach, and the kinds it may create now.
#[derive(Clone, Debug)]
pub struct IntentPage {
	pub origin: String,
	pub reach: Reach,
	pub kinds: Vec<IntentKind>,
}

/// The assurance an act was authorised with: the level its proof achieved, or none. Written
/// `aal<N>` / `none` in the audit, the store and the confirm answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Assurance(Option<Aal>);

impl Assurance {
	pub const NONE: Self = Self(None);
}

impl From<Aal> for Assurance {
	fn from(aal: Aal) -> Self {
		Self(Some(aal))
	}
}

impl fmt::Display for Assurance {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.0 {
			Some(aal) => write!(f, "aal{}", aal.level()),
			None => f.write_str("none"),
		}
	}
}

impl FromStr for Assurance {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		if s == "none" {
			return Ok(Self::NONE);
		}
		s.strip_prefix("aal")
			.filter(|level| level.bytes().all(|b| b.is_ascii_digit()))
			.and_then(|level| level.parse::<u8>().ok())
			.and_then(Aal::named)
			.map(Self::from)
			.ok_or_else(|| format!("{s:?} is not an assurance"))
	}
}

impl Serialize for Assurance {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.collect_str(self)
	}
}

/// The thresholds of the risk classes (spec (h)): a move within both needs no step-up.
#[derive(Clone, Copy, Debug)]
pub struct RiskPolicy {
	pub no_stepup_per_op: Usdt,
	/// The user's moves through every page together in 24 h.
	pub no_stepup_24h: Usdt,
}

/// Everything the intent routes need.
pub struct WalletActions {
	pages: Vec<IntentPage>,
	pub keys: keys::IntentKeys,
	pub store: Arc<dyn store::WalletStore>,
	pub proofs: proof::ProofVerifier,
	pub policy: RiskPolicy,
}

impl WalletActions {
	/// Expects a config that passed [`AppConfig::check_wallet_actions`], and refuses one that
	/// did not rather than serving a page it would not have booted with.
	pub async fn connect(config: &AppConfig, jwks_endpoint: &str) -> Result<Self, String> {
		config.check_wallet_actions()?;
		let store: Arc<dyn store::WalletStore> = match config.redis_url.as_deref() {
			Some(url) => Arc::new(store::RedisStore::connect(url).await.map_err(|e| e.to_string())?),
			None => {
				tracing::warn!("wallet actions run on an in-process store: one replica only, never production");
				Arc::new(store::MemoryStore::default())
			}
		};
		Self::with_store(config, store, jwks_endpoint)
	}

	pub fn with_store(config: &AppConfig, store: Arc<dyn store::WalletStore>, jwks_endpoint: &str) -> Result<Self, String> {
		let hosts = config.wallet_action_origins.iter().map(|origin| {
			let host = trusted_host(origin.as_str()).ok_or_else(|| format!("{} is not in the host matrix", origin.as_str()))?;
			Ok(IntentPage {
				origin: host.origin.to_owned(),
				reach: Reach::Scope(host.scope),
				kinds: host.kinds.iter().copied().filter(|kind| config.wallet_action_kinds.contains(kind)).collect(),
			})
		});
		// Unset only outside production, where the first-party gate admits no page either.
		let wallet = config.wallet_ws_origin.iter().map(|origin| {
			Ok(IntentPage {
				origin: origin.clone(),
				reach: Reach::Wallet,
				kinds: WALLET_KINDS.to_vec(),
			})
		});
		Ok(Self {
			pages: hosts.chain(wallet).collect::<Result<Vec<_>, String>>()?,
			keys: keys::IntentKeys::derive(config.wallet_intent_key.as_bytes()),
			store,
			proofs: proof::ProofVerifier::new(config.auth_issuer.clone(), jwks_endpoint)?,
			policy: RiskPolicy {
				no_stepup_per_op: usdt(&config.wallet_move_no_stepup_per_op_usdt)?,
				no_stepup_24h: usdt(&config.wallet_move_no_stepup_24h_usdt)?,
			},
		})
	}

	/// The page whose origin is exactly `origin`.
	pub fn page(&self, origin: &str) -> Option<&IntentPage> {
		self.pages.iter().find(|page| page.origin == origin)
	}
}

pub fn usdt(threshold: &UsdtThreshold) -> Result<Usdt, String> {
	Usdt::parse_decimal(threshold.as_str()).map_err(|e| format!("{}: {e}", threshold.as_str()))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The matrix itself must pass what production demands of the env, or production could
	/// never switch an entry on.
	#[test]
	fn every_matrix_entry_passes_the_production_rule() {
		for host in TRUSTED_HOSTS {
			assert_eq!(production_host_rule(host.origin), Ok(()), "{}", host.origin);
			assert!(!host.kinds.is_empty(), "{}", host.origin);
			assert!(!host.kinds.contains(&IntentKind::ApprovePayment), "{}", host.origin);
		}
	}

	#[test]
	fn the_production_rule_refuses_everything_but_a_bare_https_subdomain() {
		for (origin, why) in [
			("http://sa.evinvest.ltd", "not https"),
			("https://sa.evinvest.ltd:8443", "has a port"),
			("https://sa.evinvest.ltd/wallet", "is not a bare host"),
			("https://sa.evinvest.ltd/", "is not a bare host"),
			("https://*.evinvest.ltd", "is not a bare host"),
			("https://evinvest.ltd", "is not a subdomain of evinvest.ltd"),
			("https://evinvest.ltd.evil.example", "is not a subdomain of evinvest.ltd"),
			("https://sa.evinvest.ltdx", "is not a subdomain of evinvest.ltd"),
			("null", "not https"),
			("", "not https"),
		] {
			assert_eq!(production_host_rule(origin), Err(why), "{origin}");
		}
		assert_eq!(production_host_rule("https://rogue.evinvest.ltd"), Ok(()), "shape alone passes; the matrix refuses it");
		assert!(trusted_host("https://rogue.evinvest.ltd").is_none());
	}

	#[test]
	fn an_assurance_reads_back_as_written() {
		for assurance in [Assurance::NONE, Assurance::from(Aal::SECOND_FACTOR), Assurance::from(Aal::named(3).unwrap())] {
			assert_eq!(assurance.to_string().parse::<Assurance>(), Ok(assurance));
		}
		assert_eq!(Assurance::from(Aal::SECOND_FACTOR).to_string(), "aal2");
		for bad in ["", "aal1", "aal", "aal+2", "passkey", "AAL2"] {
			assert!(bad.parse::<Assurance>().is_err(), "{bad}");
		}
	}
}
