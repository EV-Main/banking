//! Outbox relay — the saga dispatcher.
//!
//! A single worker drains undispatched [`outbox`] rows in strict
//! `seq` order and applies each to TigerBeetle (and, for withdrawals, the custody
//! service) **after** the control-plane commit (Write-Last). "Single worker" is an
//! enforced invariant, not a deploy assumption: [`Relay::run`] holds a fixed-key session
//! `pg_advisory_lock` and only drains while held, so a second core instance blocks on
//! the lock and never touches the outbox (see `OUTBOX_LOCK_KEY`); losing the lock's
//! connection re-enters that acquisition (with backoff) rather than exiting, so a DB
//! blip never cascades into a whole-process teardown. The lock's liveness is re-probed
//! on its own connection before **every** dispatched row (a session lock lives exactly
//! as long as its backend), so a backend reaped mid-catch-up stops the drain within one
//! row instead of leaving a lockless relay racing the standby that inherited the lock —
//! the probe narrows the lockless window to a single dispatch, it cannot close it (a
//! reap *between* probe and dispatch still overlaps one row; full closure needs a
//! fencing token, a follow-up).
//! Delivery is
//! at-least-once, so every op is idempotent: ledger transfer ids are deterministic
//! (a posted transfer uses the event id; a reservation's pending uses an
//! aggregate-derived id so its completion can reference it), the gateway treats
//! `Exists`/`AlreadyPosted`/`AlreadyVoided` as success, and the custody broadcast is
//! idempotent by withdrawal id.
//!
//! Strict order means a reservation's pending is always applied before its
//! settle/cancel, so a two-phase completion never races its own pending. Transient
//! failures (`Unavailable`/`Retryable`, or a custody outage) stop the cycle and retry
//! from the same `seq`; a genuine inconsistency, `InsufficientFunds`, or a custody
//! rejection is **parked** — moved to a distinct `parked_at` terminal state (NOT
//! `dispatched_at`) so the row stays queryable yet is excluded from the drain, with a
//! loud (Sentry-shipped) `error!` + `last_error`. One bad event can't wedge the queue; a
//! park *after* an earlier leg posted is flagged half-applied (`compensated_at`). The
//! [`super::reconciliation`] job and the [`super::reaper`] then own recovery: nothing is
//! silently dropped.

use std::{sync::Arc, time::Duration};

use domain::{
	architecture::DomainEvent,
	balance::{LedgerAccountKey, LedgerEvent, TransferCode},
	book::{BookEvent, Locked, Side},
	fees::FeeEvent,
	issuance::{IssuanceEvent, IssuanceSource},
	money::Usdt,
	payments::PaymentEvent,
	redemptions::RedemptionEvent,
	scopes::{CompartmentEvent, MoveRefusal, MoveState},
	subscriptions::SubscriptionEvent,
	users::UserId,
	withdrawals::{WithdrawalEvent, WithdrawalSource},
};
use sqlx::{PgPool, pool::PoolConnection, postgres::Postgres};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
	infrastructure::{
		book, compartments, fee_accrual, issuance,
		outbox::{self, OutboxRow},
		rails::now_unix_i64,
	},
	ports::{
		custody::{BroadcastRequest, Custody, CustodyError},
		ledger::{CompletionKind, Ledger, LedgerError, LedgerTransfer, PendingCompletion},
	},
};

/// Distinct salts deriving a withdrawal's deterministic TigerBeetle transfer ids from
/// the (stable) withdrawal id. The reservation locks the gross against the user's claim
/// into clearing; settle posts that pending and redistributes the net to the rail's
/// custody and the retained fee to the event's payee — banking's treasury since D17, the
/// fee allocation on an event raised before; fail/cancel void the pending (refund). A
/// completion references the reservation's id as its `pending_id`.
const CLEARING_RESERVE: &[u8] = b"withdraw:clearing";
const CLEARING_SETTLE: &[u8] = b"withdraw:clearing:settle";
const WALLET_REDISTRIBUTE: &[u8] = b"withdraw:wallet";
const FEE_REDISTRIBUTE: &[u8] = b"withdraw:fee";
const CLEARING_VOID_FAIL: &[u8] = b"withdraw:clearing:void:fail";
const CLEARING_VOID_CANCEL: &[u8] = b"withdraw:clearing:void:cancel";

/// Salts for a subscription's two posted legs (one event → two transfers, so they can't
/// share the event id): the cash move `Dr user / Cr service` and the unit mint
/// `Dr user-shares / Cr shares-outstanding`. Cash-leg first, so an insufficient claim
/// parks before any units are minted.
const SUBSCRIBE_CASH: &[u8] = b"subscribe:cash";
const SUBSCRIBE_MINT: &[u8] = b"subscribe:mint";

/// Salt for an in-kind issuance's single posted leg, the mint `Dr <holder shares> / Cr
/// shares-outstanding`. Derived from the issuance id (not the event id) so an operator
/// can recompute the transfer from the row alone when reconciling supply against the
/// issuance table.
const ISSUE_MINT: &[u8] = b"issue:mint";
/// The same, for a retirement: the burn `Dr shares-outstanding / Cr <holder shares>`.
/// Its own salt for the same reason — the transfer id alone must say which way the row
/// moved the supply.
const ISSUE_RETIRE: &[u8] = b"issue:retire";

/// Salts for a redemption's saga: the reservation locks the units as a pending burn;
/// settle posts the burn and pays the cash out of the fund's claim; fail/cancel void the
/// burn (returning the units). A completion references the reservation's id as `pending_id`.
const BURN_RESERVE: &[u8] = b"redeem:burn";
const BURN_SETTLE: &[u8] = b"redeem:burn:settle";
const REDEEM_PAYOUT: &[u8] = b"redeem:payout";
const BURN_VOID_FAIL: &[u8] = b"redeem:burn:void:fail";
const BURN_VOID_CANCEL: &[u8] = b"redeem:burn:void:cancel";

/// Salts for a payment's two money legs. A payment settles exactly as a withdrawal does —
/// reserve the gross against the source claim into `clearing` on approval, then post that
/// pending and move the gross out of `clearing` into the destination claim on settlement —
/// so the same three-id shape applies. `payment:reserve` and `payment:transfer` are the two
/// the design names; `payment:reserve:settle` is the completion's own id, distinct from both
/// because `saga_steps.tb_transfer_id` is unique and a completion is a transfer of its own.
/// `payment:reserve:void` is the other completion — the release a failed execution issues
/// instead of the settle, the counterpart of a withdrawal's fail void.
///
/// FROZEN, like every other salt here: they are derived from the (stable) payment id, so a
/// redelivered event recomputes the same ids and a completion recomputes its `pending_id`.
/// Changing one would make a retry issue a second, different transfer instead of an `Exists`.
const PAYMENT_RESERVE: &[u8] = b"payment:reserve";
const PAYMENT_RESERVE_SETTLE: &[u8] = b"payment:reserve:settle";
const PAYMENT_RESERVE_VOID: &[u8] = b"payment:reserve:void";
const PAYMENT_TRANSFER: &[u8] = b"payment:transfer";

/// Salts for a fee settlement's two posted legs — burning the accumulated fee units and
/// paying their value out of the fund's claim into the fee allocation's. The *charge* itself needs
/// no salt: it is a single leg, so it uses the event id directly, like a deposit.
const FEE_SETTLE_BURN: &[u8] = b"fee:settle:burn";
const FEE_SETTLE_PAYOUT: &[u8] = b"fee:settle:payout";

/// Salts for the book. An order has exactly one lock and at most one release, both
/// derived from the ORDER id (an operator reconciling the escrow recomputes them from the
/// row alone); a trade's three legs are derived from the TRADE id and posted as one
/// linked chain, so the units, the cash and the fee land together or not at all.
const BOOK_LOCK: &[u8] = b"book:lock";
const BOOK_RELEASE: &[u8] = b"book:release";
const BOOK_FILL_UNITS: &[u8] = b"book:fill:units";
const BOOK_FILL_CASH: &[u8] = b"book:fill:cash";
const BOOK_FILL_FEE: &[u8] = b"book:fill:fee";

/// Salt for a compartment move's one posted leg, derived from the MOVE id rather than the
/// event id so an operator reconciling a `compartment_moves` row recomputes the transfer id
/// from the row alone. FROZEN like every salt here.
const COMPARTMENT_MOVE: &[u8] = b"compartment:move";

/// Bound on consecutive `RetryBounded` attempts before a never-resolving retryable (a
/// completion whose pending was itself parked, so it can never be found) is parked
/// rather than wedging the single-worker queue forever.
const MAX_RETRYABLE_ATTEMPTS: i32 = 25;

/// Fixed key for the session-level `pg_advisory_lock` the relay holds while draining.
/// The whole ordering/atomicity argument (strict `seq`, reserve-before-complete, the
/// settle-time liquidity pre-check) is valid *only* with a single drainer; this lock
/// makes that singleton an enforced invariant rather than a deploy assumption — a
/// second instance blocks here and never touches the outbox. The value is an arbitrary
/// stable constant (ASCII "EVBKOBX_"); only stability matters — changing it would let
/// two cohorts drain concurrently across a deploy that mixed old and new keys.
const OUTBOX_LOCK_KEY: i64 = 0x4556_424b_4f42_585f_u64 as i64;

/// Salt deriving the subscription projection marker's id, distinct from any TB transfer salt
/// so the `saga_steps.tb_transfer_id` unique constraint never aliases a real transfer.
const SUBSCRIBE_POSITION: &[u8] = b"subscribe:position";
/// The same, for an issuance's `applied` stamp + cost-basis projection.
const ISSUE_APPLIED: &[u8] = b"issue:applied";
/// The same, for a trade's two-sided cost-basis projection.
const BOOK_TRADE_POSITION: &[u8] = b"book:trade:position";
/// The relay task: drains the outbox to the ledger + custody. Cloneable handles
/// (`pool`, `ledger`, `custody`, `notify`) so command handlers can `notify` it to
/// dispatch promptly.
pub struct Relay {
	pool: PgPool,
	ledger: Arc<dyn Ledger>,
	custody: Arc<dyn Custody>,
	notify: Arc<Notify>,
}

impl Relay {
	pub fn new(pool: PgPool, ledger: Arc<dyn Ledger>, custody: Arc<dyn Custody>, notify: Arc<Notify>) -> Self {
		Self { pool, ledger, custody, notify }
	}

