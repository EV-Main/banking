//! Integration tests for the allocation book — real Postgres **and** TigerBeetle (no
//! mocks). They run when `DATABASE_URL` is set and a TigerBeetle replica is reachable
//! (`nix run .#db` + `.#tb`), and skip otherwise.
//!
//! The load-bearing behaviour is the money: an order is an ESCROW the relay moves, a
//! fill is one linked delivery-versus-payment batch, and whatever the escrow did not
//! spend comes back. Every test here drives the relay deterministically and then reads
//! the ledger, because "the order row says filled" proves nothing about whose units and
//! whose cash they are now. Driving it deterministically takes two things: every test
//! holds `common::outbox_serial()` for its whole life, since the suite's tests share one
//! database and two relays draining it at once pick up the same row; and it drains
//! through `common::drain_to_quiescence`, since one `drain` pass returns early on a
//! transient failure (#294, #437).

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use domain::{
	allocations::{Allocation, AllocationAccess, AllocationBacking, AllocationIcon, AllocationId},
	auth::AuthSubject,
	balance::{LedgerAccountKey, ServiceId},
	book::{BookPolicy, CancelReason, CandleResolution, ClientOrderId, Locked, OrderKind, OrderState, Price, PriceTimeEngine, Side, Tif},
	error::DomainError,
	issuance::{IdempotencyKey, UnitHolder},
	money::{Network, Shares, TxRef, Usdt, WalletAddress},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{
		balance as balance_app,
		book::{self as book_app, BookFeed, BookPorts, PlaceOrderRequest},
		funds as funds_app, issuance as issuance_app,
		wallet::{self as wallet_app, WalletBalance, WalletPorts},
	},
	config::KycGate,
	infrastructure::{
		allocations::PgAllocations, book::PgBook, custody::StubCustody, deposits::PgDeposits, issuance::PgUnitIssuances, nav::PgNav, operations, outflow::PgOutflowPolicy,
		positions::PgFundPositions, relay::Relay, users::PgUsers,
	},
	ports::{AllocationRegistry, BookStore, DepositAddresses, OrderRecord, UserRepository, ledger::Ledger},
};
use sqlx::PgPool;
use tokio::sync::{MutexGuard, Notify};
use uuid::Uuid;

/// The fee allocation's claim (`service:fee`) is one platform-wide account, so a test that
/// brackets it must not interleave with another that credits it. Every test that reads or
/// credits it takes this exclusively.
///
/// Kept, though the outbox fence in [`harness`] now serializes the whole suite and makes it
/// redundant in this process: it states a requirement of its own — this bracket needs
/// exclusivity even if the fence is ever narrowed. What neither lock reaches is another
/// test BINARY: both are one-process `LazyLock`s, while `service:fee` lives in the shared
/// TigerBeetle cluster (see the note on the bracket itself).
static REVENUE: std::sync::LazyLock<tokio::sync::Mutex<()>> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

struct Harness {
	pool: PgPool,
	allocations: PgAllocations,
	users: PgUsers,
	issuances: PgUnitIssuances,
	positions: PgFundPositions,
	nav: PgNav,
	deposits: PgDeposits,
	book: PgBook,
	ledger: Arc<dyn Ledger>,
	relay: Relay,
	notify: Arc<Notify>,
	feed: Arc<BookFeed>,
	/// Held for the test's whole life — declared last so it is released after the relay.
	_serial: MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "book tests").await?;
	let notify = Arc::new(Notify::new());
	Some(Harness {
		allocations: PgAllocations::new(pool.clone()),
		users: PgUsers::new(pool.clone()),
		issuances: PgUnitIssuances::new(pool.clone()),
		positions: PgFundPositions::new(pool.clone()),
		nav: PgNav::new(pool.clone()),
		deposits: PgDeposits::new(pool.clone()),
		book: PgBook::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		ledger,
		notify,
		feed: BookFeed::new(),
		pool,
		_serial: serial,
	})
}

fn fund_ports(h: &Harness) -> funds_app::FundPorts<'_> {
	funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &h.nav,
		relay: &h.notify,
	}
}

fn book_ports<'a>(h: &'a Harness, outflow: &'a PgOutflowPolicy) -> BookPorts<'a> {
	BookPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &h.nav,
		store: &h.book,
		engine: &PriceTimeEngine,
		outflow,
		relay: &h.notify,
		feed: &h.feed,
	}
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn shares(decimal: &str) -> Shares {
	Shares::parse_decimal(decimal).unwrap()
}

fn price(decimal: &str) -> Price {
	Price::parse_decimal(decimal).unwrap()
}

fn unique_service() -> ServiceId {
	ServiceId::parse(&format!("book-{}", Uuid::new_v4())).unwrap()
}

