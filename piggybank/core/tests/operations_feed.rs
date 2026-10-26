//! Operations feed — `PgOperationFeed::list_by_user` merges the money projections
//! (deposits, withdrawals) into one time-ordered timeline.
//! Real Postgres (no DB mocks) — runs when `DATABASE_URL` is set and skips otherwise.
//! No TigerBeetle: every write here commits its Postgres row and its outbox event and
//! stops there, which is exactly the state the read model projects from.
//!
//! What's proven: the kinds genuinely interleave by time rather than concatenating per
//! source (the bug the wallet's client-side merge could not avoid — it had no timestamp to
//! sort withdrawals on); another user's activity never leaks in; each kind narrows back to
//! the right variant with its fields intact; and the page cap reports `truncated` honestly.

use domain::{
	money::{Network, TxRef, Usdt, WalletAddress},
	users::UserId,
	withdrawals::{Withdrawal, WithdrawalId, WithdrawalSource, WithdrawalState},
};
use piggybank_core::{
	infrastructure::{deposits::PgDeposits, operation_feed::PgOperationFeed, withdrawals::PgWithdrawals},
	ports::{
		Deposits, WithdrawalRepository,
		operations::{MAX_PAGE, Operation, OperationFeed},
	},
};
use sqlx::PgPool;
use uuid::Uuid;

mod common;

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn unique_tx_ref() -> TxRef {
	TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap()
}

/// A BEP20 address is a plain `0x…` — enough to satisfy `WalletAddress::parse` without
/// dragging a fixture chain into a read-model test.
fn address() -> WalletAddress {
	WalletAddress::parse(Network::Bep20, "0x000102030405060708090a0b0c0d0e0f10111213").unwrap()
}

/// `created_at` defaults to the insert's transaction time, so rows written in a row
/// can land inside the same clock tick. Ageing each one by a known offset is what makes
/// "newest first" an assertion about the merge rather than about machine speed. Written
/// out per table rather than built from a format string — sqlx only accepts static SQL,
/// and a test is no place to start interpolating identifiers.
async fn age_deposit(pool: &PgPool, tx_ref: &TxRef, minutes: i64) {
	sqlx::query("UPDATE deposits SET created_at = now() - make_interval(mins => $1) WHERE tx_ref = $2")
		.bind(i32::try_from(minutes).unwrap())
		.bind(tx_ref.as_str())
		.execute(pool)
		.await
		.expect("age the deposit");
}

async fn age_withdrawal(pool: &PgPool, id: WithdrawalId, minutes: i64) {
	sqlx::query("UPDATE withdrawals SET created_at = now() - make_interval(mins => $1) WHERE id = $2")
		.bind(i32::try_from(minutes).unwrap())
		.bind(id.raw())
		.execute(pool)
		.await
		.expect("age the withdrawal");
}

/// Two of each kind for `user`, aged so the expected newest-first order is:
/// withdrawal (1m) → deposit (2m) → withdrawal (3m) → deposit (4m).
async fn seed_interleaved(pool: &PgPool, user: UserId) -> (TxRef, WithdrawalId) {
	let deposits = PgDeposits::new(pool.clone());
	let withdrawals = PgWithdrawals::new(pool.clone());

	let oldest_deposit = unique_tx_ref();
	assert!(deposits.record(oldest_deposit.clone(), user, Network::Ton, usdt("125.5")).await.expect("record deposit"));
	age_deposit(pool, &oldest_deposit, 4).await;
	let newer_deposit = unique_tx_ref();
	assert!(deposits.record(newer_deposit.clone(), user, Network::Bep20, usdt("10")).await.expect("record deposit"));
	age_deposit(pool, &newer_deposit, 2).await;

	let mut older = Withdrawal::request(WithdrawalId::new(), WithdrawalSource::User(user), Network::Bep20, address(), usdt("20"), usdt("1")).expect("build withdrawal");
	withdrawals.open(&mut older, &common::NoSpendCheck).await.expect("open withdrawal");
	age_withdrawal(pool, older.id(), 3).await;
	let mut newest = Withdrawal::request(WithdrawalId::new(), WithdrawalSource::User(user), Network::Bep20, address(), usdt("50"), usdt("1")).expect("build withdrawal");
	withdrawals.open(&mut newest, &common::NoSpendCheck).await.expect("open withdrawal");
	age_withdrawal(pool, newest.id(), 1).await;

	(oldest_deposit, newest.id())
}