	/// Run until `shutdown` is cancelled. Take the singleton outbox lock on a dedicated
	/// connection (blocking until acquired — a second instance idles here, owning
	/// nothing), then drain the backlog and wait for a nudge or a poll-fallback. The held
	/// connection keeps the session-level lock for as long as it lives; dropping it
	/// (process exit, or a lost backend) releases it so a standby can take over.
	///
	/// A transient DB failure — the lock acquisition erroring, or the lock-holding
	/// connection dropping mid-run — must NOT return: the composition root cancels every
	/// sibling task when any branch completes, so an early return turns a DB blip into a
	/// full money-plane teardown. Instead the outer loop re-acquires the lock with capped
	/// exponential backoff (a standby may legitimately win it first — we then block on it,
	/// which is exactly the singleton hand-off working).
	///
	/// Graceful shutdown is cooperative: cancellation is observed during the lock/backoff
	/// waits, at the wait points between drains, and at every batch boundary inside a
	/// drain — never mid-batch. Each batch runs to completion before exit (it is already
	/// crash-safe to stop between rows), so an in-flight dispatch is never torn down
	/// partway, and a multi-minute catch-up backlog can no longer delay a deploy past the
	/// current batch.
	pub async fn run(self, shutdown: CancellationToken) {
		const MAX_BACKOFF: Duration = Duration::from_secs(30);
		let mut backoff = Duration::from_millis(500);
		'acquire: loop {
			let mut lock = tokio::select! {
				biased;
				() = shutdown.cancelled() => return,
				lock = self.acquire_outbox_lock() => match lock {
					Ok(lock) => lock,
					Err(err) => {
						error!("relay: could not acquire the outbox lock (retrying in {backoff:?}): {err}");
						tokio::select! {
							() = shutdown.cancelled() => return,
							() = tokio::time::sleep(backoff) => {},
						}
						backoff = (backoff * 2).min(MAX_BACKOFF);
						continue 'acquire;
					}
				},
			};
			backoff = Duration::from_millis(500);
			info!("relay: acquired the outbox lock — draining as the singleton worker");
			loop {
				// Drain batch-by-batch so the singleton invariant is re-validated while
				// catching up: `drain_batch` probes the lock connection before every row,
				// and cancellation is observed here between batches. A long catch-up drain
				// otherwise never re-checks the lock — a reaped idle backend releases it, a
				// standby wins it, and two lockless drainers race the same `Dispatched`
				// rows (the on-chain double-send).
				let throttle = loop {
					match self.drain_batch(Some(&mut lock)).await {
						DrainStep::Drained => break false,
						DrainStep::Throttle => break true,
						DrainStep::LostLock => {
							error!("relay: lost the outbox lock mid-drain — re-acquiring");
							// Close, never drop: a drop returns the connection to the pool, and
							// if the probe failed non-fatally (a cancelled query, not a dead
							// backend) that pooled session would keep holding the advisory lock
							// forever, wedging the re-acquisition below against ourselves.
							let _ = lock.close().await;
							continue 'acquire;
						}
						DrainStep::More =>
							if shutdown.is_cancelled() {
								info!("relay: shutdown requested — batch complete, stopping");
								return;
							},
					}
				};
				if shutdown.is_cancelled() {
					info!("relay: shutdown requested — drain iteration complete, stopping");
					return;
				}
				let wait = if throttle { Duration::from_secs(2) } else { Duration::from_millis(500) };
				tokio::select! {
					() = shutdown.cancelled() => {
						info!("relay: shutdown requested — stopping");
						return;
					},
					() = self.notify.notified() => {},
					() = tokio::time::sleep(wait) => {},
				}
				// Touch the lock-holding connection so a dropped backend surfaces here rather
				// than letting a lockless relay silently keep draining. Postgres released the
				// lock with the dead session, so re-acquiring (not exiting) is the recovery.
				if let Err(err) = sqlx::query("SELECT 1").execute(lock.as_mut()).await {
					error!("relay: lost the outbox lock connection — re-acquiring: {err}");
					// Same close-never-drop rule as the mid-drain fence above.
					let _ = lock.close().await;
					continue 'acquire;
				}
			}
		}
	}

	/// Block on a dedicated pooled connection until the fixed-key session advisory lock
	/// is held, then return that connection — its lifetime *is* the lock's. The first
	/// instance returns immediately; any other blocks inside Postgres until the holder's
	/// connection closes.
	pub async fn acquire_outbox_lock(&self) -> Result<PoolConnection<Postgres>, sqlx::Error> {
		let mut conn = self.pool.acquire().await?;
		sqlx::query("SELECT pg_advisory_lock($1)").bind(OUTBOX_LOCK_KEY).execute(conn.as_mut()).await?;
		Ok(conn)
	}

	/// Non-blocking sibling of [`acquire_outbox_lock`](Self::acquire_outbox_lock): take
	/// the lock if free (returning the holding connection), else `None` — used by tests
	/// to observe that a held lock makes a second drainer back off without blocking.
	pub async fn try_acquire_outbox_lock(&self) -> Result<Option<PoolConnection<Postgres>>, sqlx::Error> {
		let mut conn = self.pool.acquire().await?;
		let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)").bind(OUTBOX_LOCK_KEY).fetch_one(conn.as_mut()).await?;
		Ok(acquired.then_some(conn))
	}

	/// Drain the current backlog in `seq` order. Returns `true` if it stopped on a
	/// transient failure (caller should back off before retrying). Unfenced — no lock
	/// probe between rows — so only for tests, which own the sole drainer by
	/// construction; [`run`](Self::run) always drains through the fenced
	/// [`drain_batch`](Self::drain_batch). Public so integration tests can apply
	/// committed events deterministically (one call processes the whole backlog).
	pub async fn drain(&self) -> bool {
		loop {
			match self.drain_batch(None).await {
				DrainStep::Drained => return false,
				DrainStep::Throttle => return true,
				DrainStep::More => {}
				DrainStep::LostLock => unreachable!("no fence connection was passed"),
			}
		}
	}

	/// Apply at most one batch (128 rows) of the backlog in `seq` order, probing `fence`
	/// — the lock-holding connection — before every row. A session advisory lock lives
	/// exactly as long as its backend, so a successful probe proves this relay held the
	/// lock *at probe time*; a failed one means Postgres already freed the lock (a
	/// standby may be draining) and the caller must stop **immediately** — above all
	/// before a custody broadcast, where a concurrent drainer is an on-chain
	/// double-send. The probe narrows the lockless overlap from a whole catch-up drain
	/// to the one dispatch in flight when the backend dies; closing that residual window
	/// needs a fencing token checked at `mark_dispatched`, a follow-up. Public for the
	/// integration tests that exercise the fence.
	pub async fn drain_batch(&self, mut fence: Option<&mut PoolConnection<Postgres>>) -> DrainStep {
		let batch = match outbox::next_batch(&self.pool, 128).await {
			Ok(batch) => batch,
			Err(err) => {
				warn!("relay: reading the outbox failed: {err}");
				return DrainStep::Throttle;
			}
		};
		if batch.is_empty() {
			return DrainStep::Drained;
		}
		let mut advanced = false;
		for row in &batch {
			if let Some(lock) = fence.as_mut()
				&& let Err(err) = sqlx::query("SELECT 1").execute(lock.as_mut()).await
			{
				warn!(seq = row.seq, "relay: outbox lock probe failed before dispatch: {err}");
				return DrainStep::LostLock;
			}
			match self.dispatch(row).await {
				Outcome::Done => {
					if let Err(err) = outbox::mark_dispatched(&self.pool, row.seq).await {
						warn!(seq = row.seq, "relay: failed to mark dispatched (event will re-deliver): {err}");
					}
					advanced = true;
				}
				Outcome::Park { reason, applied_legs } => {
					self.park(row, &reason, applied_legs).await;
					advanced = true;
				}
				Outcome::Retry(reason) => {
					warn!(seq = row.seq, "relay: transient failure (unbounded), retrying from here: {reason}");
					if let Err(err) = outbox::record_failure(&self.pool, row.seq, &reason).await {
						warn!(seq = row.seq, "relay: failed to record the retry attempt: {err}");
					}
					return DrainStep::Throttle;
				}
				Outcome::RetryBounded(reason) => {
					// A retryable that can never resolve (a completion whose pending was
					// itself parked, so it is never found) must not wedge the single-worker
					// queue forever — park it after a bound so later events keep flowing.
					if row.attempts + 1 >= MAX_RETRYABLE_ATTEMPTS {
						self.park(row, &format!("retryable exhausted after {} attempts: {reason}", row.attempts + 1), 0).await;
						advanced = true;
					} else {
						if let Err(err) = outbox::record_failure(&self.pool, row.seq, &reason).await {
							warn!(seq = row.seq, "relay: failed to record the retry attempt: {err}");
						}
						warn!(seq = row.seq, attempts = row.attempts + 1, "relay: retryable, retrying from here: {reason}");
						return DrainStep::Throttle;
					}
				}
			}
		}
		if advanced { DrainStep::More } else { DrainStep::Drained }
	}

	/// Park a non-retryable event into its distinct terminal state (NOT dispatched), so it
	/// stays queryable for reconciliation/the reaper instead of being silently dropped. If
	/// any earlier leg of a multi-leg event already applied (`applied_legs > 0`), the event
	/// is half-applied: stamp a compensation marker and alert at `error!` (which the Sentry
	/// `tracing_layer` ships) so the aggregate's PG-vs-TB divergence is surfaced. Auto-
	/// reversing the applied TB legs stays a follow-up — reconciliation owns recovery today.
	async fn park(&self, row: &OutboxRow, reason: &str, applied_legs: usize) {
		if let Err(err) = outbox::mark_parked(&self.pool, row.seq, reason).await {
			error!(seq = row.seq, "relay: failed to stamp parked_at (event will re-deliver): {err}");
		}
		// A parked escrow lock is an order standing on the book with nothing behind it —
		// the one park that must ALSO change control-plane state, because the matcher
		// would otherwise keep filling an order whose settlement is bound to park too.
		if row.kind == BookEvent::KIND
			&& let Ok(BookEvent::OrderPlaced { order_id, .. }) = serde_json::from_str::<BookEvent>(&row.payload)
			&& let Err(err) = book::mark_rejected(&self.pool, order_id.raw(), &format!("ledger refused the escrow: {reason}")).await
		{
			error!(seq = row.seq, order = %order_id, "relay: failed to mark the order rejected after its lock parked: {err}");
		}
		// A parked move is a refused move: the user is told it failed, the money it promised
		// is spendable again, and an operator's unpark replays an event whose row is
		// terminal — a no-op (see `dispatch`). Nothing of a move applies before its one leg.
		if row.kind == CompartmentEvent::KIND
			&& let Err(err) = compartments::mark_failed(&self.pool, row.aggregate_id, &format!("parked:{}", park_kind(reason))).await
		{
			error!(seq = row.seq, aggregate_id = %row.aggregate_id, "relay: failed to mark the parked compartment move failed: {err}");
		}
		if applied_legs > 0 {
			error!(seq = row.seq, event_id = %row.event_id, aggregate = %row.aggregate, applied_legs, "relay: PARKED HALF-APPLIED event (compensation owed): {reason}");
			if let Err(err) = outbox::mark_compensated(&self.pool, row.seq).await {
				error!(seq = row.seq, "relay: failed to stamp compensated_at: {err}");
			}
		} else {
			error!(seq = row.seq, event_id = %row.event_id, aggregate = %row.aggregate, "relay: parking event (needs intervention): {reason}");
		}
	}

	async fn dispatch(&self, row: &OutboxRow) -> Outcome {
		let ops = match plan(row) {
			Ok(ops) => ops,
			Err(reason) => return Outcome::park(format!("unplannable event: {reason}")),
		};
		let is_move = row.kind == CompartmentEvent::KIND;
		// Only a `pending` move may reach the ledger. A `failed` one is the user's refused
		// intent: an operator unparking its event, or a replay, must not carry it out later.
		// A `posted` one already did.
		if is_move {
			match compartments::move_state(&self.pool, row.aggregate_id).await {
				Ok(Some(MoveState::Pending)) => {}
				Ok(Some(MoveState::Posted | MoveState::Failed)) => return Outcome::Done,
				Ok(None) => return Outcome::park("compartment move row is missing".into()),
				Err(err) => return Outcome::Retry(format!("compartment move state read: {err}")),
			}
			// A replayed event (the reaper's re-enqueue) carries a new event id over the same
			// transfer id: posting it again would answer `Exists`, but its saga step would
			// collide with the first one's on `tb_transfer_id` and wedge the queue. The
			// ledger is the authority on "already moved" — stamp the row and move on.
			let Some(leg) = ops.first() else {
				return Outcome::park("compartment move planned no leg".into());
			};
			match self.ledger.transfer_exists(leg.transfer_id).await {
				Ok(true) =>
					return match compartments::mark_posted(&self.pool, row.aggregate_id).await {
						Ok(()) => Outcome::Done,
						Err(err) => Outcome::Retry(format!("compartment move posted stamp: {err}")),
					},
				Ok(false) => {}
				Err(LedgerError::Unavailable(err)) => return Outcome::Retry(err),
				Err(LedgerError::Retryable(err)) => return Outcome::RetryBounded(err),
				Err(err) => return Outcome::park(format!("compartment move applied check: {err}")),
			}
		}
		// Settle-time liquidity pre-check. The relay is single-worker and sequential, so a
		// Read-First check over *all* legs here guarantees a gated leg cannot fail with
		// InsufficientFunds mid-event (which would half-apply and strand value). A shortfall
		// parks the whole event atomically (nothing applied); a top-up + reconciliation
		// recovers it. Two gated legs, on opposite sides:
		//   - `withdraw_disburse` — the net leaves a rail's custody (`Cr wallet:<net>`), so
		//     guard that *credit* account's posted liquidity.
		//   - `redeem_payout` — cash leaves the fund's claim (`Dr service`), so guard that
		//     *debit* account's available balance (other queued payouts may hold it).
		for op in &ops {
			let LedgerAction::Post(transfer) = &op.action else { continue };
			let (guarded, liquidity) = match op.role {
				"withdraw_disburse" => (&transfer.credit, None),
				//   - `fee_payout` — the same shape as a redemption payout: cash leaves the
				//     fund's claim (`Dr service`), so a fund short of it must park the whole
				//     settlement before any fee units are burned, never after.
				"redeem_payout" | "fee_payout" => (&transfer.debit, Some(())),
				_ => continue,
			};
			// Delivery is at-least-once, and legs apply as separate TB calls — a redelivery
			// may re-plan an event whose guarded leg already posted (a later leg, or the
			// leg's own saga-step insert, failed transiently). The re-read balance then
			// already reflects this leg's outflow, so re-checking it against the full
			// amount double-counts the event's own move and spuriously parks a partially-
			// applied settle. TB is the authority on "already applied" (the saga step may
			// have failed to record): an existing transfer re-submits as `Exists`, so its
			// guard is vacuous — skip it.
			match self.ledger.transfer_exists(op.transfer_id).await {
				Ok(true) => continue,
				Ok(false) => {}
				Err(LedgerError::Unavailable(err)) => return Outcome::Retry(err),
				Err(LedgerError::Retryable(err)) => return Outcome::RetryBounded(err),
				Err(err) => return Outcome::park(format!("settle applied-leg check: {err}")),
			}
			match self.ledger.balance(guarded).await {
				Ok(balance) => {
					let have = if liquidity.is_some() { balance.available() } else { balance.posted };
					if have < transfer.amount {
						// Pre-check parks before any leg applies — nothing half-applied.
						return Outcome::park(format!("{} liquidity insufficient at settle", guarded.logical_key()));
					}
				}
				Err(LedgerError::Unavailable(err)) => return Outcome::Retry(err),
				Err(LedgerError::Retryable(err)) => return Outcome::RetryBounded(err),
				Err(err) => return Outcome::park(format!("settle liquidity check: {err}")),
			}
		}
		// The custody Broadcast is the one op with an external side effect and no TB leg
		// whose flags could refuse it, so it gets its own Read-First: the withdrawal's
		// clearing reservation must have **actually applied** — its deterministic transfer
		// id recorded in `saga_steps` by the (strictly earlier in `seq`) Requested leg.
		// Without this, a parked reserve (the optimistic solvency check reads TB, which
		// lags committed-but-undrained outbox rows, so a double-submit passes it twice)
		// leaves the trailing Dispatched row free to send real money on-chain with
		// nothing locked behind it — the over-withdrawal race.
		for op in &ops {
			if !matches!(op.action, LedgerAction::Broadcast(_)) {
				continue;
			}
			match reserve_applied(&self.pool, row.aggregate_id).await {
				Ok(true) => {}
				Ok(false) => return Outcome::park("withdrawal reserve never applied to the ledger (parked?) — refusing to broadcast".into()),
				Err(err) => return Outcome::Retry(format!("reserve-applied check: {err}")),
			}
			// The withdrawal row must still be `processing`: a `Dispatched` event unparked
			// AFTER the withdrawal was failed/cancelled (its reservation voided) would
			// otherwise send real money with nothing locked behind it — the runbook's
			// unpark-after-fail double-pay hazard, made structurally impossible here.
			match withdrawal_state(&self.pool, row.aggregate_id).await {
				Ok(Some(state)) if state == "processing" => {}
				Ok(Some(state)) => return Outcome::park(format!("withdrawal is {state}, not processing — refusing to broadcast")),
				Ok(None) => return Outcome::park("withdrawal row is missing — refusing to broadcast".into()),
				Err(err) => return Outcome::Retry(format!("broadcast-state check: {err}")),
			}
		}
		// The fail void's Read-First — the cardinal never-void rule, enforced
		// structurally: every custody adapter records its `withdrawal_broadcasts` row
		// before the signed bytes go out, so a row here means the transfer may already
		// have landed on-chain, and voiding the clearing reservation would refund the
		// user while real money left custody too — the double-pay. Park for the operator
		// instead. Because the single worker applies this (strictly later in `seq`)
		// Failed row only after any broadcast leg recorded its row, the guard also
		// closes the send-side TOCTOU the broadcast-state check above leaves open: a
		// fail raced past that state read is serialized behind the broadcast row it
		// must observe, and refuses here.
		for op in &ops {
			if op.transfer_id != tid(row.aggregate_id, CLEARING_VOID_FAIL) {
				continue;
			}
			match broadcast_recorded(&self.pool, row.aggregate_id).await {
				Ok(true) => return Outcome::park("broadcast row exists — refusing to void a possibly-landed withdrawal".into()),
				Ok(false) => {}
				Err(err) => return Outcome::Retry(format!("broadcast-row check: {err}")),
			}
		}
		// Track applied legs so a park *after* an earlier leg posted (`applied > 0`) is flagged
		// half-applied — the genuine residual a balance pre-check can't predict (a bare TB
		// Conflict on a non-first leg). A first-leg park leaves nothing applied.
		let mut applied = 0usize;
		for (leg, op) in ops.iter().enumerate() {
			let result = match &op.action {
				LedgerAction::Post(transfer) => self.ledger.post(transfer).await,
				LedgerAction::PostLinked(transfers) => self.ledger.post_linked(transfers).await,
				LedgerAction::Reserve(transfer) => self.ledger.reserve(transfer).await,
				LedgerAction::Complete(completion) => self.ledger.complete(completion).await,
				LedgerAction::Broadcast(request) => self.custody.broadcast(request).await.map_err(custody_to_ledger),
			};
			match result {
				Ok(()) => {
					// A broadcast is an external side effect, not a TB transfer — no saga step.
					// The step record is load-bearing (the broadcast guard above reads it), so
					// a failed insert retries the whole event: every leg is idempotent
					// (`Exists`/`AlreadyPosted` are success), so the redelivery re-applies
					// cleanly and records the step it owes.
					if !matches!(op.action, LedgerAction::Broadcast(_))
						&& let Err(err) = record_saga_step(&self.pool, row.event_id, leg as i32, op.role, op.transfer_id).await
					{
						return Outcome::Retry(format!("saga step record: {err}"));
					}
					applied += 1;
				}
				Err(LedgerError::Unavailable(err)) => return Outcome::Retry(err),
				Err(LedgerError::Retryable(err)) => return Outcome::RetryBounded(err),
				// A move is one leg, so nothing applied before it: the refusal is the move's
				// terminal outcome, recorded on its row, not an event for an operator.
				Err(LedgerError::InsufficientFunds) if is_move => {
					return match compartments::mark_failed(&self.pool, row.aggregate_id, MoveRefusal::InsufficientFunds.reason()).await {
						Ok(()) => Outcome::Done,
						Err(err) => Outcome::Retry(format!("compartment move failed stamp: {err}")),
					};
				}
				Err(LedgerError::InsufficientFunds) => {
					return Outcome::Park {
						reason: "insufficient funds".into(),
						applied_legs: applied,
					};
				}
				Err(LedgerError::Conflict(err)) => {
					return Outcome::Park {
						reason: format!("ledger conflict: {err}"),
						applied_legs: applied,
					};
				}
			}
		}
		// A subscription's cost-basis projection is written **here**, after both ledger legs
		// post — never on the synchronous open path, where a later parked cash-leg would leave
		// a phantom `fund_positions` row (basis without units or cash). The upsert is idempotent
		// under at-least-once delivery: a per-event `saga_steps` marker gates the relative add,
		// applied in the same transaction so a redelivery never double-counts the basis.
		if row.kind == "subscriptions"
			&& let Err(err) = project_subscription(&self.pool, row).await
		{
			return Outcome::Retry(format!("subscribe cost-basis projection: {err}"));
		}
		// An issuance's `applied` stamp (and, for a user holder, its cost-basis projection)
		// follows the same rule: written here, after the mint posted, never on the open
		// path — a parked mint must leave the row `queued`, not claiming units it never got.
		if row.kind == "issuances"
			&& let Err(err) = project_issuance(&self.pool, row).await
		{
			return Outcome::Retry(format!("issuance applied projection: {err}"));
		}
		// A trade moves cost basis on both sides — the buyer gains what they paid, the
		// seller loses the pro-rata share of theirs — under the same discipline: after the
		// linked batch posted, at most once per event.
		if row.kind == BookEvent::KIND
			&& let Err(err) = project_trade(&self.pool, row).await
		{
			return Outcome::Retry(format!("book trade projection: {err}"));
		}
		if is_move && let Err(err) = compartments::mark_posted(&self.pool, row.aggregate_id).await {
			return Outcome::Retry(format!("compartment move posted stamp: {err}"));
		}
		Outcome::Done
	}
}

