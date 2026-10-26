//! Application layer — use cases, split CQRS-style.
//!
//! - **Command handlers** (write side) build an aggregate and hand it to a single
//!   repository method; that method is its own atomic unit (see [`crate::ports`]) —
//!   it opens the Postgres transaction, mutates the aggregate, and drains its
//!   `EmitsEvents` into the event log in that same transaction. Money is moved in
//!   TigerBeetle **after** the Postgres commit (Write-Last), via the outbox
//!   relay/sagas. The transaction boundary lives in the adapter, not here: there is
//!   no application-layer `UnitOfWork`, because cross-boundary moves are TB sagas
//!   (money written last), never multi-aggregate Postgres transactions.
//! - **Query handlers** (read side) depend only on the narrow `Reader` ports and
//!   read Postgres projections; authoritative balances come from TigerBeetle.
//!
//! Both depend on the ports in [`crate::ports`], never on concrete adapters.
//!
//! [`auth_sync`] is the first use case: it drains the in-process `Provisioner`
//! channel from the auth task and resolves the user the auth task is minting a
//! money-plane token for (users themselves are mirrored from concierge by the
//! one-way bridge, not provisioned here).
//! [`balance`] and [`withdrawals`] are the money use cases — each notifies the relay after
//! its commit so the ledger move follows promptly. [`users`] holds the account commands
//! (profile / revoke / disable) shared by the service and the auth task.
//! [`operations`] is the read-only activity timeline — a query use case over the
//! projections the money commands already write, joining nothing new.
//! [`payments`] is the order that moves money between two named ends of the platform —
//! authorized by the owners' concierge approval when the source is banking's treasury, by
//! the investor's own answer in the wallet host when it is theirs — and the execution that
//! turns an approved order into a withdrawal or one settled transfer.
//! [`scope_ledger`] is what a scope's own service may do with money in its scope — open an
//! account, read its balances, sweep fees into the scope's collector — held against banking's
//! grants for the client the transport proved. [`scope_operations`] adds its payouts and the
//! operations that wait on concierge's approval, with the executor that carries them out.

pub mod auth_sync;
pub mod balance;
pub mod compartments;
pub mod operations;
pub mod payment_approvals;
pub mod payments;
pub mod scope_charges;
pub mod scope_invoices;
pub mod scope_ledger;
pub mod scope_mandates;
pub mod scope_operations;
pub mod users;
pub mod wallet;
pub mod withdrawal_limits;
pub mod withdrawals;
