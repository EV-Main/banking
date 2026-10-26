//! On-chain deposit watcher — credits user balances from confirmed USDT transfers.
//!
//! A read-mostly background task (sibling to the [`bridge`](super::bridge) consumer): it
//! polls an EVM JSON-RPC for ERC-20 `Transfer` logs of the USDT contract whose `to` is
//! one of our users' **derived** deposit addresses, waits `confirmations` blocks (reorg
//! safety), and records each via [`record_deposit`](crate::application::balance::record_deposit)
//! — idempotent by the on-chain `tx_ref` (`txhash:logindex`), so a re-scan never double-
//! credits. The relay then posts `Dr wallet:<net> / Cr user-claim`; the watcher itself
//! never touches TigerBeetle, so money is still written last, in the relay.
//!
//! Resume is from `deposit_scan_cursor` (per network). On first run it starts at the
//! configured start block, else the current safe head (watch from now). Only blocks at or
//! below `latest − confirmations` are scanned, so shallow reorgs are absorbed; a reorg
//! deeper than `confirmations` is a known, out-of-scope residual (reconciliation territory).
//!
//! A provider that has **pruned** the history the cursor points at (shared nodes keep a
//! bounded window of logs) can never serve that window, so the live scan bisects for the
//! oldest block it still has, jumps the cursor there, and reports the skipped range as an
//! incident at `error!` — those deposits are NOT credited and must be reconciled by hand
//! (`RecordDeposit`). Skipping is the lesser evil: retrying the pruned window forever wedges
//! the rail and loses every deposit AFTER the gap too.
//!
//! USDT arriving at the rail's **treasury** hot wallet from outside is watched but never
//! credited (#245): the chain names the wallet, not the person, and every claim needs a
//! holder. It is reported at `error!` with a counter (`telemetry`) — and by the treasury
//! drift watch as a surplus.
//!
//! Generic over the EVM rail — one instance per chain (BEP20, Polygon), keyed by
//! `config.network`; the raw log value is scaled into canonical base units via
//! [`Usdt::from_onchain`], so a 6-dp rail (Polygon) and an 18-dp rail (BEP20) credit correctly.
//! The `eth_getLogs` `to`-topic filter (an OR over the watched addresses) means the endpoint
//! MUST support `eth_getLogs` — some public nodes don't; point the rail's RPC URL at one that does.