fn now_unix() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// A real `users` row — orders reference the table, as they do in production.
async fn provisioned_user(h: &Harness) -> UserId {
	let subject = AuthSubject::parse(&format!("book-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("b{}@example.com", Uuid::new_v4().simple())).unwrap();
	h.users.provision(subject, email, true).await.unwrap().id()
}

/// A registered, open, invest-to-everyone product whose book is open at `fee_bps`.
async fn tradable_product(h: &Harness, fee_bps: u32) -> ServiceId {
	let service = unique_service();
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "Service Arb", "Arbitrage", AllocationIcon::Arbitrage).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	h.allocations.open(&service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	open_book(h, &service, fee_bps).await;
	service
}

/// Open with unbacked trading acknowledged: every seller here is seeded by `issue_units`,
/// an in-kind mint that flips the product to `in_kind` — the same shape as `service_arb` in
/// production — and an unacknowledged book on such a product takes no order.
async fn open_book(h: &Harness, service: &ServiceId, fee_bps: u32) {
	let policy = BookPolicy::new(true, fee_bps, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, true).unwrap();
	book_app::set_policy(&h.allocations, &h.book, service, policy).await.unwrap();
}

/// Units in kind, applied: the seller's side of every test starts here.
async fn issue_units(h: &Harness, service: &ServiceId, user: UserId, units: &str) {
	issuance_app::issue_units(
		&fund_ports(h),
		&h.issuances,
		&h.users,
		issuance_app::IssueUnitsRequest {
			service: service.clone(),
			holder: UnitHolder::User(user),
			units: shares(units),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse(&format!("issue-{}", Uuid::new_v4())).unwrap(),
		},
		now_unix(),
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

/// Cash on the user's claim: the buyer's side of every test starts here.
async fn fund_user(h: &Harness, user: UserId, amount: &str) {
	let tx_ref = TxRef::parse(&format!("book-{}", Uuid::new_v4())).unwrap();
	balance_app::record_deposit(&h.deposits, &h.notify, tx_ref, user, Network::Bep20, usdt(amount)).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

// The order's own fields, spelled out so each test reads as the order it places.
#[allow(clippy::too_many_arguments)]
async fn place(h: &Harness, user: UserId, service: &ServiceId, side: Side, kind: OrderKind, tif: Tif, at: Option<&str>, size: &str) -> Result<OrderRecord, DomainError> {
	place_keyed(h, user, service, side, kind, tif, at, size, &format!("c-{}", Uuid::new_v4())).await
}

// Same, plus the retry key for the idempotency test.
#[allow(clippy::too_many_arguments)]
async fn place_keyed(h: &Harness, user: UserId, service: &ServiceId, side: Side, kind: OrderKind, tif: Tif, at: Option<&str>, size: &str, key: &str) -> Result<OrderRecord, DomainError> {
	let outflow = PgOutflowPolicy::new(h.pool.clone());
	book_app::place_order(
		&book_ports(h, &outflow),
		user,
		PlaceOrderRequest {
			service: service.clone(),
			side,
			kind,
			tif,
			price: at.map(price),
			size: shares(size),
			client_order_id: ClientOrderId::parse(key).unwrap(),
		},
	)
	.await
}

async fn sell(h: &Harness, user: UserId, service: &ServiceId, at: &str, size: &str) -> OrderRecord {
	place(h, user, service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some(at), size).await.unwrap()
}

async fn buy(h: &Harness, user: UserId, service: &ServiceId, at: &str, size: &str) -> OrderRecord {
	place(h, user, service, Side::Buy, OrderKind::Limit, Tif::Gtc, Some(at), size).await.unwrap()
}

async fn units_of(h: &Harness, key: LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

async fn cash_of(h: &Harness, key: LedgerAccountKey) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

async fn order(h: &Harness, record: &OrderRecord) -> OrderRecord {
	h.book.find_order(record.order.id()).await.unwrap().expect("the order row stands")
}

async fn position(h: &Harness, user: UserId, service: &ServiceId) -> funds_app::PositionView {
	funds_app::get_position(&h.positions, h.ledger.as_ref(), &h.nav, user, service.clone()).await.unwrap()
}

/// No deposit rails at all: the wallet read here is about the lifecycle figures, and with
/// no configured network the address gateway is never asked.
struct NoRails;

impl domain::architecture::Gateway for NoRails {}

#[async_trait]
impl DepositAddresses for NoRails {
	async fn address(&self, _user: UserId, _network: Network) -> Result<Option<WalletAddress>, DomainError> {
		Ok(None)
	}
}

/// The caller's wallet as `GetWallet` presents it — the same use case the RPC runs.
async fn wallet(h: &Harness, user: UserId) -> WalletBalance {
	let ports = WalletPorts {
		ledger: h.ledger.as_ref(),
		positions: &h.positions,
		nav: &h.nav,
		deposit_addresses: &NoRails,
		users: &h.users,
		compartments: &piggybank_core::infrastructure::compartments::PgCompartments::new(h.pool.clone()),
	};
	wallet_app::get_wallet(&ports, &[], KycGate::LIFTED, user).await.unwrap().balance
}

/// The parked outbox rows for one aggregate, with their reasons.
async fn parked(h: &Harness, aggregate_id: Uuid) -> Vec<String> {
	sqlx::query_scalar::<_, Option<String>>("SELECT last_error FROM outbox WHERE aggregate_id = $1 AND parked_at IS NOT NULL ORDER BY seq")
		.bind(aggregate_id)
		.fetch_all(&h.pool)
		.await
		.unwrap()
		.into_iter()
		.map(Option::unwrap_or_default)
		.collect()
}

#[tokio::test]
async fn a_crossing_limit_buy_settles_delivery_versus_payment_with_the_takers_fee() {
	let _revenue = REVENUE.lock().await;
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 100).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "100").await;
	fund_user(&h, buyer, "100").await;
	// The one global account this suite reads. `REVENUE` and the outbox fence are both
	// one-process locks, so they order this bracket against the rest of THIS binary only —
	// `service:fee` lives in the shared TigerBeetle cluster, where another test binary
	// crediting a fee would land inside the bracket. Sequential test targets keep that from
	// firing under `cargo test`; a per-test-process runner (nextest) or two `cargo test`
	// invocations side by side would need a cluster per binary, not another mutex (#437).
	let revenue_before = cash_of(&h, LedgerAccountKey::ServiceClaim(ServiceId::fee())).await;

	// The seller rests 10 at 1.50: its units leave the holding for the book's escrow.
	let ask = sell(&h, seller, &service, "1.5", "10").await;
	assert_eq!(ask.order.state(), OrderState::Open);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("90"));
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, shares("10"));
	let resting = position(&h, seller, &service).await;
	assert_eq!((resting.units, resting.units_in_orders), (shares("90"), shares("10")), "the position shows what is in orders");
	assert_eq!(resting.value, usdt("100"), "units in orders are still valued as the holder's");

	// The buyer crosses it at the same price and pays 1 % as the taker.
	let bid = buy(&h, buyer, &service, "1.5", "10").await;
	assert_eq!(bid.order.state(), OrderState::Filled);
	assert_eq!(bid.order.cancel_reason(), None, "a filled order ended by filling, not by a cancel");
	assert_eq!(bid.order.average_fill_price(), Some(price("1.5")));
	assert_eq!(bid.order.fee_paid(), usdt("0.15"));
	assert_eq!(order(&h, &ask).await.order.state(), OrderState::Filled, "the maker filled too");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Delivery versus payment: 10 units to the buyer, 15 USDT to the seller, 0.15 to the
	// fund — and nothing left in either escrow.
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), buyer)).await, shares("10"));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("90"));
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, Shares::ZERO);
	assert_eq!(cash_of(&h, LedgerAccountKey::UserClaim(seller)).await, usdt("15"));
	assert_eq!(cash_of(&h, LedgerAccountKey::UserClaim(buyer)).await, usdt("84.85"));
	assert_eq!(cash_of(&h, LedgerAccountKey::BookCash(buyer)).await, Usdt::ZERO);
	let revenue_after = cash_of(&h, LedgerAccountKey::ServiceClaim(ServiceId::fee())).await;
	assert_eq!(
		revenue_after.checked_sub(revenue_before),
		Some(usdt("0.15")),
		"the taker's fee landed in the fee allocation's claim"
	);
	// Supply is untouched: the book never mints or burns.
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("100"));
	assert_eq!(parked(&h, ask.order.id().raw()).await, Vec::<String>::new());
	assert_eq!(parked(&h, bid.order.id().raw()).await, Vec::<String>::new());

	// Cost basis moved on both sides: the buyer paid 15.15 for 10 units, the seller's
	// 100-unit basis of 100 shrank pro rata to 90.
	let bought = position(&h, buyer, &service).await;
	assert_eq!((bought.units, bought.cost_basis), (shares("10"), usdt("15.15")));
	let sold = position(&h, seller, &service).await;
	assert_eq!((sold.units, sold.cost_basis), (shares("90"), usdt("90")));

	// The tape, the caller's own view of it, and the snapshot after the trade.
	let tape = book_app::list_trades(&h.book, &service, 10).await.unwrap();
	assert_eq!(tape.len(), 1);
	assert_eq!((tape[0].trade.price, tape[0].trade.size, tape[0].trade.taker_side), (price("1.5"), shares("10"), Side::Buy));
	let mine = book_app::list_user_trades(&h.book, buyer, Some(&service), 10).await.unwrap();
	assert_eq!((mine[0].side, mine[0].order_id, mine[0].fee), (Side::Buy, bid.order.id(), usdt("0.15")));
	let theirs = book_app::list_user_trades(&h.book, seller, Some(&service), 10).await.unwrap();
	assert_eq!(
		(theirs[0].side, theirs[0].order_id, theirs[0].fee),
		(Side::Sell, ask.order.id(), Usdt::ZERO),
		"the maker paid nothing"
	);
	let snapshot = book_app::snapshot(&h.book, &h.nav, h.ledger.as_ref(), &service, 20, now_unix()).await.unwrap();
	assert!(snapshot.bids.is_empty() && snapshot.asks.is_empty());
	assert_eq!(snapshot.last, Some((price("1.5"), Side::Buy)));
	assert_eq!(snapshot.volume_24h, shares("10"));
	assert_eq!(snapshot.change_24h_pct.as_deref(), Some("0.00"), "one trade: the reference is itself");
	assert!(snapshot.revision >= 2, "a placement and a fill each bumped the revision");
	assert_eq!(book_app::list_open_orders(&h.book, seller, None).await.unwrap().len(), 0);
	assert_eq!(book_app::list_order_history(&h.book, seller, Some(&service), 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_buy_below_its_limit_gets_the_price_improvement_back() {
	// Pays a fee, so it must not interleave with the test bracketing `service:fee`.
	let _revenue = REVENUE.lock().await;
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 100).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100").await;
	sell(&h, seller, &service, "1", "10").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Bid 1.20 for 10: the escrow takes 12 + 1 % = 12.12, the fill lands at the maker's
	// 1.00 for 10 + 0.10 fee, and the 2.02 difference comes back on completion.
	let bid = buy(&h, buyer, &service, "1.2", "10").await;
	assert_eq!(bid.order.state(), OrderState::Filled);
	assert_eq!(bid.order.notional_filled(), usdt("10"));
	assert_eq!(bid.order.fee_paid(), usdt("0.1"));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cash_of(&h, LedgerAccountKey::UserClaim(buyer)).await, usdt("89.9"));
	assert_eq!(cash_of(&h, LedgerAccountKey::BookCash(buyer)).await, Usdt::ZERO, "the price improvement was released");
	assert_eq!(cash_of(&h, LedgerAccountKey::UserClaim(seller)).await, usdt("10"));
}