/// Apply a trade's cost-basis projection to both parties, idempotently under a per-event
/// `saga_steps` marker, in one transaction. The buyer's basis grows by what they paid —
/// the notional plus the fee when they were the taker — and their units by the size,
/// with the high-water mark blended at the fund's NAV of the moment (the accounting
/// price, not the quote: performance is still measured against NAV, so the mark has to
/// be one). The seller's basis shrinks pro rata, as a redemption's does.
///
/// The seller's reduction clamps at zero rather than refusing (`reduce_cost_basis` on the
/// redemption path rolls back with `Conflict`, which its synchronous caller can retry):
/// here a refusal would be a `Retry` that wedges the single-worker relay on a projection
/// that can never catch up, and every unit a seller can lock came through this same relay
/// in an earlier `seq` — so a short projection is a fee clawback or a drift already
/// reconciled elsewhere, and an understated basis is the recoverable side of that.
async fn project_trade(pool: &PgPool, row: &OutboxRow) -> Result<(), sqlx::Error> {
	const PROJECTION_LEG: i32 = 100;
	let event: BookEvent = serde_json::from_str(&row.payload).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
	let BookEvent::TradeExecuted {
		service,
		buyer,
		seller,
		taker_side,
		size,
		notional,
		fee,
		nav,
		..
	} = event
	else {
		return Ok(());
	};
	let marker_id = tid(row.aggregate_id, BOOK_TRADE_POSITION);
	let mut tx = pool.begin().await?;
	let marked = sqlx::query("INSERT INTO saga_steps (event_id, leg, role, tb_transfer_id) VALUES ($1, $2, 'book_trade_position', $3) ON CONFLICT (event_id, leg) DO NOTHING")
		.bind(row.event_id)
		.bind(PROJECTION_LEG)
		.bind(&marker_id.to_be_bytes()[..])
		.execute(&mut *tx)
		.await?
		.rows_affected();
	if marked == 1 {
		let buyer_paid = match taker_side {
			Side::Buy => notional.checked_add(fee).ok_or_else(|| sqlx::Error::Protocol("trade cost overflows".into()))?,
			Side::Sell => notional,
		};
		// Both positions settle what their old basis accrued before it moves (see
		// [`super::fee_accrual`]); the buyer's row may not exist yet, which is fine.
		fee_accrual::carry_accrual(&mut tx, buyer.raw(), service.as_str(), now_unix_i64())
			.await
			.map_err(|err| sqlx::Error::Protocol(format!("carry fee accrual before buy basis change: {err}")))?;
		sqlx::query(
			"INSERT INTO fund_positions (user_id, service, cost_basis, units, high_water_mark) VALUES ($1, $2, $3, $4, $5) \
			 ON CONFLICT (user_id, service) DO UPDATE SET \
			 cost_basis = (fund_positions.cost_basis::numeric + EXCLUDED.cost_basis::numeric)::text, \
			 units = (fund_positions.units::numeric + EXCLUDED.units::numeric)::text, \
			 high_water_mark = GREATEST(fund_positions.high_water_mark::numeric, EXCLUDED.high_water_mark::numeric)::text, \
			 updated_at = now()",
		)
		.bind(buyer.raw())
		.bind(service.as_str())
		.bind(buyer_paid.base_units().to_string())
		.bind(size.base_units().to_string())
		.bind(nav.base_units().to_string())
		.execute(&mut *tx)
		.await?;
		fee_accrual::carry_accrual(&mut tx, seller.raw(), service.as_str(), now_unix_i64())
			.await
			.map_err(|err| sqlx::Error::Protocol(format!("carry fee accrual before sell basis change: {err}")))?;
		sqlx::query(
			"UPDATE fund_positions SET \
			 cost_basis = CASE WHEN units::numeric > $3::numeric THEN trunc(cost_basis::numeric * (units::numeric - $3::numeric) / units::numeric)::text ELSE '0' END, \
			 units = GREATEST(units::numeric - $3::numeric, 0)::text, \
			 updated_at = now() \
			 WHERE user_id = $1 AND service = $2",
		)
		.bind(seller.raw())
		.bind(service.as_str())
		.bind(size.base_units().to_string())
		.execute(&mut *tx)
		.await?;
	}
	tx.commit().await?;
	Ok(())
}

