//! Shared chain-rail plumbing — the pieces the EVM, TON and Tron rails would otherwise
//! carry in three verbatim copies.
//!
//! Each rail runs the same background shapes (deposit watcher, withdrawal watcher, treasury
//! sweep) against a different protocol. Their failure taxonomies, their wall clock, and the
//! sweep's Postgres/signer plumbing are identical — only the network name and the protocol
//! RPC differ — so they live here once, parameterised by `network: &str`, and a fix lands in
//! one place instead of three.
//!
//! What deliberately stays per-rail: the RPC clients and their `RpcError` enums (one per
//! protocol), the node error-string classifiers (`is_idempotent` matches different node
//! wording on EVM and Tron), the signing calls, and every `info!`/`warn!` the rails emit.
//! Logging stays home on purpose: `tracing` derives an event's `target` from the module it is
//! written in, and that target is both serialised into the JSON logs and what `RUST_LOG`
//! filters on. Hoisting the sweep loop's logs here would collapse
//! `piggybank_core::infrastructure::{sweep,ton_sweep,tron_sweep}` into one target and cost
//! operators the ability to filter or alert on a single rail.

use std::{
	collections::HashMap,
	fmt::Display,
	time::{Duration, Instant},
};

use domain::money::Network;
use evbanking_auth::ServiceTokenSource;
use evbanking_contracts::signer::v1::{ProvisionAddressRequest, signer_service_client::SignerServiceClient};
use sqlx::PgPool;
use tokio::sync::OnceCell;
use tonic::{Code, Request, Status, transport::Channel};
use uuid::Uuid;

/// The reserved gas-station account id, shared by every rail's sweep and distinct from the nil
/// treasury: a wallet holding only the rail's native coin (BNB/POL, Toncoin, TRX), used to top
/// up user deposit addresses with the gas/fee budget their USDT sweep burns. A separate account
/// means a separate nonce sequence, so the sweep never races the withdrawal custody path.
pub(super) const GAS_STATION: Uuid = Uuid::from_u128(1);

/// Current unix time in seconds.
///
/// These are infrastructure workers, not use cases, so they have no injected clock — the
/// figure only ever bounds a signed message's validity window or an elapsed window that the
/// chain or Postgres then stores. A clock before the epoch reads as 0.
pub(super) fn now_unix_secs() -> u64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The failure taxonomy every rail's deposit and withdrawal watcher shares. No single watcher
/// constructs every variant — the TON deposit watcher, for one, never lets a per-owner RPC
/// failure end a cycle, so it never builds [`Rpc`](WatcherError::Rpc) — but the `Display`
/// prefixes are what operators read in the logs, so they stay one set across all six.
#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
	#[error("rpc: {0}")]
	Rpc(String),
	#[error("decode: {0}")]
	Decode(String),
	#[error("credit: {0}")]
	Credit(String),
	#[error("db: {0}")]
	Db(String),
	#[error("custody: {0}")]
	Custody(String),
}

/// A control-plane query that failed — the watchers' `map_err` for `sqlx`.
pub(super) fn repo(err: sqlx::Error) -> WatcherError {
	WatcherError::Db(err.to_string())
}

/// Advance a deposit watcher's resume point. Committed without waiting for fsync: a lost advance only re-scans, and crediting is idempotent.
pub(super) async fn set_scan_cursor(pool: &PgPool, network: Network, cursor: i64) -> Result<(), WatcherError> {
	let mut tx = pool.begin().await.map_err(repo)?;
	sqlx::query("SET LOCAL synchronous_commit = off").execute(&mut *tx).await.map_err(repo)?;
	sqlx::query("UPDATE deposit_scan_cursor SET last_scanned_block = $2, updated_at = now() WHERE network = $1")
		.bind(network.as_str())
		.bind(cursor)
		.execute(&mut *tx)
		.await
		.map_err(repo)?;
	tx.commit().await.map_err(repo)
}