#[tokio::test]
async fn a_partial_fill_leaves_the_maker_resting_with_the_rest() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100").await;
	let ask = sell(&h, seller, &service, "1", "10").await;
	let bid = buy(&h, buyer, &service, "1", "4").await;
	assert_eq!(bid.order.state(), OrderState::Filled);
	let ask = order(&h, &ask).await;
	assert_eq!(
		(ask.order.state(), ask.order.filled(), ask.order.remaining()),
		(OrderState::PartiallyFilled, shares("4"), shares("6"))
	);

	let snapshot = book_app::snapshot(&h.book, &h.nav, h.ledger.as_ref(), &service, 20, now_unix()).await.unwrap();
	assert_eq!(snapshot.asks.len(), 1);
	assert_eq!((snapshot.asks[0].price, snapshot.asks[0].size, snapshot.asks[0].orders), (price("1"), shares("6"), 1));
	assert_eq!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().len(), 1);

	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// Six units still in escrow, four delivered.
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, shares("6"));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), buyer)).await, shares("4"));
}

#[tokio::test]
async fn an_ioc_fills_what_it_can_and_releases_the_rest() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "5").await;
	fund_user(&h, buyer, "100").await;
	sell(&h, seller, &service, "1", "5").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let bid = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Ioc, Some("1"), "8").await.unwrap();
	assert_eq!(
		(bid.order.state(), bid.order.filled()),
		(OrderState::Cancelled, shares("5")),
		"the unfilled 3 are cancelled, never rested"
	);
	// `cancelled` with `filled > 0` is exactly what a user's cancel after a partial fill
	// looks like — the reason is what tells the two apart, and it is what the row stores.
	assert_eq!(bid.order.cancel_reason(), Some(CancelReason::IocRemainder));
	assert_eq!(order(&h, &bid).await.order.cancel_reason(), Some(CancelReason::IocRemainder));
	assert!(book_app::list_open_orders(&h.book, buyer, Some(&service)).await.unwrap().is_empty());
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// 8 was escrowed, 5 spent, 3 released.
	assert_eq!(cash_of(&h, LedgerAccountKey::UserClaim(buyer)).await, usdt("95"));
	assert_eq!(cash_of(&h, LedgerAccountKey::BookCash(buyer)).await, Usdt::ZERO);

	// An IOC against an empty side fills nothing and holds nothing.
	let nothing = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Ioc, Some("1"), "1").await.unwrap();
	assert_eq!((nothing.order.state(), nothing.order.filled()), (OrderState::Cancelled, Shares::ZERO));
	assert_eq!(nothing.order.cancel_reason(), Some(CancelReason::IocRemainder));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cash_of(&h, LedgerAccountKey::UserClaim(buyer)).await, usdt("95"));
}

