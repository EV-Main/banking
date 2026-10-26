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
	balance::{LedgerAccountKey, LedgerEvent, Party, TransferCode},
	money::Usdt,
	payments::PaymentEvent,
	scope_charges::ChargeEvent,
	scope_invoices::{InvoiceEvent, InvoiceRefusal},
	scope_ledger::{HoldDestination, HoldEvent, LedgerRefusal, ScopeLedgerEvent},
	scope_operations::ScopePayoutEvent,
	scopes::{CompartmentEvent, MoveRefusal, MoveState},
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
		compartments,
		money_events::{self, MoneyEventError},
		outbox::{self, OutboxRow},
		scope_charges, scope_holds, scope_invoices, scope_ledger, scope_payouts,
	},
	ports::{
		custody::{BroadcastRequest, Custody, CustodyError},
		ledger::{CompletionKind, Ledger, LedgerError, LedgerTransfer, LinkedLeg, PendingCompletion},
	},
};

/// Distinct salts deriving a withdrawal's deterministic TigerBeetle transfer ids from
/// the (stable) withdrawal id. The reservation locks the gross against the user's claim
/// into clearing; settle posts that pending and redistributes the net to the rail's
/// custody and the retained fee to banking's treasury (D17); fail/cancel void the pending (refund). A
/// completion references the reservation's id as its `pending_id`.
const CLEARING_RESERVE: &[u8] = b"withdraw:clearing";
const CLEARING_SETTLE: &[u8] = b"withdraw:clearing:settle";
const WALLET_REDISTRIBUTE: &[u8] = b"withdraw:wallet";
const FEE_REDISTRIBUTE: &[u8] = b"withdraw:fee";
const CLEARING_VOID_FAIL: &[u8] = b"withdraw:clearing:void:fail";
const CLEARING_VOID_CANCEL: &[u8] = b"withdraw:clearing:void:cancel";

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

/// Salt for a compartment move's one posted leg, derived from the MOVE id rather than the
/// event id so an operator reconciling a `compartment_moves` row recomputes the transfer id
/// from the row alone. FROZEN like every salt here.
const COMPARTMENT_MOVE: &[u8] = b"compartment:move";

/// Salt for a scope transfer's one posted leg, derived from the TRANSFER id for the same
/// reason. FROZEN.
const SCOPE_TRANSFER: &[u8] = b"scope:transfer";

/// Salts of a hold's ledger ids, each followed by the version of the event that applies it:
/// the reservation a version creates, the void and the capture post a version applies.
/// Derived from the HOLD id, so the next event can name the reservation it voids from the
/// row's `pending_version` alone. FROZEN.
const HOLD_PENDING: &str = "scope:hold:pending:";
const HOLD_VOID: &str = "scope:hold:void:";
const HOLD_CAPTURE: &str = "scope:hold:capture:";

/// Salt of a payout's legs, followed by the line's place in the chain: derived from the PAYOUT
/// id, so an operator reconciling a `scope_payout_lines` row recomputes its transfer id from
/// the row alone. FROZEN.
const SCOPE_PAYOUT: &str = "scope:payout:";

/// The salt of an invoice payment's one posted leg, over the invoice id, followed by the version
/// of the payment's event: a refused attempt reopens the invoice, and the next attempt must not
/// share its id — nor may a replay of the refused one ever post as the later one. FROZEN.
const INVOICE_PAY: &str = "scope:invoice:pay:";

/// Salt of a charge's one posted leg, derived from the CHARGE id so an operator reconciling a
/// `scope_charges` row recomputes its transfer id from the row alone. FROZEN.
const SCOPE_CHARGE: &[u8] = b"scope:charge";

/// An event whose ledger move is recorded on a control-plane row of its own — `pending` until
/// the relay posts it, then `posted`, or `failed` for good when the ledger refuses it. A
/// compartment move, a scope transfer, a payout, an invoice payment and a charge are the five; all
/// follow one discipline (see `dispatch`), so it is spelled once. A payout's legs are one
/// linked chain, which applies whole or not at all, so for this discipline it is one posting
/// too. An invoice payment is the one whose "failed" is not terminal for the invoice: the
/// refusal reopens it (`scope_invoices::fail_payment`), and the stamps match the attempt the
/// event carries.
#[derive(Clone, Copy)]
enum Posting {
	Move,
	ScopeTransfer,
	ScopePayout,
	InvoicePayment,
	ScopeCharge,
}

impl Posting {
	fn of(kind: &str) -> Option<Self> {
		if kind == CompartmentEvent::KIND {
			Some(Self::Move)
		} else if kind == ScopeLedgerEvent::KIND {
			Some(Self::ScopeTransfer)
		} else if kind == ScopePayoutEvent::KIND {
			Some(Self::ScopePayout)
		} else if kind == InvoiceEvent::KIND {
			Some(Self::InvoicePayment)
		} else if kind == ChargeEvent::KIND {
			Some(Self::ScopeCharge)
		} else {
			None
		}
	}

