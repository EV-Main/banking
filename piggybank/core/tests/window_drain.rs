//! `window-tool drain` against a real book — real Postgres **and** TigerBeetle (no mocks).
//! Skips without `DATABASE_URL` and a reachable replica, fails under CI.
//!
//! Pinned: resting orders of both sides are cancelled and their escrow comes back through the
//! relay (the units stay the holder's throughout, so the anchor does not move), the verdict
//! stays "not drained" until the relay has applied the releases, and a dry run cancels nothing.

use std::{sync::Arc, time::Duration};

use domain::{
	allocations::{Allocation, AllocationAccess, AllocationIcon, AllocationId},
	auth::AuthSubject,
	balance::{LedgerAccountKey, ServiceId},
	book::{BookPolicy, ClientOrderId, OrderKind, PriceTimeEngine, Side, Tif},
	issuance::{IdempotencyKey, UnitHolder},
	money::{Network, Shares, TxRef, Usdt},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{
		balance as balance_app,
		book::{self as book_app, BookFeed, BookPorts, PlaceOrderRequest},
		funds as funds_app, issuance as issuance_app,
	},
	infrastructure::{
		allocations::PgAllocations,
		book::PgBook,
		custody::StubCustody,
		deposits::PgDeposits,
		issuance::PgUnitIssuances,
		nav::PgNav,
		operations,
		outflow::PgOutflowPolicy,
		relay::Relay,
		users::PgUsers,
		window_tool::{Anchor, Mode, Window, drain},
	},
	ports::{AllocationRegistry, UserRepository, ledger::Ledger},
};
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

const UNIT: u128 = 1_000_000_000_000_000_000;

fn now_unix() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

#[tokio::test]
async fn drain_cancels_resting_orders_of_both_sides_and_waits_for_the_relay() {
	let _serial = common::outbox_serial().await;
	let Some(pool) = common::pool().await else { return };
	let Some(ledger) = common::seeded_ledger(&pool, "window drain test").await else { return };
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone());
	let (allocations, users, issuances, nav, deposits, book) = (
		PgAllocations::new(pool.clone()),
		PgUsers::new(pool.clone()),
		PgUnitIssuances::new(pool.clone()),
		PgNav::new(pool.clone()),
		PgDeposits::new(pool.clone()),
		PgBook::new(pool.clone()),
	);
	let feed = BookFeed::new();

	let service = ServiceId::parse(&format!("drain-{}", Uuid::new_v4())).unwrap();
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "SA", "drain test", AllocationIcon::Arbitrage).unwrap();
	allocations.register(&mut allocation).await.unwrap();
	allocations.open(&service).await.unwrap();
	allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	let policy = BookPolicy::new(true, 0, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, true).unwrap();
	book_app::set_policy(&allocations, &book, &service, policy).await.unwrap();

	let provision = |tag: &'static str| {
		let users = &users;
		async move {
			let subject = AuthSubject::parse(&format!("{tag}-{}", Uuid::new_v4())).unwrap();
			let email = Email::parse(&format!("d{}@example.com", Uuid::new_v4().simple())).unwrap();
			users.provision(subject, email, true).await.unwrap().id()
		}
	};
	let (seller, buyer): (UserId, UserId) = (provision("seller").await, provision("buyer").await);
	let ports = funds_app::FundPorts {
		allocations: &allocations,
		ledger: ledger.as_ref(),
		nav: &nav,
		relay: &notify,
	};
	issuance_app::issue_units(
		&ports,
		&issuances,
		&users,
		issuance_app::IssueUnitsRequest {
			service: service.clone(),
			holder: UnitHolder::User(seller),
			units: Shares::parse_decimal("10").unwrap(),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse(&format!("issue-{}", Uuid::new_v4())).unwrap(),
		},
		now_unix(),
	)
	.await
	.unwrap();
	balance_app::record_deposit(
		&deposits,
		&notify,
		TxRef::parse(&format!("drain-{}", Uuid::new_v4())).unwrap(),
		buyer,
		Network::Bep20,
		Usdt::parse_decimal("100").unwrap(),
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&relay, &pool).await;

	let outflow = PgOutflowPolicy::new(pool.clone());
	let book_ports = BookPorts {
		allocations: &allocations,
		ledger: ledger.as_ref(),
		nav: &nav,
		store: &book,
		engine: &PriceTimeEngine,
		outflow: &outflow,
		relay: &notify,
		feed: &feed,
	};
	for (user, side, at, size) in [(seller, Side::Sell, "5", "10"), (buyer, Side::Buy, "1", "20")] {
		book_app::place_order(
			&book_ports,
			user,
			PlaceOrderRequest {
				service: service.clone(),
				side,
				kind: OrderKind::Limit,
				tif: Tif::Gtc,
				price: Some(domain::book::Price::parse_decimal(at).unwrap()),
				size: Shares::parse_decimal(size).unwrap(),
				client_order_id: ClientOrderId::parse(&format!("c-{}", Uuid::new_v4())).unwrap(),
			},
		)
		.await
		.unwrap();
	}
	common::drain_to_quiescence(&relay, &pool).await;
	let units = |key: LedgerAccountKey| {
		let ledger = ledger.clone();
		async move { ledger.balance(&key).await.unwrap().posted }
	};
	assert_eq!(
		units(LedgerAccountKey::BookShares(service.clone(), seller)).await,
		10 * UNIT,
		"the sell rests, its units in escrow"
	);
	let resting: i64 = sqlx::query_scalar("SELECT count(*) FROM book_orders WHERE service = $1 AND state IN ('open', 'partially_filled')")
		.bind(service.as_str())
		.fetch_one(&pool)
		.await
		.unwrap();
	assert_eq!(resting, 2);

	operations::set_read_only(&pool, true).await.unwrap();
	let window = Window {
		pool: pool.clone(),
		ledger: ledger.clone() as Arc<dyn Ledger>,
		anchor: Anchor::new(service.clone(), vec![10 * UNIT]),
	};
	let options = drain::Options {
		release_queued_redemptions: false,
		wait: Duration::ZERO,
	};

	let dry = drain::run(&window, Mode::DryRun, options).await.unwrap();
	assert!(!dry.ok && dry.text.contains("cancel 2 open order(s)") && dry.text.contains("dry run"), "{}", dry.text);
	let still: i64 = sqlx::query_scalar("SELECT count(*) FROM book_orders WHERE service = $1 AND state IN ('open', 'partially_filled')")
		.bind(service.as_str())
		.fetch_one(&pool)
		.await
		.unwrap();
	assert_eq!(still, 2, "a dry run cancels nothing");

	// Cancelled, but the escrow's release is in the outbox until a relay applies it.
	let first = drain::run(&window, Mode::Apply, options).await.unwrap();
	assert!(!first.ok && first.text.contains("open orders 0") && first.text.contains("NOT DRAINED"), "{}", first.text);
	common::drain_to_quiescence(&relay, &pool).await;
	let second = drain::run(&window, Mode::Apply, options).await.unwrap();
	assert!(second.ok && second.text.contains("DRAINED") && !second.text.contains("NOT DRAINED"), "{}", second.text);

	assert_eq!(
		units(LedgerAccountKey::UserShares(service.clone(), seller)).await,
		10 * UNIT,
		"every unit is back with its holder"
	);
	assert_eq!(units(LedgerAccountKey::BookShares(service.clone(), seller)).await, 0);
	assert_eq!(units(LedgerAccountKey::BookCash(buyer)).await, 0, "the buyer's cash is out of escrow too");

	operations::set_read_only(&pool, false).await.unwrap();
}