/// Stamp an issuance `applied` and, when the holder is a user, add its cost basis to
/// their `fund_positions` projection — idempotently, under the same per-event
/// `saga_steps` marker discipline as [`project_subscription`], in one transaction. The
/// projection carries the issuance's `nav` as the high-water mark blend, exactly as a
/// subscription at that NAV would: an investor handed units in kind is measured for
/// performance fees from the price they were handed them at. An allocation holder gets no
/// projection — there is no investor to report P&L or charge fees to. The projection
/// itself is [`issuance::project_holder_position`]: one place says what a holder's
/// position gains from a mint.
async fn project_issuance(pool: &PgPool, row: &OutboxRow) -> Result<(), sqlx::Error> {
	const PROJECTION_LEG: i32 = 100;
	let IssuanceEvent::Issued {
		holder,
		source,
		service,
		units,
		nav,
		cost_basis,
		..
	} = serde_json::from_str(&row.payload).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
	let marker_id = tid(row.aggregate_id, ISSUE_APPLIED);
	let mut tx = pool.begin().await?;
	let marked = sqlx::query("INSERT INTO saga_steps (event_id, leg, role, tb_transfer_id) VALUES ($1, $2, 'issue_applied', $3) ON CONFLICT (event_id, leg) DO NOTHING")
		.bind(row.event_id)
		.bind(PROJECTION_LEG)
		.bind(&marker_id.to_be_bytes()[..])
		.execute(&mut *tx)
		.await?
		.rows_affected();
	if marked == 1 {
		sqlx::query("UPDATE unit_issuances SET state = 'applied', applied_at = now() WHERE id = $1 AND state = 'queued'")
			.bind(row.aggregate_id)
			.execute(&mut *tx)
			.await?;
		issuance::project_holder_position(&mut tx, &holder, source, &service, units, nav, cost_basis).await?;
	}
	tx.commit().await?;
	Ok(())
}

/// Apply a settled subscription's cost-basis projection (`fund_positions.cost_basis +=
/// cash`, `units += minted units`, high-water mark = max) idempotently: a synthetic
/// `saga_steps` leg keys the apply to the event, so the relative add lands at most once even
/// on a redelivery. The upsert runs in the same transaction as that marker insert — both
/// commit or neither does. The projection-tracked `units` is the denominator the redemption
/// settle reduces the cost basis against (see [`super::redemptions::reduce_cost_basis`]),
/// kept off TB so it never lags the async burn.
async fn project_subscription(pool: &PgPool, row: &OutboxRow) -> Result<(), sqlx::Error> {
	const PROJECTION_LEG: i32 = 100;
	let SubscriptionEvent::Subscribed {
		user, service, cash, nav, units, ..
	} = serde_json::from_str(&row.payload).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
	let marker_id = tid(row.aggregate_id, SUBSCRIBE_POSITION);
	let mut tx = pool.begin().await?;
	let marked = sqlx::query("INSERT INTO saga_steps (event_id, leg, role, tb_transfer_id) VALUES ($1, $2, 'subscribe_position', $3) ON CONFLICT (event_id, leg) DO NOTHING")
		.bind(row.event_id)
		.bind(PROJECTION_LEG)
		.bind(&marker_id.to_be_bytes()[..])
		.execute(&mut *tx)
		.await?
		.rows_affected();
	if marked == 1 {
		// The basis is about to move, so whatever the OLD basis accrued has to be settled
		// first — otherwise the next assessment charges this whole elapsed window on money
		// that arrives on the next line. See [`super::fee_accrual`] for why resetting the
		// clock without settling would be the worse bug of the two.
		fee_accrual::carry_accrual(&mut tx, user.raw(), service.as_str(), now_unix_i64())
			.await
			.map_err(|err| sqlx::Error::Protocol(format!("carry fee accrual before subscribe basis change: {err}")))?;
		sqlx::query(
			"INSERT INTO fund_positions (user_id, service, cost_basis, units, high_water_mark) VALUES ($1, $2, $3, $4, $5) \
			 ON CONFLICT (user_id, service) DO UPDATE SET \
			 cost_basis = (fund_positions.cost_basis::numeric + EXCLUDED.cost_basis::numeric)::text, \
			 units = (fund_positions.units::numeric + EXCLUDED.units::numeric)::text, \
			 high_water_mark = GREATEST(fund_positions.high_water_mark::numeric, EXCLUDED.high_water_mark::numeric)::text, \
			 updated_at = now()",
		)
		.bind(user.raw())
		.bind(service.as_str())
		.bind(cash.base_units().to_string())
		.bind(units.base_units().to_string())
		.bind(nav.base_units().to_string())
		.execute(&mut *tx)
		.await?;
	}
	tx.commit().await?;
	Ok(())
}

/// Custody failures fold into the existing ledger outcomes: an outage is transient
/// (retry — nothing was sent), a policy/liquidity refusal is parked.
fn custody_to_ledger(err: CustodyError) -> LedgerError {
	match err {
		CustodyError::Unavailable(detail) => LedgerError::Unavailable(format!("custody unavailable: {detail}")),
		CustodyError::Rejected(detail) => LedgerError::Conflict(format!("custody rejected: {detail}")),
	}
}

/// One [`Relay::drain_batch`] step — how the caller decides to continue, idle, back
/// off, or abandon the drain.
pub enum DrainStep {
	/// The backlog is empty (or nothing can advance) — idle until a nudge/poll.
	Drained,
	/// A full batch applied and more rows may remain — re-check shutdown, keep draining.
	More,
	/// Stopped on a transient failure — back off, then retry from the same `seq`.
	Throttle,
	/// The fence probe failed: the lock backend is gone and the session lock with it — a
	/// standby may already be draining. Stop and re-acquire; **never** keep applying.
	LostLock,
}

enum Outcome {
	Done,
	/// Transient infra outage (TB/custody unreachable) — stop and retry from this `seq`
	/// **unbounded** (it resolves when the dependency recovers).
	Retry(String),
	/// A retryable ledger state (a pending not yet visible) — retry from this `seq`, but
	/// **bounded**: a pending that can never appear (its reserve was itself parked) must
	/// not wedge the single-worker queue forever, so park after `MAX_RETRYABLE_ATTEMPTS`.
	RetryBounded(String),
	/// Terminal for this event — move it to the distinct **parked** state (stays queryable,
	/// excluded from the drain). `applied_legs` is how many legs of this (possibly
	/// multi-leg) event already posted before the park: `> 0` means it half-applied and a
	/// compensation is owed (reconciliation catches it; auto-reversal is a follow-up).
	Park {
		reason: String,
		applied_legs: usize,
	},
}

impl Outcome {
	/// Park before any leg applied — the common case (unplannable, pre-check shortfall).
	fn park(reason: String) -> Self {
		Outcome::Park { reason, applied_legs: 0 }
	}
}

struct PlannedOp {
	role: &'static str,
	transfer_id: u128,
	action: LedgerAction,
}

enum LedgerAction {
	Post(LedgerTransfer),
	/// Several posted legs as one TigerBeetle linked chain — all or nothing. Not
	/// liquidity-gated in the pre-check: the chain's own atomicity is the guarantee a
	/// gate would otherwise provide, and a refused chain parks with nothing applied.
	PostLinked(Vec<LedgerTransfer>),
	Reserve(LedgerTransfer),
	Complete(PendingCompletion),
	Broadcast(BroadcastRequest),
}

/// Map an outbox event to the ledger op(s) (and any custody broadcast) it performs.
/// `reference` (stamped into `user_data_128`) is always the aggregate id, tying every
/// TB transfer back to its allocation/deposit/withdrawal for reconciliation.
fn plan(row: &OutboxRow) -> Result<Vec<PlannedOp>, String> {
	let reference = row.aggregate_id.as_u128();
	let event_tid = row.event_id.as_u128();
	match row.kind.as_str() {
		"balance" => {
			let event: LedgerEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(vec![plan_balance(event, event_tid, reference)])
		}
		"withdrawals" => {
			let event: WithdrawalEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			plan_withdrawal(event, row.aggregate_id, reference)
		}
		"subscriptions" => {
			let event: SubscriptionEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(plan_subscription(event, row.aggregate_id, reference))
		}
		"redemptions" => {
			let event: RedemptionEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(plan_redemption(event, row.aggregate_id, reference))
		}
		"fees" => {
			let event: FeeEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(plan_fee(event, row.aggregate_id, event_tid, reference))
		}
		"issuances" => {
			let event: IssuanceEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			plan_issuance(event, row.aggregate_id, reference).map(|op| vec![op])
		}
		"book" => {
			let event: BookEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(vec![plan_book(event, row.aggregate_id, reference)])
		}
		"payments" => {
			let event: PaymentEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(plan_payment(event, row.aggregate_id, reference))
		}
		"compartments" => {
			let event: CompartmentEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(vec![plan_compartment(event, row.aggregate_id, reference)])
		}
		// A non-money event reached the outbox (shouldn't happen) — a benign no-op.
		_ => Ok(Vec::new()),
	}
}

/// The kind of a park, for a parked move's `failure_reason` (`parked:<kind>`): what the user
/// and the operator need to tell apart, not the relay's full sentence.
fn park_kind(reason: &str) -> &'static str {
	if reason.starts_with("ledger conflict") {
		"conflict"
	} else if reason.starts_with("compartment move applied check") {
		"applied_check"
	} else if reason.starts_with("unplannable") {
		"unplannable"
	} else if reason.starts_with("retryable exhausted") {
		"retry_exhausted"
	} else {
		"other"
	}
}

/// A user's own move between `main` and a scope compartment: one posted claim-to-claim
/// leg, net-zero on the cash invariant. A source that cannot cover it after all — another
/// flow's reservation landed first — is refused by the claim's non-negative flag, and the
/// move is stamped `failed` with nothing moved (see `dispatch`).
fn plan_compartment(event: CompartmentEvent, aggregate_id: Uuid, reference: u128) -> PlannedOp {
	let CompartmentEvent::Moved { user, from, to, amount, .. } = event;
	PlannedOp {
		role: "compartment_move",
		transfer_id: tid(aggregate_id, COMPARTMENT_MOVE),
		action: LedgerAction::Post(LedgerTransfer {
			id: tid(aggregate_id, COMPARTMENT_MOVE),
			debit: from.claim_key(user),
			credit: to.claim_key(user),
			amount: amount.base_units(),
			code: TransferCode::CompartmentMove,
			reference,
		}),
	}
}

/// A deposit credits the party the chain named.
fn plan_balance(event: LedgerEvent, event_tid: u128, reference: u128) -> PlannedOp {
	match event {
		LedgerEvent::Deposited { party, network, amount } => PlannedOp {
			role: "deposit",
			transfer_id: event_tid,
			action: LedgerAction::Post(LedgerTransfer {
				id: event_tid,
				debit: LedgerAccountKey::CryptoWallet(network),
				credit: party.claim_key(),
				amount: amount.base_units(),
				code: TransferCode::Deposit,
				reference,
			}),
		},
	}
}

/// A subscription mints fund units in two posted legs, **cash-leg first**: move the cash
/// into the fund pool (`Dr user / Cr service`), then mint the units (`Dr user-shares /
/// Cr shares-outstanding`). The mint never fails (it only moves between two share
/// accounts whose supply we control), and an insufficient claim parks the cash leg before
/// the mint runs — so the relay can never strand minted units without the cash behind them.
fn plan_subscription(event: SubscriptionEvent, aggregate_id: Uuid, reference: u128) -> Vec<PlannedOp> {
	let SubscriptionEvent::Subscribed { user, service, cash, units, .. } = event;
	vec![
		PlannedOp {
			role: "subscribe_cash",
			transfer_id: tid(aggregate_id, SUBSCRIBE_CASH),
			action: LedgerAction::Post(LedgerTransfer {
				id: tid(aggregate_id, SUBSCRIBE_CASH),
				debit: LedgerAccountKey::UserClaim(user),
				credit: LedgerAccountKey::ServiceClaim(service.clone()),
				amount: cash.base_units(),
				code: TransferCode::Subscribe,
				reference,
			}),
		},
		PlannedOp {
			role: "subscribe_mint",
			transfer_id: tid(aggregate_id, SUBSCRIBE_MINT),
			action: LedgerAction::Post(LedgerTransfer {
				id: tid(aggregate_id, SUBSCRIBE_MINT),
				debit: LedgerAccountKey::UserShares(service.clone(), user),
				credit: LedgerAccountKey::SharesOutstanding(service),
				amount: units.base_units(),
				code: TransferCode::ShareMint,
				reference,
			}),
		},
	]
}