use std::{
	collections::HashMap,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use domain::{
	money::{Network, TxRef, Usdt},
	users::UserId,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// The watcher failure taxonomy, shared with every other rail's watcher and defined in
/// [`rails`](super::rails). Re-exported under this module's original path so importers
/// outside `infrastructure` are unaffected by the move.
pub use crate::infrastructure::rails::WatcherError;
use crate::{
	application::balance::record_deposit,
	config::EvmConfig,
	infrastructure::{
		custody::ChainCustody,
		deposits::PgDeposits,
		evm_rpc::{TRANSFER_TOPIC, address_from_topic, hex_to_u64, pad_topic, word_to_u128},
		rails::{repo, set_scan_cursor},
		telemetry,
	},
};

/// Max retries for a single RPC call (`eth_blockNumber` / `eth_getLogs`) when the
/// error is retryable (rate-limit, timeout). Caps the total backoff within one scan
/// cycle so a stuck endpoint doesn't stall the watcher forever.
const RPC_MAX_RETRIES: u32 = 5;
/// Base backoff in ms before the first retry.
const RPC_BASE_BACKOFF_MS: u64 = 1000;
/// Max backoff between retries — caps the exponential ramp.
const RPC_MAX_BACKOFF_MS: u64 = 30_000;
/// Max poll interval after repeated cycle failures, in seconds — the adaptive ceiling
/// so a down endpoint isn't hammered at the normal cadence.
const CYCLE_MAX_BACKOFF_SECS: u64 = 300;
/// Floor for the adaptive `eth_getLogs` window. Below this the endpoint is refusing the
/// query for a reason other than its width (free tiers also cap how deep into history a
/// getLogs may reach), so narrowing further is pointless and the cycle fails instead.
const MIN_BLOCK_RANGE: u64 = 16;
/// Pause between consecutive `eth_getLogs` chunks while catching up. Without it the
/// catch-up loop fires chunks as fast as the endpoint answers, which is precisely the
/// burst a free-tier rate limit rejects — pacing trades a slower backfill for one that
/// actually completes.
const CHUNK_PACE_MS: u64 = 250;
/// How far the scan may fall behind the safe head before it is called out on its own.
/// A lagging scan means deposits are landing on-chain UNCREDITED, which is worth an alert
/// in a way that an individual throttled RPC call is not.
const LAG_WARN_BLOCKS: u64 = 5_000;
/// How many times one scan may jump its cursor past pruned history before it gives up and
/// fails the cycle. The retained window moves with the chain, so a clamp can land a hair
/// behind it and be refused once more; a provider that keeps refusing after this many
/// bisections is not serving logs at all, and the cycle backoff is the right response.
const PRUNE_CLAMPS_PER_SCAN: u32 = 3;
/// Blocks skipped PAST a confirmed retention boundary before the scan resumes, doubled on
/// each further clamp in the same scan. The boundary is not a line but an edge moving with
/// the chain (~1.3 blocks/s on BSC): resuming exactly at the block the bisection confirmed
/// puts the next chunk on that edge, and by the time it is requested the edge has passed it
/// — refused again, another bisection, another 10–40-block "incident", three times, cycle
/// failed, repeat. v0.16.0 sat in that livelock on BSC with the lag frozen at the width of
/// the provider's window while Polygon (half the block rate) escaped on the first clamp. 256
/// blocks is ~3 min of BSC or ~8 min of Polygon — nothing against a ~90k-block window, and
/// the operator reconciles the slack with the rest of the skipped range.
const PRUNE_SLACK_BLOCKS: u64 = 256;

/// Whether a scan owns the persistent cursor. The live cycle does; a backfill must not
/// touch it, or it would drag the live scan backwards or skip it past unread blocks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorPolicy {
	Advance,
	Leave,
}

/// What a scan did, for the operator who asked for it. `credited` counts only NEW records —
/// a re-run over the same window reports zero, which is the honest answer to "did anything
/// come of this", not a failure.
#[derive(Clone, Copy, Debug)]
pub struct ScanSummary {
	pub scanned_to: u64,
	pub credited: u64,
}

/// The on-chain deposit watcher task. Holds its own pool clone so its polling reads don't
/// compete with request traffic, and the relay `Notify` so a credit dispatches promptly.
pub struct DepositWatcher {
	pool: PgPool,
	deposits: PgDeposits,
	relay: Arc<Notify>,
	http: reqwest::Client,
	config: EvmConfig,
	/// The `eth_getLogs` window actually in use, in blocks. Starts at the configured
	/// `max_block_range` and halves whenever the endpoint rejects the query as too wide, so
	/// the scan settles on the widest window the endpoint accepts. It never widens again:
	/// a free tier's cap is static, and re-probing it would reintroduce the failed call
	/// this narrowing exists to avoid.
	block_range: AtomicU64,
	/// Resolves the rail's treasury + gas-station addresses so an out-of-band arrival on
	/// the hot wallet is seen and reported (never credited — see the module doc) and the
	/// sweep is told apart from it. `None` leaves the watcher user-deposits-only — what an
	/// unwired rail gets.
	custody: Option<Arc<ChainCustody>>,
}

impl DepositWatcher {
	pub fn new(pool: PgPool, relay: Arc<Notify>, config: EvmConfig, custody: Option<Arc<ChainCustody>>) -> Self {
		let http = reqwest::Client::builder()
			.timeout(Duration::from_secs(20))
			.build()
			.expect("reqwest client builds with default config");
		let deposits = PgDeposits::new(pool.clone());
		let block_range = AtomicU64::new(config.max_block_range.max(1));
		Self {
			pool,
			deposits,
			relay,
			http,
			config,
			block_range,
			custody,
		}
	}

	/// The rail's treasury + gas-station addresses, lowercased, or `None` when the rail has
	/// no custody adapter or the signer could not be reached this cycle. A failure must NOT
	/// fail the scan: user deposits are the watcher's primary job and they keep crediting
	/// with or without the treasury view. `ChainCustody` caches each address in a `OnceCell`,
	/// so this is one signer round-trip per process, not per cycle.
	async fn treasury_addresses(&self) -> Option<(String, Option<String>)> {
		let custody = self.custody.as_ref()?;
		match custody.treasury_address().await {
			Ok(treasury) => {
				let gas_station = custody.gas_station_address().await.ok().map(|a| a.to_lowercase());
				Some((treasury.to_lowercase(), gas_station))
			}
			Err(err) => {
				tracing::debug!(network = %self.config.network, "deposit watcher: treasury address unavailable this cycle, watching user deposits only: {err}");
				None
			}
		}
	}

	/// Poll until `shutdown` is cancelled. A failed cycle is logged and retried next poll
	/// from the unchanged cursor — at-least-once, and crediting is idempotent, so nothing is
	/// lost or double-counted.
	///
	/// Consecutive failures ramp the poll interval exponentially (capped at
	/// [`CYCLE_MAX_BACKOFF_SECS`]) so a down or rate-limited endpoint isn't hammered at the
	/// normal cadence. A single successful cycle resets the backoff to `poll_secs`.
	pub async fn run(self, shutdown: CancellationToken) {
		// Report the endpoint the SCAN actually uses, not `rpc_url` — they differ whenever a
		// dedicated logs endpoint is configured, and logging the wrong one sends whoever is
		// debugging a silent watcher to the wrong provider's dashboard.
		info!(network = %self.config.network, rpc = %rpc_host(self.scan_endpoint()), contract = %self.config.usdt_contract, confirmations = self.config.confirmations, "deposit watcher: watching EVM USDT deposits");
		if self.config.logs_rpc_url.is_none() {
			// The fallback is `rpc_url`, chosen for the money-moving paths (nonce, balance,
			// broadcast). The public full nodes that serve those well are exactly the ones that
			// refuse eth_getLogs (-32005), so this combination detects NO deposits at all — a
			// silent failure worth naming loudly at boot rather than discovering from a
			// customer's missing balance.
			warn!(
				network = %self.config.network,
				rpc = %rpc_host(&self.config.rpc_url),
				"deposit watcher: no dedicated eth_getLogs endpoint set (BSC_LOGS_RPC_URL / POLYGON_LOGS_RPC_URL) — falling back to the main RPC, which on the default public nodes REJECTS eth_getLogs; deposits will go undetected until one is configured"
			);
		}
		let mut consecutive_failures: u64 = 0;
		loop {
			let delay = match self.scan_once().await {
				Ok(()) => {
					consecutive_failures = 0;
					self.config.poll_secs
				}
				Err(err) => {
					consecutive_failures += 1;
					let backoff = cycle_backoff_secs(self.config.poll_secs, consecutive_failures);
					warn!(network = %self.config.network, "deposit watcher: scan cycle failed, retrying in {backoff}s (failure #{consecutive_failures}): {err}");
					backoff
				}
			};
			tokio::select! {
				() = shutdown.cancelled() => {
					info!("deposit watcher: shutdown requested — stopping");
					return;
				}
				() = tokio::time::sleep(Duration::from_secs(delay)) => {}
			}
		}
	}

	async fn scan_once(&self) -> Result<(), WatcherError> {
		let network = self.config.network;
		let latest = self.block_number().await?;
		let safe_head = latest.saturating_sub(self.config.confirmations);
		let last_scanned = self.cursor(network, safe_head).await?;
		if safe_head <= last_scanned {
			return Ok(());
		}
		// A scan that has fallen behind is the condition worth alerting on: those blocks hold
		// deposits nobody has been credited for yet. Reported once per cycle, against the
		// per-cycle backoff, rather than once per throttled RPC call.
		let lag = safe_head - last_scanned;
		if lag > LAG_WARN_BLOCKS {
			warn!(network = %network, lag_blocks = lag, last_scanned, safe_head, "deposit watcher: scan is behind the chain — deposits in the gap are not yet credited");
		}
		self.scan_range(last_scanned + 1, safe_head, CursorPolicy::Advance).await?;
		Ok(())
	}

	/// Scan `[from, to]` for arrivals at our addresses and credit each one.
	///
	/// The live cycle and an operator backfill are the SAME scan over different windows —
	/// same address set, same attribution, same idempotent credit. Only the cursor differs,
	/// which is what [`CursorPolicy`] carries: the live scan owns the cursor and advances it
	/// chunk by chunk, while a backfill must leave it alone. Moving it from a backfill would
	/// either strand the live scan in the past or, worse, jump it forward over blocks nobody
	/// has read — the exact way a gap gets created rather than closed.
	///
	/// Safe to re-run over any window: crediting is idempotent by `tx_ref`, so an overlapping
	/// or repeated backfill costs RPC calls and changes nothing else.
	///
	/// Pruned history is handled only for the live scan: it jumps the cursor past the oldest
	/// block the provider still serves (see [`first_retained_block`] and
	/// [`resume_after_prune`] for why "past" and not "to") and files the skipped range as an
	/// incident, because the alternative — retrying the pruned window until the backoff cap —
	/// loses every deposit after the gap as well. A backfill asked for a window
	/// its endpoint no longer has gets the error back instead: the operator chose that window
	/// deliberately, and "done, credited 0" would be a lie.
	pub async fn scan_range(&self, from: u64, to: u64, cursor: CursorPolicy) -> Result<ScanSummary, WatcherError> {
		let network = self.config.network;
		let mut summary = ScanSummary {
			scanned_to: from.saturating_sub(1),
			credited: 0,
		};
		if from > to {
			return Ok(summary);
		}
		let watched = self.watched_addresses(network).await?;
		let (treasury, gas_station) = match self.treasury_addresses().await {
			Some((treasury, gas_station)) => (Some(treasury), gas_station),
			None => (None, None),
		};
		if watched.is_empty() && treasury.is_none() {
			// Nothing fundable yet. The live scan still fast-forwards, or it would re-read the
			// same empty window every cycle forever; a backfill just reports the window done.
			if matches!(cursor, CursorPolicy::Advance) {
				set_scan_cursor(&self.pool, network, to as i64).await?;
			}
			summary.scanned_to = to;
			return Ok(summary);
		}
		let topic_addrs: Vec<Value> = watched.keys().chain(treasury.iter()).map(|a| Value::String(pad_topic(a))).collect();

		let mut next = from;
		let mut clamps: u32 = 0;
		while next <= to {
			let (logs, chunk_end) = match self.get_logs(next, to, &topic_addrs).await {
				Ok(chunk) => chunk,
				Err(err) if matches!(cursor, CursorPolicy::Advance) && is_pruned(&err) && clamps < PRUNE_CLAMPS_PER_SCAN => {
					clamps += 1;
					let Some(first) = first_retained_block(next, to, |block| self.is_retained(block, to, &topic_addrs)).await? else {
						// No confirmed boundary: the provider refuses everything (its log index is
						// off), or it answered the same block both ways (a pool of nodes with
						// different retention). Neither is a prune to skip past — keep the cursor
						// where it is and back off as before, so a re-read on a better node or a
						// recovered index still credits these blocks.
						return Err(err);
					};
					let resume = resume_after_prune(first, clamps, to);
					let skipped_to = resume - 1;
					// `error!`, not `warn!`: this is a money incident that reaches Sentry, and the
					// from/to are what the operator needs to reconcile the window by hand. The
					// range INCLUDES the slack past the boundary: those blocks are not read either.
					error!(
						network = %network,
						from = next,
						to = skipped_to,
						skipped_blocks = resume - next,
						retained_from = first,
						"deposit watcher: provider has pruned history below block {first} — skipping blocks {next}..={skipped_to} (boundary plus a {}-block slack to get off the moving edge); USDT deposits landing in that window were NOT credited and must be reconciled by hand (RecordDeposit, or a backfill on a full-history endpoint): {err}",
						resume - first
					);
					// Persist the jump before anything else can fail, so the gap is filed exactly
					// once: a later throttled chunk must not re-run the bisection and re-alert.
					set_scan_cursor(&self.pool, network, skipped_to as i64).await?;
					next = resume;
					continue;
				}
				Err(err) => return Err(err),
			};
			for log in &logs {
				let Some(transfer) = decode_transfer(log) else { continue };
				match recipient_of(&transfer, &watched, treasury.as_deref(), gas_station.as_deref()) {
					Recipient::User(user) =>
						if self.credit(user, network, &transfer).await? {
							summary.credited += 1;
						},
					Recipient::UnattributedTreasury => {
						let amount = Usdt::from_onchain(network, transfer.value).map_err(|e| WatcherError::Decode(e.to_string()))?;
						if !amount.is_zero() {
							telemetry::note_unattributed_treasury_inflow(network.as_str(), &transfer.tx_hash, &transfer.from, &amount.to_decimal_string());
						}
					}
					Recipient::NotOurs => {}
				}
			}
			// Advance only after the chunk's deposits are recorded. A crash between recording
			// and this update re-scans the chunk; `record_deposit` is idempotent by tx_ref.
			// An unattributed treasury arrival advances the cursor exactly like a credit: it
			// was reported once, and re-reading the chunk forever would only repeat the
			// report while crediting nothing.
			if matches!(cursor, CursorPolicy::Advance) {
				set_scan_cursor(&self.pool, network, chunk_end as i64).await?;
			}
			summary.scanned_to = chunk_end;
			next = chunk_end + 1;
			if next <= to {
				tokio::time::sleep(Duration::from_millis(CHUNK_PACE_MS)).await;
			}
		}
		Ok(summary)
	}

	async fn credit(&self, user: UserId, network: Network, transfer: &Transfer) -> Result<bool, WatcherError> {
		let amount = Usdt::from_onchain(network, transfer.value).map_err(|e| WatcherError::Decode(e.to_string()))?;
		if amount.is_zero() {
			return Ok(false); // a legal but meaningless zero-value Transfer — not a deposit.
		}
		let tx_ref = TxRef::parse(&transfer.tx_ref()).map_err(|e| WatcherError::Decode(e.to_string()))?;
		let newly = record_deposit(&self.deposits, &self.relay, tx_ref, user, network, amount)
			.await
			.map_err(|e| WatcherError::Credit(e.to_string()))?;
		if newly {
			info!(tx = %transfer.tx_hash, "deposit watcher: credited on-chain USDT deposit");
		}
		Ok(newly)
	}

	// ── JSON-RPC ────────────────────────────────────────────────────────────────
	/// The endpoint the scan's reads go to: the dedicated logs endpoint when configured,
	/// else the rail's main RPC. Free full nodes increasingly paywall `eth_getLogs`
	/// specifically, so the scan and the money-moving paths can want different providers.
	fn scan_endpoint(&self) -> &str {
		self.config.logs_rpc_url.as_deref().unwrap_or(&self.config.rpc_url)
	}

	async fn block_number(&self) -> Result<u64, WatcherError> {
		let result = self.rpc("eth_blockNumber", json!([])).await?;
		let hex = result.as_str().ok_or_else(|| WatcherError::Rpc("eth_blockNumber: non-string result".into()))?;
		hex_to_u64(hex).ok_or_else(|| WatcherError::Rpc(format!("eth_blockNumber: unparseable {hex}")))
	}

	/// One chunk of Transfer logs starting at `from`, plus the last block it covers — the
	/// caller advances the cursor to that block rather than assuming the window it asked for.
	///
	/// A "range too wide" rejection is DETERMINISTIC: the identical request fails every time,
	/// so retrying it (as a rate limit is retried) only burns attempts and log lines. The one
	/// response that can succeed is a narrower window, so that is what this does — halving
	/// until the endpoint accepts it or [`MIN_BLOCK_RANGE`] is reached, at which point the
	/// refusal is about something other than width and the cycle fails.
	async fn get_logs(&self, from: u64, safe_head: u64, addresses: &[Value]) -> Result<(Vec<Value>, u64), WatcherError> {
		loop {
			let range = self.block_range.load(Ordering::Relaxed);
			let to = from.saturating_add(range - 1).min(safe_head);
			match self.rpc("eth_getLogs", self.logs_params(from, to, addresses)).await {
				Ok(result) => {
					let logs = result.as_array().cloned().ok_or_else(|| WatcherError::Rpc("eth_getLogs: result is not an array".into()))?;
					return Ok((logs, to));
				}
				Err(err) if matches!(classify(&err.to_string()), RpcAction::Narrow) && range > MIN_BLOCK_RANGE => {
					let narrowed = (range / 2).max(MIN_BLOCK_RANGE);
					self.block_range.store(narrowed, Ordering::Relaxed);
					// Bounded: the window only ever shrinks, so this fires at most
					// log2(max_block_range / MIN_BLOCK_RANGE) times for the process's life.
					warn!(network = %self.config.network, "deposit watcher: endpoint refused a {range}-block eth_getLogs window, narrowing to {narrowed}: {err}");
				}
				Err(err) => return Err(err),
			}
		}
	}

	fn logs_params(&self, from: u64, to: u64, addresses: &[Value]) -> Value {
		json!([{
			"fromBlock": format!("0x{from:x}"),
			"toBlock": format!("0x{to:x}"),
			"address": self.config.usdt_contract,
			"topics": [TRANSFER_TOPIC, Value::Null, addresses],
		}])
	}

	/// Does the provider still serve logs starting at `block`? The probe for
	/// [`first_retained_block`]: a [`MIN_BLOCK_RANGE`]-wide `eth_getLogs`, narrow enough that
	/// no endpoint refuses it for width, so the only refusal left to read is "pruned". Any
	/// other failure is the caller's to handle — a rate limit mid-bisection has already been
	/// retried by [`rpc`] and fails the cycle as it would anywhere else. Paced like the
	/// catch-up chunks, for the same reason.
	async fn is_retained(&self, block: u64, safe_head: u64, addresses: &[Value]) -> Result<bool, WatcherError> {
		let to = block.saturating_add(MIN_BLOCK_RANGE - 1).min(safe_head);
		tokio::time::sleep(Duration::from_millis(CHUNK_PACE_MS)).await;
		match self.rpc("eth_getLogs", self.logs_params(block, to, addresses)).await {
			Ok(_) => Ok(true),
			Err(err) if is_pruned(&err) => Ok(false),
			Err(err) => Err(err),
		}
	}

	/// Issue a single JSON-RPC call — no retry, the raw transport. Used by the retry
	/// loop in [`rpc`] and kept thin so the retry counter and backoff live above it.
	async fn rpc_once(&self, method: &str, params: &Value) -> Result<Value, WatcherError> {
		let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
		let response = self
			.http
			.post(self.scan_endpoint())
			.json(&body)
			.send()
			.await
			.map_err(|e| WatcherError::Rpc(format!("{method}: request failed: {e}")))?;
		// A throttling proxy answers 429 (or a 5xx) with an HTML body, not a JSON-RPC error
		// object, so the status has to be inspected before the body is parsed as JSON —
		// otherwise a throttle reads as a permanent "bad json" and the cycle gives up
		// instead of backing off.
		//
		// The body still has to come along. Providers also return a perfectly good JSON-RPC
		// error object WITH a 4xx status (drpc answers an over-wide `eth_getLogs` that way),
		// and that body carries the only signal separating "narrow the window and retry"
		// from "give up". Reporting the bare status discarded it, so every 4xx looked fatal
		// and the adaptive narrowing below never engaged — the Polygon rail sat refused for
		// days behind a `http status 400` that was really a range error.
		let status = response.status();
		if !status.is_success() {
			let body = response.text().await.unwrap_or_default();
			// One line, bounded: this lands in a log and the useful part (the JSON-RPC error
			// object) is at the front. An HTML error page just contributes noise.
			let detail: String = body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(400).collect();
			return Err(WatcherError::Rpc(format!("{method}: http status {} {detail}", status.as_u16())));
		}
		let response: Value = response.json().await.map_err(|e| WatcherError::Rpc(format!("{method}: bad json: {e}")))?;
		if let Some(err) = response.get("error").filter(|e| !e.is_null()) {
			return Err(WatcherError::Rpc(format!("{method}: rpc error: {err}")));
		}
		response.get("result").cloned().ok_or_else(|| WatcherError::Rpc(format!("{method}: response had no result")))
	}

	/// Call `rpc_once` with exponential-backoff retry for transient errors (rate
	/// limits, timeouts). Non-retryable errors fail immediately; retryable errors
	/// back off 1s → 2s → 4s → 8s → 16s (capped at 30s), up to
	/// [`RPC_MAX_RETRIES`] attempts total. The scan's calls (blockNumber +
	/// getLogs) go to the logs endpoint when one is configured — free full nodes
	/// increasingly paywall eth_getLogs specifically, while the money-moving paths
	/// stay on the (reliable) main rpc_url.
	///
	/// Retries log at DEBUG, not WARN. Being throttled by a public endpoint is the expected
	/// steady state here, and a retry that then succeeds is a non-event; emitting a warning
	/// per attempt turned one throttled call into five alerts. The cycle-level `warn!` in
	/// [`run`] is the signal — it fires once per cycle, after the retries are exhausted, and
	/// rides the cycle backoff instead of the RPC cadence.
	async fn rpc(&self, method: &str, params: Value) -> Result<Value, WatcherError> {
		let mut attempt: u32 = 0;
		loop {
			match self.rpc_once(method, &params).await {
				Ok(val) => return Ok(val),
				Err(e) => {
					let msg = e.to_string();
					if !matches!(classify(&msg), RpcAction::Backoff) || attempt >= RPC_MAX_RETRIES {
						return Err(e);
					}
					attempt += 1;
					let ms = (RPC_BASE_BACKOFF_MS << (attempt - 1)).min(RPC_MAX_BACKOFF_MS);
					tracing::debug!(network = %self.config.network, "deposit watcher: {method} retryable error (attempt {attempt}/{RPC_MAX_RETRIES}), backing off {ms}ms: {msg}");
					tokio::time::sleep(Duration::from_millis(ms)).await;
				}
			}
		}
	}

	// ── cursor + watched addresses (Postgres control plane) ───────────────────────
	/// The last fully-scanned block. On first run, initialize to `start_block − 1` (so the
	/// configured start block is the first scanned) or the current safe head (watch from now).
	async fn cursor(&self, network: Network, default_head: u64) -> Result<u64, WatcherError> {
		let existing: Option<i64> = sqlx::query_scalar("SELECT last_scanned_block FROM deposit_scan_cursor WHERE network = $1")
			.bind(network.as_str())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo)?;
		if let Some(block) = existing {
			return Ok(block.max(0) as u64);
		}
		let init = self.config.start_block.map(|b| b.saturating_sub(1)).unwrap_or(default_head);
		sqlx::query("INSERT INTO deposit_scan_cursor (network, last_scanned_block) VALUES ($1, $2) ON CONFLICT (network) DO NOTHING")
			.bind(network.as_str())
			.bind(init as i64)
			.execute(&self.pool)
			.await
			.map_err(repo)?;
		Ok(init)
	}

	async fn watched_addresses(&self, network: Network) -> Result<HashMap<String, UserId>, WatcherError> {
		let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as("SELECT user_id, address FROM user_deposit_addresses WHERE network = $1 AND address_kind = 'derived'")
			.bind(network.as_str())
			.fetch_all(&self.pool)
			.await
			.map_err(repo)?;
		Ok(rows.into_iter().map(|(uid, address)| (address.to_lowercase(), UserId::from_raw(uid))).collect())
	}
}