#[tokio::test]
async fn a_post_only_order_rests_or_is_refused_and_a_self_trade_is_refused() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100").await;
	fund_user(&h, seller, "100").await;
	sell(&h, seller, &service, "1", "10").await;

	// Post-only under the ask rests; at the ask it would take and is refused outright.
	let posted = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Alo, Some("0.99"), "1").await.unwrap();
	assert_eq!(posted.order.state(), OrderState::Open);
	let err = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Alo, Some("1"), "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("post-only")), "got {err:?}");

	// The seller bidding into their own ask is refused, whatever the time in force.
	let err = buy_result(&h, seller, &service, "1", "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("own")), "got {err:?}");
	// Nothing was recorded for either refusal.
	assert_eq!(book_app::list_order_history(&h.book, buyer, Some(&service), 10).await.unwrap().len(), 1);
	assert_eq!(book_app::list_order_history(&h.book, seller, Some(&service), 10).await.unwrap().len(), 1);
}

async fn buy_result(h: &Harness, user: UserId, service: &ServiceId, at: &str, size: &str) -> Result<OrderRecord, DomainError> {
	place(h, user, service, Side::Buy, OrderKind::Limit, Tif::Gtc, Some(at), size).await
}

#[tokio::test]
async fn a_market_order_is_priced_off_the_best_quote_and_refused_on_an_empty_side() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100").await;

	// No asks at all: nothing to price a market buy from.
	let err = place(&h, buyer, &service, Side::Buy, OrderKind::Market, Tif::Ioc, None, "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("liquidity")), "got {err:?}");

	sell(&h, seller, &service, "1", "5").await;
	sell(&h, seller, &service, "1.04", "5").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// Best ask 1.00, 5 % slippage → limit 1.05, so both asks are reachable: 5 at 1.00,
	// then 3 at 1.04, and the derived limit is what the row records.
	let market = place(&h, buyer, &service, Side::Buy, OrderKind::Market, Tif::Ioc, None, "8").await.unwrap();
	assert_eq!(market.order.price(), price("1.05"));
	assert_eq!((market.order.state(), market.order.filled()), (OrderState::Filled, shares("8")));
	assert_eq!(market.order.notional_filled(), usdt("8.12"));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cash_of(&h, LedgerAccountKey::UserClaim(buyer)).await, usdt("91.88"));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), buyer)).await, shares("8"));

	// Only 2 rest at 1.04 now. A market buy for 5 is priced 1.04 + 5 % → 1.10 (ceiled to
	// the tick), takes the 2 and ends cancelled for the 3 it could not reach — with the
	// reason saying so, not a user's cancel.
	let leftover = place(&h, buyer, &service, Side::Buy, OrderKind::Market, Tif::Ioc, None, "5").await.unwrap();
	assert_eq!(leftover.order.price(), price("1.1"));
	assert_eq!((leftover.order.state(), leftover.order.filled()), (OrderState::Cancelled, shares("2")));
	assert_eq!(leftover.order.cancel_reason(), Some(CancelReason::MarketRemainder));
	assert_eq!(order(&h, &leftover).await.order.cancel_reason(), Some(CancelReason::MarketRemainder));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cash_of(&h, LedgerAccountKey::UserClaim(buyer)).await, usdt("89.8"), "2 × 1.04 spent, the rest of the escrow back");
	assert_eq!(cash_of(&h, LedgerAccountKey::BookCash(buyer)).await, Usdt::ZERO);

	// A market order names no price and is never good-till-cancelled.
	assert!(place(&h, buyer, &service, Side::Buy, OrderKind::Market, Tif::Ioc, Some("1"), "1").await.is_err());
	assert!(place(&h, buyer, &service, Side::Buy, OrderKind::Market, Tif::Gtc, None, "1").await.is_err());
}

