//! On-chain TON deposit watcher — credits user balances from incoming USDT (jetton)
//! transfers.
//!
//! The TON sibling of [`deposit_watcher`](super::deposit_watcher). USDT on TON is a
//! TEP-74 **jetton**, so a deposit lands in the user's derived wallet's *jetton wallet*
//! contract; we attribute it via toncenter's server-side `owner_address` filter on the
//! decoded `/jetton/transfers` feed (no client-side address decoding, no `eth_getLogs`).
//! Each transfer is recorded via [`record_deposit`](crate::application::balance::record_deposit)
//! — idempotent by `transaction_hash:user`, so a re-scan never double-credits — and
//! the relay then posts `Dr wallet:ton / Cr user-claim`; the watcher never touches
//! TigerBeetle, so money is still written last, in the relay.
//!
//! USDT arriving at the **treasury** from outside is watched but never credited (#245):
//! the chain names the wallet, not the person, and every claim needs a holder. It is
//! reported at `error!` with a counter (`telemetry`) — and by the treasury drift watch
//! as a surplus.
//!
//! **Finality.** TON is fast and categorical: toncenter only surfaces transactions from
//! committed masterchain blocks, so anything `/jetton/transfers` returns is already final —
//! there is no N-confirmations counter (unlike BEP20).
//!
//! **Cursor (deliberate divergence from per-account `lt`).** A single network-scoped row in
//! `deposit_scan_cursor` holds a **unix-time** high-watermark (`transaction_now`), not a
//! logical time. A logical time (`lt`) is monotonic only *per account*, so one network-wide
//! `lt` cursor over many owners would skip a lagging owner's deposit outright. A wall-clock
//! watermark is globally comparable, so the only way it drops a deposit is if the indexer
//! surfaces a final transaction MORE than `LOOKBACK_SECS` after its `transaction_now` while a
//! busier owner has already pushed the watermark past it — i.e. the safety margin is exactly
//! `LOOKBACK_SECS` of effective indexer lag, not infinite. It is set generously for that
//! reason. Each cycle re-scans that window below the watermark; the overlap is idempotent
//! (`record_deposit` dedupes by `transaction_hash`).
//!
//! Only an observed transfer raises the watermark, so a rail with no traffic would hold its
//! initial value forever — an unboundedly widening re-scan window, and a `updated_at` that
//! says nothing about liveness. A cycle that drains every owner therefore floats the
//! watermark up to `now - LOOKBACK_SECS` (see [`next_watermark`]), which is where a rail
//! carrying traffic already sits.

use std::{
	collections::HashSet,
	sync::{Arc, Mutex},
	time::Duration,
};

use domain::{
	money::{Network, TxRef, Usdt},
	users::UserId,
};
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
	application::balance::record_deposit,
	config::TonConfig,
	infrastructure::{
		deposits::PgDeposits,
		rails::{WatcherError, now_unix_secs, repo, set_scan_cursor},
		telemetry,
		ton_custody::TonCustody,
		ton_rpc::{JettonDeposit, TonRpc},
	},
};

/// Is a jetton transfer into the treasury NEW money, or our own funds being consolidated?
///
/// The TON sweep sends USDT **from a user's derived deposit address to the treasury**, and
/// that dollar is already counted in `wallet:ton` from the moment the deposit was credited —
/// the ledger tracks what we control, not which of our wallets holds it. Reporting the
/// arrival as unattributed would ask an operator to seed it, and that seed would post a
/// second `Dr wallet:ton` for one dollar, inventing money and breaking the global
/// `sum(custody) == sum(claims)` invariant.
///
/// A missing source is treated as NOT external: the indexer omits it for a mint, and the
/// safe failure here is to stay quiet about a real injection (visible on the drift watch,
/// and seedable by hand) rather than to prompt a double count.
///
/// `ours` is every wallet we control on this rail, lowercased raw `0:<hex>` — derived
/// deposit addresses, the gas station, and the treasury itself.
fn is_external_source(source: Option<&str>, ours: &HashSet<String>) -> bool {
	source.is_some_and(|source| !ours.contains(source))
}

/// Per-owner page size for `/jetton/transfers`.
const PAGE_LIMIT: u32 = 128;

/// Seconds re-scanned below the cursor each cycle, to absorb indexer lag (the indexer can
/// surface a final transaction after its `transaction_now`). This IS the safety margin against
/// dropping a lagging owner's deposit (see the module docstring), so it is set well above
/// toncenter's observed lag rather than at the bare minimum. The overlap is idempotent —
/// `record_deposit` dedupes by `transaction_hash`.
const LOOKBACK_SECS: u64 = 900;