/// The failure taxonomy every rail's treasury sweep shares.
#[derive(Debug, thiserror::Error)]
pub(super) enum SweepError {
	#[error("rpc: {0}")]
	Rpc(String),
	/// The signer could not answer *right now* — retried on the next poll, like an RPC blip.
	#[error("signer: {0}")]
	Signer(String),
	/// The signer refused ON THE MERITS and the same request would reproduce the refusal:
	/// the custodian's policy said no, the request is malformed, or the custodian parked
	/// the activity for a human (`activity_id` set — the `turnkey-activity-id` trailer).
	/// Re-asking every poll would only mint another pending activity per cycle, so the
	/// refused key (`key`) is held back with an exponential backoff instead
	/// ([`RefusalBackoff`]).
	#[error("signer refused: {detail}")]
	SignerRefused { detail: String, activity_id: Option<String>, key: HoldKey },
	#[error("db: {0}")]
	Db(String),
	#[error("config: {0}")]
	Config(String),
}

/// A read-path RPC failure, whatever the rail's `RpcError` type is: each protocol keeps its
/// own enum, and each one's `Display` is already the operator-facing detail — so the sweeps
/// share the wrapping, not the error type.
pub(super) fn read_err(err: impl Display) -> SweepError {
	SweepError::Rpc(err.to_string())
}

/// The trailer the signer attaches to a `FailedPrecondition` when the key custodian parked
/// the activity for a human (`BackendError::RequiresApproval`). The hub does not depend on
/// the signer crate, so this mirrors its `ACTIVITY_ID_METADATA_KEY` rather than importing it.
pub(super) const ACTIVITY_ID_METADATA_KEY: &str = "turnkey-activity-id";

/// Classify a signer status on a sweep's signing call, `what` naming the attempt for the log
/// and `key` the wallet whose signing was refused.
///
/// The split mirrors the withdrawal path (`custody.rs`: only `Unavailable`/`DeadlineExceeded`
/// retry) on the codes that matter here. A refusal on the merits — `FailedPrecondition`
/// (custodian approval, an unprovisioned wallet), `PermissionDenied` (the custodian's
/// policy), `InvalidArgument` (a request the signer will never accept) — is
/// [`SweepError::SignerRefused`], and the sweep backs off. Everything else keeps the
/// per-cycle retry: an `Unavailable` is a blip, and an `Internal` is the dead-key alarm that
/// `telemetry::note_signer_error` counts on every hit — slowing that count down would mute
/// the signal an operator watches.
pub(super) fn signer_err(what: impl Display, key: HoldKey, status: &Status) -> SweepError {
	let detail = format!("{what}: {}", status.message());
	match status.code() {
		Code::FailedPrecondition | Code::PermissionDenied | Code::InvalidArgument => SweepError::SignerRefused {
			detail,
			activity_id: status.metadata().get(ACTIVITY_ID_METADATA_KEY).and_then(|value| value.to_str().ok()).map(str::to_owned),
			key,
		},
		_ => SweepError::Signer(detail),
	}
}

/// The wallet a signer refusal is held against. A refusal is a property of the KEY that was
/// asked to sign, so the hold follows the key: a user's sweep is held per deposit address,
/// while a gas top-up is signed by the one gas-station key for every address — held per
/// address, a station under custodian consensus would still mint one pending activity per
/// address per hold, N times the noise the backoff exists to remove.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum HoldKey {
	Address(String),
	GasStation,
}

/// Per-key hold-off after a terminal signer refusal — the sweep's stand-in for the
/// withdrawal path's park.
///
/// A withdrawal that the custodian parks stays parked: the relay never re-signs it, an
/// operator resolves it. A sweep cannot park for good — its state is in memory, there is no
/// unpark RPC, and re-signing IS the only way it ever resumes once the operator has fixed
/// the policy or approved the activity. So instead of "never again" it is "less and less
/// often": each refusal doubles the hold from the poll interval up to [`CAP`](Self::CAP),
/// which turns ~2 880 pending activities per key per day into ~24, and a success clears the
/// record. A held key costs no RPC and no signer call.
///
/// The clock is passed in rather than read, so the arithmetic is testable without sleeping.
#[derive(Default)]
pub(super) struct RefusalBackoff {
	holds: HashMap<HoldKey, Hold>,
}

struct Hold {
	until: Instant,
	strikes: u32,
}

/// What one refusal did to the address's hold — the sweeps log it in their own module.
pub(super) struct Strike {
	/// How long the address is skipped for from now.
	pub(super) hold: Duration,
	/// 1 on the first refusal since the last success; the first is the one to alert on.
	pub(super) strikes: u32,
}

impl RefusalBackoff {
	/// The longest an address is held between two attempts. An hour bounds both the noise
	/// (one activity per address per hour, at worst) and the wait after an operator acts.
	pub(super) const CAP: Duration = Duration::from_secs(60 * 60);

