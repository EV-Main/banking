//! Withdrawal dispatcher — the treasury worker that drains the accept-and-queue
//! backlog. A withdrawal on a rail short of liquidity is accepted and left `queued`
//! (PATTERNS: never refused); until now only the admin `DispatchWithdrawal` RPC ever
//! re-dispatched one. This periodic job is the automatic path: each sweep re-checks
//! every `queued` withdrawal against **both** liquidity gates — the TigerBeetle rail
//! accounting balance AND the custody adapter's real on-chain treasury view — and
//! dispatches the ones both cover, so a rail top-up self-heals the queue within one
//! interval. The outflow policy gates — the read-only kill-switch, the per-owner
//! cross-plane freeze and the tier-1 verification floor — are NOT re-implemented here:
//! they live inside `withdrawal_app::dispatch_withdrawal`, the one funnel this sweep and
//! the admin `DispatchWithdrawal` RPC share, so the async path cannot drift into a weaker
//! rule than the synchronous one. The only thing the sweep keeps for itself is a
//! whole-cycle short-circuit on the kill-switch, through that same gate, so a paused
//! platform costs one read per interval instead of one per queued row. The reaper's 24h
//! auto-cancel of `queued` withdrawals remains the final backstop (the de-facto rail
//! top-up SLA).

use std::{collections::HashMap, sync::Arc, time::Duration};

use domain::{
	balance::LedgerAccountKey,
	money::{Network, Usdt},
	withdrawals::WithdrawalId,
};
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
	application::withdrawals as withdrawal_app,
	config::KycGate,
	infrastructure::outflow::PgOutflowPolicy,
	ports::{Custody, OutflowPolicy, WithdrawalRepository, ledger::Ledger},
};

/// How often the dispatcher re-checks the queued backlog. Well inside the reaper's
/// abandonment window, so any top-up within the SLA wins the race against auto-cancel.
const DISPATCH_INTERVAL: Duration = Duration::from_secs(30);

/// The dispatcher job: sweep on an interval until the process exits.
pub struct Dispatcher {
	pool: PgPool,
	withdrawals: Arc<dyn WithdrawalRepository>,
	ledger: Arc<dyn Ledger>,
	custody: Arc<dyn Custody>,
	notify: Arc<Notify>,
	/// The deployment's verification gate, handed to every `dispatch_withdrawal` this
	/// sweep makes — the same value the admission path used, so a lifted gate cannot
	/// leave an admitted withdrawal parked here forever.
	kyc_gate: KycGate,
}

impl Dispatcher {
	pub fn new(pool: PgPool, withdrawals: Arc<dyn WithdrawalRepository>, ledger: Arc<dyn Ledger>, custody: Arc<dyn Custody>, notify: Arc<Notify>, kyc_gate: KycGate) -> Self {
		Self {
			pool,
			withdrawals,
			ledger,
			custody,
			notify,
			kyc_gate,
		}
	}

	pub async fn run(self, shutdown: CancellationToken) {
		info!("dispatcher: sweeping queued withdrawals every {DISPATCH_INTERVAL:?}");
		loop {
			match self.sweep().await {
				Ok(dispatched) if dispatched > 0 => info!(dispatched, "dispatcher: dispatched queued withdrawals onto their topped-up rails"),
				Ok(_) => {}
				Err(err) => warn!("dispatcher: sweep failed (will retry): {err}"),
			}
			tokio::select! {
				() = shutdown.cancelled() => return,
				() = tokio::time::sleep(DISPATCH_INTERVAL) => {},
			}
		}
	}