#[tokio::test]
async fn cancelling_returns_the_escrow_and_is_idempotent() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	let stranger = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;
	let ask = sell(&h, seller, &service, "2", "10").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, shares("10"));

	// Not the stranger's to cancel.
	let err = book_app::cancel_order(&h.book, &h.notify, &h.feed, ask.order.id(), stranger).await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)), "got {err:?}");

	let cancelled = book_app::cancel_order(&h.book, &h.notify, &h.feed, ask.order.id(), seller).await.unwrap();
	assert_eq!(cancelled.order.state(), OrderState::Cancelled);
	assert_eq!(cancelled.order.cancel_reason(), Some(CancelReason::User));
	assert_eq!(order(&h, &ask).await.order.cancel_reason(), Some(CancelReason::User));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("10"), "every unit came back");
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, Shares::ZERO);
	assert_eq!(position(&h, seller, &service).await.units_in_orders, Shares::ZERO);

	// Again: the documented no-op, and nothing new for the relay.
	let again = book_app::cancel_order(&h.book, &h.notify, &h.feed, ask.order.id(), seller).await.unwrap();
	assert_eq!(again.order.state(), OrderState::Cancelled);
	assert_eq!(again.order.cancel_reason(), Some(CancelReason::User));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("10"));
	assert_eq!(parked(&h, ask.order.id().raw()).await, Vec::<String>::new());
}

