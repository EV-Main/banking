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
//! [`balance`], [`funds`], and [`withdrawals`] are the money use cases — each notifies
//! the relay after its commit so the ledger move follows promptly. [`users`] holds the
//! account commands (profile / revoke / disable) shared by the service and the auth task.
//! [`operations`] is the read-only activity timeline — a query use case over the
//! projections the money commands already write, joining nothing new.
//! [`allocations`] is the pure control-plane registry of investable products — it moves
//! no money, and its `require_subscribable` gate is what every subscribe runs before one
//! does.
//! [`payments`] is the order that moves money between two named ends of the platform —
//! authorized by the owners' [`consilium`] when the source is fund-owned, by the investor's
//! own emailed consent when it is theirs — and the execution that turns an approved order
//! into a withdrawal or one settled transfer.
//! [`fees`] assesses a fund's management + performance terms against a holding. It is the
//! one money use case that never moves cash while charging: a fee is collected by moving
//! *units* between holders, so it costs no chain fee and cannot touch an investor's cash
//! claim. Only the periodic bulk settlement of accumulated fee units crosses into cash.
//! [`issuance`] is the operator's in-kind mint — units to an investor or to the fee allocation
//! with no cash leg — the one supply path beside a subscription, run through the same
//! registry, NAV and cap gates.
//! [`book`] is the secondary market: holders trading an allocation's units with each
//! other. It gates on the registry's access axis but not its lifecycle, escrows what an
//! order commits through the relay, and settles each fill delivery-versus-payment.
//! [`ownership`] is the read of who holds an allocation — its claim, its supply and its
//! holders, summed from the ledger — behind the cap table, the treasury and the
//! reconciliation's "every unit at a holder" checks.
//! [`scope_ledger`] is what a scope's own service may do with money in its scope — open an
//! account, read its balances, sweep fees into the scope's collector — held against banking's
//! grants for the client the transport proved. [`scope_operations`] adds its payouts and the
//! operations that wait on concierge's approval, with the executor that carries them out.

pub mod allocations;
pub mod auth_sync;
pub mod balance;
pub mod book;
pub mod compartments;
pub mod consilium;
pub(crate) mod credentials;
pub mod fees;
pub mod funds;
pub mod issuance;
pub mod operations;
pub mod ownership;
pub mod payments;
pub mod scope_ledger;
pub mod scope_operations;
pub mod users;
pub mod wallet;
pub mod withdrawals;