/// Who a scanned transfer is for — the one decision the scan makes per log, pure so it
/// is provable without a chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Recipient {
	/// Landed on this user's derived deposit address: credit them.
	User(UserId),
	/// Landed on the treasury from outside every wallet we control. NOT credited (#245):
	/// the chain names the wallet, not the person, and a claim needs a holder — reported.
	UnattributedTreasury,
	/// The sweep consolidating our own funds, or an address that is not ours.
	NotOurs,
}

fn recipient_of(transfer: &Transfer, watched: &HashMap<String, UserId>, treasury: Option<&str>, gas_station: Option<&str>) -> Recipient {
	if let Some(&user) = watched.get(&transfer.to) {
		Recipient::User(user)
	} else if treasury == Some(transfer.to.as_str()) && is_external_source(&transfer.from, watched, gas_station, treasury) {
		Recipient::UnattributedTreasury
	} else {
		Recipient::NotOurs
	}
}

/// Is USDT arriving at the treasury NEW money, or our own funds being consolidated?
///
/// This is what keeps the sweep out of the incident log. The sweep moves USDT **from a
/// user's derived deposit address to the treasury**, and that dollar is already on the
/// ledger — `wallet:<net>` counts it from the moment the deposit was credited, whichever of
/// our addresses it physically sits on. Reporting it as an unattributed arrival would ask an
/// operator to seed a dollar that is already behind a user's claim, and a seed of it would
/// break the global `sum(custody) == sum(claims)` invariant in the direction that invents
/// money out of nothing. Sweeps are not an edge case — they are the steady state, so this
/// predicate runs before every treasury report.
///
/// Only a source outside every wallet we control is genuinely new money: not a derived deposit
/// address, not the gas station, and not the treasury paying itself.
fn is_external_source(from: &str, watched: &HashMap<String, UserId>, gas_station: Option<&str>, treasury: Option<&str>) -> bool {
	!watched.contains_key(from) && gas_station != Some(from) && treasury != Some(from)
}