/// Max poll interval after repeated cycle failures, in seconds — the adaptive ceiling
/// so a rate-limited endpoint isn't hammered at the normal cadence.
const CYCLE_MAX_BACKOFF_SECS: u64 = 300;

pub struct TonDepositWatcher {
	pool: PgPool,
	deposits: PgDeposits,
	relay: Arc<Notify>,
	rpc: TonRpc,
	config: TonConfig,
	/// Resolves the treasury + gas-station addresses so an out-of-band arrival on the hot
	/// wallet is seen and reported (never credited — see the module doc) and the sweep is
	/// told apart from it. `None` leaves the watcher user-deposits-only.
	custody: Option<Arc<TonCustody>>,
	/// Treasury arrivals already reported this process. The cycle re-reads
	/// `LOOKBACK_SECS` below the watermark every time, so without this one arrival would
	/// be filed as an incident on every poll for fifteen minutes; with it, once per process
	/// (a restart inside the window re-reports it once more, which is the honest side to
	/// err on). Bounded by the traffic a hot wallet receives from outside — operator
	/// top-ups, not user deposits.
	reported_treasury_arrivals: Mutex<HashSet<String>>,
}

impl TonDepositWatcher {
	pub fn new(pool: PgPool, relay: Arc<Notify>, config: TonConfig, custody: Option<Arc<TonCustody>>) -> Self {
		let rpc = TonRpc::new(config.api_url.clone(), config.api_key.clone());
		let deposits = PgDeposits::new(pool.clone());
		Self {
			pool,
			deposits,
			relay,
			rpc,
			config,
			custody,
			reported_treasury_arrivals: Mutex::new(HashSet::new()),
		}
	}

	/// The treasury + gas-station addresses, lowercased raw `0:<hex>`. A resolution failure
	/// degrades to `None` rather than failing the cycle: user deposits are the primary job
	/// and must keep crediting regardless. Both are `OnceCell`-cached in the adapter.
	async fn treasury_addresses(&self) -> Option<(String, Option<String>)> {
		let custody = self.custody.as_ref()?;
		match custody.treasury_address().await {
			Ok(treasury) => {
				let gas_station = custody.gas_station_address().await.ok().map(|a| a.to_lowercase());
				Some((treasury.to_lowercase(), gas_station))
			}
			Err(err) => {
				tracing::debug!("ton deposit watcher: treasury address unavailable this cycle, watching user deposits only: {err}");
				None
			}
		}
	}

	/// Poll until `shutdown` is cancelled. A failed cycle is logged and retried next poll
	/// from the unchanged cursor — at-least-once, and crediting is idempotent.
	///
	/// Consecutive failures ramp the poll interval exponentially (capped at
	/// [`CYCLE_MAX_BACKOFF_SECS`]) so a down or rate-limited endpoint isn't hammered at the
	/// normal cadence. A single successful cycle resets the backoff to `poll_secs`.
	pub async fn run(self, shutdown: CancellationToken) {
		info!(master = %self.config.usdt_master, testnet = self.config.is_testnet, "ton deposit watcher: watching jetton USDT deposits");
		let mut consecutive_failures: u64 = 0;
		loop {
			let delay = match self.scan_once().await {
				Ok(()) => {
					consecutive_failures = 0;
					self.config.poll_secs
				}
				Err(err) => {
					consecutive_failures += 1;
					let backoff = if consecutive_failures > 1 {
						(self.config.poll_secs * 2u64.pow(consecutive_failures as u32 - 1)).min(CYCLE_MAX_BACKOFF_SECS)
					} else {
						self.config.poll_secs
					};
					warn!("ton deposit watcher: scan cycle failed, retrying in {backoff}s (failure #{consecutive_failures}): {err}");
					backoff
				}
			};
			tokio::select! {
				() = shutdown.cancelled() => {
					info!("ton deposit watcher: shutdown requested — stopping");
					return;
				}
				() = tokio::time::sleep(Duration::from_secs(delay)) => {}
			}
		}
	}

