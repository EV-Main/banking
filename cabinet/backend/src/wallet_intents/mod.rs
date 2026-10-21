//! Wallet actions a trusted host page runs in place (spec controls (a), (b), (d), (h)): the
//! host asks for an intent, the BFF computes and stores its terms, and the same session on
//! the same page confirms it — with a concierge step-up proof when the risk class asks for
//! one. The routes are `routes::intents`; the contract is `docs/WALLET-INTENTS.md`.

pub mod intent;
pub mod keys;
pub mod proof;
pub mod store;

use std::{str::FromStr, sync::Arc};

use domain::money::Usdt;
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, UsdtThreshold};

/// What a host may ask for.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
	Move,
	Withdraw,
	Pay,
	Mandate,
}

impl FromStr for IntentKind {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"move" => Ok(Self::Move),
			"withdraw" => Ok(Self::Withdraw),
			"pay" => Ok(Self::Pay),
			"mandate" => Ok(Self::Mandate),
			other => Err(format!("{other:?} is not a wallet action (move, withdraw, pay, mandate)")),
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

/// The matrix. Mirrors `cabinet/frontend/shared/config/wallet-scopes.ts`. The SA panel has no
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

/// A host that is switched on: its matrix entry narrowed by `WALLET_ACTION_KINDS`.
#[derive(Clone, Debug)]
pub struct EnabledHost {
	pub origin: &'static str,
	pub scope: &'static str,
	pub kinds: Vec<IntentKind>,
}

/// The thresholds of the risk classes (spec (h)).
#[derive(Clone, Copy, Debug)]
pub struct RiskPolicy {
	/// A move at or below this needs no step-up…
	pub no_stepup_per_op: Usdt,
	/// …while the user's moves through the host in 24 h stay at or below this.
	pub no_stepup_24h: Usdt,
	/// The largest amount an emailed code may confirm; above it only a passkey.
	pub email_cap: Usdt,
}

/// Everything the intent routes need, present only when a host is switched on.
pub struct WalletActions {
	hosts: Vec<EnabledHost>,
	pub keys: keys::IntentKeys,
	pub store: Arc<dyn store::WalletStore>,
	pub proofs: proof::ProofVerifier,
	pub policy: RiskPolicy,
}

impl WalletActions {
	/// `None` when `WALLET_ACTION_ORIGINS` is empty. Expects a config that passed
	/// [`AppConfig::check_wallet_actions`], and refuses one that did not rather than
	/// serving a host it would not have booted with.
	pub async fn connect(config: &AppConfig, jwks_endpoint: &str) -> Result<Option<Self>, String> {
		if config.wallet_action_origins.is_empty() {
			return Ok(None);
		}
		config.check_wallet_actions()?;
		let store: Arc<dyn store::WalletStore> = match config.redis_url.as_deref() {
			Some(url) => Arc::new(store::RedisStore::connect(url).await.map_err(|e| e.to_string())?),
			None => {
				tracing::warn!("wallet actions run on an in-process store: one replica only, never production");
				Arc::new(store::MemoryStore::default())
			}
		};
		Self::with_store(config, store, jwks_endpoint).map(Some)
	}

	pub fn with_store(config: &AppConfig, store: Arc<dyn store::WalletStore>, jwks_endpoint: &str) -> Result<Self, String> {
		let key = config.wallet_intent_key.as_ref().ok_or("WALLET_INTENT_KEY is not set")?;
		let hosts = config
			.wallet_action_origins
			.iter()
			.map(|origin| {
				let host = trusted_host(origin.as_str()).ok_or_else(|| format!("{} is not in the host matrix", origin.as_str()))?;
				Ok(EnabledHost {
					origin: host.origin,
					scope: host.scope,
					kinds: host.kinds.iter().copied().filter(|kind| config.wallet_action_kinds.contains(kind)).collect(),
				})
			})
			.collect::<Result<Vec<_>, String>>()?;
		Ok(Self {
			hosts,
			keys: keys::IntentKeys::derive(key.as_bytes()),
			store,
			proofs: proof::ProofVerifier::new(config.auth_issuer.clone(), jwks_endpoint)?,
			policy: RiskPolicy {
				no_stepup_per_op: usdt(&config.wallet_move_no_stepup_per_op_usdt)?,
				no_stepup_24h: usdt(&config.wallet_move_no_stepup_24h_usdt)?,
				email_cap: usdt(&config.wallet_email_stepup_cap_usdt)?,
			},
		})
	}

	/// The enabled host whose origin is exactly `origin`.
	pub fn host(&self, origin: &str) -> Option<&EnabledHost> {
		self.hosts.iter().find(|host| host.origin == origin)
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
}