/// An in-kind issuance is one posted transfer on the Share ledger, by `source`:
/// - **Mint** — a subscription's mint leg with no cash leg in front of it, `Dr <holder
///   shares> / Cr shares-outstanding`, under its own [`TransferCode::UnitIssue`] so
///   supply growth the fund's cash never paid for is distinguishable from a
///   subscription's on the Share ledger alone. It cannot fail for funds (both accounts
///   are supply we control), so the only park is a genuine conflict.
/// - **Retire** — the mint reversed, `Dr shares-outstanding / Cr <holder shares>` under
///   [`TransferCode::UnitRetire`]: supply shrinks by `units`, no cash moves. The
///   holder's account is debit-normal with the non-negative flag, so retiring more than
///   it holds parks — the backstop under the use case's Read-First on `available()`.
///
/// A holder with no account in the product (an allocation other than `fee`) is
/// unplannable rather than a
/// transfer TigerBeetle would refuse anyway: the aggregate cannot build one, so the park
/// reason should say "corrupt payload", not "accounts must differ".
fn plan_issuance(event: IssuanceEvent, aggregate_id: Uuid, reference: u128) -> Result<PlannedOp, String> {
	let IssuanceEvent::Issued { holder, source, service, units, .. } = event;
	let (role, salt, debit, credit, code) = match source {
		IssuanceSource::Mint => (
			"issue_mint",
			ISSUE_MINT,
			holder.shares_key(&service).map_err(|err| err.to_string())?,
			LedgerAccountKey::SharesOutstanding(service),
			TransferCode::UnitIssue,
		),
		IssuanceSource::Retire => (
			"issue_retire",
			ISSUE_RETIRE,
			LedgerAccountKey::SharesOutstanding(service.clone()),
			holder.shares_key(&service).map_err(|err| err.to_string())?,
			TransferCode::UnitRetire,
		),
	};
	Ok(PlannedOp {
		role,
		transfer_id: tid(aggregate_id, salt),
		action: LedgerAction::Post(LedgerTransfer {
			id: tid(aggregate_id, salt),
			debit,
			credit,
			amount: units.base_units(),
			code,
			reference,
		}),
	})
}

/// The book in the ledger. Every fact is a posted transfer — no two-phase pendings,
/// because an order's escrow is a real balance in a real account (`BookShares` /
/// `BookCash`) that a fill draws on, and a pending cannot be partially consumed.
/// - **OrderPlaced** → lock the escrow: a sell `Dr BookShares / Cr UserShares` (the
///   holding's non-negative flag refuses an over-lock), a buy `Dr UserClaim / Cr BookCash`
///   (the claim's flag does the same). A refusal parks, and the relay marks the order
///   `rejected` so the matcher stops filling an order with nothing behind it.
/// - **TradeExecuted** → ONE linked chain: `Dr UserShares(buyer) / Cr BookShares(seller)`
///   for the units, `Dr BookCash(buyer) / Cr UserClaim(seller)` for the cash, and — when
///   the taker owes one — the fee out of the taker's side into the event's `payee`
///   claim (the `fee` allocation's),
///   which for a taking seller debits the claim the cash leg just credited (linked legs
///   see each other's effect). Delivery versus payment: units and cash move together or
///   not at all, and neither party can end up with both or neither.
/// - **OrderReleased** → hand the unspent escrow back: `Dr UserShares / Cr BookShares`
///   or `Dr BookCash / Cr UserClaim`.
fn plan_book(event: BookEvent, aggregate_id: Uuid, reference: u128) -> PlannedOp {
	match event {
		BookEvent::OrderPlaced { service, user, locked, .. } => {
			let (debit, credit, amount) = match locked {
				Locked::Units(units) => (
					LedgerAccountKey::BookShares(service.clone(), user),
					LedgerAccountKey::UserShares(service, user),
					units.base_units(),
				),
				Locked::Cash(cash) => (LedgerAccountKey::UserClaim(user), LedgerAccountKey::BookCash(user), cash.base_units()),
			};
			PlannedOp {
				role: "book_lock",
				transfer_id: tid(aggregate_id, BOOK_LOCK),
				action: LedgerAction::Post(LedgerTransfer {
					id: tid(aggregate_id, BOOK_LOCK),
					debit,
					credit,
					amount,
					code: TransferCode::BookLock,
					reference,
				}),
			}
		}
		BookEvent::OrderReleased { service, user, released, .. } => {
			let (debit, credit, amount) = match released {
				Locked::Units(units) => (
					LedgerAccountKey::UserShares(service.clone(), user),
					LedgerAccountKey::BookShares(service, user),
					units.base_units(),
				),
				Locked::Cash(cash) => (LedgerAccountKey::BookCash(user), LedgerAccountKey::UserClaim(user), cash.base_units()),
			};
			PlannedOp {
				role: "book_release",
				transfer_id: tid(aggregate_id, BOOK_RELEASE),
				action: LedgerAction::Post(LedgerTransfer {
					id: tid(aggregate_id, BOOK_RELEASE),
					debit,
					credit,
					amount,
					code: TransferCode::BookRelease,
					reference,
				}),
			}
		}
		BookEvent::TradeExecuted {
			service,
			buyer,
			seller,
			taker_side,
			size,
			notional,
			fee,
			payee,
			..
		} => {
			let mut legs = vec![
				LedgerTransfer {
					id: tid(aggregate_id, BOOK_FILL_UNITS),
					debit: LedgerAccountKey::UserShares(service.clone(), buyer),
					credit: LedgerAccountKey::BookShares(service, seller),
					amount: size.base_units(),
					code: TransferCode::BookFill,
					reference,
				},
				LedgerTransfer {
					id: tid(aggregate_id, BOOK_FILL_CASH),
					debit: LedgerAccountKey::BookCash(buyer),
					credit: LedgerAccountKey::UserClaim(seller),
					amount: notional.base_units(),
					code: TransferCode::BookFill,
					reference,
				},
			];
			// TB refuses a zero-amount transfer, so a fee-free fill has two legs.
			if !fee.is_zero() {
				let debit = match taker_side {
					Side::Buy => LedgerAccountKey::BookCash(buyer),
					Side::Sell => LedgerAccountKey::UserClaim(seller),
				};
				legs.push(LedgerTransfer {
					id: tid(aggregate_id, BOOK_FILL_FEE),
					debit,
					credit: payee.claim_key(),
					amount: fee.base_units(),
					code: TransferCode::BookFee,
					reference,
				});
			}
			PlannedOp {
				role: "book_fill",
				transfer_id: tid(aggregate_id, BOOK_FILL_UNITS),
				action: LedgerAction::PostLinked(legs),
			}
		}
	}
}

/// A redemption's saga in the ledger (accept-and-queue, settle-time priced):
/// - **Requested** → reserve a pending burn `Dr shares-outstanding / Cr user-shares`,
///   which locks the user's units (TigerBeetle's flag rejects an over-redeem here).
/// - **Settled** → **burn first, pay second**: post the pending burn, then pay the cash
///   `Dr service / Cr user`. The settle pre-check guards the payout's fund claim, so a
///   short fund parks before either leg; and burn-first means a parked reserve (a raced
///   over-redeem) fails the burn-post *before* any cash leaves — neither half-applies.
/// - **Failed/Cancelled** → void the pending burn (the units return to the user).
fn plan_redemption(event: RedemptionEvent, aggregate_id: Uuid, reference: u128) -> Vec<PlannedOp> {
	match event {
		RedemptionEvent::Requested { user, service, units, .. } => vec![PlannedOp {
			role: "redeem_reserve",
			transfer_id: tid(aggregate_id, BURN_RESERVE),
			action: LedgerAction::Reserve(LedgerTransfer {
				id: tid(aggregate_id, BURN_RESERVE),
				debit: LedgerAccountKey::SharesOutstanding(service.clone()),
				credit: LedgerAccountKey::UserShares(service, user),
				amount: units.base_units(),
				code: TransferCode::ShareBurn,
				reference,
			}),
		}],
		RedemptionEvent::Settled { user, service, units, cash, .. } => vec![
			PlannedOp {
				role: "redeem_burn",
				transfer_id: tid(aggregate_id, BURN_SETTLE),
				action: LedgerAction::Complete(PendingCompletion {
					id: tid(aggregate_id, BURN_SETTLE),
					pending_id: tid(aggregate_id, BURN_RESERVE),
					kind: CompletionKind::Post,
					debit: LedgerAccountKey::SharesOutstanding(service.clone()),
					credit: LedgerAccountKey::UserShares(service.clone(), user),
					amount: units.base_units(),
					code: TransferCode::ShareBurn,
					reference,
				}),
			},
			PlannedOp {
				role: "redeem_payout",
				transfer_id: tid(aggregate_id, REDEEM_PAYOUT),
				action: LedgerAction::Post(LedgerTransfer {
					id: tid(aggregate_id, REDEEM_PAYOUT),
					debit: LedgerAccountKey::ServiceClaim(service),
					credit: LedgerAccountKey::UserClaim(user),
					amount: cash.base_units(),
					code: TransferCode::Redeem,
					reference,
				}),
			},
		],
		RedemptionEvent::Failed { user, service, units, .. } => vec![void_burn(aggregate_id, user, service, units, reference, BURN_VOID_FAIL)],
		RedemptionEvent::Cancelled { user, service, units, .. } => vec![void_burn(aggregate_id, user, service, units, reference, BURN_VOID_CANCEL)],
	}
}

/// Void the pending burn (return the units) — shared by fail and cancel, which differ only
/// by the completion's deterministic id salt (a redemption reaches at most one).
fn void_burn(aggregate_id: Uuid, user: UserId, service: domain::balance::ServiceId, units: domain::money::Shares, reference: u128, salt: &[u8]) -> PlannedOp {
	PlannedOp {
		role: "redeem_void",
		transfer_id: tid(aggregate_id, salt),
		action: LedgerAction::Complete(PendingCompletion {
			id: tid(aggregate_id, salt),
			pending_id: tid(aggregate_id, BURN_RESERVE),
			kind: CompletionKind::Void,
			debit: LedgerAccountKey::SharesOutstanding(service.clone()),
			credit: LedgerAccountKey::UserShares(service, user),
			amount: units.base_units(),
			code: TransferCode::ShareBurn,
			reference,
		}),
	}
}

/// The fee plane in the ledger.
///
/// - **Charged** → one posted leg, `Dr FeeShares / Cr UserShares`, entirely on the Share
///   ledger. No cash account is touched, so no chain fee is ever paid to collect a fee
///   and an investor's cash claim cannot be moved by one. Because it is a transfer
///   *between two holders*, `SharesOutstanding` does not change and NAV per unit is
///   unaffected — nobody else pays for this investor's charge. TigerBeetle's
///   `credits_must_not_exceed_debits` flag on the holder's account is the backstop
///   under the application's own cap: a clawback larger than the holding is refused by
///   the ledger rather than driving a balance negative.
/// - **SharesSettled** → **burn first, pay second**: post `Dr SharesOutstanding /
///   Cr FeeShares` to destroy the units, then `Dr ServiceClaim / Cr <payee claim>` for
///   their value — the product buying its fee class back from the `fee` allocation at
///   the day's NAV. Same
///   ordering rule as a redemption settle, for the same reason — the payout leg is
///   liquidity-gated in the pre-check above, so a short fund parks the whole event with
///   nothing applied instead of burning units it cannot pay for.
fn plan_fee(event: FeeEvent, aggregate_id: Uuid, event_tid: u128, reference: u128) -> Vec<PlannedOp> {
	match event {
		FeeEvent::Charged { user, service, units, .. } => vec![PlannedOp {
			role: "fee_clawback",
			transfer_id: event_tid,
			action: LedgerAction::Post(LedgerTransfer {
				id: event_tid,
				debit: LedgerAccountKey::FeeShares(service.clone()),
				credit: LedgerAccountKey::UserShares(service, user),
				amount: units.base_units(),
				code: TransferCode::FeeClawback,
				reference,
			}),
		}],
		FeeEvent::SharesSettled { service, units, cash, payee, .. } => vec![
			PlannedOp {
				role: "fee_burn",
				transfer_id: tid(aggregate_id, FEE_SETTLE_BURN),
				action: LedgerAction::Post(LedgerTransfer {
					id: tid(aggregate_id, FEE_SETTLE_BURN),
					debit: LedgerAccountKey::SharesOutstanding(service.clone()),
					credit: LedgerAccountKey::FeeShares(service.clone()),
					amount: units.base_units(),
					code: TransferCode::ShareBurn,
					reference,
				}),
			},
			PlannedOp {
				role: "fee_payout",
				transfer_id: tid(aggregate_id, FEE_SETTLE_PAYOUT),
				action: LedgerAction::Post(LedgerTransfer {
					id: tid(aggregate_id, FEE_SETTLE_PAYOUT),
					debit: LedgerAccountKey::ServiceClaim(service),
					credit: payee.claim_key(),
					amount: cash.base_units(),
					code: TransferCode::FeeSettle,
					reference,
				}),
			},
		],
	}
}