	async fn scan_once(&self) -> Result<(), WatcherError> {
		let network = Network::Ton;
		let cursor = self.cursor(network).await?;
		let watched = self.watched_addresses(network).await?;
		// The treasury is drained as one more owner: jettons arriving there from outside our
		// own wallets are money the ledger does not describe, and nothing else in the system
		// would notice them (the drift watch reports the surplus, not the transfer).
		let (treasury, gas_station) = match self.treasury_addresses().await {
			Some((treasury, gas_station)) => (Some(treasury), gas_station),
			None => (None, None),
		};
		if watched.is_empty() && treasury.is_none() {
			// Nothing fundable yet — fast-forward to now so we don't re-scan an empty window.
			set_scan_cursor(&self.pool, network, now_unix_secs() as i64).await?;
			return Ok(());
		}
		// `None` marks the treasury owner — its arrivals are reported, never credited.
		let owners: Vec<(&String, Option<UserId>)> = watched.iter().map(|(a, u)| (a, Some(*u))).chain(treasury.iter().map(|t| (t, None))).collect();
		// Every wallet we control, folded to the one case the indexer's `source` is folded to.
		// The treasury report is gated on a sender outside this set.
		let ours: HashSet<String> = watched
			.iter()
			.map(|(a, _)| a.to_lowercase())
			.chain(treasury.iter().cloned())
			.chain(gas_station.iter().cloned())
			.collect();

		let from = cursor.saturating_sub(LOOKBACK_SECS);
		let mut high = cursor;
		// Whether every owner was drained to the head this cycle. Only then is it safe to float
		// the watermark up on an idle rail (see `next_watermark`) — a skipped owner may still
		// have an unseen transfer below `now - LOOKBACK_SECS`.
		let mut all_owners_drained = true;
		for (owner, user) in &owners {
			// Drain this owner to the head, page by page: a single capped page could leave
			// older transfers unfetched while ANOTHER owner's newer transfer advances the
			// global watermark past them — skipped forever. Paging until a short raw page
			// makes the post-loop watermark advance safe for every owner.
			let mut owner_from = from;
			loop {
				let page = match self.rpc.incoming_jetton_transfers(owner, &self.config.usdt_master, owner_from, PAGE_LIMIT).await {
					Ok(page) => page,
					Err(err) => {
						// One owner's fetch failing MUST NOT abort the whole cycle: a `?` here froze the
						// entire TON rail on a single unparseable stored address (indexer 422) — the
						// cursor never advanced and NO user's deposits were seen. Skip just this owner;
						// a transiently-failed valid owner is re-scanned within the LOOKBACK_SECS window
						// next cycle, and a permanently-bad address is never a real deposit target. So an RPC
						// failure never becomes a `WatcherError` here: the only cycle-fatal ones are DB,
						// decode and credit.
						warn!(owner, "ton deposit watcher: owner scan failed, skipping this owner this cycle: {err}");
						all_owners_drained = false;
						break;
					}
				};
				for transfer in &page.transfers {
					// Seen is seen: the watermark advances for a consolidation we deliberately
					// ignore exactly as for one we credit, or the cursor would stall on it.
					high = high.max(transfer.now);
					match user {
						Some(user) => self.credit(*user, network, transfer).await?,
						// The sweep also lands here — from OUR derived address — and that dollar is
						// already `wallet:ton` behind a user's claim; only outside money is news, and
						// even that is reported rather than credited (#245): the chain names the
						// wallet, not the person, and a claim needs a holder.
						None if is_external_source(transfer.source.as_deref(), &ours) => self.report_treasury_arrival(network, transfer)?,
						None => {}
					}
				}
				if page.raw_len < PAGE_LIMIT as usize {
					break;
				}
				if page.max_now <= owner_from {
					// A full page whose entries all share the start second, so time-paging can't
					// advance within THIS owner (≥128 credits to one address in one second — a
					// pathological case a time-cursor indexer genuinely can't page past). The 128
					// fetched here were credited; break to the NEXT owner rather than `return`ing
					// out of the whole cycle, which would starve every later owner forever (the
					// cursor never advances, so each cycle re-hits this same stuck page first). The
					// `LOOKBACK_SECS` window re-scans this owner next cycle; crediting is idempotent.
					warn!(owner, "ton deposit watcher: full page without time progress — skipping this owner for the cycle");
					all_owners_drained = false;
					break;
				}
				// Resume AT the newest raw time (inclusive): boundary entries are refetched
				// and deduped, filtered rows still advance the window.
				owner_from = page.max_now;
			}
		}
		// Advance to the newest transaction time seen (never backwards). The next cycle
		// re-scans `LOOKBACK_SECS` below this; the overlap is deduped by `record_deposit`.
		let next = next_watermark(cursor, high, all_owners_drained, now_unix_secs());
		if next > cursor {
			set_scan_cursor(&self.pool, network, next as i64).await?;
		}
		Ok(())
	}