	/// One sweep; returns how many withdrawals it dispatched. Public so an integration
	/// test can drive it deterministically. Dispatch is via the same application command
	/// the admin RPC uses (row-locked, idempotent — [`domain::withdrawals::Withdrawal::dispatch`]
	/// no-ops from `Processing`), so a raced operator dispatch is harmless. Per-withdrawal
	/// failures warn-and-continue; only the backlog read itself fails the sweep.
	///
	/// The backlog is FIFO and each dispatched net is deducted from the rail's remaining
	/// liquidity for the rest of the sweep: both gates read balances that only move when
	/// the relay settles, so a batch summing past the rail would otherwise pass each
	/// static check, dispatch in full, and mass-park at the custody backstop. What no
	/// longer fits stays queued for the next top-up.
	pub async fn sweep(&self) -> Result<usize, sqlx::Error> {
		let policy = PgOutflowPolicy::new(self.pool.clone());
		// Whole-sweep short-circuit on the global pause. `dispatch_withdrawal` re-checks it
		// per withdrawal anyway — this is the same gate, called once, so an operator pause
		// (or an unreadable flag: it fails closed) does not cost a refusal and a log line
		// for every row in the backlog every 30 seconds.
		if let Err(err) = withdrawal_app::require_outflows_enabled(&policy).await {
			info!("dispatcher: skipping this sweep — {err}");
			return Ok(0);
		}
		// Read once so a frozen rail's backlog waits silently rather than logging a refusal
		// per row every sweep; `dispatch_withdrawal` re-checks it for the operator path.
		let frozen = match policy.frozen_rails().await {
			Ok(frozen) => frozen,
			Err(err) => {
				warn!("dispatcher: frozen rails unreadable — skipping this sweep: {err}");
				return Ok(0);
			}
		};
		let queued: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM withdrawals WHERE state = 'queued' AND (release_at IS NULL OR release_at <= now()) ORDER BY created_at")
			.fetch_all(&self.pool)
			.await?;
		let mut dispatched = 0usize;
		// Per-rail net already dispatched this sweep — the running deduction both gates
		// subtract before comparing.
		let mut in_flight: HashMap<Network, Usdt> = HashMap::new();
		for id in queued {
			let id = WithdrawalId::from_raw(id);
			let withdrawal = match self.withdrawals.find_by_id(id).await {
				Ok(Some(withdrawal)) => withdrawal,
				// Raced a terminal transition (cancel/reap) — nothing to dispatch.
				Ok(None) => continue,
				Err(err) => {
					warn!(withdrawal_id = %id, "dispatcher: could not load the queued withdrawal: {err}");
					continue;
				}
			};
			let net = withdrawal.net_amount();
			let network = withdrawal.network();
			if frozen.contains(&network) {
				continue;
			}
			let spent = in_flight.get(&network).copied().unwrap_or(Usdt::ZERO);
			// Gate 1 — the TB rail accounting balance, less this sweep's dispatches, must
			// cover the net.
			match self.ledger.balance(&LedgerAccountKey::CryptoWallet(network)).await {
				Ok(balance) if Usdt::from_base_units(balance.posted).checked_sub(spent).is_some_and(|remaining| remaining >= net) => {}
				Ok(_) => continue,
				Err(err) => {
					warn!(withdrawal_id = %id, %network, "dispatcher: rail balance read failed — skipping this cycle: {err}");
					continue;
				}
			}
			// Gate 2 — the real on-chain treasury, under the same deduction, must cover it
			// too (when there is a chain view). Unlike the operator RPC, an `Err` here has
			// no human judgment behind it, so the automatic path stays conservative: skip
			// and retry next interval.
			match self.custody.treasury_liquidity(network).await {
				Ok(None) => {}
				Ok(Some(onchain)) if onchain.checked_sub(spent).is_some_and(|remaining| remaining >= net) => {}
				Ok(Some(_)) => continue,
				Err(err) => {
					warn!(withdrawal_id = %id, %network, "dispatcher: treasury liquidity read failed — skipping this cycle: {err}");
					continue;
				}
			}
			// The owner's freeze and verification standing are gated inside the command, so a
			// refusal here is an ordinary per-withdrawal skip: it stays queued (and
			// cancellable) for the next interval, or until the reaper takes it.
			match withdrawal_app::dispatch_withdrawal(self.withdrawals.as_ref(), self.custody.as_ref(), &policy, self.kyc_gate, &self.notify, id).await {
				Ok(_) => {
					dispatched += 1;
					// A saturating add: an (impossible in practice) overflow keeps the rail gated.
					in_flight.insert(network, spent.checked_add(net).unwrap_or(Usdt::from_base_units(u128::MAX)));
				}
				Err(err) => warn!(withdrawal_id = %id, "dispatcher: could not dispatch: {err}"),
			}
		}
		Ok(dispatched)
	}
}