	/// Whether `key` is still inside its hold at `now`.
	pub(super) fn is_held(&self, key: &HoldKey, now: Instant) -> bool {
		self.holds.get(key).is_some_and(|hold| now < hold.until)
	}

	/// Record a refusal: the hold starts at `base` and doubles per consecutive strike, capped.
	pub(super) fn strike(&mut self, key: HoldKey, now: Instant, base: Duration) -> Strike {
		let strikes = self.holds.get(&key).map_or(1, |hold| hold.strikes.saturating_add(1));
		let hold = base.saturating_mul(1u32 << (strikes - 1).min(31)).min(Self::CAP);
		self.holds.insert(key, Hold { until: now + hold, strikes });
		Strike { hold, strikes }
	}

	/// The signer accepted a request for `key` again: forget its strikes.
	pub(super) fn release(&mut self, key: &HoldKey) {
		self.holds.remove(key);
	}
}

/// A system wallet's on-chain address for `network`, resolved once via `ProvisionAddress`
/// (`Uuid::nil()` = treasury, [`GAS_STATION`] = gas station) and cached in `cell`. A transient
/// failure leaves the cell empty so a later cycle retries.
pub(super) async fn address(
	cell: &OnceCell<String>,
	signer: &SignerServiceClient<Channel>,
	service_token: Option<&ServiceTokenSource>,
	network: &str,
	id: Uuid,
) -> Result<String, SweepError> {
	cell.get_or_try_init(|| async {
		let mut request = Request::new(ProvisionAddressRequest {
			user_id: id.to_string(),
			network: network.to_owned(),
		});
		if let Some(token) = service_token {
			request = token.authorize(request);
		}
		let response = signer
			.clone()
			.provision_address(request)
			.await
			.map_err(|s| SweepError::Signer(format!("resolve system wallet {id}: {}", s.message())))?
			.into_inner();
		if response.address_kind != "derived" {
			return Err(SweepError::Config(format!("system wallet {id} is not a derived address (kind={})", response.address_kind)));
		}
		Ok(response.address)
	})
	.await
	.cloned()
}

/// Addresses on `network` that can still hold funds: a credited deposit exists that no sweep
/// cycle has yet observed drained. Everything else is skipped without an RPC — the scan is
/// O(active deposits), not O(all addresses ever provisioned).
pub(super) async fn deposit_addresses(pool: &PgPool, network: &str) -> Result<Vec<(Uuid, String)>, SweepError> {
	sqlx::query_as::<_, (Uuid, String)>(
		"SELECT DISTINCT a.user_id, a.address FROM deposits d \
		 JOIN user_deposit_addresses a ON a.user_id::text = d.party_id AND a.network = d.network \
		 WHERE d.network = $1 AND d.party_kind = 'user' AND d.swept_at IS NULL AND a.address_kind = 'derived'",
	)
	.bind(network)
	.fetch_all(pool)
	.await
	.map_err(|e| SweepError::Db(e.to_string()))
}