	async fn credit(&self, user: UserId, network: Network, transfer: &JettonDeposit) -> Result<(), WatcherError> {
		let amount = Usdt::from_onchain(network, transfer.amount).map_err(|e| WatcherError::Decode(e.to_string()))?;
		if amount.is_zero() {
			return Ok(());
		}
		// Disambiguate per recipient like the BEP20/TRC20 watchers: `deposits.tx_ref` is a global
		// key, so compose the on-chain transaction hash with the credited user. In practice each
		// incoming jetton transfer is its own transaction on the recipient's jetton wallet (so the
		// hash is already unique), but the user id makes two transfers under one hash — an indexer
		// quirk — impossible to collapse across users. The user id (a 36-char uuid) keeps the key
		// well under `TxRef`'s length cap regardless of the indexer's hash encoding, and is stable
		// across re-scans (the address→user map is fixed), so idempotency holds. A treasury
		// arrival recorded by hand uses the reserved `piggybank` word in this position instead
		// (`ton_custody::PIGGYBANK_RECIPIENT`) — never a uuid, so the two can never collide.
		let recipient = user.to_string();
		let tx_ref = TxRef::parse(&format!("{}:{recipient}", transfer.tx_hash)).map_err(|e| WatcherError::Decode(e.to_string()))?;
		let newly = record_deposit(&self.deposits, &self.relay, tx_ref, user, network, amount)
			.await
			.map_err(|e| WatcherError::Credit(e.to_string()))?;
		if newly {
			info!(recipient, tx = %transfer.tx_hash, "ton deposit watcher: credited on-chain jetton USDT deposit");
		}
		Ok(())
	}

	/// File an outside arrival on the treasury as an incident, once per process: the
	/// counter and the `error!` live in `telemetry`, this only dedupes across the
	/// `LOOKBACK_SECS` re-scans. Nothing is written to the ledger — see the module doc.
	fn report_treasury_arrival(&self, network: Network, transfer: &JettonDeposit) -> Result<(), WatcherError> {
		let amount = Usdt::from_onchain(network, transfer.amount).map_err(|e| WatcherError::Decode(e.to_string()))?;
		if amount.is_zero() {
			return Ok(());
		}
		let first_sighting = self
			.reported_treasury_arrivals
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.insert(transfer.tx_hash.clone());
		if first_sighting {
			telemetry::note_unattributed_treasury_inflow(network.as_str(), &transfer.tx_hash, transfer.source.as_deref().unwrap_or("?"), &amount.to_decimal_string());
		}
		Ok(())
	}

	/// The deposit cursor (a unix-time watermark). On first run, initialize to the configured
	/// start (`TON_DEPOSIT_START_UTIME`, unix seconds) or the current time (watch from now),
	/// ignoring pre-existing on-chain history.
	async fn cursor(&self, network: Network) -> Result<u64, WatcherError> {
		let existing: Option<i64> = sqlx::query_scalar("SELECT last_scanned_block FROM deposit_scan_cursor WHERE network = $1")
			.bind(network.as_str())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo)?;
		if let Some(cursor) = existing {
			return Ok(cursor.max(0) as u64);
		}
		let init = self.config.start_cursor.unwrap_or_else(now_unix_secs);
		sqlx::query("INSERT INTO deposit_scan_cursor (network, last_scanned_block) VALUES ($1, $2) ON CONFLICT (network) DO NOTHING")
			.bind(network.as_str())
			.bind(init as i64)
			.execute(&self.pool)
			.await
			.map_err(repo)?;
		Ok(init)
	}

	/// The watched (owner address → user) map: only `derived` (fundable) TON addresses. The
	/// stored address is the raw `0:hex` owner wallet, passed straight to toncenter's
	/// `owner_address` filter.
	async fn watched_addresses(&self, network: Network) -> Result<Vec<(String, UserId)>, WatcherError> {
		let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as("SELECT user_id, address FROM user_deposit_addresses WHERE network = $1 AND address_kind = 'derived'")
			.bind(network.as_str())
			.fetch_all(&self.pool)
			.await
			.map_err(repo)?;
		Ok(rows.into_iter().map(|(uid, address)| (address, UserId::from_raw(uid))).collect())
	}
}

