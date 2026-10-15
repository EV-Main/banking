//! `piggybank-core` — the hub server library.
//!
//! The central bank's driving adapter is **gRPC** (tonic): the closed, internal
//! surface other services call. There is no HTTP here — browser/client traffic
//! reaches the hub through the `cabinet/backend` BFF, which proxies HTTP↔gRPC.
//!
//! `main` is the composition root for the whole `piggybank` system: it runs the
//! core gRPC services **and** the [`evbanking_auth`] auth service as separate
//! in-process tasks. Auth hands core an [`Authorizer`] — a channel to the auth
//! task — so core authorizes incoming gRPC requests across a task boundary
//! instead of over the network.
//!
//! Hexagonal layout over the shared `domain` — each module is named for its role,
//! not its transport:
//!   services        — driving adapter (the hub's gRPC service surface; tonic-web)
//!   application     — use cases (CQRS command/query handlers)
//!   ports           — driven ports (Repository/Reader leaf traits, Ledger gateway)
//!   infrastructure  — driven adapters (Postgres control plane, TigerBeetle ledger,
//!                     telemetry)
//!
//! The money plane (balance, subscriptions, redemptions, withdrawals, the allocation
//! book) is implemented end to end through these layers; remaining domain areas stay
//! documented placeholders until a feature explicitly asks.

use std::sync::Arc;

use application::book::BookFeed;
use domain::{book::MatchingEngine, money::Network};
use ev::analytics::Analytics;
use evbanking_auth::Authorizer;
use ports::{
	AllocationRegistry, BookStore, CompartmentStore, ConsiliumRepository, Custody, DepositAddresses, Deposits, FeePorts, FundPositionReader, NavMarks, OperationFeed, OutflowPolicy,
	PaymentFeed, PaymentRepository, RedemptionRepository, SubscriptionRepository, UnitIssuanceRepository, UserRepository, WithdrawalRepository, ledger::Ledger, scope_invoices::InvoiceStore,
};
use sqlx::PgPool;
use tokio::sync::Notify;

pub mod application;
pub mod config;
pub mod crypto;
pub mod infrastructure;
pub mod ports;
pub mod services;

/// Shared, cheaply-cloneable handles injected into the gRPC services. The
/// Postgres pool is the **control plane** (metadata, id-mapping, event
/// log/outbox, projections); the [`Ledger`] gateway is the **data plane**
/// (authoritative money in TigerBeetle); the [`Authorizer`] verifies inbound
/// requests via the in-process channel to the auth task; [`Analytics`] is the
/// product-analytics seam (native PostHog capture, a no-op until `POSTHOG_KEY` is
/// set). Command handlers `notify` the [`relay_notify`](AppState::relay_notify)
/// after a commit so the outbox relay moves money promptly.
#[derive(Clone)]
pub struct AppState {
	pub pool: PgPool,
	/// The TigerBeetle money gateway (data plane).
	pub ledger: Arc<dyn Ledger>,
	pub authorizer: Authorizer,
	pub analytics: Analytics,
	/// The `users` aggregate's driven port (Postgres control plane).
	pub users: Arc<dyn UserRepository>,
	/// The `withdrawals` aggregate's driven port (Postgres control plane).
	pub withdrawals: Arc<dyn WithdrawalRepository>,
	/// The `consilium` aggregate's driven port — multi-owner authorization for revenue
	/// payouts. Control plane only; it moves no money itself, and reaches the money plane
	/// exactly once, through the ordinary payout path, when a request is approved.
	pub consilia: Arc<dyn ConsiliumRepository>,
	/// The `payments` aggregate's driven port — the order that moves money between two named
	/// ends of the platform, plus the consent seat that authorizes an investor-sourced one.
	/// Control plane only: the money leaves through the relay, from the two events the order
	/// drains to the outbox.
	pub payments: Arc<dyn PaymentRepository>,
	/// The admin payments screen's read model. A separate handle from [`Self::payments`] for
	/// the reason `operations` is separate from the aggregates it reads: it owns nothing.
	pub payment_feed: Arc<dyn PaymentFeed>,
	/// The registry of investable products — the gate every subscribe resolves its
	/// service through. Control plane only; moves no money.
	pub allocations: Arc<dyn AllocationRegistry>,
	/// The `subscriptions` aggregate's driven port (mint records + position cost basis).
	pub subscriptions: Arc<dyn SubscriptionRepository>,
	/// The `issuance` aggregate's driven port — an operator's in-kind mints (units to an
	/// investor, no cash leg), idempotent by key.
	pub issuances: Arc<dyn UnitIssuanceRepository>,
	/// The `redemptions` aggregate's driven port (the accept-and-queue saga).
	pub redemptions: Arc<dyn RedemptionRepository>,
	/// The allocation book — orders, fills and per-product terms (Postgres control
	/// plane); the escrow and the settlement of every fill go through the relay.
	pub book: Arc<dyn BookStore>,
	/// The matching rule the book runs under its write lock. Pure; wired here so the
	/// composition root, not the store, decides how orders match.
	pub book_engine: Arc<dyn MatchingEngine>,
	/// The in-process fan-out every `WatchBook` subscriber hangs on.
	pub book_feed: Arc<BookFeed>,
	/// The aggregate-less on-chain arrival facts (the `tx_ref` deposit gate) + outbox.
	pub deposits: Arc<dyn Deposits>,
	/// Fund valuation marks → the derived NAV (the operator-posted AUM history).
	pub nav: Arc<dyn NavMarks>,
	/// The per-investor fund-position projection (cost basis, high-water mark).
	pub positions: Arc<dyn FundPositionReader>,
	/// The fee plane — a fund's "2 and 20" terms, the per-investor accrual clocks, the
	/// charge, and the bulk settlement of accumulated fee units. A product with no policy
	/// row charges nothing, so the fee is opt-in per product.
	pub fees: FeePorts,
	/// The read-side merge of the four money projections into one activity timeline.
	/// Query only — it writes nothing and owns no aggregate.
	pub operations: Arc<dyn OperationFeed>,
	/// Per-user deposit-address provisioning (stub HD derivation behind a port).
	pub deposit_addresses: Arc<dyn DepositAddresses>,
	/// The custody gateway (the same registry the relay broadcasts through) — read-only
	/// here: the withdrawal handlers gate dispatch on its on-chain treasury liquidity.
	pub custody: Arc<dyn Custody>,
	/// The rails with a running on-chain watcher — the wallet surface presents
	/// deposit/withdraw rails only for these, and the health probe reports scan-cursor
	/// age only for these.
	pub configured_networks: Arc<[Network]>,
	/// Whether money movement requires a verified identity tier — read from the
	/// environment once at boot ([`KycGate`](config::KycGate)) and enforced unless the
	/// deployment explicitly says otherwise. Carried here rather than re-read per call so
	/// the deposit, admission and dispatch gates cannot disagree about it mid-flight.
	pub kyc_gate: config::KycGate,
	/// The outflow policy — the read-only kill-switch and the per-owner money-out standing —
	/// every payout path clears at the moment the money leaves: the dispatcher, the admin
	/// dispatch RPC and a payment order's execution. One handle, so the three cannot read
	/// the pause differently. Built here from the pool rather than passed in.
	pub outflow: Arc<dyn OutflowPolicy>,
	/// The scopes banking knows, their policies, and users' moves between `main` and a
	/// scope compartment. Control plane only; the relay posts each move.
	pub compartments: Arc<dyn CompartmentStore>,
	/// Invoices scopes issue, as their payers read and pay them. Control plane only; the
	/// relay posts each payment.
	pub invoices: Arc<dyn InvoiceStore>,
	/// Nudges the outbox relay to dispatch right after a command commits.
	pub relay_notify: Arc<Notify>,
	/// Base URL the emailed consilium approval link is built on (`<base>/<token>`). The
	/// page it points at is served by the cabinet, not here; the hub only has to mint a
	/// link an owner's mail client will render.
	pub consilium_approval_url_base: String,
	/// Base URL the emailed payment CONSENT link is built on (`<base>/<token>`) — the
	/// investor-facing twin of the approval base.
	pub payment_consent_url_base: String,
	/// Whether the TON rail is on testnet — surfaced on its deposit addresses so the client
	/// renders the correct (testnet-tagged) user-friendly TON address. `false` when TON is
	/// unconfigured or on mainnet. The other rails have no testnet-specific address form.
	pub ton_is_testnet: bool,
}