#[tokio::test]
async fn merges_the_kinds_newest_first_and_only_for_the_caller() {
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping operations-feed test");
		return;
	};
	let feed = PgOperationFeed::new(pool.clone());
	let user = UserId::new();
	let other = UserId::new();

	let (tx_ref, withdrawal_id) = seed_interleaved(&pool, user).await;
	// Another user's full history must stay invisible.
	seed_interleaved(&pool, other).await;

	let page = feed.list_by_user(user, MAX_PAGE).await.expect("list");
	assert_eq!(page.operations.len(), 4, "exactly the caller's four operations");
	assert!(!page.truncated, "four rows are not a truncated page");

	assert_eq!(
		page.operations.iter().map(Operation::kind).collect::<Vec<_>>(),
		vec!["withdrawal", "deposit", "withdrawal", "deposit"],
		"the merge interleaves by time across sources, it does not concatenate them"
	);
	let timestamps: Vec<i64> = page.operations.iter().map(Operation::created_at).collect();
	assert!(timestamps.windows(2).all(|w| w[0] >= w[1]), "newest first: {timestamps:?}");
	assert!(timestamps.iter().all(|t| *t > 0), "created_at is unix seconds");

	// Each variant narrows back with the fields its kind carries.
	match &page.operations[0] {
		Operation::Withdrawal {
			id,
			network,
			address: dest,
			amount,
			fee,
			state,
			tx_ref,
			..
		} => {
			assert_eq!(*id, withdrawal_id.raw());
			assert_eq!(*network, Network::Bep20);
			assert_eq!(dest, address().as_str(), "the destination is passed through as stored");
			assert_eq!(*amount, usdt("50"));
			assert_eq!(*fee, usdt("1"));
			assert_eq!(*state, WithdrawalState::Queued);
			assert!(tx_ref.is_none(), "a queued withdrawal has not been broadcast");
		}
		other => panic!("expected a withdrawal, got {}", other.kind()),
	}
	match &page.operations[3] {
		Operation::Deposit {
			tx_ref: listed, network, amount, ..
		} => {
			assert_eq!(listed.as_str(), tx_ref.as_str());
			assert_eq!(*network, Network::Ton);
			assert_eq!(*amount, usdt("125.5"), "the amount round-trips through Usdt");
		}
		other => panic!("expected a deposit, got {}", other.kind()),
	}
}

#[tokio::test]
async fn a_short_page_reports_truncation_and_keeps_the_newest_rows() {
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping operations-feed paging test");
		return;
	};
	let feed = PgOperationFeed::new(pool.clone());
	let user = UserId::new();
	seed_interleaved(&pool, user).await;

	// The limit applies to the MERGED stream, which is the whole reason the join lives in
	// the hub: two rows means the two newest overall, not the two newest of one source.
	let page = feed.list_by_user(user, 2).await.expect("list a short page");
	assert_eq!(page.operations.len(), 2, "the page is capped at the requested limit");
	assert!(page.truncated, "there are more operations than the page returned");
	assert_eq!(page.operations.iter().map(Operation::kind).collect::<Vec<_>>(), vec!["withdrawal", "deposit"]);

	// A limit that exactly covers the history is not truncated — the over-fetch probe
	// must not report a phantom extra row.
	let exact = feed.list_by_user(user, 4).await.expect("list the exact page");
	assert_eq!(exact.operations.len(), 4);
	assert!(!exact.truncated, "a page that covers the whole history is not truncated");
}

#[tokio::test]
async fn a_user_with_no_activity_reads_as_an_empty_page() {
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping operations-feed empty test");
		return;
	};
	let feed = PgOperationFeed::new(pool.clone());

	let page = feed.list_by_user(UserId::new(), MAX_PAGE).await.expect("list");
	assert!(page.operations.is_empty(), "a fresh user has no operations");
	assert!(!page.truncated, "an empty page is never truncated");
}