/// One decoded ERC-20 `Transfer` to a watched address.
struct Transfer {
	/// Lowercase `0x…` 20-byte sender — the only thing separating an operator's capital
	/// injection from the sweep consolidating funds we already booked.
	from: String,
	/// Lowercase `0x…` 20-byte recipient address (the matched deposit address).
	to: String,
	/// Transferred value in raw on-chain units, scaled into canonical base units by
	/// [`Usdt::from_onchain`] at the credit edge — 1:1 for BEP20 (18-dp), ×10^12 for
	/// Polygon (6-dp).
	value: u128,
	tx_hash: String,
	log_index: u64,
}

impl Transfer {
	/// The idempotency key for [`record_deposit`]: a single tx can carry several Transfers
	/// (to different users), so the log index disambiguates.
	fn tx_ref(&self) -> String {
		format!("{}:{}", self.tx_hash, self.log_index)
	}
}

/// Decode an `eth_getLogs` Transfer log. `None` if the shape is unexpected or the value
/// exceeds `u128` (an impossible USDT amount we refuse to credit).
fn decode_transfer(log: &Value) -> Option<Transfer> {
	let topics = log.get("topics")?.as_array()?;
	if topics.len() < 3 {
		return None;
	}
	// Defensive: the RPC already filtered on topic0, but verify before crediting.
	if !topics[0].as_str()?.eq_ignore_ascii_case(TRANSFER_TOPIC) {
		return None;
	}
	let from = address_from_topic(topics[1].as_str()?)?;
	let to = address_from_topic(topics[2].as_str()?)?;
	let value = word_to_u128(log.get("data")?.as_str()?)?;
	let tx_hash = log.get("transactionHash")?.as_str()?.to_lowercase();
	let log_index = hex_to_u64(log.get("logIndex")?.as_str()?)?;
	Some(Transfer {
		from,
		to,
		value,
		tx_hash,
		log_index,
	})
}

