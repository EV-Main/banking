//! Infrastructure: driven adapters over the concrete external systems the hub
//! runs on.
//!
//! - [`db`] — Postgres **control plane**: pool and migrations.
//! - [`tigerbeetle`] — the connected TigerBeetle client.
//! - [`ledger`] — the **data plane** `Ledger` [`Gateway`](domain::architecture::Gateway)
//!   over TigerBeetle (the chart of accounts, transfers, two-phase saga ops).
//! - [`users`] — Postgres repository for the `User` aggregate;
//!   [`subscriptions`] / [`redemptions`] / [`withdrawals`] / [`issuance`] — repositories
//!   for the money-plane aggregates (atomic state + drained events).
//! - [`outbox`] — the transactional outbox written inside the same transaction as
//!   the state change, plus its drain side.
//! - [`relay`] — the single-worker saga dispatcher that drains the outbox and
//!   issues TigerBeetle transfers (Write-Last), idempotently.
//! - [`reconciliation`] — the periodic PG-vs-TB discrepancy scan (cash invariant,
//!   clearing vs in-flight withdrawals, parked-row surface); alert-only, TB wins.
//! - [`reaper`] — the abandoned-saga sweep: alerts on stuck `processing` withdrawals
//!   and auto-resolves the safe `queued` redemptions/withdrawals past a max age.
//! - [`dispatcher`] — the treasury worker: re-checks `queued` withdrawals against both
//!   liquidity gates (TB rail + on-chain treasury) and dispatches the covered ones.
//! - [`fees`] — Postgres adapters for the fee plane (policy, accrual clocks, the
//!   charge, the bulk settlement of accumulated fee units).
//! - [`fee_accrual`] — the obligation the fee plane places on everyone who moves a cost
//!   basis: settle what the old basis accrued before writing the new one.
//! - [`fee_sweeper`] — the periodic worker that assesses management + performance fees
//!   against every unit-holding position that is due.
//! - [`book`] — Postgres adapter for the allocation book: the single-writer placement
//!   transaction (advisory lock, the pure matcher, orders + trades + outbox facts), the
//!   depth/tape/candle reads, and the per-product policy.
//! - [`compartments`] — Postgres adapter for the scope registry, the scope policies and
//!   users' compartment moves (the money itself is posted by the relay).
//! - [`scope_ledger`] — Postgres adapter for the scope ledger: scope-account owners, the
//!   service clients' idempotency keys and their transfers (posted by the relay);
//!   [`scope_payouts`] their payouts, [`scope_operations`] the operations awaiting an
//!   approval, [`approvals`] concierge's approval execution seam over the bridge.
//! - [`scope_invoices`] — Postgres adapter for invoices scopes issue and users pay (the
//!   payment is posted by the relay), plus the reaper's expiry of open invoices.
//! - [`scope_charges`] — Postgres adapter for one-off charges of users' compartments (posted by
//!   the relay), plus the reaper's replay of a lost one.
//! - [`scope_events`] — the consumer of concierge's scope stream: keeps `scopes` (state,
//!   version) as concierge has them, registering scopes banking has not seen.
//! - [`operation_feed`] — the read-side merge of the four money projections into one
//!   time-ordered activity timeline (query side only; writes nothing).
//! - [`outflow`] — the `OutflowPolicy` adapter: the read-only kill-switch plus the
//!   owner's freeze/KYC standing, the facts every payout path clears before dispatch.
//! - [`telemetry`] — the observability adapter: the one seam that hands errors to
//!   the monitoring vendor, so call sites stay vendor-agnostic.
//! - [`rails`] — what the EVM/TON/Tron rails share verbatim: the watcher and sweep error
//!   taxonomies, the wall clock, and the sweep's Postgres/signer plumbing parameterised by
//!   network. Per-protocol RPC, node error wording and log prefixes stay in the rail modules.

pub mod allocations;
pub mod approvals;
pub mod book;
pub mod bridge;
pub mod claims;
pub mod compartments;
pub mod custody;
pub mod db;
pub mod deposit_watcher;
pub mod deposits;
pub mod dispatcher;
pub mod evm_rpc;
pub mod fee_accrual;
pub mod fee_sweeper;
pub mod fees;
pub mod issuance;
pub mod ledger;
pub mod money_events;
pub mod nav;
pub mod operation_feed;
pub mod operations;
pub mod outbox;
pub mod outflow;
pub mod payments;
pub mod payments_sweeper;
pub mod payout_review;
pub mod positions;
pub mod rails;
pub mod reaper;
pub mod reconciliation;
pub mod redemptions;
pub mod relay;
pub mod scope_charges;
pub mod scope_events;
pub mod scope_holds;
pub mod scope_invoices;
pub mod scope_ledger;
pub mod scope_mandates;
pub mod scope_notifications;
pub mod scope_operations;
pub mod scope_payouts;
pub mod signer_addresses;
pub mod singleton;
pub mod subscriptions;
pub mod sweep;
pub mod telemetry;
pub mod tigerbeetle;
pub mod ton_custody;
pub mod ton_deposit_watcher;
pub mod ton_rpc;
pub mod ton_sweep;
pub mod ton_withdrawal_watcher;
pub mod treasury_drift;
pub mod tron_custody;
pub mod tron_deposit_watcher;
pub mod tron_rpc;
pub mod tron_sweep;
pub mod tron_withdrawal_watcher;
pub mod users;
pub mod withdrawal_watcher;
pub mod withdrawals;
