//! `piggybank-core` — the hub server library.
//!
//! The central bank's driving adapter is **gRPC** (tonic): the closed, internal
//! surface other services call. There is no HTTP here — browser/client traffic
//! reaches the hub through the `wallet/backend` BFF, which proxies HTTP↔gRPC.
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
//! The money plane (balance, withdrawals, payments, compartments, the scope ledger) is
//! implemented end to end through these layers.

use std::sync::Arc;

use domain::{money::Network, scope_ledger::Grants};
use ev::analytics::Analytics;
use evbanking_auth::Authorizer;
use ports::{
	CompartmentStore, Custody, DepositAddresses, Deposits, OperationFeed, OutflowPolicy, PaymentFeed, PaymentRepository, UserRepository, WithdrawalRepository,
	ledger::Ledger,
	scope_invoices::InvoiceStore,
	scope_mandates::{MandateRequestStore, MandateStore},
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
	/// The `payments` aggregate's driven port — the order that moves money between two named
	/// ends of the platform, plus the seat that authorizes it.
	/// Control plane only: the money leaves through the relay, from the two events the order
	/// drains to the outbox.
	pub payments: Arc<dyn PaymentRepository>,
	/// The admin payments screen's read model. A separate handle from [`Self::payments`] for
	/// the reason `operations` is separate from the aggregates it reads: it owns nothing.
	pub payment_feed: Arc<dyn PaymentFeed>,
	/// The aggregate-less on-chain arrival facts (the `tx_ref` deposit gate) + outbox.
	pub deposits: Arc<dyn Deposits>,
	/// The read-side merge of the money projections into one activity timeline.
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
	/// Users' mandates — their consent to a scope's service drawing on their compartment.
	pub mandates: Arc<dyn MandateStore>,
	/// What scope services ask users to grant; users read and grant them by id.
	pub mandate_requests: Arc<dyn MandateRequestStore>,
	/// Banking's grants to scope service clients, as the scope ledger serves them: a user may
	/// grant a mandate only to a client these let touch compartments. Empty — every grant
	/// refused — while the scope ledger is not configured ([`Self::with_scope_grants`]).
	pub scope_grants: Arc<Grants>,
	/// The owner's withdrawal limits and holds ([`application::withdrawal_limits`]); the
	/// defaults unless [`Self::with_withdrawal_limits`] set the deployment's own.
	pub withdrawal_limits: application::withdrawal_limits::WithdrawalLimits,
	/// Nudges the outbox relay to dispatch right after a command commits.
	pub relay_notify: Arc<Notify>,
	/// What opens a treasury payment's approval with the proof of who asked.
	pub payment_proposer: Arc<application::payment_approvals::PaymentProposer>,
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
		payments: Arc<dyn PaymentRepository>,
		payment_feed: Arc<dyn PaymentFeed>,
		deposits: Arc<dyn Deposits>,
		operations: Arc<dyn OperationFeed>,
		deposit_addresses: Arc<dyn DepositAddresses>,
		custody: Arc<dyn Custody>,
		configured_networks: Arc<[Network]>,
		kyc_gate: config::KycGate,
		relay_notify: Arc<Notify>,
		payment_proposer: Arc<application::payment_approvals::PaymentProposer>,
		ton_is_testnet: bool,
		outflow: Arc<dyn OutflowPolicy>,
		compartments: Arc<dyn CompartmentStore>,
		invoices: Arc<dyn InvoiceStore>,
	) -> Self {
		let scope_ledger = Arc::new(infrastructure::scope_ledger::PgScopeLedger::new(pool.clone()));
		Self {
			pool,
			ledger,
			authorizer,
			analytics,
			users,
			withdrawals,
			payments,
			payment_feed,
			deposits,
			operations,
			deposit_addresses,
			custody,
			configured_networks,
			kyc_gate,
			relay_notify,
			payment_proposer,
			ton_is_testnet,
			outflow,
			compartments,
			invoices,
			mandates: scope_ledger.clone(),
			mandate_requests: scope_ledger,
			scope_grants: Arc::new(Grants::default()),
			withdrawal_limits: application::withdrawal_limits::WithdrawalLimits::default(),
		}
	}

	pub fn with_withdrawal_limits(mut self, limits: application::withdrawal_limits::WithdrawalLimits) -> Self {
		self.withdrawal_limits = limits;
		self
	}

	/// The grants the scope ledger was configured with, for the user's side of mandates.
	pub fn with_scope_grants(mut self, grants: Grants) -> Self {
		self.scope_grants = Arc::new(grants);
		self
	}
}