/// The watermark to store after a cycle: the newest transfer time seen, never below the cursor
/// it started from.
///
/// On an idle rail `high` never rises above `cursor` — nothing is found to raise it — so a rail
/// with no traffic would hold its initial watermark forever. Two costs, both real: the re-scanned
/// window (`cursor - LOOKBACK_SECS` .. head) grows without bound, so each cycle asks the indexer
/// for a longer history than the last; and `updated_at` stops being a liveness signal, leaving
/// "healthy but idle" indistinguishable from "wedged" — the EVM watcher has no such ambiguity
/// because it advances to the chain head whether or not any log matched.
///
/// So when every owner drained to the head, float the watermark up to `now - LOOKBACK_SECS`.
/// That preserves the invariant exactly as a busy rail has it: with traffic the cursor sits at
/// the newest transfer, i.e. ~`now`, and a transaction is only missed if the indexer surfaces it
/// more than `LOOKBACK_SECS` late. Idling now gets that same margin instead of an ever-widening
/// one. If any owner was skipped, hold the cursor — that owner may still have an unseen transfer
/// below the floor, and re-scanning it next cycle is the only thing keeping it.
fn next_watermark(cursor: u64, high: u64, all_owners_drained: bool, now: u64) -> u64 {
	let floor = if all_owners_drained { now.saturating_sub(LOOKBACK_SECS) } else { 0 };
	high.max(floor).max(cursor)
}

#[cfg(test)]
mod tests {
	use super::*;

	const NOW: u64 = 1_800_000_000;

	/// The invariant-critical case, mirroring the EVM watcher's. The sweep arrives at the
	/// treasury looking exactly like an operator top-up, and only the sender separates them:
	/// report a consolidation as an unattributed inflow and an operator seeds a dollar that
	/// is already on the ledger.
	#[test]
	fn only_a_sender_outside_our_own_wallets_is_new_capital() {
		let deposit_address = "0:aaaa000000000000000000000000000000000000000000000000000000000001";
		let gas_station = "0:e13207248f7f8dd9d1d332c3b7d5cc0092c2b01db41b40234614b0c83ac066bb";
		let treasury = "0:94784e25d2a4b6113576222ca09f782a765f22c895a1a8a512b38382d2ee8ec1";
		let ours: HashSet<String> = [deposit_address, gas_station, treasury].into_iter().map(str::to_string).collect();

		// The sweep consolidating a user's deposit — already `wallet:ton`, must NOT re-credit.
		assert!(!is_external_source(Some(deposit_address), &ours));
		assert!(!is_external_source(Some(gas_station), &ours));
		assert!(!is_external_source(Some(treasury), &ours));
		// The operator's own wallet funding the rail — the one arrival that is new money,
		// reported for the operator to seed under their own name.
		assert!(is_external_source(Some("0:7d133d4e425c8e00de015513a44e66e6d163b21e71720aec7579965e5de28c55"), &ours));
		// A source the indexer omitted is never assumed external: skipping a real injection is
		// recoverable by hand, double-counting one is not.
		assert!(!is_external_source(None, &ours));
	}

	#[test]
	fn an_idle_rail_floats_up_instead_of_holding_its_initial_watermark() {
		// No transfers, so `high` stays at the cursor — the case that pinned the TON cursor at
		// its creation time for weeks and made the rail look wedged.
		let cursor = NOW - 3_000_000;
		assert_eq!(next_watermark(cursor, cursor, true, NOW), NOW - LOOKBACK_SECS);
	}

	#[test]
	fn floating_up_keeps_a_full_lookback_window_below_the_cursor() {
		// The floor must stay a whole LOOKBACK_SECS behind now, never at now itself, or the
		// next cycle would scan a window that has already passed.
		let next = next_watermark(NOW - 3_000_000, NOW - 3_000_000, true, NOW);
		assert_eq!(NOW - next, LOOKBACK_SECS);
	}

	#[test]
	fn a_skipped_owner_holds_the_cursor_still() {
		// Floating up past a skipped owner would drop any transfer of theirs below the floor.
		let cursor = NOW - 3_000_000;
		assert_eq!(next_watermark(cursor, cursor, false, NOW), cursor);
	}

	#[test]
	fn a_real_transfer_still_wins_over_the_idle_floor() {
		let cursor = NOW - 3_000_000;
		let newest = NOW - 10;
		assert_eq!(next_watermark(cursor, newest, true, NOW), newest);
	}

	#[test]
	fn the_watermark_never_moves_backwards() {
		// A cursor already ahead of the floor (a busy rail, or a configured future start) must
		// not be dragged back down to `now - LOOKBACK_SECS`.
		let cursor = NOW - 10;
		assert_eq!(next_watermark(cursor, cursor, true, NOW), cursor);
	}

	#[test]
	fn a_clock_before_the_lookback_window_cannot_underflow() {
		assert_eq!(next_watermark(0, 0, true, LOOKBACK_SECS / 2), 0);
	}
}