#[tokio::test]
async fn the_wallet_shows_the_escrow_of_resting_orders_and_total_does_not_move() {
	let Some(h) = harness().await else { return };
	// 1 % taker fee: the buy escrows notional PLUS the fee, and both must show as in orders.
	let service = tradable_product(&h, 100).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100").await;
	let fresh = wallet(&h, buyer).await;
	assert_eq!((fresh.available, fresh.in_orders, fresh.total), (usdt("100"), Usdt::ZERO, usdt("100")));

	// A bid for 10 at 1.20 with nothing to hit rests: 12 + 0.12 leave `available` for the
	// book's escrow, and the wallet says so instead of showing an unexplained dip.
	let bid = buy(&h, buyer, &service, "1.2", "10").await;
	assert_eq!((bid.order.state(), bid.order.reserved()), (OrderState::Open, Locked::Cash(usdt("12.12"))));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let resting = wallet(&h, buyer).await;
	assert_eq!(resting.in_orders, usdt("12.12"), "exactly the reserve");
	assert_eq!(resting.available, usdt("87.88"), "available dropped by exactly the reserve");
	assert_eq!(resting.total, usdt("100"), "the reserve moved between two of total's terms, not out of it");
	assert_eq!(resting.pending_withdrawal, Usdt::ZERO, "an order is not a withdrawal");

	// The seller's side: units in a resting sell are still theirs, so `invested` holds.
	let sellers_before = wallet(&h, seller).await;
	assert_eq!(sellers_before.invested, usdt("10"), "10 units at the seed NAV");
	let ask = sell(&h, seller, &service, "1.5", "4").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("6"));
	let sellers = wallet(&h, seller).await;
	assert_eq!((sellers.invested, sellers.total), (usdt("10"), usdt("10")), "the escrowed 4 are still valued as the holder's");

	// Cancelling hands the escrow back and the figures return to where they started.
	book_app::cancel_order(&h.book, &h.notify, &h.feed, bid.order.id(), buyer).await.unwrap();
	book_app::cancel_order(&h.book, &h.notify, &h.feed, ask.order.id(), seller).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let released = wallet(&h, buyer).await;
	assert_eq!((released.available, released.in_orders, released.total), (usdt("100"), Usdt::ZERO, usdt("100")));
	assert_eq!(wallet(&h, seller).await.invested, usdt("10"));
}

#[tokio::test]
async fn the_gates_a_closed_book_view_access_hidden_and_read_only() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;

	// The operator closes the book: no new orders, whatever the allocation's state.
	let closed = BookPolicy::new(false, 0, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, false).unwrap();
	book_app::set_policy(&h.allocations, &h.book, &service, closed).await.unwrap();
	let err = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("closed")), "got {err:?}");
	open_book(&h, &service, 0).await;

	// A product with no policy row at all has a closed book.
	let unset = tradable_product(&h, 0).await;
	sqlx::query("DELETE FROM book_policies WHERE service = $1").bind(unset.as_str()).execute(&h.pool).await.unwrap();
	assert!(!book_app::policy(&h.book, &unset).await.unwrap().policy.book_open());

	// Access: `view` may look but not trade; `hidden` cannot even find the book.
	h.allocations.set_access(&service, AllocationAccess::View).await.unwrap();
	let err = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("invest")), "got {err:?}");
	book_app::require_visible(&h.allocations, &service, seller, false).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Hidden).await.unwrap();
	assert!(matches!(
		place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await.unwrap_err(),
		DomainError::NotFound { entity: "allocation", .. }
	));
	assert!(matches!(
		book_app::require_visible(&h.allocations, &service, seller, false).await.unwrap_err(),
		DomainError::NotFound { entity: "allocation", .. }
	));
	book_app::require_visible(&h.allocations, &service, seller, true).await.expect("a manager still sees it");
	// A per-user grant lets the holder back in.
	h.allocations.grant_access(&service, seller, AllocationAccess::Invest, seller).await.unwrap();
	sell(&h, seller, &service, "1", "1").await;

	// The allocation's lifecycle is not a gate: holders of a closed product still trade.
	h.allocations.close(&service).await.unwrap();
	sell(&h, seller, &service, "1", "1").await;

	// The read-only kill-switch pauses placing. Cleared before asserting, so a failing
	// assertion cannot leave it on for every other test on this database.
	operations::set_read_only(&h.pool, true).await.unwrap();
	let refused = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await;
	operations::set_read_only(&h.pool, false).await.unwrap();
	assert!(matches!(refused, Err(DomainError::Precondition(ref m)) if m.contains("paused")), "got {refused:?}");

	// A frozen owner may not place either.
	sqlx::query("UPDATE users SET frozen = TRUE WHERE id = $1").bind(seller.raw()).execute(&h.pool).await.unwrap();
	let refused = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await;
	sqlx::query("UPDATE users SET frozen = FALSE WHERE id = $1").bind(seller.raw()).execute(&h.pool).await.unwrap();
	assert!(matches!(refused, Err(DomainError::Forbidden(_))), "got {refused:?}");
}

#[tokio::test]
async fn the_grid_and_the_balance_are_checked_before_anything_is_written() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "10").await;

	assert!(
		place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1.005"), "1").await.is_err(),
		"off the 0.01 tick"
	);
	assert!(
		place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "0.00005").await.is_err(),
		"off the 0.0001 lot"
	);
	assert!(
		place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, None, "1").await.is_err(),
		"a limit order needs a price"
	);
	let err = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "11").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("units")), "more units than held: {err:?}");
	let err = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Gtc, Some("1"), "11").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("balance")), "more cash than held: {err:?}");
	assert!(book_app::list_order_history(&h.book, seller, None, 10).await.unwrap().is_empty());
	assert!(book_app::list_order_history(&h.book, buyer, None, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_client_order_id_makes_a_retry_land_once_and_a_reuse_a_conflict() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;

	let key = format!("retry-{}", Uuid::new_v4());
	let first = place_keyed(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "3", &key).await.unwrap();
	let again = place_keyed(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "3", &key).await.unwrap();
	assert_eq!(again.order.id(), first.order.id(), "the retry is the same order");
	assert_eq!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().len(), 1);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, shares("3"), "escrowed once");

	let err = place_keyed(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "4", &key).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "the same key for a different order: {err:?}");
}