	const fn name(self) -> &'static str {
		match self {
			Self::Move => "compartment move",
			Self::ScopeTransfer => "scope transfer",
			Self::ScopePayout => "scope payout",
			Self::InvoicePayment => "invoice payment",
			Self::ScopeCharge => "scope charge",
		}
	}

	/// The reason recorded on a row the ledger refused for want of funds — the wire's own.
	const fn insufficient_funds(self) -> &'static str {
		match self {
			Self::Move => MoveRefusal::InsufficientFunds.reason(),
			Self::ScopeTransfer | Self::ScopePayout | Self::ScopeCharge => LedgerRefusal::InsufficientFunds.reason(),
			Self::InvoicePayment => InvoiceRefusal::InsufficientFunds.reason(),
		}
	}

	async fn state(self, pool: &PgPool, row: &OutboxRow) -> Result<Option<MoveState>, sqlx::Error> {
		match self {
			Self::Move => compartments::move_state(pool, row.aggregate_id).await,
			Self::ScopeTransfer => scope_ledger::transfer_state(pool, row.aggregate_id).await,
			Self::ScopePayout => scope_payouts::payout_state(pool, row.aggregate_id).await,
			Self::InvoicePayment => scope_invoices::payment_state(pool, row.aggregate_id, &row.payload).await,
			Self::ScopeCharge => scope_charges::charge_state(pool, row.aggregate_id).await,
		}
	}

	async fn mark_posted(self, pool: &PgPool, row: &OutboxRow) -> Result<(), sqlx::Error> {
		match self {
			Self::Move => compartments::mark_posted(pool, row.aggregate_id).await,
			Self::ScopeTransfer => scope_ledger::mark_posted(pool, row.aggregate_id).await,
			Self::ScopePayout => scope_payouts::mark_posted(pool, row.aggregate_id).await,
			Self::InvoicePayment => scope_invoices::mark_payment_posted(pool, row.aggregate_id, &row.payload).await,
			Self::ScopeCharge => scope_charges::mark_posted(pool, row.aggregate_id).await,
		}
	}

	async fn mark_failed(self, pool: &PgPool, row: &OutboxRow, reason: &str) -> Result<(), sqlx::Error> {
		match self {
			Self::Move => compartments::mark_failed(pool, row.aggregate_id, reason).await,
			Self::ScopeTransfer => scope_ledger::mark_failed(pool, row.aggregate_id, reason).await,
			Self::ScopePayout => scope_payouts::mark_failed(pool, row.aggregate_id, reason).await,
			Self::InvoicePayment => scope_invoices::fail_payment(pool, row.aggregate_id, &row.payload, reason).await,
			Self::ScopeCharge => scope_charges::mark_failed(pool, row.aggregate_id, reason).await,
		}
	}
}

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
				Outcome::MoneyEvent(reason) => {
					// The ledger took the event; only its money event is missing. Bounded, so one
					// row that can never be recorded stops itself and not every deposit and
					// withdrawal behind it (banking#514).
					match outbox::record_money_event_failure(&self.pool, row.seq, &reason).await {
						Ok(tries) if tries >= MAX_RETRYABLE_ATTEMPTS => {
							self.park_unrecorded(row, &format!("money event: {reason}")).await;
							advanced = true;
						}
						Ok(tries) => {
							warn!(seq = row.seq, attempts = tries, "relay: money event not recorded, retrying from here: {reason}");
							return DrainStep::Throttle;
						}
						// Uncounted, so not parked on this try: the database that refused the count
						// is the likelier fault, and the next pass asks again.
						Err(err) => {
							warn!(seq = row.seq, "relay: money event not recorded ({reason}), nor its attempt: {err}");
							return DrainStep::Throttle;
						}
					}
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
		// A hold whose reservation the ledger did not place must stop answering captures in
		// the same breath as its event parks, or a Capture is told CAPTURED for money nobody
		// holds (banking#514).
		let parked = match refused_reserve(row, applied_legs) {
			Some(hold_id) => scope_holds::park_refused_reserve(&self.pool, row.seq, reason, hold_id).await.map_err(|e| e.to_string()),
			None => outbox::mark_parked(&self.pool, row.seq, reason).await.map_err(|e| e.to_string()),
		};
		if let Err(err) = parked {
			error!(seq = row.seq, "relay: failed to stamp parked_at (event will re-deliver): {err}");
		}
		// A parked move, scope transfer or invoice payment is a refused one: the caller is told it
		// failed (a payment's invoice is open again), the money it promised is spendable again,
		// and an operator's unpark replays an event whose attempt is over — a no-op (see
		// `dispatch`). Nothing of it applies before its one leg.
		if let Some(posting) = Posting::of(&row.kind)
			&& !(matches!(posting, Posting::ScopeCharge) && charge_stays_pending_on_park(reason))
			&& let Err(err) = posting.mark_failed(&self.pool, row, &format!("parked:{}", park_kind(reason))).await
		{
			error!(seq = row.seq, aggregate_id = %row.aggregate_id, "relay: failed to mark the parked {} failed: {err}", posting.name());
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

	/// Park an event the ledger fully applied but whose money event could not be recorded. Not
	/// a refusal: a move or transfer it carries stays `pending` (its money did move), nothing is
	/// owed back, and an unpark replays only what is missing — the legs answer `Exists`, the
	/// money event and the posted stamp are written then.
	async fn park_unrecorded(&self, row: &OutboxRow, reason: &str) {
		if let Err(err) = outbox::mark_parked(&self.pool, row.seq, reason).await {
			error!(seq = row.seq, "relay: failed to stamp parked_at (event will re-deliver): {err}");
		}
		error!(seq = row.seq, event_id = %row.event_id, aggregate = %row.aggregate, "relay: parking an applied event whose money event cannot be recorded (needs intervention): {reason}");
	}

	/// What a failed money event means for the drain: an outage of Postgres or the ledger is
	/// waited out like any other; anything else — a row that cannot be read or decoded, an
	/// event the database refuses — is retried a bounded number of times and then parked alone.
	fn money_event_failed(context: &str, err: MoneyEventError) -> Outcome {
		match err {
			MoneyEventError::Unavailable(reason) => Outcome::Retry(format!("{context}money event: {reason}")),
			MoneyEventError::Unrecordable(reason) => Outcome::MoneyEvent(format!("{context}{reason}")),
		}
	}

	async fn dispatch(&self, row: &OutboxRow) -> Outcome {
		// The ledger refused this reservation once and its hold was voided for it: an unpark of
		// it must not place money nobody will ever release.
		if let Some(hold_id) = refused_reserve(row, 0) {
			match scope_holds::reserve_refused(&self.pool, hold_id).await {
				Ok(true) => return Outcome::Done,
				Ok(false) => {}
				Err(err) => return Outcome::Retry(format!("hold refusal read: {err}")),
			}
		}
		let ops = match plan(row) {
			Ok(ops) => ops,
			Err(reason) => return Outcome::park(format!("unplannable event: {reason}")),
		};
		let posting = Posting::of(&row.kind);
		// Only a `pending` move or transfer may reach the ledger. A `failed` one is a refused
		// intent: an operator unparking its event, or a replay, must not carry it out later.
		// A `posted` one already did.
		if let Some(posting) = posting {
			let name = posting.name();
			match posting.state(&self.pool, row).await {
				Ok(Some(MoveState::Pending)) => {}
				Ok(Some(MoveState::Posted | MoveState::Failed)) => return Outcome::Done,
				Ok(None) => return Outcome::park(format!("{name} row is missing")),
				Err(err) => return Outcome::Retry(format!("{name} state read: {err}")),
			}
			// A replayed event (the reaper's re-enqueue) carries a new event id over the same
			// transfer id: posting it again would answer `Exists`, but its saga step would
			// collide with the first one's on `tb_transfer_id` and wedge the queue. The
			// ledger is the authority on "already moved" — stamp the row and move on.
			let Some(leg) = ops.first() else {
				return Outcome::park(format!("{name} planned no leg"));
			};
			match self.ledger.transfer_exists(leg.transfer_id).await {
				Ok(true) => {
					// The leg posted on an earlier delivery that stopped before its money event.
					if let Err(err) = money_events::record_relayed(&self.pool, self.ledger.as_ref(), row).await {
						return Self::money_event_failed(&format!("{name}: "), err);
					}
					return match posting.mark_posted(&self.pool, row).await {
						Ok(()) => Outcome::Done,
						Err(err) => Outcome::Retry(format!("{name} posted stamp: {err}")),
					};
				}
				Ok(false) => {}
				Err(LedgerError::Unavailable(err)) => return Outcome::Retry(err),
				Err(LedgerError::Retryable(err)) => return Outcome::RetryBounded(err),
				Err(err) => return Outcome::park(format!("{name} applied check: {err}")),
			}
			// The leg is not on the ledger and the charge has waited past its window (a long
			// outage, a parked row unparked days later): the user consented to a charge at the
			// time of the call, so it is refused rather than carried out now.
			if matches!(posting, Posting::ScopeCharge) {
				match scope_charges::past_posting_window(&self.pool, row.aggregate_id).await {
					Ok(true) => {
						return match posting.mark_failed(&self.pool, row, "expired").await {
							Ok(()) => Outcome::Done,
							Err(err) => Outcome::Retry(format!("{name} failed stamp: {err}")),
						};
					}
					Ok(false) => {}
					Err(err) => return Outcome::Retry(format!("{name} age read: {err}")),
				}
			}
		}
		// Settle-time liquidity pre-check. The relay is single-worker and sequential, so a
		// Read-First check over *all* legs here guarantees a gated leg cannot fail with
		// InsufficientFunds mid-event (which would half-apply and strand value). A shortfall
		// parks the whole event atomically (nothing applied); a top-up + reconciliation
		// recovers it. The gated leg is `withdraw_disburse`: the net leaves a rail's custody
		// (`Cr wallet:<net>`), so guard that *credit* account's posted liquidity.
		for op in &ops {
			let LedgerAction::Post(transfer) = &op.action else { continue };
			if op.role != "withdraw_disburse" {
				continue;
			}
			let guarded = &transfer.credit;
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
				// Pre-check parks before any leg applies — nothing half-applied.
				Ok(balance) if balance.posted < transfer.amount => return Outcome::park(format!("{} liquidity insufficient at settle", guarded.logical_key())),
				Ok(_) => {}
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
		// A hold's capture or release voids the reservation an earlier event (strictly earlier in
		// `seq`) created. Neither it nor this event's own first leg in the ledger means that
		// reservation was parked, never applied: voiding it would answer "not found" until the
		// bounded retries ran out, holding up the whole queue meanwhile. Park at once instead.
		for op in &ops {
			let LedgerAction::Linked(legs) = &op.action else { continue };
			let Some(LinkedLeg::Complete(void)) = legs.first() else { continue };
			if !matches!(op.role, "hold_capture" | "hold_release") {
				continue;
			}
			let applied = match (self.ledger.transfer_exists(void.id).await, self.ledger.transfer_exists(void.pending_id).await) {
				(Ok(this), Ok(reserved)) => this || reserved,
				(Err(LedgerError::Unavailable(err)), _) | (_, Err(LedgerError::Unavailable(err))) => return Outcome::Retry(err),
				(Err(err), _) | (_, Err(err)) => return Outcome::park(format!("hold reservation check: {err}")),
			};
			if !applied {
				return Outcome::park("hold reservation never applied to the ledger (parked?) — nothing to capture or release".into());
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
				LedgerAction::Linked(legs) => self.ledger.apply_linked(legs).await,
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
				Err(LedgerError::InsufficientFunds) => {
					// A move or a scope transfer is one leg, so nothing applied before it: the
					// refusal is its terminal outcome, recorded on its row, not an event for an
					// operator.
					if let Some(posting) = posting {
						return match posting.mark_failed(&self.pool, row, posting.insufficient_funds()).await {
							Ok(()) => Outcome::Done,
							Err(err) => Outcome::Retry(format!("{} failed stamp: {err}", posting.name())),
						};
					}
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
		// Before the posted stamp: a failure here retries the row, and the stamp's early path
		// above records the event that is still owed.
		if let Err(err) = money_events::record_relayed(&self.pool, self.ledger.as_ref(), row).await {
			return Self::money_event_failed("", err);
		}
		if let Some(posting) = posting
			&& let Err(err) = posting.mark_posted(&self.pool, row).await
		{
			return Outcome::Retry(format!("{} posted stamp: {err}", posting.name()));
		}
		Outcome::Done
	}
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
	/// The ledger applied the event but its money event was not recorded, for a reason other
	/// than an outage — retry from this `seq`, bounded like [`Self::RetryBounded`], then park
	/// without failing or compensating anything ([`Relay::park_unrecorded`]).
	MoneyEvent(String),
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

/// The hold of a reservation event that applied nothing — the one park that voids a hold.
fn refused_reserve(row: &OutboxRow, applied_legs: usize) -> Option<Uuid> {
	if row.kind != HoldEvent::KIND || applied_legs > 0 {
		return None;
	}
	// An undecodable payload names no hold to void; the relay parks it as unplannable anyway.
	match serde_json::from_str::<HoldEvent>(&row.payload).ok()? {
		HoldEvent::Placed { hold_id, .. } => Some(hold_id.raw()),
		HoldEvent::Captured { .. } | HoldEvent::Released { .. } => None,
	}
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
	/// Posted, pending and completing legs as one TigerBeetle linked chain — a hold's
	/// capture: void the reservation, post the capture, reserve the rest. A release is a
	/// chain of one, so every hold step that voids shares one reservation pre-check.
	Linked(Vec<LinkedLeg>),
	Broadcast(BroadcastRequest),
}

/// Σ what `row`'s legs will take out of `key` once the relay applies them — the event's
/// share of [`claims::unapplied_debits`](super::claims::unapplied_debits). Read off the
/// same plan the relay executes, so a new kind of event is counted the moment it can be
/// relayed. (A move, transfer, payout, invoice payment or charge is counted from its own row by
/// `claims::unapplied_debits` and its kind is left out of the outbox scan that calls this.) Only
/// new outflows count: a posted or reserved leg debiting `key`. Completing a
/// pending takes nothing more than its reserve already locked, and a void gives back.
pub(crate) fn planned_debit(row: &OutboxRow, key: &LedgerAccountKey) -> Result<u128, String> {
	let mut total: u128 = 0;
	for op in plan(row)? {
		let legs = match &op.action {
			LedgerAction::Post(transfer) | LedgerAction::Reserve(transfer) => std::slice::from_ref(transfer),
			LedgerAction::PostLinked(transfers) => transfers.as_slice(),
			LedgerAction::Linked(legs) => {
				total = total.checked_add(chain_debit(legs, key)?).ok_or("planned debits overflow")?;
				continue;
			}
			LedgerAction::Complete(_) | LedgerAction::Broadcast(_) => &[],
		};
		for leg in legs.iter().filter(|leg| &leg.debit == key) {
			total = total.checked_add(leg.amount).ok_or("planned debits overflow")?;
		}
	}
	Ok(total)
}

/// What a chain takes out of `key` beyond what it gives back. A capture voids the standing
/// reservation and takes the captured part and the rest out of exactly that, so it takes
/// nothing the ledger does not already show as locked; counting its post and new reservation
/// without the void would hold the user's money twice until the relay applied it.
fn chain_debit(legs: &[LinkedLeg], key: &LedgerAccountKey) -> Result<u128, String> {
	let (mut taken, mut released) = (0u128, 0u128);
	for leg in legs {
		match leg {
			LinkedLeg::Post(transfer) | LinkedLeg::Reserve(transfer) if &transfer.debit == key => {
				taken = taken.checked_add(transfer.amount).ok_or("planned debits overflow")?;
			}
			LinkedLeg::Complete(completion) if completion.kind == CompletionKind::Void && &completion.debit == key => {
				released = released.checked_add(completion.amount).ok_or("planned releases overflow")?;
			}
			LinkedLeg::Post(_) | LinkedLeg::Reserve(_) | LinkedLeg::Complete(_) => {}
		}
	}
	Ok(taken.saturating_sub(released))
}

/// Map an outbox event to the ledger op(s) (and any custody broadcast) it performs.
/// `reference` (stamped into `user_data_128`) is always the aggregate id, tying every
/// TB transfer back to its deposit/withdrawal/payment for reconciliation.
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
		"payments" => {
			let event: PaymentEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(plan_payment(event, row.aggregate_id, reference))
		}
		"compartments" => {
			let event: CompartmentEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(vec![plan_compartment(event, row.aggregate_id, reference)])
		}
		"scope_ledger" => {
			let event: ScopeLedgerEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(vec![plan_scope_ledger(event, row.aggregate_id, reference)])
		}
		"scope_holds" => {
			let event: HoldEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(vec![plan_hold(event, row.aggregate_id, reference)])
		}
		"scope_payouts" => {
			let event: ScopePayoutEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			plan_payout(event, row.aggregate_id, reference).map(|op| vec![op])
		}
		"scope_invoices" => {
			let event: InvoiceEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(plan_invoice(event, row.aggregate_id, reference).into_iter().collect())
		}
		"scope_charges" => {
			let event: ChargeEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			Ok(vec![plan_charge(event, row.aggregate_id, reference)])
		}
		kind => Err(format!("no plan for outbox kind `{kind}`")),
	}
}

/// A parked charge is left `pending` when the ledger's answer is unknown — the applied check
/// failed or the retries ran out: the leg may have posted, so failing the charge could release
/// money the ledger already took. Its amount keeps counting against the compartment until an
/// operator settles it by hand (PATTERNS.md, Charges).
fn charge_stays_pending_on_park(reason: &str) -> bool {
	matches!(park_kind(reason), "applied_check" | "retry_exhausted")
}

/// The kind of a park, for a parked move's `failure_reason` (`parked:<kind>`): what the user
/// and the operator need to tell apart, not the relay's full sentence.
fn park_kind(reason: &str) -> &'static str {
	if reason.starts_with("ledger conflict") {
		"conflict"
	} else if reason.starts_with("compartment move applied check")
		|| reason.starts_with("scope transfer applied check")
		|| reason.starts_with("scope payout applied check")
		|| reason.starts_with("scope charge applied check")
	{
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

/// A client's transfer between two scope accounts: one posted account-to-account leg,
/// net-zero on the cash invariant. A source that cannot cover it after all is refused by the
/// account's non-negative flag, and the transfer is stamped `failed` with nothing moved.
fn plan_scope_ledger(event: ScopeLedgerEvent, aggregate_id: Uuid, reference: u128) -> PlannedOp {
	let ScopeLedgerEvent::Transferred {
		scope,
		from_label,
		to_scope,
		to_label,
		amount,
		..
	} = event;
	PlannedOp {
		role: "scope_transfer",
		transfer_id: tid(aggregate_id, SCOPE_TRANSFER),
		action: LedgerAction::Post(LedgerTransfer {
			id: tid(aggregate_id, SCOPE_TRANSFER),
			debit: LedgerAccountKey::ScopeAccount(scope, from_label),
			credit: LedgerAccountKey::ScopeAccount(to_scope, to_label),
			amount: amount.base_units(),
			code: TransferCode::ScopeTransfer,
			reference,
		}),
	}
}

/// A payout out of a scope account: one posted leg per line, `Dr scope_account:<s>:<from> / Cr
/// scope_claim:<s>:<user>`, in ONE linked chain — every line lands or none does, and the
/// source's non-negative flag refuses the whole chain when it cannot cover it, so the payout
/// is stamped `failed` with nothing moved (see `dispatch`). Net-zero on the cash invariant.
fn plan_payout(event: ScopePayoutEvent, payout_id: Uuid, reference: u128) -> Result<PlannedOp, String> {
	let ScopePayoutEvent::PaidOut { scope, from_label, lines, .. } = event;
	let source = LedgerAccountKey::ScopeAccount(scope.clone(), from_label);
	let legs: Vec<LedgerTransfer> = lines
		.iter()
		.enumerate()
		.map(|(line_no, line)| LedgerTransfer {
			id: payout_tid(payout_id, line_no),
			debit: source.clone(),
			credit: LedgerAccountKey::ScopeClaim(line.user, scope.clone()),
			amount: line.amount.base_units(),
			code: TransferCode::ScopePayout,
			reference,
		})
		.collect();
	let first = legs.first().map(|leg| leg.id).ok_or("a payout without lines")?;
	Ok(PlannedOp {
		role: "scope_payout",
		transfer_id: first,
		action: LedgerAction::PostLinked(legs),
	})
}

/// The ledger id of a payout's leg `line_no`.
pub fn payout_tid(payout_id: Uuid, line_no: usize) -> u128 {
	tid(payout_id, format!("{SCOPE_PAYOUT}{line_no}").as_bytes())
}

fn hold_tid(hold_id: Uuid, salt: &str, version: u64) -> u128 {
	tid(hold_id, format!("{salt}{version}").as_bytes())
}

/// A hold's steps (see [`domain::scope_ledger::Hold`]): a placement reserves the amount out of
/// the user's compartment; a capture voids the standing reservation, posts what it takes out of
/// the compartment to its destination and reserves the rest, linked; a release voids the
/// reservation. Every reservation is credited to `hold_clearing`, so nothing posted ever lands
/// there.
fn plan_hold(event: HoldEvent, hold_id: Uuid, reference: u128) -> PlannedOp {
	let reservation = |scope: &domain::scopes::ScopeId, user: UserId, version: u64, amount: Usdt| LedgerTransfer {
		id: hold_tid(hold_id, HOLD_PENDING, version),
		debit: LedgerAccountKey::ScopeClaim(user, scope.clone()),
		credit: LedgerAccountKey::HoldClearing,
		amount: amount.base_units(),
		code: TransferCode::ScopeHold,
		reference,
	};
	let void = |scope: &domain::scopes::ScopeId, user: UserId, version: u64, released: u64, held: Usdt| {
		LinkedLeg::Complete(PendingCompletion {
			id: hold_tid(hold_id, HOLD_VOID, version),
			pending_id: hold_tid(hold_id, HOLD_PENDING, released),
			kind: CompletionKind::Void,
			debit: LedgerAccountKey::ScopeClaim(user, scope.clone()),
			credit: LedgerAccountKey::HoldClearing,
			amount: held.base_units(),
			code: TransferCode::ScopeHold,
			reference,
		})
	};
	match event {
		HoldEvent::Placed { scope, user, amount, version, .. } => {
			let leg = reservation(&scope, user, version, amount);
			PlannedOp {
				role: "hold_reserve",
				transfer_id: leg.id,
				action: LedgerAction::Reserve(leg),
			}
		}
		HoldEvent::Captured {
			scope,
			user,
			version,
			released,
			held,
			amount,
			to,
			..
		} => {
			let credit = match to {
				HoldDestination::Account(label) => LedgerAccountKey::ScopeAccount(scope.clone(), label),
				HoldDestination::User(other) => LedgerAccountKey::ScopeClaim(other, scope.clone()),
			};
			let mut legs = vec![
				void(&scope, user, version, released, held),
				LinkedLeg::Post(LedgerTransfer {
					id: hold_tid(hold_id, HOLD_CAPTURE, version),
					debit: LedgerAccountKey::ScopeClaim(user, scope.clone()),
					credit,
					amount: amount.base_units(),
					code: TransferCode::ScopeHold,
					reference,
				}),
			];
			// The domain never captures more than is held, so `None` is never the answer.
			let rest = held.checked_sub(amount).unwrap_or(Usdt::ZERO);
			if !rest.is_zero() {
				legs.push(LinkedLeg::Reserve(reservation(&scope, user, version, rest)));
			}
			PlannedOp {
				role: "hold_capture",
				transfer_id: legs[0].id(),
				action: LedgerAction::Linked(legs),
			}
		}
		HoldEvent::Released {
			scope,
			user,
			version,
			released,
			held,
			..
		} => {
			let leg = void(&scope, user, version, released, held);
			PlannedOp {
				role: "hold_release",
				transfer_id: leg.id(),
				action: LedgerAction::Linked(vec![leg]),
			}
		}
	}
}

/// A charge: one posted transfer out of the user's compartment straight into the scope account
/// the client named. A compartment that cannot cover it after all — another flow's reservation
/// landed first — is refused by the claim's non-negative flag, and the charge is stamped `failed`
/// with nothing moved (see `dispatch`).
fn plan_charge(event: ChargeEvent, charge_id: Uuid, reference: u128) -> PlannedOp {
	let ChargeEvent::Charged { scope, user, to_label, amount, .. } = event;
	let id = tid(charge_id, SCOPE_CHARGE);
	PlannedOp {
		role: "scope_charge",
		transfer_id: id,
		action: LedgerAction::Post(LedgerTransfer {
			id,
			debit: LedgerAccountKey::ScopeClaim(user, scope.clone()),
			credit: LedgerAccountKey::ScopeAccount(scope, to_label),
			amount: amount.base_units(),
			code: TransferCode::ScopeCharge,
			reference,
		}),
	}
}

fn invoice_pay_tid(invoice_id: Uuid, version: u64) -> u128 {
	tid(invoice_id, format!("{INVOICE_PAY}{version}").as_bytes())
}

/// An invoice payment: one posted transfer out of the payer's `main` or compartment straight into
/// the scope account the invoice names. The other steps of an invoice move no money and never
/// reach the outbox; should one, it plans nothing.
fn plan_invoice(event: InvoiceEvent, invoice_id: Uuid, reference: u128) -> Option<PlannedOp> {
	let InvoiceEvent::Paid {
		scope,
		to_label,
		amount,
		payer,
		from,
		version,
		..
	} = event
	else {
		return None;
	};
	let id = invoice_pay_tid(invoice_id, version);
	Some(PlannedOp {
		role: "invoice_pay",
		transfer_id: id,
		action: LedgerAction::Post(LedgerTransfer {
			id,
			debit: from.claim_key(payer),
			credit: LedgerAccountKey::ScopeAccount(scope, to_label),
			amount: amount.base_units(),
			code: TransferCode::InvoicePayment,
			reference,
		}),
	})
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

/// A payment's money in the ledger — the SAME two-phase shape a withdrawal uses, because it
/// spends the same claims and must be just as unable to overdraw one:
/// - **Reserved** (raised on approval, L3 only) → a pending `Dr <source> / Cr clearing`,
///   so N concurrently approved payments physically cannot overdraw one claim;
/// - **Settled** → post that pending, then move the gross out of `clearing` into the
///   destination claim. The Vec order matters: the post must land before the second leg
///   debits the now-posted clearing balance.
/// - **Released** (raised on a failed execution, L3 only) → void that pending, returning
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
/// already derivable from the claim pair a transfer names, so a code per tier would encode
/// one fact twice and let the two disagree.
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
///   non-zero) the retained fee→banking's treasury (D17). The `Cr wallet:<net>` is where
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
		WithdrawalEvent::Settled { source, network, amount, fee, .. } => {
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
						credit: Party::Treasury.claim_key(),
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

/// The transfer id of a payment's reservation leg — what the payment execution path asks
/// `saga_steps` and the ledger about before recording an L3 settlement, so the settle can
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
	use domain::money::Usdt;

	use super::*;

	#[test]
	fn a_settled_payment_posts_its_reservation_before_it_moves_the_money() {
		let aggregate_id = Uuid::new_v4();
		let user = UserId::new();
		let event = PaymentEvent::Settled {
			payment_id: domain::payments::PaymentId::from_raw(aggregate_id),
			from: Party::User(user),
			to: Party::User(UserId::new()),
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

	// A failed L3 execution VOIDS the reservation the approval raised — the same completion
	// a failed withdrawal issues — and names that reservation as its pending, or the release
	// would void a transfer that does not exist and leave the amount locked for good.
	#[test]
	fn a_released_payment_voids_the_reservation_its_approval_raised() {
		let aggregate_id = Uuid::new_v4();
		let event = PaymentEvent::Released {
			payment_id: domain::payments::PaymentId::from_raw(aggregate_id),
			from: Party::Treasury,
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
		assert_eq!(completion.debit, Party::Treasury.claim_key());
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

	// A deposit credits the person's claim and nothing else; a v2.20 seed payload is unreadable, never a credit.
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
		assert_eq!(leg.credit, LedgerAccountKey::UserClaim(depositor), "the depositor's own claim");
		assert_eq!(leg.debit, LedgerAccountKey::CryptoWallet(domain::money::Network::Bep20));
		assert_eq!(leg.code, TransferCode::Deposit);

		let legacy = r#"{"type":"capital_seeded","network":"bep20","amount":"1000"}"#;
		assert!(serde_json::from_str::<LedgerEvent>(legacy).is_err(), "a retired seed payload must not read as anything");
		let legacy_deposit = r#"{"type":"deposited","party":{"kind":"piggybank"},"network":"bep20","amount":"1000"}"#;
		assert!(serde_json::from_str::<LedgerEvent>(legacy_deposit).is_err(), "nor a deposit onto the retired piggybank claim");
	}

	// The retained withdrawal fee lands in banking's treasury (D17).
	#[test]
	fn the_withdrawal_fee_goes_to_the_treasury() {
		let withdrawal_id = domain::withdrawals::WithdrawalId::new();
		let settled = WithdrawalEvent::Settled {
			withdrawal_id,
			source: WithdrawalSource::User(UserId::new()),
			network: domain::money::Network::Bep20,
			amount: Usdt::parse_decimal("100").unwrap(),
			fee: Usdt::parse_decimal("1").unwrap(),
			tx_ref: domain::money::TxRef::parse("0xabc").unwrap(),
		};
		let ops = plan_withdrawal(settled, withdrawal_id.raw(), withdrawal_id.raw().as_u128()).unwrap();
		let op = ops.into_iter().find(|op| op.role == "withdraw_fee").expect("a non-zero fee has its own leg");
		let LedgerAction::Post(transfer) = op.action else { panic!("the fee is a posted leg") };
		assert_eq!(
			transfer.credit,
			LedgerAccountKey::ScopeAccount(domain::scopes::ScopeId::main(), domain::scopes::ScopeAccountLabel::treasury())
		);
		assert_eq!(transfer.id, tid(withdrawal_id.raw(), FEE_REDISTRIBUTE));
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

	#[test]
	fn an_invoice_payment_posts_once_out_of_the_chosen_source() {
		let (invoice, payer) = (Uuid::new_v4(), UserId::new());
		let scope = domain::scopes::ScopeId::parse("sa").unwrap();
		let paid = |from: domain::scopes::Compartment| InvoiceEvent::Paid {
			invoice_id: domain::scope_invoices::InvoiceId::from_raw(invoice),
			scope: scope.clone(),
			to_label: domain::scopes::ScopeAccountLabel::parse("sales").unwrap(),
			amount: Usdt::parse_decimal("25").unwrap(),
			payer,
			from,
			version: 2,
		};
		let row = |event: InvoiceEvent| OutboxRow {
			seq: 1,
			event_id: Uuid::new_v4(),
			aggregate: "scope_invoice".into(),
			aggregate_id: invoice,
			kind: "scope_invoices".into(),
			payload: serde_json::to_string(&event).unwrap(),
			attempts: 0,
		};
		let from_main = row(paid(domain::scopes::Compartment::Main));
		let ops = plan(&from_main).unwrap();
		let [op] = ops.as_slice() else { panic!("one leg") };
		let LedgerAction::Post(leg) = &op.action else { panic!("a posted transfer") };
		assert_eq!((leg.id, leg.code), (invoice_pay_tid(invoice, 2), TransferCode::InvoicePayment));
		assert_ne!(invoice_pay_tid(invoice, 2), invoice_pay_tid(invoice, 4), "a later attempt never shares a refused one's id");
		assert_eq!(
			leg.credit,
			LedgerAccountKey::ScopeAccount(scope.clone(), domain::scopes::ScopeAccountLabel::parse("sales").unwrap())
		);
		let amount = Usdt::parse_decimal("25").unwrap().base_units();
		assert_eq!(planned_debit(&from_main, &LedgerAccountKey::UserClaim(payer)).unwrap(), amount);
		assert_eq!(planned_debit(&from_main, &LedgerAccountKey::ScopeClaim(payer, scope.clone())).unwrap(), 0);
		let from_compartment = row(paid(domain::scopes::Compartment::Scope(scope.clone())));
		assert_eq!(planned_debit(&from_compartment, &LedgerAccountKey::ScopeClaim(payer, scope.clone())).unwrap(), amount);
		let cancelled = row(InvoiceEvent::Cancelled {
			invoice_id: domain::scope_invoices::InvoiceId::from_raw(invoice),
			scope,
			version: 2,
		});
		assert!(plan(&cancelled).unwrap().is_empty(), "only a payment moves money");
	}

	// A kind with no plan is never treated as done: it would otherwise be marked dispatched with
	// nothing on the ledger.
	#[test]
	fn an_outbox_kind_without_a_plan_is_unplannable() {
		for kind in ["subscriptions", "redemptions", "fees", "issuances", "book", "users", ""] {
			let row = OutboxRow {
				seq: 1,
				event_id: Uuid::new_v4(),
				aggregate: kind.into(),
				aggregate_id: Uuid::new_v4(),
				kind: kind.into(),
				payload: "{}".into(),
				attempts: 0,
			};
			assert!(plan(&row).is_err(), "`{kind}` planned");
		}
	}

	// A charge is one posted leg out of the user's compartment into the client's account, and an
	// undispatched one counts against that compartment — and only that one — in every spend check.
	#[test]
	fn a_charge_posts_once_out_of_the_compartment() {
		let (charge, user) = (Uuid::new_v4(), UserId::new());
		let scope = domain::scopes::ScopeId::parse("sa").unwrap();
		let to = domain::scopes::ScopeAccountLabel::parse("sales").unwrap();
		let event = ChargeEvent::Charged {
			charge_id: domain::scope_charges::ChargeId::from_raw(charge),
			scope: scope.clone(),
			user,
			to_label: to.clone(),
			amount: Usdt::parse_decimal("25").unwrap(),
		};
		let row = OutboxRow {
			seq: 1,
			event_id: Uuid::new_v4(),
			aggregate: "scope_charge".into(),
			aggregate_id: charge,
			kind: "scope_charges".into(),
			payload: serde_json::to_string(&event).unwrap(),
			attempts: 0,
		};
		assert!(matches!(Posting::of(&row.kind), Some(Posting::ScopeCharge)));
		let ops = plan(&row).unwrap();
		let [op] = ops.as_slice() else { panic!("one leg") };
		let LedgerAction::Post(leg) = &op.action else { panic!("a posted transfer") };
		assert_eq!((leg.id, leg.code), (tid(charge, SCOPE_CHARGE), TransferCode::ScopeCharge));
		assert_eq!(leg.credit, LedgerAccountKey::ScopeAccount(scope.clone(), to));
		let amount = Usdt::parse_decimal("25").unwrap().base_units();
		assert_eq!(planned_debit(&row, &LedgerAccountKey::ScopeClaim(user, scope.clone())).unwrap(), amount);
		assert_eq!(planned_debit(&row, &LedgerAccountKey::UserClaim(user)).unwrap(), 0, "main is untouched");
		assert_eq!(
			planned_debit(&row, &LedgerAccountKey::ScopeClaim(user, domain::scopes::ScopeId::parse("other").unwrap())).unwrap(),
			0
		);
	}

	#[test]
	fn a_parked_charge_whose_fate_is_unknown_stays_pending() {
		assert!(charge_stays_pending_on_park("scope charge applied check: unreachable"));
		assert!(charge_stays_pending_on_park("retryable exhausted after 25 attempts: busy"));
		assert!(!charge_stays_pending_on_park("ledger conflict: exists with other terms"));
		assert!(!charge_stays_pending_on_park("unplannable event: bad"));
		assert!(!charge_stays_pending_on_park("insufficient funds"));
	}

	// A capture voids the standing reservation and takes the captured part and the rest out of
	// it: nothing beyond what is already locked, or an undispatched capture would hold the
	// user's money twice.
	#[test]
	fn a_hold_capture_chains_void_post_reserve_and_takes_nothing_beyond_the_reservation() {
		let (hold, user, other) = (Uuid::new_v4(), UserId::new(), UserId::new());
		let scope = domain::scopes::ScopeId::parse("sa").unwrap();
		let usdt = |raw: &str| Usdt::parse_decimal(raw).unwrap();
		let op = plan_hold(
			HoldEvent::Captured {
				hold_id: domain::scope_ledger::HoldId::from_raw(hold),
				scope: scope.clone(),
				user,
				version: 3,
				released: 2,
				held: usdt("10"),
				amount: usdt("4"),
				to: HoldDestination::User(other),
				subject_ref: domain::scope_ledger::SubjectRef::parse("order:4f0c2d1e-0000-4000-8000-0123456789ab").unwrap(),
			},
			hold,
			0,
		);
		let LedgerAction::Linked(legs) = &op.action else { panic!("a capture is one linked chain") };
		let [LinkedLeg::Complete(void), LinkedLeg::Post(post), LinkedLeg::Reserve(rest)] = legs.as_slice() else {
			panic!("{legs:?}")
		};
		assert_eq!(void.pending_id, hold_tid(hold, HOLD_PENDING, 2), "voids the reservation version 2 made");
		assert_eq!(rest.id, hold_tid(hold, HOLD_PENDING, 3), "the rest is version 3's reservation");
		assert_eq!((post.amount, rest.amount), (usdt("4").base_units(), usdt("6").base_units()));
		assert_eq!(post.credit, LedgerAccountKey::ScopeClaim(other, scope.clone()));
		let held = LedgerAccountKey::ScopeClaim(user, scope);
		assert_eq!(chain_debit(legs, &held).unwrap(), 0, "the void gives back what the post and the new reservation take");
	}
}
