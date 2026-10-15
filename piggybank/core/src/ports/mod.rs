//! Driven ports — the outbound interfaces the application depends on, implemented
//! by `infrastructure`. The hexagonal "domain/port" layer over the generic DDD
//! building blocks in [`domain::architecture`].
//!
//! [`UserRepository`] is the first leaf port: it ties the [`User`] aggregate to its
//! Postgres persistence and the narrow read side ([`Reader`]). Methods are
//! use-case-shaped and each is internally atomic — the aggregate's drained events
//! are written to the event log in the same transaction as the state change (the
//! ACID point), so callers never juggle a transaction across the port boundary.
//!
//! The money plane adds the fund-currency ports — [`SubscriptionRepository`],
//! [`RedemptionRepository`], [`WithdrawalRepository`], [`NavMarks`], and
//! [`Deposits`] — each an atomic, row-locked persistence seam, plus the external
//! gateways ([`Gateway`](domain::architecture::Gateway)): [`Ledger`] (TigerBeetle),
//! [`Custody`] (the signing service), and [`DepositAddresses`] — which by
//! construction cannot enrol in a Postgres `UnitOfWork` (money is written last, in
//! the relay).
//!
//! The fee plane adds [`FeePolicies`] (a fund's terms), [`PositionAccruals`] (the
//! per-investor accrual clocks), [`FeeAssessments`] (the charge), and
//! [`FeeSettlements`] (converting accumulated fee units to cash).
//!
//! [`UnitIssuanceRepository`] records an operator's in-kind mint — units to an investor
//! with no cash leg — the one supply path beside a subscription.
//!
//! [`BookStore`] is the allocation book — orders, fills, the per-product terms — one port
//! for the one transaction a placement is.
//!
//! [`ScopeLedgerStore`] is the scope ledger's control plane: the owners of scope accounts,
//! the idempotency keys of service clients, and their transfers; [`scope_operations`] adds
//! their payouts, the operations that wait on an approval, and concierge's approvals seam.

pub mod allocations;
pub mod book;
pub mod compartments;
pub mod consilium;
pub mod custody;
pub mod deposit_addresses;
pub mod deposits;
pub mod fees;
pub mod governance_mail;
pub mod issuance;
pub mod ledger;
pub mod nav;
pub mod operations;
pub mod outflow;
pub mod payments;
pub mod positions;
pub mod redemptions;
pub mod scope_invoices;
pub mod scope_ledger;
pub mod scope_mandates;
pub mod scope_operations;
pub mod subscriptions;
pub mod withdrawals;

pub use allocations::{AllocationAccessGrant, AllocationRecord, AllocationRegistry};
use async_trait::async_trait;
pub use book::{BookDepth, BookLevel, BookPolicyRecord, BookStore, CancelOutcome, Candle, OrderRecord, PlaceOutcome, TradeRecord, UserTrade};
pub use compartments::CompartmentStore;
pub use consilium::ConsiliumRepository;
pub use custody::{BroadcastRequest, Custody, CustodyError};
pub use deposit_addresses::DepositAddresses;
pub use deposits::Deposits;
use domain::{
	architecture::{Reader, Repository},
	auth::AuthSubject,
	error::DomainError,
	users::{ConciergeUserId, Email, ProfileFields, User, UserId},
};
pub use fees::{
	AssessmentRecord, ConsiliumOpening, FeeAssessments, FeePolicies, FeePolicyChange, FeePolicyChanges, FeePorts, FeeSettlements, NewFeePolicyChange, PolicyRecord, PositionAccrual,
	PositionAccruals, SettlementRecord,
};
pub use governance_mail::GovernanceMailer;
pub use issuance::{IssueOutcome, UnitIssuanceRecord, UnitIssuanceRepository};
pub use ledger::{CompletionKind, Ledger, LedgerBalance, LedgerError, LedgerTransfer, PendingCompletion};
pub use nav::{NavMarks, Valuation};
pub use operations::OperationFeed;
pub use outflow::{OutflowPolicy, PayoutStanding};
pub use payments::{PaymentFeed, PaymentRepository};
pub use positions::{FundPosition, FundPositionReader, UnitFlow};
pub use redemptions::{QueuedRedemption, RedemptionRepository};
pub use scope_ledger::ScopeLedgerStore;
pub use subscriptions::SubscriptionRepository;
pub use withdrawals::WithdrawalRepository;