#[tokio::test]
async fn a_raced_over_lock_is_refused_by_the_ledger_and_the_order_marked_rejected() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;

	// Two sells of the whole holding before the relay moves the first: both pass the
	// optimistic Read-First (TigerBeetle still shows 10 free), both are recorded.
	let first = sell(&h, seller, &service, "1", "10").await;
	let second = sell(&h, seller, &service, "1", "10").await;
	assert_eq!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().len(), 2);

	// The holding's non-negative flag refuses the second escrow; the relay parks the
	// fact and takes the order off the book so nothing can ever fill against it.
	// The park IS what this test pins, so the one-pass drain stays here: `drain_to_quiescence`
	// reports a park raised during the call as a failure.
	h.relay.drain().await;
	assert_eq!(
		units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await,
		shares("10"),
		"exactly one lock applied"
	);
	assert_eq!(order(&h, &first).await.order.state(), OrderState::Open);
	let rejected = order(&h, &second).await.order;
	assert_eq!(rejected.state(), OrderState::Rejected);
	assert!(rejected.reject_reason().is_some_and(|reason| reason.contains("escrow")), "got {:?}", rejected.reject_reason());
	assert_eq!(parked(&h, second.order.id().raw()).await.len(), 1, "the lock is parked for reconciliation");
	assert!(
		book_app::cancel_order(&h.book, &h.notify, &h.feed, second.order.id(), seller).await.is_err(),
		"nothing to release"
	);
	assert_eq!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().len(), 1);
}

#[tokio::test]
async fn candles_aggregate_the_tape_by_bucket() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100").await;
	// Three fills at 1.00, 1.10 and 0.90, in that order, all inside one minute.
	sell(&h, seller, &service, "1", "1").await;
	buy(&h, buyer, &service, "1", "1").await;
	sell(&h, seller, &service, "1.1", "2").await;
	buy(&h, buyer, &service, "1.1", "2").await;
	sell(&h, seller, &service, "0.9", "3").await;
	buy(&h, buyer, &service, "0.9", "3").await;

	let now = now_unix();
	let candles = book_app::candles(&h.book, &service, CandleResolution::M1, now - 600, now + 60).await.unwrap();
	// One or two buckets (a minute boundary may fall between the fills); the aggregate
	// over them is what a chart would draw.
	assert!(!candles.is_empty() && candles.len() <= 2, "got {candles:?}");
	assert_eq!(candles.first().unwrap().open, price("1"));
	assert_eq!(candles.last().unwrap().close, price("0.9"));
	assert_eq!(candles.iter().map(|c| c.high).max(), Some(price("1.1")));
	assert_eq!(candles.iter().map(|c| c.low).min(), Some(price("0.9")));
	let volume = candles.iter().fold(Shares::ZERO, |acc, c| acc.checked_add(c.volume).unwrap());
	assert_eq!(volume, shares("6"));
	assert!(candles.windows(2).all(|pair| pair[0].time < pair[1].time), "oldest first");
	// A window wider than the bucket bound is refused rather than aggregated unbounded.
	assert!(book_app::candles(&h.book, &service, CandleResolution::M1, 0, now).await.is_err());
	assert!(book_app::candles(&h.book, &service, CandleResolution::M1, now, now).await.is_err());
	// The snapshot's 24h change measures the last fill against the first.
	let snapshot = book_app::snapshot(&h.book, &h.nav, h.ledger.as_ref(), &service, 20, now).await.unwrap();
	assert_eq!(snapshot.change_24h_pct.as_deref(), Some("-10.00"));
	assert_eq!(snapshot.volume_24h, shares("6"));
}

#[tokio::test]
async fn the_feed_frames_on_every_change_and_reports_where_the_callers_orders_moved() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100").await;
	let mut receiver = h.feed.subscribe(&service);
	receiver.changed().await.unwrap();
	receiver.borrow_and_update();

	sell(&h, seller, &service, "1", "10").await;
	receiver.changed().await.expect("a placement frames");
	let after_ask = *receiver.borrow_and_update();
	let frame = book_app::watch_frame(&h.book, &h.nav, h.ledger.as_ref(), &service, buyer, 20, 20, now_unix()).await.unwrap();
	assert_eq!(frame.snapshot.revision, after_ask);
	assert_eq!(frame.orders_revision, 0, "the buyer has no orders yet");
	assert_eq!(frame.snapshot.asks.len(), 1);
	assert!(frame.trades.is_empty());

	buy(&h, buyer, &service, "1", "4").await;
	receiver.changed().await.expect("a fill frames");
	let after_fill = *receiver.borrow_and_update();
	assert!(after_fill > after_ask);
	let frame = book_app::watch_frame(&h.book, &h.nav, h.ledger.as_ref(), &service, buyer, 20, 20, now_unix()).await.unwrap();
	assert_eq!(frame.orders_revision, after_fill, "the buyer's order moved at this revision");
	assert_eq!(frame.trades.len(), 1);
	let sellers = book_app::watch_frame(&h.book, &h.nav, h.ledger.as_ref(), &service, seller, 20, 20, now_unix()).await.unwrap();
	assert_eq!(sellers.orders_revision, after_fill, "so did the maker's");
	assert_eq!(sellers.snapshot.mid, None, "one-sided book: no mid");
	assert!(!receiver.has_changed().unwrap(), "nothing more until the next change");
}