/// A payment's money in the ledger — the SAME two-phase shape a withdrawal uses, because it
/// spends the same claims and must be just as unable to overdraw one:
/// - **Reserved** (raised on approval, L2/L3 only) → a pending `Dr <source> / Cr clearing`,
///   so N concurrently approved payments physically cannot overdraw one claim;
/// - **Settled** → post that pending, then move the gross out of `clearing` into the
///   destination claim. The Vec order matters: the post must land before the second leg
///   debits the now-posted clearing balance.
/// - **Released** (raised on a failed execution, L2/L3 only) → void that pending, returning
///   the amount to the source in full — a withdrawal's fail void, by another name. The
///   reservation and the settlement commit in different transactions, so `approved` is a
///   real state an execution can fail out of, and without this leg the amount would stay
///   locked in `clearing` under a terminal order.
///
/// NO OTHER EVENT REACHES HERE, and the match says so without a `_` arm. `Executed` in
/// particular must never plan an op: for an external payment the money is already moving
/// under `WithdrawalEvent::Requested`, and a second relayed leg would reserve the amount
/// twice against one claim. `PaymentEvent::relays` is the gate that keeps them out of the
/// outbox; this match is the second statement of the same fact, so a new event that slips
/// past the first breaks the build here rather than moving money by accident.
///
/// NO SETTLE-TIME PRE-CHECK, unlike a withdrawal's. Both legs here are claim-to-claim: the
/// destination is credit-normal and can never be "short", so there is no liquidity that could
/// vanish between the two legs and half-apply the settlement.
///
/// ONE TRANSFER CODE for both legs and every tier. The code is forensic and the tier is
/// already derivable from the claim pair a transfer names — a credit to `service:<id>` IS the
/// service tier — so three codes would encode one fact twice and let the two disagree.
fn plan_payment(event: PaymentEvent, aggregate_id: Uuid, reference: u128) -> Vec<PlannedOp> {
	match event {
		PaymentEvent::Reserved { from, amount, .. } => vec![PlannedOp {
			role: "payment_reserve",
			transfer_id: tid(aggregate_id, PAYMENT_RESERVE),
			action: LedgerAction::Reserve(LedgerTransfer {
				id: tid(aggregate_id, PAYMENT_RESERVE),
				debit: from.claim_key(),
				credit: LedgerAccountKey::WithdrawalClearing,
				amount: amount.base_units(),
				code: TransferCode::PaymentTransfer,
				reference,
			}),
		}],
		PaymentEvent::Settled { from, to, amount, .. } => vec![
			PlannedOp {
				role: "payment_settle",
				transfer_id: tid(aggregate_id, PAYMENT_RESERVE_SETTLE),
				action: LedgerAction::Complete(PendingCompletion {
					id: tid(aggregate_id, PAYMENT_RESERVE_SETTLE),
					pending_id: tid(aggregate_id, PAYMENT_RESERVE),
					kind: CompletionKind::Post,
					debit: from.claim_key(),
					credit: LedgerAccountKey::WithdrawalClearing,
					amount: amount.base_units(),
					code: TransferCode::PaymentTransfer,
					reference,
				}),
			},
			PlannedOp {
				role: "payment_transfer",
				transfer_id: tid(aggregate_id, PAYMENT_TRANSFER),
				action: LedgerAction::Post(LedgerTransfer {
					id: tid(aggregate_id, PAYMENT_TRANSFER),
					debit: LedgerAccountKey::WithdrawalClearing,
					credit: to.claim_key(),
					amount: amount.base_units(),
					code: TransferCode::PaymentTransfer,
					reference,
				}),
			},
		],
		PaymentEvent::Released { from, amount, .. } => vec![PlannedOp {
			role: "payment_release",
			transfer_id: tid(aggregate_id, PAYMENT_RESERVE_VOID),
			action: LedgerAction::Complete(PendingCompletion {
				id: tid(aggregate_id, PAYMENT_RESERVE_VOID),
				pending_id: tid(aggregate_id, PAYMENT_RESERVE),
				kind: CompletionKind::Void,
				debit: from.claim_key(),
				credit: LedgerAccountKey::WithdrawalClearing,
				amount: amount.base_units(),
				code: TransferCode::PaymentTransfer,
				reference,
			}),
		}],
		PaymentEvent::Opened { .. }
		| PaymentEvent::ApprovalRecorded { .. }
		| PaymentEvent::Approved { .. }
		| PaymentEvent::Executed { .. }
		| PaymentEvent::ExecutionFailed { .. }
		| PaymentEvent::Rejected { .. }
		| PaymentEvent::Expired { .. }
		| PaymentEvent::Cancelled { .. } => Vec::new(),
	}
}

/// A withdrawal's saga in the ledger:
/// - **Requested** → reserve the gross as a pending `Dr <source> / Cr clearing` (no rail
///   touched, so acceptance never depends on rail liquidity).
/// - **Dispatched** → broadcast the net to custody (idempotent by withdrawal id).
/// - **Settled** → post the clearing pending, then move net→`wallet:<net>` and (when
///   non-zero) the retained fee→the event's `payee` account — banking's treasury since D17,
///   the `fee` allocation's claim on an event raised before. The `Cr wallet:<net>` is where
///   rail liquidity is finally checked by the non-negative flag.
/// - **Failed/Cancelled** → void the clearing pending, refunding the source in full.
///
/// The **source's account** (`user:<uuid>`, or banking's treasury for an approved treasury
/// payment) is the only thing that differs between the two — every other leg, id and guard
/// is shared, which is the point of one saga rather than two. TigerBeetle's non-negative
/// flag on the treasury is therefore the same last-line backstop against paying out more
/// than banking earned that it is against over-spending a user's claim.
fn plan_withdrawal(event: WithdrawalEvent, aggregate_id: Uuid, reference: u128) -> Result<Vec<PlannedOp>, String> {
	Ok(match event {
		WithdrawalEvent::Requested { source, amount, .. } => vec![PlannedOp {
			role: "withdraw_reserve",
			transfer_id: tid(aggregate_id, CLEARING_RESERVE),
			action: LedgerAction::Reserve(LedgerTransfer {
				id: tid(aggregate_id, CLEARING_RESERVE),
				debit: source.claim_key(),
				credit: LedgerAccountKey::WithdrawalClearing,
				amount: amount.base_units(),
				code: TransferCode::Withdraw,
				reference,
			}),
		}],
		WithdrawalEvent::Dispatched { network, address, amount, fee, .. } => {
			let net = amount.checked_sub(fee).ok_or("withdrawal fee exceeds amount")?;
			vec![PlannedOp {
				role: "withdraw_broadcast",
				transfer_id: 0,
				action: LedgerAction::Broadcast(BroadcastRequest {
					withdrawal_id: aggregate_id,
					network,
					address,
					amount: net,
				}),
			}]
		}
		WithdrawalEvent::Settled {
			source,
			network,
			amount,
			fee,
			payee,
			..
		} => {
			let net = amount.checked_sub(fee).ok_or("withdrawal fee exceeds amount")?;
			// Post the clearing reservation, then disburse: net leaves the rail's custody,
			// the fee is retained. The Vec order matters — the post must land before the
			// disbursements debit the now-posted clearing balance.
			let mut ops = vec![
				PlannedOp {
					role: "withdraw_settle",
					transfer_id: tid(aggregate_id, CLEARING_SETTLE),
					action: LedgerAction::Complete(PendingCompletion {
						id: tid(aggregate_id, CLEARING_SETTLE),
						pending_id: tid(aggregate_id, CLEARING_RESERVE),
						kind: CompletionKind::Post,
						debit: source.claim_key(),
						credit: LedgerAccountKey::WithdrawalClearing,
						amount: amount.base_units(),
						code: TransferCode::Withdraw,
						reference,
					}),
				},
				PlannedOp {
					role: "withdraw_disburse",
					transfer_id: tid(aggregate_id, WALLET_REDISTRIBUTE),
					action: LedgerAction::Post(LedgerTransfer {
						id: tid(aggregate_id, WALLET_REDISTRIBUTE),
						debit: LedgerAccountKey::WithdrawalClearing,
						credit: LedgerAccountKey::CryptoWallet(network),
						amount: net.base_units(),
						code: TransferCode::Withdraw,
						reference,
					}),
				},
			];
			if !fee.is_zero() {
				ops.push(PlannedOp {
					role: "withdraw_fee",
					transfer_id: tid(aggregate_id, FEE_REDISTRIBUTE),
					action: LedgerAction::Post(LedgerTransfer {
						id: tid(aggregate_id, FEE_REDISTRIBUTE),
						debit: LedgerAccountKey::WithdrawalClearing,
						credit: payee.claim_key(),
						amount: fee.base_units(),
						code: TransferCode::WithdrawFee,
						reference,
					}),
				});
			}
			ops
		}
		WithdrawalEvent::Failed { source, amount, .. } => vec![void_clearing(aggregate_id, source, amount, reference, CLEARING_VOID_FAIL)],
		WithdrawalEvent::Cancelled { source, amount, .. } => vec![void_clearing(aggregate_id, source, amount, reference, CLEARING_VOID_CANCEL)],
	})
}

/// Void the clearing reservation (full refund to the source) — shared by fail and cancel,
/// which only differ by the completion's deterministic id salt (a withdrawal reaches at
/// most one).
fn void_clearing(aggregate_id: Uuid, source: WithdrawalSource, amount: Usdt, reference: u128, salt: &[u8]) -> PlannedOp {
	PlannedOp {
		role: "withdraw_void",
		transfer_id: tid(aggregate_id, salt),
		action: LedgerAction::Complete(PendingCompletion {
			id: tid(aggregate_id, salt),
			pending_id: tid(aggregate_id, CLEARING_RESERVE),
			kind: CompletionKind::Void,
			debit: source.claim_key(),
			credit: LedgerAccountKey::WithdrawalClearing,
			amount: amount.base_units(),
			code: TransferCode::Withdraw,
			reference,
		}),
	}
}

/// A deterministic TigerBeetle transfer id for one leg/phase of a withdrawal, derived
/// from the (stable) withdrawal id + a per-leg salt, so a retried delivery recomputes
/// the same id and a completion can recompute its reservation's `pending_id`.
fn tid(aggregate_id: Uuid, salt: &[u8]) -> u128 {
	Uuid::new_v5(&aggregate_id, salt).as_u128()
}

/// The transfer id of an issuance's mint leg, derived from the issuance id the way
/// [`plan_issuance`] derives it — exposed so the ownership data migration, which posts its
/// mints itself, stamps the ids the relay would have, and a reconciler recomputing a
/// transfer from a `unit_issuances` row finds the migration's mints like any other.
pub fn issuance_mint_id(issuance_id: Uuid) -> u128 {
	tid(issuance_id, ISSUE_MINT)
}

/// The transfer id of a payment's reservation leg — what the payment execution path asks
/// `saga_steps` and the ledger about before recording an L2/L3 settlement, so the settle can
/// never be recorded over a reserve that has not landed. The same derivation
/// [`plan_payment`] uses, exposed rather than duplicated so the two cannot drift.
pub fn payment_reserve_id(payment_id: Uuid) -> u128 {
	tid(payment_id, PAYMENT_RESERVE)
}

/// Whether a withdrawal's clearing reservation actually applied to the ledger: its
/// deterministic transfer id (`tid(aggregate, CLEARING_RESERVE)`) was recorded in
/// `saga_steps` when the Requested leg posted. Strict `seq` order (single worker)
/// guarantees the Requested row was fully processed — applied or parked — before its
/// Dispatched row is reached, so a missing step means the reserve parked, not that it
/// is still pending.
async fn reserve_applied(pool: &PgPool, aggregate_id: Uuid) -> Result<bool, sqlx::Error> {
	let reserve_tid = tid(aggregate_id, CLEARING_RESERVE);
	sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM saga_steps WHERE tb_transfer_id = $1)")
		.bind(&reserve_tid.to_be_bytes()[..])
		.fetch_one(pool)
		.await
}

