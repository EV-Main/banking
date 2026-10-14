//! Operations feed — `PgOperationFeed::list_by_user` merges the four money projections
//! (deposits, withdrawals, subscriptions, redemptions) into one time-ordered timeline.
//! Real Postgres (no DB mocks) — runs when `DATABASE_URL` is set and skips otherwise.
//! No TigerBeetle: every write here commits its Postgres row and its outbox event and
//! stops there, which is exactly the state the read model projects from.
//!
//! What's proven: the four kinds genuinely interleave by time rather than concatenating
//! per source (the bug the wallet's client-side merge could not avoid — it had no
//! timestamp to sort withdrawals on); another user's activity never leaks in; each kind
//! narrows back to the right variant with its fields intact; an unsettled redemption
//! keeps its NAV and cash absent; and the page cap reports `truncated` honestly.

use domain::{
	balance::ServiceId,
	money::{Nav, Network, Shares, TxRef, Usdt, WalletAddress},
	redemptions::{Redemption, RedemptionId, RedemptionState},
	subscriptions::{Subscription, SubscriptionId},
	users::UserId,
	withdrawals::{Withdrawal, WithdrawalId, WithdrawalSource, WithdrawalState},
};
use piggybank_core::{
	infrastructure::{deposits::PgDeposits, operation_feed::PgOperationFeed, redemptions::PgRedemptions, subscriptions::PgSubscriptions, withdrawals::PgWithdrawals},
	ports::{
		Deposits, RedemptionRepository, SubscriptionRepository, WithdrawalRepository,
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

fn service() -> ServiceId {
	ServiceId::parse("itest-fund").unwrap()
}

/// A BEP20 address is a plain `0x…` — enough to satisfy `WalletAddress::parse` without
/// dragging a fixture chain into a read-model test.
fn address() -> WalletAddress {
	WalletAddress::parse(Network::Bep20, "0x000102030405060708090a0b0c0d0e0f10111213").unwrap()
}

/// `created_at` defaults to the insert's transaction time, so four rows written in a row
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

async fn age_subscription(pool: &PgPool, id: SubscriptionId, minutes: i64) {
	sqlx::query("UPDATE subscriptions SET created_at = now() - make_interval(mins => $1) WHERE id = $2")
		.bind(i32::try_from(minutes).unwrap())
		.bind(id.raw())
		.execute(pool)
		.await
		.expect("age the subscription");
}

async fn age_redemption(pool: &PgPool, id: RedemptionId, minutes: i64) {
	sqlx::query("UPDATE redemptions SET created_at = now() - make_interval(mins => $1) WHERE id = $2")
		.bind(i32::try_from(minutes).unwrap())
		.bind(id.raw())
		.execute(pool)
		.await
		.expect("age the redemption");
}

/// One of each kind for `user`, aged so the expected newest-first order is:
/// withdrawal (1m) → redemption (2m) → subscription (3m) → deposit (4m).
async fn seed_one_of_each(pool: &PgPool, user: UserId) -> (TxRef, WithdrawalId, SubscriptionId, RedemptionId) {
	let deposits = PgDeposits::new(pool.clone());
	let withdrawals = PgWithdrawals::new(pool.clone());
	let subscriptions = PgSubscriptions::new(pool.clone());
	let redemptions = PgRedemptions::new(pool.clone());

	let tx_ref = unique_tx_ref();
	assert!(deposits.record(tx_ref.clone(), user, Network::Ton, usdt("125.5")).await.expect("record deposit"));
	age_deposit(pool, &tx_ref, 4).await;

	let mut withdrawal = Withdrawal::request(WithdrawalId::new(), WithdrawalSource::User(user), Network::Bep20, address(), usdt("50"), usdt("1")).expect("build withdrawal");
	withdrawals.open(&mut withdrawal, &piggybank_core::ports::ledger::NoSpendCheck).await.expect("open withdrawal");
	age_withdrawal(pool, withdrawal.id(), 1).await;

	let mut subscription = Subscription::open(SubscriptionId::new(), user, service(), usdt("200"), Nav::SEED).expect("build subscription");
	subscriptions
		.open(&mut subscription, &piggybank_core::ports::ledger::NoSpendCheck)
		.await
		.expect("open subscription");
	age_subscription(pool, subscription.id(), 3).await;

	let mut redemption = Redemption::request(RedemptionId::new(), user, service(), Shares::parse_decimal("10").unwrap()).expect("build redemption");
	redemptions.open(&mut redemption).await.expect("open redemption");
	age_redemption(pool, redemption.id(), 2).await;

	(tx_ref, withdrawal.id(), subscription.id(), redemption.id())
}

#[tokio::test]
async fn merges_all_four_kinds_newest_first_and_only_for_the_caller() {
	let Some(pool) = common::pool().await else {
		eprintln!("DATABASE_URL unset — skipping operations-feed test");
		return;
	};
	let feed = PgOperationFeed::new(pool.clone());
	let user = UserId::new();
	let other = UserId::new();

	let (tx_ref, withdrawal_id, subscription_id, redemption_id) = seed_one_of_each(&pool, user).await;
	// Another user's full history must stay invisible. (The seed's historical `piggybank`
	// deposits are pinned over a pre-0047 schema in `ownership_contract`.)
	seed_one_of_each(&pool, other).await;

	let page = feed.list_by_user(user, MAX_PAGE).await.expect("list");
	assert_eq!(page.operations.len(), 4, "exactly the caller's four operations");
	assert!(!page.truncated, "four rows are not a truncated page");

	assert_eq!(
		page.operations.iter().map(Operation::kind).collect::<Vec<_>>(),
		vec!["withdrawal", "redemption", "subscription", "deposit"],
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
	match &page.operations[1] {
		Operation::Redemption {
			id,
			service: svc,
			units,
			nav,
			cash,
			state,
			..
		} => {
			assert_eq!(*id, redemption_id.raw());
			assert_eq!(svc.as_str(), service().as_str());
			assert_eq!(*units, Shares::parse_decimal("10").unwrap());
			assert_eq!(*state, RedemptionState::Queued);
			assert!(nav.is_none() && cash.is_none(), "a queued redemption is priced at settle, not at request");
		}
		other => panic!("expected a redemption, got {}", other.kind()),
	}
	match &page.operations[2] {
		Operation::Subscription {
			id, service: svc, cash, nav, units, ..
		} => {
			assert_eq!(*id, subscription_id.raw());
			assert_eq!(svc.as_str(), service().as_str());
			assert_eq!(*cash, usdt("200"));
			assert_eq!(*nav, Nav::SEED);
			assert_eq!(*units, Shares::parse_decimal("200").unwrap(), "200 cash at a NAV of 1.0 mints 200 units");
		}
		other => panic!("expected a subscription, got {}", other.kind()),
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
	seed_one_of_each(&pool, user).await;

	// The limit applies to the MERGED stream, which is the whole reason the join lives in
	// the hub: two rows means the two newest overall, not the two newest of one source.
	let page = feed.list_by_user(user, 2).await.expect("list a short page");
	assert_eq!(page.operations.len(), 2, "the page is capped at the requested limit");
	assert!(page.truncated, "there are more operations than the page returned");
	assert_eq!(page.operations.iter().map(Operation::kind).collect::<Vec<_>>(), vec!["withdrawal", "redemption"]);

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