/// A 32-byte big-endian uint256 word → `u128`. `None` if it exceeds `u128` (the high 16
/// bytes are non-zero) — refused rather than silently truncated.
/// Host (and port) of the RPC URL, for logging without leaking an API key in the path.
fn rpc_host(url: &str) -> &str {
	url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url)
}

/// The oldest block in `(pruned, hi]` that `probe` reports as still served, by bisection —
/// or `None` when no boundary can be trusted, in which case nothing may be skipped.
///
/// Bisection assumes retention is a suffix of the chain, so "served" is monotonic in the
/// block number. Two things break that in practice, and both are checked with the probe
/// rather than assumed: a provider that refuses EVERYTHING (log index off, reindexing) has
/// no boundary at all, and a keyed pool of nodes with different retention answers the same
/// block both ways depending on which backend picks up the call. So `pruned` is re-probed
/// first (a refusal that does not repeat is not a prune), `hi` is probed rather than taken
/// on faith, and the block just below the found boundary is probed once more — it was
/// refused during the bisection, and if it is served now the boundary is noise. Only a
/// boundary that survives all three is returned; the caller persists a skip on nothing less.
///
/// Cost is `log2(hi − pruned) + 3` probes — twenty-odd calls for a scan a million blocks
/// behind — against a cycle that was otherwise failing forever.
async fn first_retained_block<E>(pruned: u64, mut hi: u64, mut probe: impl AsyncFnMut(u64) -> Result<bool, E>) -> Result<Option<u64>, E> {
	if hi <= pruned || probe(pruned).await? || !probe(hi).await? {
		return Ok(None);
	}
	let mut lo = pruned;
	while hi - lo > 1 {
		let mid = lo + (hi - lo) / 2;
		if probe(mid).await? {
			hi = mid;
		} else {
			lo = mid;
		}
	}
	// `lo` was refused once on the way down (or is `pruned`, refused twice by now); a second
	// answer that differs is the pool disagreeing with itself, not a boundary.
	if probe(lo).await? {
		return Ok(None);
	}
	Ok(Some(hi))
}

/// Where the scan resumes after the `clamp`-th confirmed boundary of one scan: `first` plus
/// [`PRUNE_SLACK_BLOCKS`], doubled per clamp (256 → 512 → 1024), never past `to`.
///
/// The bisection confirms a block that was served a moment ago, and the edge keeps moving
/// while the confirmation probe and the chunk request go out — so "resume at `first`" is a
/// request for the one block the provider is about to drop. The slack lands the next chunk
/// well inside the window; doubling covers an edge that moves faster than the first slack
/// assumed (a slower endpoint, a faster chain) without a per-rail knob.
fn resume_after_prune(first: u64, clamp: u32, to: u64) -> u64 {
	let slack = PRUNE_SLACK_BLOCKS.saturating_mul(1u64 << clamp.saturating_sub(1).min(63));
	first.saturating_add(slack).min(to)
}

/// The refusal that only a LATER window can get past — see [`RpcAction::Pruned`].
fn is_pruned(err: &WatcherError) -> bool {
	matches!(err, WatcherError::Rpc(msg) if classify(msg) == RpcAction::Pruned)
}

/// How the caller should react to an RPC error.
#[derive(Debug, Eq, PartialEq)]
enum RpcAction {
	/// Transient (rate limit, timeout, transport, 429/5xx) — the same call may succeed later.
	Backoff,
	/// The endpoint refused the query's block window — only a narrower one can succeed.
	Narrow,
	/// The endpoint no longer holds history at `fromBlock` — only a LATER window can
	/// succeed; narrowing or retrying the same one never will.
	Pruned,
	/// Repeating changes nothing (invalid params, method-not-found, unparseable) — fail now.
	Fail,
}