/// The withdrawal row's current state — the broadcast Read-First's second guard (beside
/// [`reserve_applied`]): only a `processing` withdrawal may broadcast.
async fn withdrawal_state(pool: &PgPool, aggregate_id: Uuid) -> Result<Option<String>, sqlx::Error> {
	sqlx::query_scalar("SELECT state FROM withdrawals WHERE id = $1").bind(aggregate_id).fetch_optional(pool).await
}

/// Whether custody recorded a `withdrawal_broadcasts` row for this withdrawal — the
/// fail void's Read-First: every adapter persists the signed transaction there before
/// sending it, so a row means the broadcast may have landed and the reservation must
/// never be voided automatically.
async fn broadcast_recorded(pool: &PgPool, aggregate_id: Uuid) -> Result<bool, sqlx::Error> {
	sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM withdrawal_broadcasts WHERE withdrawal_id = $1)")
		.bind(aggregate_id)
		.fetch_one(pool)
		.await
}

async fn record_saga_step(pool: &PgPool, event_id: Uuid, leg: i32, role: &str, transfer_id: u128) -> Result<(), sqlx::Error> {
	sqlx::query("INSERT INTO saga_steps (event_id, leg, role, tb_transfer_id) VALUES ($1, $2, $3, $4) ON CONFLICT (event_id, leg) DO NOTHING")
		.bind(event_id)
		.bind(leg)
		.bind(role)
		.bind(&transfer_id.to_be_bytes()[..])
		.execute(pool)
		.await?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use domain::{
		balance::{Party, ServiceId},
		book::{OrderId, TradeId},
		issuance::{UnitHolder, UnitIssuanceId},
		money::{Nav, Shares, Usdt},
	};

	use super::*;

	// A subscription — an investor's, or the depositor's into `fund` that a seed opens —
	// is the one planned event that puts cash on an allocation's claim AND mints the units
	// that own it, and the two legs must name the same allocation: cash on `service:X`
	// with units of `Y` would be value on `X` nobody holds. There is no transit account
	// to zero out: every leg is a direct double entry, so a chain can never create value —
	// only put it where nobody holds it, which is the ledger-state finding the
	// reconciliation's unheld-value check reports.
	#[test]
	fn a_subscription_credits_the_allocation_whose_units_it_mints() {
		for service in [ServiceId::parse("trading").unwrap(), ServiceId::fund()] {
			let subscriber = UserId::new();
			let id = Uuid::new_v4();
			let ops = plan_subscription(
				SubscriptionEvent::Subscribed {
					subscription_id: domain::subscriptions::SubscriptionId::from_raw(id),
					user: subscriber,
					service: service.clone(),
					cash: Usdt::parse_decimal("100").unwrap(),
					nav: Nav::SEED,
					units: Shares::parse_decimal("100").unwrap(),
				},
				id,
				id.as_u128(),
			);
			let [cash, mint] = ops.as_slice() else { panic!("a subscription is a cash leg then a mint") };
			let (LedgerAction::Post(cash), LedgerAction::Post(mint)) = (&cash.action, &mint.action) else {
				panic!("both legs are posted, so a parked cash leg leaves the money on the subscriber's own claim")
			};
			assert_eq!(cash.debit, LedgerAccountKey::UserClaim(subscriber), "{service}: the cash is the subscriber's");
			assert_eq!(cash.credit, LedgerAccountKey::ServiceClaim(service.clone()), "{service}: it lands on the allocation's claim");
			assert_eq!(mint.debit, LedgerAccountKey::UserShares(service.clone(), subscriber), "{service}: the subscriber holds the units");
			assert_eq!(
				mint.credit,
				LedgerAccountKey::SharesOutstanding(service.clone()),
				"{service}: of the same allocation the cash went to"
			);
			assert_eq!(cash.debit.ledger(), cash.credit.ledger(), "{service}: a leg never crosses ledgers");
			assert_eq!(mint.debit.ledger(), mint.credit.ledger(), "{service}: a leg never crosses ledgers");
		}
	}

	// A mint grows supply and a retirement shrinks it: same event, two legs, told apart by
	// the source — and by the transfer id, so a reconciler can tell them from the row.
	#[test]
	fn an_issuance_mints_or_retires_by_its_source() {
		let user = UserId::new();
		let service = ServiceId::parse("service_arb").unwrap();
		let fee = UnitHolder::Allocation(ServiceId::fee());
		let issuance_id = UnitIssuanceId::new();
		let event = |holder, source| IssuanceEvent::Issued {
			issuance_id,
			service: service.clone(),
			holder,
			source,
			units: Shares::parse_decimal("13000").unwrap(),
			nav: Nav::SEED,
			cost_basis: Usdt::parse_decimal("13000").unwrap(),
		};
		let plan = |holder, source| plan_issuance(event(holder, source), issuance_id.raw(), issuance_id.raw().as_u128());

		// The fee allocation's holding of a product is its fee class: the mint lands on
		// `FeeShares`, the same account a clawback moves units into.
		let mint = plan(fee.clone(), IssuanceSource::Mint).unwrap();
		let LedgerAction::Post(leg) = &mint.action else { panic!("a mint is one posted leg") };
		assert_eq!(
			(leg.debit.clone(), leg.credit.clone()),
			(LedgerAccountKey::FeeShares(service.clone()), LedgerAccountKey::SharesOutstanding(service.clone()))
		);
		assert_eq!(leg.code, TransferCode::UnitIssue);
		assert_eq!(mint.transfer_id, tid(issuance_id.raw(), ISSUE_MINT));

		// A holder with no account in the product (the fund allocation, phase 1) is not a
		// leg the ledger should even see.
		assert!(plan(UnitHolder::Allocation(ServiceId::fund()), IssuanceSource::Mint).is_err());
		assert!(plan(UnitHolder::Allocation(ServiceId::fund()), IssuanceSource::Retire).is_err());

		// A retirement is the mint reversed — same two accounts, swapped sides — for either
		// holder, under its own code and its own transfer id.
		for holder in [UnitHolder::User(user), fee] {
			let retire = plan(holder.clone(), IssuanceSource::Retire).unwrap();
			let LedgerAction::Post(leg) = &retire.action else { panic!("a retirement is one posted leg") };
			assert_eq!(
				(leg.debit.clone(), leg.credit.clone()),
				(LedgerAccountKey::SharesOutstanding(service.clone()), holder.shares_key(&service).unwrap())
			);
			assert_eq!(leg.code, TransferCode::UnitRetire);
			assert_eq!(leg.amount, Shares::parse_decimal("13000").unwrap().base_units(), "the row's units are the magnitude");
			assert_eq!(retire.role, "issue_retire");
			assert_eq!(retire.transfer_id, tid(issuance_id.raw(), ISSUE_RETIRE));
			assert_ne!(retire.transfer_id, mint.transfer_id, "a burn must never alias the mint it reverses");
		}
	}

	// A fill is delivery versus payment or nothing: units, cash and the taker's fee are
	// ONE linked chain, and the fee leg comes off the taker's side — a taking seller pays
	// it out of the claim the cash leg just credited, a taking buyer out of the escrow.
	#[test]
	fn a_trade_is_one_linked_chain_with_the_fee_on_the_takers_side() {
		let (buyer, seller) = (UserId::new(), UserId::new());
		let service = ServiceId::parse("service_arb").unwrap();
		let trade_id = TradeId::new();
		let event = |taker_side, fee| BookEvent::TradeExecuted {
			trade_id,
			service: service.clone(),
			buyer,
			seller,
			buy_order_id: OrderId::new(),
			sell_order_id: OrderId::new(),
			taker_side,
			size: Shares::parse_decimal("2").unwrap(),
			price: domain::book::Price::parse_decimal("1.5").unwrap(),
			notional: Usdt::parse_decimal("3").unwrap(),
			fee: Usdt::parse_decimal(fee).unwrap(),
			nav: Nav::SEED,
			payee: Party::fee_payee(),
		};

		let op = plan_book(event(Side::Sell, "0.003"), trade_id.raw(), trade_id.raw().as_u128());
		let LedgerAction::PostLinked(legs) = &op.action else {
			panic!("a trade must be a linked chain")
		};
		assert_eq!(legs.len(), 3);
		assert_eq!(
			(legs[0].debit.clone(), legs[0].credit.clone()),
			(LedgerAccountKey::UserShares(service.clone(), buyer), LedgerAccountKey::BookShares(service.clone(), seller))
		);
		assert_eq!(
			(legs[1].debit.clone(), legs[1].credit.clone()),
			(LedgerAccountKey::BookCash(buyer), LedgerAccountKey::UserClaim(seller))
		);
		assert_eq!(
			(legs[2].debit.clone(), legs[2].credit.clone()),
			(LedgerAccountKey::UserClaim(seller), LedgerAccountKey::ServiceClaim(ServiceId::fee())),
			"the taker's fee is paid to the fee allocation"
		);
		assert_eq!(op.transfer_id, legs[0].id, "the chain is identified by its first leg");

		let op = plan_book(event(Side::Buy, "0.003"), trade_id.raw(), trade_id.raw().as_u128());
		let LedgerAction::PostLinked(legs) = &op.action else {
			panic!("a trade must be a linked chain")
		};
		assert_eq!(legs[2].debit, LedgerAccountKey::BookCash(buyer), "a taking buyer pays the fee out of its escrow");

		// No fee, no third leg — TigerBeetle refuses a zero-amount transfer.
		let op = plan_book(event(Side::Buy, "0"), trade_id.raw(), trade_id.raw().as_u128());
		let LedgerAction::PostLinked(legs) = &op.action else {
			panic!("a trade must be a linked chain")
		};
		assert_eq!(legs.len(), 2);
	}

	#[test]
	fn a_lock_and_its_release_are_mirror_images_derived_from_the_order() {
		let user = UserId::new();
		let service = ServiceId::parse("service_arb").unwrap();
		let order_id = OrderId::new();
		let lock = plan_book(
			BookEvent::OrderPlaced {
				order_id,
				service: service.clone(),
				user,
				side: Side::Buy,
				locked: Locked::Cash(Usdt::parse_decimal("10").unwrap()),
			},
			order_id.raw(),
			order_id.raw().as_u128(),
		);
		let release = plan_book(
			BookEvent::OrderReleased {
				order_id,
				service: service.clone(),
				user,
				side: Side::Buy,
				released: Locked::Cash(Usdt::parse_decimal("4").unwrap()),
			},
			order_id.raw(),
			order_id.raw().as_u128(),
		);
		let (LedgerAction::Post(lock), LedgerAction::Post(release)) = (&lock.action, &release.action) else {
			panic!("posted legs")
		};
		assert_eq!((lock.debit.clone(), lock.credit.clone()), (LedgerAccountKey::UserClaim(user), LedgerAccountKey::BookCash(user)));
		assert_eq!(
			(release.debit.clone(), release.credit.clone()),
			(LedgerAccountKey::BookCash(user), LedgerAccountKey::UserClaim(user))
		);
		assert_ne!(lock.id, release.id, "one order, two distinct deterministic ids");
	}

	// The payment settle's leg order, for the same reason the redemption's is guarded: the
	// pending must be POSTED before the second leg debits the clearing balance it creates.
	// A `transfer-first` regression would debit `clearing` against a balance the reservation
	// has not yet contributed, and TigerBeetle's non-negative flag would park a settlement
	// whose money was perfectly available.
	#[test]
	fn a_settled_payment_posts_its_reservation_before_it_moves_the_money() {
		let aggregate_id = Uuid::new_v4();
		let user = UserId::new();
		let event = PaymentEvent::Settled {
			payment_id: domain::payments::PaymentId::from_raw(aggregate_id),
			from: domain::balance::Party::User(user),
			to: domain::balance::Party::Service(ServiceId::fee()),
			amount: Usdt::parse_decimal("25").unwrap(),
			at: 0,
		};

		let ops = plan_payment(event, aggregate_id, aggregate_id.as_u128());

		assert_eq!(ops.len(), 2);
		assert_eq!(ops[0].role, "payment_settle");
		assert!(matches!(ops[0].action, LedgerAction::Complete(PendingCompletion { kind: CompletionKind::Post, .. })));
		assert_eq!(ops[1].role, "payment_transfer");
		// The completion must name the reservation the approval raised, or a redelivery would
		// post a pending that does not exist and park a valid settlement forever.
		let LedgerAction::Complete(completion) = &ops[0].action else {
			panic!("the first leg posts the pending")
		};
		assert_eq!(completion.pending_id, tid(aggregate_id, PAYMENT_RESERVE));
		assert_ne!(completion.id, completion.pending_id, "a completion is a transfer of its own");
	}

	// A failed L2/L3 execution VOIDS the reservation the approval raised — the same completion
	// a failed withdrawal issues — and names that reservation as its pending, or the release
	// would void a transfer that does not exist and leave the amount locked for good.
	#[test]
	fn a_released_payment_voids_the_reservation_its_approval_raised() {
		let aggregate_id = Uuid::new_v4();
		let event = PaymentEvent::Released {
			payment_id: domain::payments::PaymentId::from_raw(aggregate_id),
			from: domain::balance::Party::Service(ServiceId::fund()),
			amount: Usdt::parse_decimal("25").unwrap(),
			at: 0,
		};

		let ops = plan_payment(event, aggregate_id, aggregate_id.as_u128());

		assert_eq!(ops.len(), 1, "a release is one leg: nothing is moved, only unlocked");
		assert_eq!(ops[0].role, "payment_release");
		let LedgerAction::Complete(completion) = &ops[0].action else {
			panic!("a release completes the pending reservation")
		};
		assert!(matches!(completion.kind, CompletionKind::Void));
		assert_eq!(completion.pending_id, tid(aggregate_id, PAYMENT_RESERVE));
		assert_eq!(completion.debit, LedgerAccountKey::ServiceClaim(ServiceId::fund()));
		assert_eq!(completion.credit, LedgerAccountKey::WithdrawalClearing);
		// Its own id is distinct from both the reservation's and the settle's, so a settle and
		// a release over one payment can never alias in `saga_steps`.
		assert_ne!(completion.id, completion.pending_id);
		assert_ne!(completion.id, tid(aggregate_id, PAYMENT_RESERVE_SETTLE));
	}

	// `Executed` is audit trail, NOT money. For an external payment the withdrawal saga is
	// already moving the funds under its own `Requested`, so planning anything here would
	// reserve the amount a second time against one claim.
	#[test]
	fn an_executed_payment_plans_no_ledger_op() {
		let aggregate_id = Uuid::new_v4();
		let event = PaymentEvent::Executed {
			payment_id: domain::payments::PaymentId::from_raw(aggregate_id),
			effect: domain::payments::PaymentEffect::Withdrawal(domain::withdrawals::WithdrawalId::new()),
			at: 0,
		};

		assert!(plan_payment(event, aggregate_id, aggregate_id.as_u128()).is_empty());
	}

	// A seed is a deposit to the depositor plus an ordinary subscription (#245): the
	// balance plan credits the person's claim and nothing else. The retired
	// `capital_seeded` event has no plan at all: production's outbox holds none pending,
	// and a stray one is a payload this build cannot read, never a credit to a claim
	// nobody holds.
	#[test]
	fn a_deposit_credits_the_depositor_and_a_retired_seed_payload_does_not_read() {
		let depositor = UserId::new();
		let event_tid = Uuid::new_v4().as_u128();
		let deposit = plan_balance(
			LedgerEvent::Deposited {
				party: Party::User(depositor),
				network: domain::money::Network::Bep20,
				amount: Usdt::parse_decimal("250.5").unwrap(),
			},
			event_tid,
			7,
		);
		let LedgerAction::Post(leg) = &deposit.action else { panic!("a deposit is one posted leg") };
		assert_eq!(deposit.role, "deposit");
		assert_eq!(leg.credit, LedgerAccountKey::UserClaim(depositor), "the depositor's own claim, never the fund's");
		assert_eq!(leg.debit, LedgerAccountKey::CryptoWallet(domain::money::Network::Bep20));
		assert_eq!(leg.code, TransferCode::Deposit);

		let legacy = r#"{"type":"capital_seeded","network":"bep20","amount":"1000"}"#;
		assert!(serde_json::from_str::<LedgerEvent>(legacy).is_err(), "a retired seed payload must not read as anything");
		let legacy_deposit = r#"{"type":"deposited","party":{"kind":"piggybank"},"network":"bep20","amount":"1000"}"#;
		assert!(serde_json::from_str::<LedgerEvent>(legacy_deposit).is_err(), "nor a deposit onto the retired fund claim");
	}

	// A fee settlement pays the product's claim into the FEE ALLOCATION's, burn-first. A
	// payload written before the event carried a `payee` has no plan: the relay would have
	// had to guess a claim, and the only one it could have meant is retired.
	#[test]
	fn a_fee_settlement_pays_the_fee_allocation_and_a_payload_without_a_payee_does_not_read() {
		use domain::architecture::EmitsEvents;
		let service = ServiceId::parse("service_arb").unwrap();
		let settlement_id = domain::fees::FeeSettlementId::new();
		let mut settlement = domain::fees::FeeSettlement::record(settlement_id, service.clone(), Shares::parse_decimal("10").unwrap(), Nav::SEED).unwrap();
		let json = serde_json::to_string(&settlement.drain_events()[0]).unwrap();
		assert!(json.contains(r#""payee":{"kind":"service","id":"fee"}"#), "{json}");

		let ops = plan_fee(serde_json::from_str(&json).unwrap(), settlement_id.raw(), 7, settlement_id.raw().as_u128());
		assert_eq!((ops[0].role, ops[1].role), ("fee_burn", "fee_payout"));
		let LedgerAction::Post(payout) = &ops[1].action else { panic!("the payout is a posted leg") };
		assert_eq!(payout.debit, LedgerAccountKey::ServiceClaim(service.clone()));
		assert_eq!(payout.credit, LedgerAccountKey::ServiceClaim(ServiceId::fee()));

		// The same event as it was written before #245: no `payee` at all.
		assert!(serde_json::from_value::<domain::fees::FeeEvent>(without_payee(&json)).is_err());
	}

	/// A stored payload as it looked before the fee events carried a `payee`.
	fn without_payee(json: &str) -> serde_json::Value {
		let mut legacy = serde_json::from_str::<serde_json::Value>(json).unwrap();
		legacy.as_object_mut().unwrap().remove("payee");
		legacy
	}

	// The same contract for the other two fee legs: the retained withdrawal fee lands in
	// banking's treasury (D17) — or on the fee allocation when the stored event, raised
	// before D17, says so — and the book's taker fee on the fee allocation. A payload without
	// a payee does not read.
	#[test]
	fn the_withdrawal_fee_goes_to_the_treasury_and_the_taker_fee_to_the_fee_allocation() {
		let withdrawal_id = domain::withdrawals::WithdrawalId::new();
		let settled = WithdrawalEvent::Settled {
			withdrawal_id,
			source: WithdrawalSource::User(UserId::new()),
			network: domain::money::Network::Bep20,
			amount: Usdt::parse_decimal("100").unwrap(),
			fee: Usdt::parse_decimal("1").unwrap(),
			tx_ref: domain::money::TxRef::parse("0xabc").unwrap(),
			payee: domain::balance::FeePayee::treasury(),
		};
		let fee_leg = |event: WithdrawalEvent| {
			let ops = plan_withdrawal(event, withdrawal_id.raw(), withdrawal_id.raw().as_u128()).unwrap();
			let op = ops.into_iter().find(|op| op.role == "withdraw_fee").expect("a non-zero fee has its own leg");
			let LedgerAction::Post(transfer) = op.action else { panic!("the fee is a posted leg") };
			transfer
		};
		let json = serde_json::to_string(&settled).unwrap();
		assert_eq!(
			fee_leg(serde_json::from_str(&json).unwrap()).credit,
			LedgerAccountKey::ScopeAccount(domain::scopes::ScopeId::main(), domain::scopes::ScopeAccountLabel::treasury())
		);
		assert_eq!(fee_leg(serde_json::from_str(&json).unwrap()).id, tid(withdrawal_id.raw(), FEE_REDISTRIBUTE));
		assert!(serde_json::from_value::<WithdrawalEvent>(without_payee(&json)).is_err());
		let mut before_d17 = serde_json::from_str::<serde_json::Value>(&json).unwrap();
		before_d17["payee"] = serde_json::json!({"kind": "service", "id": "fee"});
		assert_eq!(
			fee_leg(serde_json::from_value(before_d17).unwrap()).credit,
			LedgerAccountKey::ServiceClaim(ServiceId::fee()),
			"an event raised before D17 pays where it said it would"
		);

		let trade_id = TradeId::new();
		let trade = BookEvent::TradeExecuted {
			trade_id,
			service: ServiceId::parse("service_arb").unwrap(),
			buyer: UserId::new(),
			seller: UserId::new(),
			buy_order_id: OrderId::new(),
			sell_order_id: OrderId::new(),
			taker_side: Side::Buy,
			size: Shares::parse_decimal("2").unwrap(),
			price: domain::book::Price::parse_decimal("1.5").unwrap(),
			notional: Usdt::parse_decimal("3").unwrap(),
			fee: Usdt::parse_decimal("0.003").unwrap(),
			nav: Nav::SEED,
			payee: Party::fee_payee(),
		};
		let fee_leg = |event: BookEvent| {
			let LedgerAction::PostLinked(legs) = plan_book(event, trade_id.raw(), trade_id.raw().as_u128()).action else {
				panic!("a trade is a linked chain")
			};
			legs[2].clone()
		};
		let json = serde_json::to_string(&trade).unwrap();
		assert_eq!(fee_leg(serde_json::from_str(&json).unwrap()).credit, LedgerAccountKey::ServiceClaim(ServiceId::fee()));
		assert!(serde_json::from_value::<BookEvent>(without_payee(&json)).is_err());
	}

	// A move is one claim-to-claim leg in the direction the event names, with an id the
	// move's row alone recomputes.
	#[test]
	fn a_compartment_move_posts_one_leg_between_the_users_own_claims() {
		use domain::{
			architecture::EmitsEvents,
			scopes::{Compartment, CompartmentMove, CompartmentMoveId, ScopeId},
		};
		let user = UserId::new();
		let sa = Compartment::parse("sa").unwrap();
		for (from, to) in [(Compartment::Main, sa.clone()), (sa.clone(), Compartment::Main)] {
			let id = CompartmentMoveId::new();
			let mut moved = CompartmentMove::request(id, user, from.clone(), to.clone(), Usdt::parse_decimal("7").unwrap()).unwrap();
			let event = moved.drain_events().remove(0);
			let json = serde_json::to_string(&event).unwrap();
			let op = plan_compartment(serde_json::from_str(&json).unwrap(), id.raw(), id.raw().as_u128());
			let LedgerAction::Post(leg) = op.action else { panic!("a move is a posted leg") };
			assert_eq!(leg.debit, from.claim_key(user));
			assert_eq!(leg.credit, to.claim_key(user));
			assert_eq!(leg.amount, Usdt::parse_decimal("7").unwrap().base_units());
			assert_eq!(leg.code, TransferCode::CompartmentMove);
			assert_eq!(leg.id, tid(id.raw(), COMPARTMENT_MOVE));
		}
		assert_eq!(sa.claim_key(user), LedgerAccountKey::ScopeClaim(user, ScopeId::parse("sa").unwrap()));
	}

	// Guards the redemption settle leg order documented on the aggregate and PATTERNS:
	// burn-first (post the pending burn), payout-second. A `payout-first` regression
	// would let a raced over-redeem pay cash before a valid burn — a double-spend path.
	#[test]
	fn settled_redemption_burns_before_it_pays() {
		let aggregate_id = Uuid::new_v4();
		let user = UserId::new();
		let service = ServiceId::parse("trading").unwrap();
		let event = RedemptionEvent::Settled {
			redemption_id: domain::redemptions::RedemptionId::new(),
			user,
			service,
			units: Shares::parse_decimal("100").unwrap(),
			nav: Nav::parse_decimal("1.5").unwrap(),
			cash: Usdt::parse_decimal("150").unwrap(),
		};

		let ops = plan_redemption(event, aggregate_id, aggregate_id.as_u128());

		assert_eq!(ops.len(), 2);
		assert_eq!(ops[0].role, "redeem_burn");
		assert!(matches!(ops[0].action, LedgerAction::Complete(PendingCompletion { kind: CompletionKind::Post, .. })));
		assert_eq!(ops[1].role, "redeem_payout");
		assert!(matches!(ops[1].action, LedgerAction::Post(_)));
	}
}