/// Stamp a user's credited deposits on `network` as swept, so the address drops out of
/// [`deposit_addresses`] until a NEW deposit is credited — the fix for the O(N)-every-cycle
/// RPC melt.
pub(super) async fn mark_swept(pool: &PgPool, network: &str, user_id: Uuid) -> Result<(), SweepError> {
	sqlx::query("UPDATE deposits SET swept_at = now() WHERE party_kind = 'user' AND party_id = $1 AND network = $2 AND swept_at IS NULL")
		.bind(user_id.to_string())
		.bind(network)
		.execute(pool)
		.await
		.map_err(|e| SweepError::Db(e.to_string()))?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use tonic::metadata::MetadataValue;

	use super::*;

	const ACTIVITY_ID: &str = "0f6a2b3c-4d5e-4f70-8a9b-0c1d2e3f4a5b";
	const POLL: Duration = Duration::from_secs(30);

	fn addr(address: &str) -> HoldKey {
		HoldKey::Address(address.to_owned())
	}

	#[test]
	fn requires_approval_is_a_refusal_that_carries_the_activity_id() {
		let mut status = Status::failed_precondition(format!("key custodian requires approval for activity {ACTIVITY_ID}"));
		status.metadata_mut().insert(ACTIVITY_ID_METADATA_KEY, MetadataValue::try_from(ACTIVITY_ID).unwrap());
		match signer_err("sweep 0xabc", addr("0xabc"), &status) {
			SweepError::SignerRefused { detail, activity_id, key } => {
				assert_eq!(activity_id.as_deref(), Some(ACTIVITY_ID));
				assert!(detail.starts_with("sweep 0xabc: "), "{detail}");
				assert_eq!(key, addr("0xabc"));
			}
			other => panic!("RequiresApproval must not be retried every poll: {other}"),
		}
	}

	#[test]
	fn refusals_on_the_merits_do_not_retry_every_poll() {
		for status in [
			Status::failed_precondition("sending wallet is not provisioned"),
			Status::permission_denied("key custodian refused: policy"),
			Status::invalid_argument("amount must be a u128 decimal"),
		] {
			let code = status.code();
			assert!(
				matches!(
					signer_err("sweep", HoldKey::GasStation, &status),
					SweepError::SignerRefused {
						activity_id: None,
						key: HoldKey::GasStation,
						..
					}
				),
				"{code:?} is terminal for this request"
			);
		}
	}

	#[test]
	fn outages_and_dead_keys_keep_the_per_poll_retry() {
		for status in [
			Status::unavailable("key custodian unavailable: timeout"),
			Status::deadline_exceeded("slow"),
			Status::internal("could not unseal the signing key"),
			Status::unauthenticated("token expired"),
		] {
			let code = status.code();
			assert!(matches!(signer_err("sweep", addr("a"), &status), SweepError::Signer(_)), "{code:?} retries next poll");
		}
	}

	#[test]
	fn a_held_address_is_skipped_until_its_hold_expires() {
		let mut backoff = RefusalBackoff::default();
		let t0 = Instant::now();
		assert!(!backoff.is_held(&addr("a"), t0), "never refused ⇒ never held");
		let strike = backoff.strike(addr("a"), t0, POLL);
		assert_eq!((strike.strikes, strike.hold), (1, POLL));
		assert!(backoff.is_held(&addr("a"), t0 + POLL / 2));
		assert!(!backoff.is_held(&addr("a"), t0 + POLL), "the hold is half-open: due exactly at its end");
		assert!(!backoff.is_held(&addr("b"), t0), "holds are per address");
		assert!(!backoff.is_held(&HoldKey::GasStation, t0), "a user key's refusal says nothing about the gas station");
	}

	#[test]
	fn the_gas_station_is_one_key_held_for_every_address() {
		let mut backoff = RefusalBackoff::default();
		let t0 = Instant::now();
		backoff.strike(HoldKey::GasStation, t0, POLL);
		assert!(backoff.is_held(&HoldKey::GasStation, t0));
		assert!(!backoff.is_held(&addr("a"), t0), "the addresses themselves stay sweepable when they hold gas");
	}

	#[test]
	fn consecutive_refusals_double_the_hold_up_to_the_cap() {
		let mut backoff = RefusalBackoff::default();
		let t0 = Instant::now();
		let holds: Vec<Duration> = (0..10u32).map(|i| backoff.strike(addr("a"), t0 + POLL * i, POLL).hold).collect();
		assert_eq!(&holds[..4], &[POLL, POLL * 2, POLL * 4, POLL * 8]);
		assert_eq!(holds[9], RefusalBackoff::CAP, "30s doubled nine times is 4h16m — clamped to the cap");
		assert!(holds.windows(2).all(|w| w[0] <= w[1]), "never shrinks while refusals continue");
	}

	#[test]
	fn a_success_clears_the_strikes() {
		let mut backoff = RefusalBackoff::default();
		let t0 = Instant::now();
		backoff.strike(addr("a"), t0, POLL);
		backoff.strike(addr("a"), t0, POLL);
		backoff.release(&addr("a"));
		assert!(!backoff.is_held(&addr("a"), t0));
		assert_eq!(backoff.strike(addr("a"), t0, POLL).strikes, 1, "the count restarts after a success");
	}

	#[test]
	fn the_shift_cannot_overflow_after_many_strikes() {
		let mut backoff = RefusalBackoff::default();
		let t0 = Instant::now();
		let mut last = backoff.strike(addr("a"), t0, POLL);
		for _ in 1..100 {
			last = backoff.strike(addr("a"), t0, POLL);
		}
		assert_eq!((last.strikes, last.hold), (100, RefusalBackoff::CAP));
	}
}