/// Classify an RPC error message. dRPC's free tier is the endpoint these run against by
/// default, so its codes lead: 15 (rate limit), 30 (timeout), 35 (block window refused);
/// -32701 (history pruned) is allnodes', the keyed nodes prod runs on.
/// The `Narrow` phrasings cover the other providers' wording for the same condition, since
/// the rail's RPC URL is operator-configured and may point anywhere.
///
/// Splitting `Narrow` out of the retryable set is the point: it used to be lumped in with
/// rate limits and retried five times, and since the identical query is refused every time,
/// those five attempts could only ever fail — five alerts for one unwinnable call.
fn classify(msg: &str) -> RpcAction {
	// Codes are matched against a whitespace-stripped copy: the JSON-RPC error object is
	// reproduced verbatim from the wire, and `{"code": 35}` is as valid as `{"code":35}`.
	let compact: String = msg.chars().filter(|c| !c.is_whitespace()).collect();
	// -32701 is allnodes' "History has been pruned for this block"; the word covers the
	// other node implementations' phrasing of the same condition (geth/erigon/nethermind).
	if compact.contains("\"code\":-32701") || msg.to_ascii_lowercase().contains("pruned") {
		return RpcAction::Pruned;
	}
	if compact.contains("\"code\":35")
		|| msg.contains("blocks are not supported")
		|| msg.contains("block range")
		|| msg.contains("query returned more than")
		|| msg.contains("exceed maximum block range")
	{
		return RpcAction::Narrow;
	}
	if compact.contains("\"code\":15") || compact.contains("\"code\":30") || msg.contains("request failed") || msg.contains("http status 429") || msg.contains("http status 5") {
		return RpcAction::Backoff;
	}
	RpcAction::Fail
}