/// Persistence + read port for the [`User`] aggregate.
#[async_trait]
pub trait UserRepository: Repository<Aggregate = User> + Reader<Aggregate = User> {
	/// Find a user by hub id.
	async fn find_by_id(&self, id: UserId) -> Result<Option<User>, DomainError>;

	/// Upsert by the immutable [`AuthSubject`] at sign-in: create (raising
	/// `Provisioned`) or refresh the email (raising `EmailChanged`), atomically with
	/// the event log. Idempotent for concurrent first-logins. Returns the current
	/// aggregate.
	async fn provision(&self, subject: AuthSubject, email: Email, email_verified: bool) -> Result<User, DomainError>;

	/// Resolve the bridge-mirrored issuance slice by the user's CONCIERGE id (the handle the
	/// BFF carries from sign-in). `None` if no local mirror exists yet (the bridge `CREATED`
	/// has not been consumed). Read-only.
	async fn resolve_issuance_by_concierge_id(&self, concierge_id: ConciergeUserId) -> Result<Option<IssuanceTarget>, DomainError>;

	/// Resolve the same issuance slice by the hub user id — the refresh-time re-check.
	async fn resolve_issuance_by_banking_id(&self, banking_id: UserId) -> Result<Option<IssuanceTarget>, DomainError>;

	/// Apply a self-service profile update under the row lock — load `FOR UPDATE`,
	/// apply [`User::update_profile`], persist + drain events in one transaction, so
	/// a stale read can never overwrite a concurrent admin transition. Returns the
	/// updated aggregate.
	async fn update_profile(&self, id: UserId, fields: ProfileFields) -> Result<User, DomainError>;

	/// Bump the user's `token_version` (invalidate all outstanding tokens) under the
	/// row lock. Returns the updated aggregate (carrying the new version).
	async fn revoke_tokens(&self, id: UserId) -> Result<User, DomainError>;

	/// Disable the account under the row lock (idempotent).
	async fn disable(&self, id: UserId) -> Result<User, DomainError>;
}
/// The minimal slice needed to mint a money-plane token for a user — and, re-read per RPC,
/// the money-path gate (`services::support::unfrozen_caller`) and the operator gate
/// (`require_permission`): the same two facts decide whether an already-minted token may
/// still move money. The money plane never sees Google's `sub`. Each field FOLDS the two
/// revoke surfaces so EITHER invalidates a money token: the cross-plane bridge columns
/// (`frozen` from a concierge SUSPENDED, `concierge_token_version` from a SESSIONS_REVOKED)
/// AND banking's own aggregate columns (`status='disabled'`, `token_version` from a
/// banking-side "revoke all"). Read by raw SQL, not via the `User` aggregate (which
/// deliberately doesn't model the bridge columns).
pub struct IssuanceTarget {
	/// The hub user id — stamped as the minted token's `sub`.
	pub user_id: UserId,
	pub email: String,
	/// True when the user must be refused a money token — a concierge SUSPENDED (`frozen`) OR a
	/// banking-side disable (`status='disabled'`). Gates issuance, refresh and every
	/// user-initiated money RPC.
	pub disabled: bool,
	/// The effective revoke floor: the GREATER of concierge's revoke version (SESSIONS_REVOKED)
	/// and banking's own `token_version`. Minted as the token's `token_version` and re-checked on
	/// refresh AND on every money-moving RPC, so EITHER a concierge or a banking "revoke all"
	/// invalidates the money family within one request, not one access-token TTL.
	pub token_version: u64,
}
