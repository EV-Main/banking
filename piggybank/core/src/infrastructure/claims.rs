//! The control plane's half of every spendable answer — the debits of a claim recorded
//! and not yet on the ledger — and [`TxReads`], the [`LockedReads`] a repository hands its
//! spend check on the connection that holds the claim's lock.

use async_trait::async_trait;
use domain::{
	architecture::DomainEvent,
	balance::LedgerAccountKey,
	scope_ledger::ScopeLedgerEvent,
	scope_operations::ScopePayoutEvent,
	scopes::{Compartment, CompartmentEvent},
	users::UserId,
};
use sqlx::PgConnection;
use tracing::warn;

use crate::{
	infrastructure::{compartments, ledger, outbox::OutboxRow, relay, scope_ledger},
	ports::ledger::{LedgerError, LockedReads},
};

/// [`LockedReads`] over the connection that holds a claim's lock.
pub struct TxReads<'c> {
	conn: &'c mut PgConnection,
}

impl<'c> TxReads<'c> {
	pub fn new(conn: &'c mut PgConnection) -> Self {
		Self { conn }
	}
}

#[async_trait]
impl LockedReads for TxReads<'_> {
	async fn unapplied_debits(&mut self, key: &LedgerAccountKey) -> Result<u128, LedgerError> {
		unapplied_debits(self.conn, key).await
	}

	async fn account_id(&mut self, key: &LedgerAccountKey) -> Result<Option<u128>, LedgerError> {
		ledger::account_id(self.conn, key).await
	}
}

/// The user whose id every outbox payload debiting `key` names, so the scan can skip the
/// rest of the backlog; `None` scans all of it.
fn owner(key: &LedgerAccountKey) -> Option<UserId> {
	match key {
		LedgerAccountKey::UserClaim(user)
		| LedgerAccountKey::ScopeClaim(user, _)
		| LedgerAccountKey::BookCash(user)
		| LedgerAccountKey::UserShares(_, user)
		| LedgerAccountKey::BookShares(_, user) => Some(*user),
		LedgerAccountKey::CryptoWallet(_)
		| LedgerAccountKey::ServiceClaim(_)
		| LedgerAccountKey::WithdrawalClearing
		| LedgerAccountKey::BankCustody
		| LedgerAccountKey::SharesOutstanding(_)
		| LedgerAccountKey::FeeShares(_)
		| LedgerAccountKey::ScopeAccount(..)
		| LedgerAccountKey::HoldClearing => None,
	}
}

/// Moves, scope transfers and payouts are counted from their own tables, not the outbox: a
/// `pending` one whose event a relay dispatched without posting (the reaper's case) is still
/// promised away.
const UNAPPLIED_EVENTS: &str = "SELECT seq, event_id, aggregate, aggregate_id, kind, payload::text AS payload, attempts FROM outbox \
	 WHERE dispatched_at IS NULL AND parked_at IS NULL AND kind <> ALL($1) AND ($2::text IS NULL OR strpos(payload::text, $2) > 0)";

/// See [`LockedReads::unapplied_debits`]. A parked event is left out: the relay will not
/// apply it on its own, and counting it would hold the owner's money for an operator's
/// decision; should it be unparked, the ledger's non-negative flag still refuses it.
pub async fn unapplied_debits(conn: &mut PgConnection, key: &LedgerAccountKey) -> Result<u128, LedgerError> {
	let recorded = match key {
		LedgerAccountKey::UserClaim(user) => Some(compartments::pending_out(&mut *conn, *user, &Compartment::Main).await),
		LedgerAccountKey::ScopeClaim(user, scope) => Some(compartments::pending_out(&mut *conn, *user, &Compartment::Scope(scope.clone())).await),
		LedgerAccountKey::ScopeAccount(scope, label) => Some(scope_ledger::pending_out(&mut *conn, scope, label).await),
		_ => None,
	}
	.transpose()
	.map_err(|e| LedgerError::Unavailable(format!("pending moves and transfers of {}: {e}", key.logical_key())))?
	.map_or(0, |pending| pending.base_units());
	let rows = sqlx::query_as::<_, OutboxRow>(UNAPPLIED_EVENTS)
		.bind([CompartmentEvent::KIND, ScopeLedgerEvent::KIND, ScopePayoutEvent::KIND].as_slice())
		.bind(owner(key).map(|user| user.to_string()))
		.fetch_all(&mut *conn)
		.await
		.map_err(|e| LedgerError::Unavailable(format!("undispatched outbox read for {}: {e}", key.logical_key())))?;
	rows.iter().try_fold(recorded, |total, row| {
		let debit = match relay::planned_debit(row, key) {
			Ok(debit) => debit,
			// The relay parks an event it cannot plan, so it never reaches the ledger;
			// refusing every spend of the claim until then would be the wrong failure.
			Err(reason) => {
				warn!(seq = row.seq, kind = %row.kind, "unapplied debits: an outbox event cannot be planned, not counted: {reason}");
				0
			}
		};
		total
			.checked_add(debit)
			.ok_or_else(|| LedgerError::Conflict(format!("unapplied debits of {} overflow", key.logical_key())))
	})
}