/// Poll interval after `failures` consecutive failed cycles: `poll_secs` doubled per
/// failure, capped at [`CYCLE_MAX_BACKOFF_SECS`].
///
/// Saturating throughout. The naive `poll_secs * 2u64.pow(failures - 1)` overflows `u64`
/// around the 60th consecutive failure — reachable in well under a day once the cap pins
/// the cycle at 5 minutes — and an overflow there wraps to a SMALL delay, turning a
/// backed-off watcher back into a hot loop against the endpoint already refusing it.
fn cycle_backoff_secs(poll_secs: u64, failures: u64) -> u64 {
	let shift = u32::try_from(failures.saturating_sub(1)).unwrap_or(u32::MAX).min(63);
	poll_secs.saturating_mul(1u64 << shift).min(CYCLE_MAX_BACKOFF_SECS)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn decodes_a_real_transfer_log() {
		// A BEP20 USDT Transfer of 5 USDT (5e18) to 0x024d…63e8.
		let log = json!({
			"topics": [
				TRANSFER_TOPIC,
				"0x0000000000000000000000001111111111111111111111111111111111111111",
				"0x000000000000000000000000024da544a76714a3812096e9ef84d40b2c8863e8"
			],
			"data": "0x0000000000000000000000000000000000000000000000004563918244f40000",
			"transactionHash": "0xABCDEF0000000000000000000000000000000000000000000000000000000001",
			"logIndex": "0x2"
		});
		let t = decode_transfer(&log).expect("valid transfer decodes");
		assert_eq!(t.from, "0x1111111111111111111111111111111111111111");
		assert_eq!(t.to, "0x024da544a76714a3812096e9ef84d40b2c8863e8");
		assert_eq!(t.value, 5_000_000_000_000_000_000); // 5 USDT at 18 dp
		assert_eq!(t.tx_hash, "0xabcdef0000000000000000000000000000000000000000000000000000000001");
		assert_eq!(t.log_index, 2);
		assert_eq!(t.tx_ref(), "0xabcdef0000000000000000000000000000000000000000000000000000000001:2");
	}

	/// The invariant-critical case. A sweep arrives at the treasury exactly like an operator
	/// top-up does — same recipient, same token, same event — and only the sender tells them
	/// apart. Credit a sweep and one dollar is booked twice, inventing capital and
	/// breaking `sum(custody) == sum(claims)`.
	#[test]
	fn only_a_source_outside_our_own_wallets_is_new_capital() {
		let deposit_address = "0x024da544a76714a3812096e9ef84d40b2c8863e8";
		let gas_station = "0x7ec1d5446115c39aab004146255ca62f97ca0514";
		let treasury = "0x7303d8dcd615548d8f46b059d1bd31a8b6a3389d";
		let watched: HashMap<String, UserId> = [(deposit_address.to_string(), UserId::from_raw(uuid::Uuid::nil()))].into_iter().collect();

		// The sweep consolidating a user's deposit — already on the ledger, must NOT re-credit.
		assert!(!is_external_source(deposit_address, &watched, Some(gas_station), Some(treasury)));
		// The gas station moving native coin never carries USDT, but it is ours either way.
		assert!(!is_external_source(gas_station, &watched, Some(gas_station), Some(treasury)));
		// The treasury paying itself is not an injection.
		assert!(!is_external_source(treasury, &watched, Some(gas_station), Some(treasury)));
		// An outside wallet — the operator funding the rail. This is the one arrival that
		// is new money, and the one the scan reports rather than credits.
		assert!(is_external_source("0x1347378b1d0eb69d3462e09b3dfa2fe28ebe74ec", &watched, Some(gas_station), Some(treasury)));
	}

	/// The per-log decision (#245): a user's address credits the user; an outside sender
	/// reaching the treasury is NOT a credit but an unattributed inflow for the operator to
	/// seed by hand; the sweep and strangers' addresses are nobody's business. Pinned here
	/// because the credit path and the report path diverge on exactly this value, and a
	/// treasury arrival quietly becoming a credit again would put a dollar on a claim with
	/// no holder.
	#[test]
	fn a_treasury_arrival_is_reported_and_a_users_is_credited() {
		let deposit_address = "0x024da544a76714a3812096e9ef84d40b2c8863e8";
		let gas_station = "0x7ec1d5446115c39aab004146255ca62f97ca0514";
		let treasury = "0x7303d8dcd615548d8f46b059d1bd31a8b6a3389d";
		let outsider = "0x1347378b1d0eb69d3462e09b3dfa2fe28ebe74ec";
		let user = UserId::from_raw(uuid::Uuid::nil());
		let watched: HashMap<String, UserId> = [(deposit_address.to_string(), user)].into_iter().collect();
		let transfer = |from: &str, to: &str| Transfer {
			tx_hash: "0xabc".to_owned(),
			log_index: 0,
			from: from.to_owned(),
			to: to.to_owned(),
			value: 1,
		};

		assert_eq!(
			recipient_of(&transfer(outsider, deposit_address), &watched, Some(treasury), Some(gas_station)),
			Recipient::User(user)
		);
		assert_eq!(
			recipient_of(&transfer(outsider, treasury), &watched, Some(treasury), Some(gas_station)),
			Recipient::UnattributedTreasury
		);
		// The sweep: a user's address paying into the treasury is money already on the ledger.
		assert_eq!(
			recipient_of(&transfer(deposit_address, treasury), &watched, Some(treasury), Some(gas_station)),
			Recipient::NotOurs
		);
		assert_eq!(recipient_of(&transfer(outsider, outsider), &watched, Some(treasury), Some(gas_station)), Recipient::NotOurs);
		// A rail with no treasury view reports nothing and credits nothing for the treasury.
		assert_eq!(recipient_of(&transfer(outsider, treasury), &watched, None, None), Recipient::NotOurs);

		// The report is a counter as well as a line: one per arrival, so a dashboard can
		// alert on an unbooked treasury arrival without grepping logs.
		let before = telemetry::unattributed_treasury_inflows();
		telemetry::note_unattributed_treasury_inflow("bep20", "0xabc", outsider, "1");
		assert_eq!(telemetry::unattributed_treasury_inflows(), before + 1);
	}

	/// A rail whose gas station could not be resolved this cycle must still refuse to count a
	/// sweep as capital — the derived-address set alone carries that guarantee.
	#[test]
	fn an_unresolved_gas_station_does_not_open_the_sweep_hole() {
		let deposit_address = "0x024da544a76714a3812096e9ef84d40b2c8863e8";
		let watched: HashMap<String, UserId> = [(deposit_address.to_string(), UserId::from_raw(uuid::Uuid::nil()))].into_iter().collect();
		assert!(!is_external_source(deposit_address, &watched, None, None));
		assert!(is_external_source("0x1347378b1d0eb69d3462e09b3dfa2fe28ebe74ec", &watched, None, None));
	}

	#[test]
	fn rejects_a_value_exceeding_u128() {
		// High 16 bytes non-zero ⇒ exceeds u128 ⇒ refused, never truncated.
		assert!(word_to_u128("0x0000000000000000000000000000000100000000000000000000000000000000").is_none());
		assert_eq!(word_to_u128("0x0000000000000000000000000000000000000000000000000000000000000001"), Some(1));
	}

	#[test]
	fn pads_address_to_a_32_byte_topic() {
		assert_eq!(
			pad_topic("0x024da544a76714a3812096e9ef84d40b2c8863e8"),
			"0x000000000000000000000000024da544a76714a3812096e9ef84d40b2c8863e8"
		);
	}

	#[test]
	fn a_rate_limit_backs_off_but_a_refused_range_narrows() {
		// The two errors seen in production, verbatim from the dRPC free tier.
		assert_eq!(
			classify(r#"rpc: eth_blockNumber: rpc error: {"code":15,"message":"You reached Public endpoint rate limit, please upgrade to paid plan"}"#),
			RpcAction::Backoff
		);
		assert_eq!(
			classify(r#"rpc: eth_getLogs: rpc error: {"code":35,"message":"ranges over 10000 blocks are not supported on free plan"}"#),
			RpcAction::Narrow
		);
		// A throttling proxy's bare 429/5xx is transient; its parse failure is not the signal.
		assert_eq!(classify("rpc: eth_getLogs: http status 429"), RpcAction::Backoff);
		assert_eq!(classify("rpc: eth_getLogs: http status 503"), RpcAction::Backoff);
		assert_eq!(classify("rpc: eth_getLogs: request failed: connection reset"), RpcAction::Backoff);
		// Other providers' wording for a refused window.
		assert_eq!(classify("rpc: eth_getLogs: rpc error: exceed maximum block range: 5000"), RpcAction::Narrow);
		assert_eq!(classify("rpc: eth_getLogs: rpc error: query returned more than 10000 results"), RpcAction::Narrow);
		// Retrying these changes nothing — fail the cycle instead of burning five attempts.
		assert_eq!(classify(r#"rpc: eth_getLogs: rpc error: {"code":-32601,"message":"method not found"}"#), RpcAction::Fail);
		assert_eq!(classify("rpc: eth_getLogs: bad json: expected value"), RpcAction::Fail);
	}

	#[test]
	fn a_range_error_delivered_with_a_4xx_status_still_narrows() {
		// The regression that left Polygon refused for five days: drpc answers an over-wide
		// eth_getLogs with a JSON-RPC error object AND an HTTP 400. Reporting the bare status
		// dropped the body, so this read as `Fail` and the narrowing never engaged.
		assert_eq!(
			classify(r#"rpc: eth_getLogs: http status 400 {"jsonrpc":"2.0","id":1,"error":{"code":35,"message":"ranges over 10000 blocks are not supported on free plan"}}"#),
			RpcAction::Narrow
		);
		// Same body with the spacing a different serializer produces.
		assert_eq!(
			classify(r#"rpc: eth_getLogs: http status 400 {"error": {"code": 35, "message": "ranges over 10000 blocks are not supported on free plan"}}"#),
			RpcAction::Narrow
		);
		// A rate limit delivered the same way is still transient, not a range problem.
		assert_eq!(
			classify(r#"rpc: eth_blockNumber: http status 429 {"error": {"code": 15, "message": "You reached Public endpoint rate limit"}}"#),
			RpcAction::Backoff
		);
		// A 4xx with nothing actionable in it stays fatal — repeating it changes nothing.
		assert_eq!(classify("rpc: eth_getLogs: http status 400 <html>Bad Request</html>"), RpcAction::Fail);
		assert_eq!(classify("rpc: eth_getLogs: http status 400"), RpcAction::Fail);
	}

	/// The prod incident (#309): allnodes' keyed BSC and Polygon nodes answered every
	/// `eth_getLogs` at the cursor with -32701, the cycle backed off to its 300 s cap, and
	/// the rail never caught up. Neither retrying nor narrowing can get past this — only a
	/// later window can — so it is its own action.
	#[test]
	fn a_pruned_window_is_neither_retried_nor_narrowed() {
		let allnodes = r#"rpc: eth_getLogs: rpc error: {"code":-32701,"message":"History has been pruned for this block. To remove restrictions, order a dedicated full node here: https://www.allnodes.com/pol/host"}"#;
		assert_eq!(classify(allnodes), RpcAction::Pruned);
		assert!(is_pruned(&WatcherError::Rpc(allnodes.trim_start_matches("rpc: ").to_owned())));
		// The same object with the spacing another serializer produces, and behind a 4xx.
		assert_eq!(
			classify(r#"rpc: eth_getLogs: http status 400 {"error": {"code": -32701, "message": "History has been pruned for this block"}}"#),
			RpcAction::Pruned
		);
		// Other node implementations' wording for the same condition.
		assert_eq!(
			classify(r#"rpc: eth_getLogs: rpc error: {"code":-32000,"message":"logs history has been PRUNED"}"#),
			RpcAction::Pruned
		);
		// A range refusal or a throttle is not a prune, whatever else the message says.
		assert_eq!(
			classify(r#"rpc: eth_getLogs: rpc error: {"code":35,"message":"ranges over 10000 blocks are not supported on free plan"}"#),
			RpcAction::Narrow
		);
		assert_eq!(classify("rpc: eth_getLogs: http status 429"), RpcAction::Backoff);
		assert!(!is_pruned(&WatcherError::Db("connection closed".into())));
	}

	/// A probe over a chain whose retention is a clean suffix starting at `retained_from`.
	fn suffix_probe(retained_from: u64, probes: &mut u32) -> impl AsyncFnMut(u64) -> Result<bool, ()> {
		async move |block| {
			*probes += 1;
			Ok(block >= retained_from)
		}
	}

	/// The clamp finds the retention boundary by bisection, without a configured window size
	/// — the provider's window is neither documented nor stable — and does so in
	/// `log2(lag)` probes, not `lag`.
	#[tokio::test]
	async fn bisection_finds_the_oldest_retained_block() {
		let mut probes = 0u32;
		let first = first_retained_block(500_000, 1_500_000, suffix_probe(1_000_000, &mut probes)).await.expect("probe never fails");
		assert_eq!(first, Some(1_000_000));
		assert!(probes <= 24, "a million-block lag must take ~log2 probes, took {probes}");

		// The boundary sits right above the refused block: the smallest possible skip.
		let first = first_retained_block(41, 100, suffix_probe(42, &mut probes)).await.expect("probe never fails");
		assert_eq!(first, Some(42));
		// Everything below the head is gone: the answer is the head itself.
		let first = first_retained_block(0, 100, suffix_probe(100, &mut probes)).await.expect("probe never fails");
		assert_eq!(first, Some(100));
	}

	/// A provider that refuses every block has no retention boundary — its log index is off
	/// or rebuilding — and the scan must NOT skip to the head on the strength of nothing:
	/// before this check the cursor jumped to `safe_head − 1` every cycle with no block
	/// confirmed, where the old behaviour (hold the cursor, back off) would have caught up
	/// by itself once the index returned.
	#[tokio::test]
	async fn a_provider_refusing_everything_yields_no_boundary() {
		let mut probes = 0u32;
		let first = first_retained_block::<()>(500_000, 1_500_000, async |_| {
			probes += 1;
			Ok(false)
		})
		.await
		.expect("probe never fails");
		assert_eq!(first, None);
		assert_eq!(probes, 2, "re-probe `pruned`, probe `hi`, and stop");
	}

	/// A keyed pool (allnodes) fronts nodes of different retention, so the same block can be
	/// refused by one backend and served by the next. Bisection over such answers lands on an
	/// arbitrary block; skipping up to it would write off a window a plain retry would have
	/// read. Every block is refused on first sight and served on second — the boundary the
	/// bisection finds fails its confirmation probe, so nothing is skipped.
	#[tokio::test]
	async fn an_inconsistent_pool_yields_no_boundary() {
		let mut seen = std::collections::HashSet::new();
		let first = first_retained_block::<()>(500, 2_000, async |block| Ok(!seen.insert(block) || block >= 1_000))
			.await
			.expect("probe never fails");
		assert_eq!(first, None);
	}

	/// A refusal that does not repeat was one backend's opinion, not a prune: the re-probe of
	/// the refused block is the first thing asked, and a "served" there ends the search
	/// without touching anything else.
	#[tokio::test]
	async fn a_refusal_that_does_not_repeat_yields_no_boundary() {
		let mut probes = 0u32;
		let first = first_retained_block::<()>(500, 2_000, async |_| {
			probes += 1;
			Ok(true)
		})
		.await
		.expect("probe never fails");
		assert_eq!(first, None);
		assert_eq!(probes, 1);
	}

	/// The v0.16.0 livelock on BSC: the retention edge moves with the chain, so the block the
	/// bisection confirmed is behind the edge again by the time the next chunk asks for it.
	/// Resuming AT the boundary is refused, clamp after clamp, with the lag frozen at the
	/// width of the window; resuming past it with the slack is not.
	#[tokio::test]
	async fn the_resume_point_outruns_a_moving_retention_edge() {
		// ~1 block per probe is what BSC does against a paced probe; modelled five times harsher.
		let drift = 5u64;
		let mut edge = 1_000_000u64;
		let first = first_retained_block::<()>(500_000, 2_000_000, async |block| {
			let served = block >= edge;
			edge += drift;
			Ok(served)
		})
		.await
		.expect("probe never fails")
		.expect("a moving edge is still a confirmed boundary");
		// Without the slack the next chunk starts on a block the edge has already passed.
		assert!(first < edge, "boundary {first} should be behind the edge {edge} by now");
		// With it, the chunk starts inside the window, on the first clamp.
		assert!(resume_after_prune(first, 1, 2_000_000) >= edge);
	}

	#[test]
	fn the_slack_doubles_per_clamp_and_stops_at_the_window_end() {
		assert_eq!(resume_after_prune(100, 1, 10_000), 100 + 256);
		assert_eq!(resume_after_prune(100, 2, 10_000), 100 + 512);
		assert_eq!(resume_after_prune(100, 3, 10_000), 100 + 1024);
		assert_eq!(resume_after_prune(100, 1, 200), 200);
		assert_eq!(resume_after_prune(u64::MAX - 1, 1, u64::MAX), u64::MAX);
	}

	/// An empty window returns `None` without asking anything.
	#[tokio::test]
	async fn bisection_over_an_empty_window_asks_nothing() {
		let mut probes = 0u32;
		let first = first_retained_block::<()>(100, 100, async |_| {
			probes += 1;
			Ok(true)
		})
		.await
		.expect("probe never fails");
		assert_eq!(first, None);
		assert_eq!(probes, 0);
	}

	/// A probe failing for any reason other than "pruned" — a rate limit that outlived its
	/// retries, say — ends the bisection: the cycle fails and backs off exactly as it did
	/// before the clamp existed.
	#[tokio::test]
	async fn a_failing_probe_aborts_the_bisection() {
		let result = first_retained_block(0, 1_000, async |block| if block > 400 { Err("http status 429") } else { Ok(false) }).await;
		assert_eq!(result, Err("http status 429"));
	}

	#[test]
	fn narrowing_halves_down_to_the_floor_and_stops() {
		// What `get_logs` walks when an endpoint keeps refusing the window: 500 → 16, then
		// the guard (`range > MIN_BLOCK_RANGE`) stops it and the error surfaces.
		let mut range = 500u64;
		let mut steps = 0;
		while range > MIN_BLOCK_RANGE {
			range = (range / 2).max(MIN_BLOCK_RANGE);
			steps += 1;
			assert!(steps < 64, "narrowing must terminate");
		}
		assert_eq!(range, MIN_BLOCK_RANGE);
		assert_eq!(steps, 5);
	}

	#[test]
	fn cycle_backoff_ramps_then_saturates() {
		assert_eq!(cycle_backoff_secs(12, 0), 12); // no failures yet
		assert_eq!(cycle_backoff_secs(12, 1), 12);
		assert_eq!(cycle_backoff_secs(12, 2), 24);
		assert_eq!(cycle_backoff_secs(12, 3), 48);
		assert_eq!(cycle_backoff_secs(12, 5), 192);
		assert_eq!(cycle_backoff_secs(12, 6), CYCLE_MAX_BACKOFF_SECS); // 384 → capped
		// The regression: a long outage must stay pinned at the ceiling, never wrap around
		// to a short delay that hammers the endpoint that is already refusing us.
		assert_eq!(cycle_backoff_secs(12, 60), CYCLE_MAX_BACKOFF_SECS);
		assert_eq!(cycle_backoff_secs(12, 64), CYCLE_MAX_BACKOFF_SECS);
		assert_eq!(cycle_backoff_secs(12, u64::MAX), CYCLE_MAX_BACKOFF_SECS);
	}

	#[test]
	fn rpc_host_hides_the_path() {
		assert_eq!(rpc_host("https://rpc.ankr.com/bsc/secret-key"), "rpc.ankr.com");
		assert_eq!(rpc_host("https://bsc-dataseed.binance.org/"), "bsc-dataseed.binance.org");
	}
}