impl AppState {
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		pool: PgPool,
		ledger: Arc<dyn Ledger>,
		authorizer: Authorizer,
		analytics: Analytics,
		users: Arc<dyn UserRepository>,
		withdrawals: Arc<dyn WithdrawalRepository>,
		consilia: Arc<dyn ConsiliumRepository>,
		payments: Arc<dyn PaymentRepository>,
		payment_feed: Arc<dyn PaymentFeed>,
		allocations: Arc<dyn AllocationRegistry>,
		subscriptions: Arc<dyn SubscriptionRepository>,
		issuances: Arc<dyn UnitIssuanceRepository>,
		redemptions: Arc<dyn RedemptionRepository>,
		book: Arc<dyn BookStore>,
		book_engine: Arc<dyn MatchingEngine>,
		book_feed: Arc<BookFeed>,
		deposits: Arc<dyn Deposits>,
		nav: Arc<dyn NavMarks>,
		positions: Arc<dyn FundPositionReader>,
		fees: FeePorts,
		operations: Arc<dyn OperationFeed>,
		deposit_addresses: Arc<dyn DepositAddresses>,
		custody: Arc<dyn Custody>,
		configured_networks: Arc<[Network]>,
		kyc_gate: config::KycGate,
		relay_notify: Arc<Notify>,
		consilium_approval_url_base: String,
		payment_consent_url_base: String,
		ton_is_testnet: bool,
		outflow: Arc<dyn OutflowPolicy>,
		compartments: Arc<dyn CompartmentStore>,
		invoices: Arc<dyn InvoiceStore>,
	) -> Self {
		Self {
			pool,
			ledger,
			authorizer,
			analytics,
			users,
			withdrawals,
			consilia,
			payments,
			payment_feed,
			allocations,
			subscriptions,
			issuances,
			redemptions,
			book,
			book_engine,
			book_feed,
			deposits,
			nav,
			positions,
			fees,
			operations,
			deposit_addresses,
			custody,
			configured_networks,
			kyc_gate,
			relay_notify,
			consilium_approval_url_base,
			payment_consent_url_base,
			ton_is_testnet,
			outflow,
			compartments,
			invoices,
		}
	}
}