#[tokio::test]
async fn the_policy_is_operator_set_and_bounded() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 25).await;
	let record = book_app::policy(&h.book, &service).await.unwrap();
	assert!(record.policy.book_open());
	assert_eq!(record.policy.taker_fee_bps(), 25);
	assert!(record.updated_at > 0);
	// A policy for a product nobody registered is refused, not silently stored.
	let ghost = unique_service();
	let err = book_app::set_policy(&h.allocations, &h.book, &ghost, BookPolicy::default()).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { entity: "allocation", .. }), "got {err:?}");
	// The grid can be widened, and the defaults answer for a product with no row.
	let coarse = BookPolicy::new(true, 0, price("0.1"), shares("1"), 1000, false).unwrap();
	let stored = book_app::set_policy(&h.allocations, &h.book, &service, coarse).await.unwrap();
	assert_eq!(
		(stored.policy.price_tick(), stored.policy.lot_size(), stored.policy.market_slippage_bps()),
		(price("0.1"), shares("1"), 1000)
	);
	let fresh = unique_service();
	let mut allocation = Allocation::register(AllocationId::new(), fresh.clone(), "Fresh", "", AllocationIcon::default()).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	let defaults = book_app::policy(&h.book, &fresh).await.unwrap();
	assert_eq!(defaults.policy, BookPolicy::default());
	assert_eq!(defaults.updated_at, 0);
	assert!(book_app::list_trades(&h.book, &fresh, 10).await.unwrap().is_empty());
	let snapshot = book_app::snapshot(&h.book, &h.nav, h.ledger.as_ref(), &fresh, 20, now_unix()).await.unwrap();
	assert_eq!(snapshot.revision, 0);
	assert_eq!(snapshot.last, None);
}

/// Opening the book on a product whose units are held in kind is refused until the
/// operator acknowledges unbacked trading — and the refusal writes nothing, so the terms in
/// force stay what they were. Closing needs no acknowledgement.
#[tokio::test]
async fn an_in_kind_book_does_not_open_without_the_acknowledgement() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().backing(), AllocationBacking::InKind);
	let before = book_app::policy(&h.book, &service).await.unwrap();

	let unacknowledged = BookPolicy::new(true, 25, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, false).unwrap();
	let err = book_app::set_policy(&h.allocations, &h.book, &service, unacknowledged).await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("allow_unbacked_trading")), "got {err:?}");
	let after = book_app::policy(&h.book, &service).await.unwrap();
	assert_eq!((after.policy, after.updated_at), (before.policy, before.updated_at), "a refused policy is not written");

	let closed = BookPolicy::new(false, 25, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, false).unwrap();
	let stored = book_app::set_policy(&h.allocations, &h.book, &service, closed).await.expect("closing needs no acknowledgement");
	assert!(!stored.policy.book_open());
	assert!(!stored.policy.allow_unbacked_trading());
}

/// With the acknowledgement the book opens, and the flag comes back on the policy for the
/// terminal to show its notice.
#[tokio::test]
async fn an_in_kind_book_opens_once_unbacked_trading_is_acknowledged() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;

	let acknowledged = BookPolicy::new(true, 25, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, true).unwrap();
	let stored = book_app::set_policy(&h.allocations, &h.book, &service, acknowledged).await.unwrap();
	assert!(stored.policy.book_open());
	assert!(stored.policy.allow_unbacked_trading());
	assert_eq!(stored.policy.taker_fee_bps(), 25);
	let read = book_app::policy(&h.book, &service).await.unwrap();
	assert!(read.policy.allow_unbacked_trading(), "the flag round-trips through the store");
	sell(&h, seller, &service, "1", "1").await;
}

/// The gate runs on every order, not only when the book opens: a book opened on a `cash`
/// product without the acknowledgement stops taking orders the moment the first in-kind
/// mint flips the product to `in_kind`, and resumes once the operator acknowledges.
#[tokio::test]
async fn a_book_opened_on_cash_stops_when_the_units_turn_in_kind_until_acknowledged() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "Service Arb", "Arbitrage", AllocationIcon::Arbitrage).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	h.allocations.open(&service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	let cash_only = BookPolicy::new(true, 0, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, false).unwrap();
	book_app::set_policy(&h.allocations, &h.book, &service, cash_only)
		.await
		.expect("a cash product's book opens unacknowledged");

	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().backing(), AllocationBacking::InKind);
	let err = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("allow_unbacked_trading")), "got {err:?}");
	assert!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().is_empty(), "nothing was recorded");

	open_book(&h, &service, 0).await;
	sell(&h, seller, &service, "1", "1").await;
	assert_eq!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().len(), 1);
}
