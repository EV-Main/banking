//! The treasury view — real Postgres **and** TigerBeetle (no mocks). Runs when
//! `DATABASE_URL` is set and a TigerBeetle replica is reachable (`nix run .#db` + `.#tb`),
//! and skips otherwise.
//!
//! What people hold is the sum of their claims, read off the ledger — never a remainder of
//! the custody — and banking's own treasury (D17) is a line of its own. Every test reads
//! platform-wide sums, so the suite runs serially under the shared outbox guard, and each
//! test brackets its own effect against what it read before — never an absolute.

use std::sync::Arc;

use domain::{
	balance::{LedgerAccountKey, Party, TransferCode},
	money::{Network, TxRef, Usdt},
	users::UserId,
};
use piggybank_core::{
	application::balance as balance_app,
	infrastructure::{custody::StubCustody, deposits::PgDeposits, relay::Relay},
	ports::ledger::{Ledger, LedgerTransfer},
};
use sqlx::PgPool;
use tokio::sync::{MutexGuard, Notify};
use uuid::Uuid;

mod common;

struct Harness {
	pool: PgPool,
	deposits: PgDeposits,
	ledger: Arc<dyn Ledger>,
	relay: Relay,
	notify: Arc<Notify>,
	/// Held for the test's whole life — declared last so it is released after the relay.
	_serial: MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "treasury test").await?;
	let notify = Arc::new(Notify::new());
	Some(Harness {
		deposits: PgDeposits::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		ledger,
		notify,
		pool,
		_serial: serial,
	})
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

async fn treasury(h: &Harness) -> balance_app::Treasury {
	balance_app::treasury(h.ledger.as_ref(), &StubCustody).await.unwrap()
}

/// A person's chain deposit, through the relay.
async fn deposit(h: &Harness, user: UserId, amount: &str) {
	let tx_ref = TxRef::parse(&format!("trs-{}", Uuid::new_v4())).unwrap();
	balance_app::record_deposit(&h.deposits, &h.notify, tx_ref, user, Network::Bep20, usdt(amount)).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

/// New custody on banking's treasury — no deposit may credit it, so the `Dr wallet / Cr
/// treasury` is posted straight to the ledger, standing in for retained withdrawal fees.
async fn fund_treasury(h: &Harness, amount: &str) {
	let transfer = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
		credit: Party::Treasury.claim_key(),
		amount: usdt(amount).base_units(),
		code: TransferCode::Deposit,
		reference: 0,
	};
	h.ledger.post(&transfer).await.unwrap();
}

/// `held_by_users` is the sum of user claims, not a remainder: a deposit raises it by
/// exactly the deposit, and cash landing on banking's treasury raises custody, not what
/// people hold.
#[tokio::test]
async fn held_by_users_is_the_sum_of_user_claims_and_not_a_remainder() {
	let Some(h) = harness().await else { return };
	let before = treasury(&h).await;

	deposit(&h, UserId::new(), "100").await;
	let deposited = treasury(&h).await;
	assert_eq!(deposited.held_by_users, before.held_by_users.checked_add(usdt("100")).unwrap());
	assert_eq!(deposited.total_custody, before.total_custody.checked_add(usdt("100")).unwrap());

	fund_treasury(&h, "5").await;
	let funded = treasury(&h).await;
	assert_eq!(funded.held_by_users, deposited.held_by_users, "banking's treasury is not people's");
	assert_eq!(funded.total_custody, deposited.total_custody.checked_add(usdt("5")).unwrap());

	// The figure is the same sum the reconciliation attributes to people.
	let cash = h.ledger.cash_invariant().await.unwrap();
	assert_eq!(funded.held_by_users, Usdt::from_base_units(cash.user_claims));
	assert_eq!(cash.custody, cash.claims, "conservation holds through all of it");
	assert_eq!(cash.unclassified(), 0);
}

/// banking's own treasury (D17) is a line of its own, not people's — the withdrawal fees it
/// keeps, with their own posted / reserved / available split.
#[tokio::test]
async fn the_treasury_shows_banking_s_own_account_on_a_line_of_its_own() {
	let Some(h) = harness().await else { return };
	let before = treasury(&h).await;
	fund_treasury(&h, "7").await;
	let after = treasury(&h).await;
	assert_eq!(after.banking_treasury.posted, before.banking_treasury.posted.checked_add(usdt("7")).unwrap());
	assert_eq!(after.banking_treasury.available, before.banking_treasury.available.checked_add(usdt("7")).unwrap());
	assert_eq!(after.held_by_users, before.held_by_users, "banking's money is not people's");
	assert_eq!(after.held_in_compartments, before.held_in_compartments);
	let cash = h.ledger.cash_invariant().await.unwrap();
	assert_eq!(cash.custody, cash.claims);
	assert_eq!(cash.unclassified(), 0, "the treasury is a named part of the claims");
}
