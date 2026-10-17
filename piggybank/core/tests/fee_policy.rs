//! Integration tests for the fee plane — real Postgres **and** TigerBeetle (no mocks,
//! per the project rules). They run when `DATABASE_URL` is set and a TigerBeetle replica
//! is reachable (e.g. `nix run .#db` + `.#tb`), and skip otherwise.
//!
//! What these pin down is the *shape* of the charge, not just its arithmetic (the maths
//! is covered by `domain::fees`' unit tests). Three properties are the whole design and
//! each has a test that would fail loudly if it regressed:
//!
//! 1. **A fee moves units, never cash.** The investor's USDT claim and the fund's claim
//!    are untouched by a charge — which is why collecting one costs no chain fee and can
//!    never drive a balance negative.
//! 2. **Units outstanding do not change.** A charge is a transfer between two holders, so
//!    NAV per unit does not move and no other investor pays for it.
//! 3. **The mark is per investor.** Two holders in the same fund at the same NAV owe
//!    different fees when they entered at different prices.
//!
//! Clocks are moved by backdating the position's accrual columns rather than by waiting,
//! so "a year of holding" is a single SQL statement.

use std::sync::Arc;

use domain::{
	allocations::{Allocation, AllocationAccess, AllocationIcon, AllocationId},
	auth::AuthSubject,
	balance::{LedgerAccountKey, ServiceId, ValuationId},
	book::{BookPolicy, ClientOrderId, OrderId, OrderKind, Price, PriceTimeEngine, Side, Tif},
	fees::{self, CrystallizationPeriod, FeeAssessment, FeeAssessmentId, FeePolicy, ManagementBasis, Trigger},
	money::{Nav, Network, Shares, TxRef, Usdt},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{
		balance as balance_app,
		book::{self as book_app, BookFeed, BookPorts, PlaceOrderRequest},
		fees as fee_app, funds as funds_app,
	},
	infrastructure::{
		allocations::PgAllocations,
		book::PgBook,
		consilium::PgConsilia,
		custody::StubCustody,
		deposits::PgDeposits,
		fee_policy_changes::PgFeePolicyChanges,
		fee_sweeper::FeeSweeper,
		fees::{PgFeeAssessments, PgFeePolicies, PgFeeSettlements, PgPositionAccruals},
		nav::PgNav,
		outflow::PgOutflowPolicy,
		redemptions::PgRedemptions,
		relay::Relay,
		subscriptions::PgSubscriptions,
		users::PgUsers,
	},
	ports::{
		AllocationRegistry, UserRepository,
		fees::{FeeAssessments, FeePolicies, FeePolicyChanges, PositionAccruals},
		ledger::Ledger,
	},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

const YEAR: i64 = 365 * 24 * 60 * 60;
/// Backdating by *exactly* one period lands a hair SHORT of the boundary: the accrual
/// clocks are stamped by Postgres `now()` (sub-second) and read back as rounded epoch
/// seconds, while the assessment is handed a `now` from Rust. The gap is under a second
/// either way — irrelevant to a fee (a second of a 2% year is 3e-8 of it, and the next
/// assessment starts from the recorded stamp, so nothing drifts) but fatal to an exact
/// `==`. Tests that need the period to have *closed* therefore overshoot by an hour, and
/// amounts are compared with a tolerance rather than for equality.
const PERIOD_MARGIN: i64 = 60 * 60;
/// Slack for that same sub-second jitter, in USDT. Four orders of magnitude above the
/// jitter and four below the figures being checked.
const EPSILON: &str = "0.05";

struct Harness {
	pool: PgPool,
	allocations: PgAllocations,
	subs: PgSubscriptions,
	reds: PgRedemptions,
	nav: PgNav,
	deposits: PgDeposits,
	policies: PgFeePolicies,
	changes: PgFeePolicyChanges,
	consilia: PgConsilia,
	accruals: PgPositionAccruals,
	assessments: PgFeeAssessments,
	settlements: PgFeeSettlements,
	ledger: Arc<dyn Ledger>,
	relay: Relay,
	notify: Arc<Notify>,
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "fee-policy test").await?;

	let notify = Arc::new(Notify::new());
	Some(Harness {
		allocations: PgAllocations::new(pool.clone()),
		subs: PgSubscriptions::new(pool.clone()),
		reds: PgRedemptions::new(pool.clone()),
		nav: PgNav::new(pool.clone()),
		deposits: PgDeposits::new(pool.clone()),
		policies: PgFeePolicies::new(pool.clone()),
		changes: PgFeePolicyChanges::new(pool.clone()),
		consilia: PgConsilia::new(pool.clone()),
		accruals: PgPositionAccruals::new(pool.clone()),
		assessments: PgFeeAssessments::new(pool.clone()),
		settlements: PgFeeSettlements::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		ledger,
		notify,
		pool,
	})
}

/// Serializes the one test that sweeps against every test that does not.
///
/// `FeeSweeper::sweep` is global by design — it charges every position in the database
/// whose accrual is due, which is exactly what a sweeper should do. These tests share one
/// database and backdate their own positions to make them due, so a sweep running in
/// parallel will happily charge a sibling test's investor: that test then finds its
/// position already assessed, its clock already advanced, and its units already reduced.
///
/// So the sweeping test takes this lock exclusively and everybody else takes it shared.
/// Everything stays parallel except the one operation that cannot be.
static SWEEP: std::sync::LazyLock<tokio::sync::RwLock<()>> = std::sync::LazyLock::new(|| tokio::sync::RwLock::new(()));

/// Hold for the duration of a test that backdates a position it wants to assess itself.
async fn no_sweeping() -> tokio::sync::RwLockReadGuard<'static, ()> {
	SWEEP.read().await
}

/// The `fee` allocation's claim (`service:fee`) is a single platform-wide account, not one
/// per fund — every product settles its fee class into the same allocation (#245). So a
/// test cannot scope an assertion about it to its own `unique_service()` the way it can
/// for a product claim or a share balance: it can only bracket its own call and compare.
/// That comparison is wrong the moment another test settles in between, and "a charge
/// never credits the fee allocation" then fails against a credit some sibling made.
///
/// Every test that brackets the global figure takes this exclusively. They are short, and
/// serialising them costs less than a suite that fails once a run.
static REVENUE: std::sync::LazyLock<tokio::sync::Mutex<()>> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Hold for the duration of a test that reads the fee allocation's claim (or its NAV)
/// before and after its own work.
async fn exclusive_revenue() -> tokio::sync::MutexGuard<'static, ()> {
	REVENUE.lock().await
}

/// The `fee` allocation's cash — where every settled, retained and taken fee lands.
fn fee_claim() -> LedgerAccountKey {
	LedgerAccountKey::ServiceClaim(ServiceId::fee())
}

/// Hold for the duration of the test that runs the sweeper.
async fn sweeping() -> tokio::sync::RwLockWriteGuard<'static, ()> {
	SWEEP.write().await
}

/// The dealing ports, borrowed out of the harness for one call — the same wiring behind
/// every fund use-case in this suite.
fn fund_ports(h: &Harness) -> funds_app::FundPorts<'_> {
	funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &h.nav,
		relay: &h.notify,
	}
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn shares(decimal: &str) -> Shares {
	Shares::parse_decimal(decimal).unwrap()
}

fn unique_service() -> ServiceId {
	ServiceId::parse(&format!("fee-{}", Uuid::new_v4())).unwrap()
}

fn now_unix() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// A registered, open product with the house 2-and-20 terms.
async fn open_fund(h: &Harness, service: &ServiceId) {
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "EV Trading", "Systematic crypto", AllocationIcon::default()).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	h.allocations.open(service).await.unwrap();
	// Fees are about holdings, not admission — every investor here is let in.
	h.allocations.set_access(service, AllocationAccess::Invest).await.unwrap();
	install_policy(h, service, FeePolicy::HOUSE).await;
}

/// Install terms the way the operator now does — a scheduled change, promoted — so every
/// fixture here takes the path production takes. Meant for a fund with no holders yet, where
/// the notice period is zero and the change binds at once; the full life of a change is the
/// subject of `tests/fee_policy_changes.rs`.
async fn install_policy(h: &Harness, service: &ServiceId, policy: FeePolicy) {
	let ports = fee_app::FeePolicyPorts {
		policies: &h.policies,
		changes: &h.changes,
		allocations: &h.allocations,
		consilia: &h.consilia,
		approval_url_base: "https://example.test/approve",
		governance_mail_wired: true,
	};
	let request = fee_app::PolicyChangeRequest {
		service: service.clone(),
		policy,
		requested_effective_from_unix: 0,
		reason: String::new(),
	};
	let change = fee_app::schedule_policy(&ports, UserId::new(), request, now_unix()).await.unwrap();
	assert!(h.changes.promote(change.id, now_unix()).await.unwrap(), "a fund with no holders takes new terms at once");
}

async fn fund_user(h: &Harness, user: UserId, amount: &str) {
	let tx_ref = TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	balance_app::record_deposit(&h.deposits, &h.notify, tx_ref, user, Network::Bep20, usdt(amount)).await.unwrap();
	h.relay.drain().await;
}

/// Subscribe and let the relay post both legs plus the position projection.
///
/// The wait at the end is load-bearing, not defensive. These tests share one database and
/// each `drain()` walks the *global* outbox, so a sibling test's drain can be the one that
/// projects this subscription. The projection stamps the accrual clocks (it has to — the
/// basis is moving), so a test that backdated before the projection landed would have its
/// backdating silently overwritten and see a position that has been held for no time at
/// all. Returning only once the basis has actually moved makes the helper mean what its
/// name says.
async fn subscribe(h: &Harness, user: UserId, service: &ServiceId, amount: &str) {
	let before = cost_basis_of(h, user, service).await;
	funds_app::subscribe(&fund_ports(h), &h.subs, user, service.clone(), usdt(amount), now_unix()).await.unwrap();
	h.relay.drain().await;
	for _ in 0..100 {
		if cost_basis_of(h, user, service).await > before {
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(50)).await;
	}
	panic!("the subscription's position projection never landed");
}

/// The position's projected cost basis, or zero before the projection exists.
async fn cost_basis_of(h: &Harness, user: UserId, service: &ServiceId) -> Usdt {
	let raw: Option<String> = sqlx::query_scalar("SELECT cost_basis FROM fund_positions WHERE user_id = $1 AND service = $2")
		.bind(user.raw())
		.bind(service.as_str())
		.fetch_optional(&h.pool)
		.await
		.unwrap();
	raw.map(|raw| Usdt::from_base_units(raw.parse().unwrap())).unwrap_or(Usdt::ZERO)
}

/// Move a position's accrual clocks back by `secs` — "hold this for a year" without
/// holding it for a year.
async fn backdate(h: &Harness, user: UserId, service: &ServiceId, secs: i64) {
	sqlx::query(
		"UPDATE fund_positions SET fees_accrued_at = now() - make_interval(secs => $3), \
		 crystallized_at = now() - make_interval(secs => $3) WHERE user_id = $1 AND service = $2",
	)
	.bind(user.raw())
	.bind(service.as_str())
	.bind(secs as f64)
	.execute(&h.pool)
	.await
	.unwrap();
}

async fn assess(h: &Harness, user: UserId, service: &ServiceId) -> Option<domain::fees::FeeCharge> {
	let assessment = fee_app::assess_position(
		&h.policies,
		&h.accruals,
		&h.assessments,
		h.ledger.as_ref(),
		&h.nav,
		&h.notify,
		user,
		service.clone(),
		Trigger::Period,
		now_unix(),
	)
	.await
	.unwrap();
	h.relay.drain().await;
	assessment.map(|a| a.charge())
}

/// The allocation book beside the fee plane, for the tests that park units in a resting
/// sell: what the fee can and cannot claw back is decided by where the units are.
struct Book {
	store: PgBook,
	users: PgUsers,
	outflow: PgOutflowPolicy,
	feed: Arc<BookFeed>,
}

impl Book {
	fn new(h: &Harness) -> Self {
		Self {
			store: PgBook::new(h.pool.clone()),
			users: PgUsers::new(h.pool.clone()),
			outflow: PgOutflowPolicy::new(h.pool.clone()),
			feed: BookFeed::new(),
		}
	}

	/// Orders reference the `users` table, so the holder here is a real row — unlike the
	/// bare `UserId::new()` the dealing-only tests get away with.
	async fn provisioned_user(&self) -> UserId {
		let subject = AuthSubject::parse(&format!("fee-book-{}", Uuid::new_v4())).unwrap();
		let email = Email::parse(&format!("fb{}@example.com", Uuid::new_v4().simple())).unwrap();
		self.users.provision(subject, email, true).await.unwrap().id()
	}

	async fn open(&self, h: &Harness, service: &ServiceId) {
		let policy = BookPolicy::new(true, 0, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, false).unwrap();
		book_app::set_policy(&h.allocations, &self.store, service, policy).await.unwrap();
	}

	/// Rest a sell for `units` at 1.00 and let the relay move them into the book's escrow.
	async fn rest_sell(&self, h: &Harness, user: UserId, service: &ServiceId, units: &str) -> OrderId {
		let ports = BookPorts {
			allocations: &h.allocations,
			ledger: h.ledger.as_ref(),
			nav: &h.nav,
			store: &self.store,
			engine: &PriceTimeEngine,
			outflow: &self.outflow,
			relay: &h.notify,
			feed: &self.feed,
		};
		let request = PlaceOrderRequest {
			service: service.clone(),
			side: Side::Sell,
			kind: OrderKind::Limit,
			tif: Tif::Gtc,
			price: Some(Price::parse_decimal("1").unwrap()),
			size: shares(units),
			client_order_id: ClientOrderId::parse(&format!("fee-{}", Uuid::new_v4())).unwrap(),
		};
		let order = book_app::place_order(&ports, user, request).await.unwrap();
		h.relay.drain().await;
		order.order.id()
	}

	async fn cancel(&self, h: &Harness, user: UserId, order: OrderId) {
		book_app::cancel_order(&self.store, &h.notify, &self.feed, order, user).await.unwrap();
		h.relay.drain().await;
	}
}

/// When the position's accrual clock last moved.
async fn accrued_at(h: &Harness, user: UserId, service: &ServiceId) -> i64 {
	h.accruals.find(user, service).await.unwrap().expect("the position exists").accrued_at_unix
}

/// Assert two amounts agree to within [`EPSILON`] — see the constant for why an exact
/// comparison is the wrong test here.
fn assert_close(actual: Usdt, expected: Usdt, what: &str) {
	let epsilon = usdt(EPSILON);
	let diff = if actual > expected {
		actual.checked_sub(expected).unwrap()
	} else {
		expected.checked_sub(actual).unwrap()
	};
	assert!(diff <= epsilon, "{what}: expected ~{expected}, got {actual}");
}

async fn units_of(h: &Harness, key: LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

async fn cash_of(h: &Harness, key: LedgerAccountKey) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

#[tokio::test]
async fn a_year_of_holding_costs_two_percent_of_units_and_moves_no_cash_at_all() {
	let _no_sweeping = no_sweeping().await;
	let _revenue = exclusive_revenue().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;

	// Balances the fee must not touch, recorded before the charge.
	let claim_before = cash_of(&h, LedgerAccountKey::UserClaim(user)).await;
	let fund_before = cash_of(&h, LedgerAccountKey::ServiceClaim(service.clone())).await;
	let outstanding_before = units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await;
	assert_eq!(outstanding_before, shares("1000"), "1000 USDT at the seed NAV mints 1000 units");
	// `service:fee` is one account shared by every test in this file, so only its DELTA
	// across this test means anything.
	let revenue_before = cash_of(&h, fee_claim()).await;

	backdate(&h, user, &service, YEAR).await;
	let charge = assess(&h, user, &service).await.expect("a year of holding owes a management fee");

	// 2% of the 1000 that actually went in, taken in units at NAV 1.0.
	assert_close(charge.management, usdt("20"), "a year of management on 1000 invested");
	assert_eq!(charge.performance, Usdt::ZERO, "a flat NAV produced no gain, so no performance fee");
	assert_eq!(charge.charged_units, Shares::from_cash(charge.due, Nav::SEED).unwrap(), "the whole charge was collectable");
	assert_eq!(charge.debt_carried, Usdt::ZERO);

	// (1) The units moved from the holder to the manager.
	let taken = charge.charged_units;
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await,
		shares("1000").checked_sub(taken).unwrap()
	);
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, taken);

	// (2) Supply is unchanged, so NAV per unit did not move and no other holder paid.
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, outstanding_before);

	// (3) Not one base unit of cash moved anywhere — the property that makes this free
	// to collect and impossible to overdraw.
	assert_eq!(cash_of(&h, LedgerAccountKey::UserClaim(user)).await, claim_before);
	assert_eq!(cash_of(&h, LedgerAccountKey::ServiceClaim(service.clone())).await, fund_before);
	assert_eq!(
		cash_of(&h, fee_claim()).await,
		revenue_before,
		"a charge never credits the fee allocation — only a settlement does"
	);
}

#[tokio::test]
async fn a_gain_above_the_mark_adds_the_twenty_percent_and_ratchets_it() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;

	// The operator marks the fund up 50%: 1000 units are now worth 1500.
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), usdt("1500"), "itest", now_unix())
		.await
		.unwrap();

	backdate(&h, user, &service, YEAR + PERIOD_MARGIN).await;
	let charge = assess(&h, user, &service).await.expect("a year at a higher NAV owes both fees");

	// Management is 2% of invested capital; performance is 20% of the gain on what is
	// left after it — not on the pre-fee holding, which would have given a round 100.
	assert_close(charge.management, usdt("20"), "a year of management on 1000 invested");
	assert_close(charge.performance, usdt("98.666666666666666666"), "20% of the gain, net of the management fee");
	assert!(charge.performance < usdt("100"), "performance must not be taken on capital management already claimed");
	assert!(charge.crystallized, "a full year elapsed, so the period closed");
	assert_eq!(charge.high_water_mark, Nav::parse_decimal("1.5").unwrap(), "the mark ratchets to the price it crystallized at");

	// Taken in units at 1.5, and supply is still untouched.
	assert_eq!(charge.charged_units, Shares::from_cash(charge.due, Nav::parse_decimal("1.5").unwrap()).unwrap());
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("1000"));

	// A second assessment immediately after takes no share of the gain: the mark now
	// equals the NAV, so there is nothing above it to charge. It may still collect a
	// sliver of management fee, because the clock is stamped from the assessing process's
	// whole second and one real second can have elapsed since — a millionth of a cent, and
	// owed, which is the point of accruing pro rata temporis rather than per visit.
	let again = assess(&h, user, &service).await;
	if let Some(again) = again {
		assert_eq!(again.performance, Usdt::ZERO, "the same gain must never be charged twice");
		// 0.01 USDT is about four hours of management fee on this position, so anything
		// under it is scheduler jitter between the two assessments. A genuine re-charge
		// would be another ~118, four orders of magnitude away — the gap is what makes
		// this a real assertion rather than a tolerance.
		assert!(again.due < usdt("0.01"), "only a sliver of elapsed management, not a second charge: {}", again.due);
	}
}

#[tokio::test]
async fn the_mark_is_per_investor_so_a_late_entrant_pays_less() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let early = UserId::new();
	let late = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;

	// The early investor enters at the seed NAV 1.0.
	fund_user(&h, early, "1000").await;
	subscribe(&h, early, &service, "1000").await;

	// The fund doubles, then the late investor enters at 2.0.
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), usdt("2000"), "itest", now_unix())
		.await
		.unwrap();
	fund_user(&h, late, "1000").await;
	subscribe(&h, late, &service, "1000").await;

	// Both are assessed a year later at the same price. Only the early investor has a
	// gain above their own mark — the late one is exactly at theirs. A fund-level mark
	// would have charged them the same, which is the inequity this design exists to avoid.
	backdate(&h, early, &service, YEAR + PERIOD_MARGIN).await;
	backdate(&h, late, &service, YEAR + PERIOD_MARGIN).await;
	let early_charge = assess(&h, early, &service).await.expect("the early investor gained");
	let late_charge = assess(&h, late, &service).await.expect("the late investor still owes management");

	assert!(early_charge.performance > Usdt::ZERO, "entered at 1.0, marked at 2.0 — a real gain");
	assert_eq!(late_charge.performance, Usdt::ZERO, "entered at 2.0, still at 2.0 — no gain, no performance fee");
	// Both still pay management on the capital they parked.
	assert_close(early_charge.management, usdt("20"), "the early investor's management fee");
	assert_close(late_charge.management, usdt("20"), "the late investor's management fee");
}

#[tokio::test]
async fn a_recovery_below_the_mark_is_never_charged() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;

	// Up to 1.4, crystallize there, then down to 0.9 and back up to 1.3. The investor is
	// up 44% over the year just past — and owes nothing on it, because they are still
	// under the mark they already paid at.
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), usdt("1400"), "itest", now_unix())
		.await
		.unwrap();
	backdate(&h, user, &service, YEAR + PERIOD_MARGIN).await;
	assess(&h, user, &service).await.expect("the first year crystallizes at 1.4");

	let units_after_first = units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await;
	let value_now = Usdt::from_base_units(units_after_first.base_units()).base_units();
	// Mark down to ~0.9 then part-way back to ~1.3, both below the 1.4 mark.
	funds_app::post_fund_valuation(
		&h.allocations,
		&h.nav,
		h.ledger.as_ref(),
		service.clone(),
		Usdt::from_base_units(value_now * 9 / 10),
		"itest",
		now_unix(),
	)
	.await
	.unwrap();
	funds_app::post_fund_valuation(
		&h.allocations,
		&h.nav,
		h.ledger.as_ref(),
		service.clone(),
		Usdt::from_base_units(value_now * 13 / 10),
		"itest",
		now_unix(),
	)
	.await
	.unwrap();

	backdate(&h, user, &service, YEAR + PERIOD_MARGIN).await;
	let charge = assess(&h, user, &service).await.expect("management still accrues in a drawdown");
	assert_eq!(charge.performance, Usdt::ZERO, "no performance fee on a mere recovery toward the mark");
	assert!(charge.management > Usdt::ZERO, "management is rent on capital and does not care about performance");
}

#[tokio::test]
async fn a_fund_with_no_policy_is_never_charged() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	// Registered and open, but deliberately given no fee policy.
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "EV Trading", "Systematic crypto", AllocationIcon::default()).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	h.allocations.open(&service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();

	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;
	backdate(&h, user, &service, YEAR * 3).await;

	assert!(assess(&h, user, &service).await.is_none(), "no policy means no fee, however long it is held");
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await, shares("1000"));
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service)).await, Shares::ZERO);
}

#[tokio::test]
async fn settling_fee_units_is_the_only_moment_a_fee_becomes_cash() {
	let _no_sweeping = no_sweeping().await;
	let _revenue = exclusive_revenue().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;
	backdate(&h, user, &service, YEAR).await;
	assess(&h, user, &service).await.expect("a year owes a fee");

	let fee_units = units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await;
	assert!(fee_units > Shares::ZERO, "the charge accumulated fee units");
	let fund_before = cash_of(&h, LedgerAccountKey::ServiceClaim(service.clone())).await;
	let outstanding_before = units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await;
	// `service:fee` is shared with every other test here, so compare the delta.
	let revenue_before = cash_of(&h, fee_claim()).await;

	// One bulk conversion for the whole fund — not one per investor. This is what the
	// unit-denominated charge buys: a single ledger operation per period.
	let settlement = fee_app::settle_fee_shares(&h.settlements, h.ledger.as_ref(), &h.nav, &h.reds, &h.notify, service.clone(), None, UserId::new(), now_unix())
		.await
		.unwrap();
	h.relay.drain().await;

	assert_eq!(settlement.units(), fee_units, "omitting `units` settles the whole accumulated balance");
	assert_close(settlement.cash(), usdt("20"), "a year's fee converted at NAV 1.0");
	// Burn-first, pay-second: the units are gone, supply fell by exactly what was burned,
	// and the cash landed in fee revenue out of the fund's claim.
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, Shares::ZERO);
	assert_eq!(
		units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await,
		outstanding_before.checked_sub(fee_units).unwrap()
	);
	// `service:fee` is where the money now sits — the `fee` allocation's own claim, held
	// by people through its units (#245): the product bought its fee class back, and the
	// allocation's holders exit by redeeming against exactly this cash.
	assert_eq!(cash_of(&h, fee_claim()).await, revenue_before.checked_add(settlement.cash()).unwrap());
	assert_eq!(cash_of(&h, LedgerAccountKey::ServiceClaim(service)).await, fund_before.checked_sub(settlement.cash()).unwrap());
}

/// Every unit in a resting sell: the fee is still owed on the whole position — an order
/// moves units into the book's escrow, it does not make them somebody else's — but none
/// of it can be taken, so the charge is recorded with nothing collected, the whole year
/// carried as debt, and the clock moved. The holding is never pushed negative and the
/// escrow never touched; the moment the units come home the next assessment collects the
/// debt, and only the debt — the year is not billed twice.
///
/// This is #255. Before it, a charge with nothing collectable persisted nothing at all,
/// so a holder with an ask resting on the book deferred their fee for as long as it
/// rested and was billed the whole stretch in one blow the day it came off.
#[tokio::test]
async fn units_escrowed_by_a_resting_sell_are_charged_as_debt_and_the_clock_moves() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let book = Book::new(&h);
	let user = book.provisioned_user().await;
	let service = unique_service();
	open_fund(&h, &service).await;
	book.open(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;

	let ask = book.rest_sell(&h, user, &service, "1000").await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await, Shares::ZERO);
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), user)).await, shares("1000"));

	backdate(&h, user, &service, YEAR).await;
	let owed_since = accrued_at(&h, user, &service).await;
	let charge = assess(&h, user, &service).await.expect("the year is owed on the escrowed position and must be recorded");
	assert_close(charge.management, usdt("20"), "a year of management on 1000 invested, escrow or not");
	assert_eq!(charge.charged_units, Shares::ZERO, "nothing was collectable");
	assert_eq!(charge.charged_cash, Usdt::ZERO);
	assert_close(charge.debt_carried, usdt("20"), "the whole charge is carried as debt");
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await,
		Shares::ZERO,
		"the holding was not pushed negative"
	);
	assert_eq!(
		units_of(&h, LedgerAccountKey::BookShares(service.clone(), user)).await,
		shares("1000"),
		"the escrow is the book's, not the fee's"
	);
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("1000"), "supply is untouched");
	assert!(accrued_at(&h, user, &service).await > owed_since, "the clock moved: the year has been assessed");
	assert_close(h.accruals.find(user, &service).await.unwrap().unwrap().debt, usdt("20"), "the debt is on the position");
	let recorded = fee_app::list_assessments(&h.assessments, user).await.unwrap();
	assert_eq!(recorded.len(), 1, "the deferred charge has its audit row");
	assert_eq!(recorded[0].charged_units, Shares::ZERO);

	// The order comes off the book and the units come home; the next assessment collects
	// the debt — and the seconds since, not the year again.
	book.cancel(&h, user, ask).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await, shares("1000"));
	let next = assess(&h, user, &service).await.expect("the carried debt is collected once the units are free");
	assert_close(next.debt_opening, usdt("20"), "the debt came in");
	assert!(
		next.management < usdt("0.01"),
		"only the seconds since the deferred charge accrued, not the year again: {}",
		next.management
	);
	assert_close(next.charged_cash, usdt("20"), "and went out in units");
	assert!(next.debt_carried < usdt("0.000001"), "nothing but the sub-unit residue is carried: {}", next.debt_carried);
	assert_close(
		charge.due.checked_add(next.management).unwrap(),
		usdt("20"),
		"the two passes together bill exactly one year's fee",
	);
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await,
		shares("1000").checked_sub(next.charged_units).unwrap()
	);
	assert_eq!(
		units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await,
		shares("1000"),
		"the book and the fee both leave supply alone"
	);
}

/// Most of the units in a resting sell: the fee is owed on all of them, the charge takes
/// what the holding still has, and the rest is carried as debt — the same road a queued
/// redemption sends it down. The escrow is never drawn on, the holding never goes
/// negative, and the debt is collected by the next assessment once the order is
/// cancelled and the units are back.
#[tokio::test]
async fn a_partial_escrow_defers_the_uncollectable_fee_into_debt_until_the_units_return() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let book = Book::new(&h);
	let user = book.provisioned_user().await;
	let service = unique_service();
	open_fund(&h, &service).await;
	book.open(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;

	// 995 of 1000 in the escrow: a year's 2 % is 20 units at the seed NAV, and only 5 are free.
	let ask = book.rest_sell(&h, user, &service, "995").await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await, shares("5"));
	backdate(&h, user, &service, YEAR).await;
	let charge = assess(&h, user, &service).await.expect("the free units carry what they can");
	assert_close(charge.management, usdt("20"), "a year of management on 1000 invested — the escrowed 995 included");
	assert_eq!(charge.charged_units, shares("5"), "capped by the free holding, not by what the book holds");
	assert_close(charge.debt_carried, usdt("15"), "the rest is debt, not a negative balance");
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await,
		Shares::ZERO,
		"the holding is exactly empty, not overdrawn"
	);
	assert_eq!(
		units_of(&h, LedgerAccountKey::BookShares(service.clone(), user)).await,
		shares("995"),
		"the escrow was not drawn on"
	);
	assert_close(h.accruals.find(user, &service).await.unwrap().unwrap().debt, usdt("15"), "the debt is on the position");

	book.cancel(&h, user, ask).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await, shares("995"));
	let next = assess(&h, user, &service).await.expect("the carried debt is collected now the units are back");
	assert_close(next.debt_opening, usdt("15"), "the debt came in");
	assert_close(next.charged_cash, usdt("15"), "and went out in units, plus the seconds of management since");
	assert!(
		next.debt_carried < usdt("0.000001"),
		"nothing but the sub-unit rounding residue is carried: {}",
		next.debt_carried
	);
	let left = units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await;
	assert_close(Nav::SEED.value(left).unwrap(), usdt("980"), "995 back, 15 taken");
}

#[tokio::test]
async fn a_settlement_the_fund_cannot_cover_is_refused_not_queued() {
	let _no_sweeping = no_sweeping().await;
	let _revenue = exclusive_revenue().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;
	backdate(&h, user, &service, YEAR).await;
	assess(&h, user, &service).await.expect("a year owes a fee");

	let fee_units = units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await;
	let held = units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await;

	// The investor redeems everything, which really does drain the fund's claim: the
	// payout settles at NAV 1.0, leaving behind only what the fee units are worth.
	funds_app::request_redemption(&fund_ports(&h), &h.reds, user, service.clone(), held, now_unix()).await.unwrap();
	h.relay.drain().await;

	// Now mark the remainder up hard. The fee units are suddenly worth several times the
	// cash left in the fund — the one situation a settlement cannot be paid.
	let remaining = cash_of(&h, LedgerAccountKey::ServiceClaim(service.clone())).await;
	// A +200% mark, past the guard: written through the shared writer the owners' override
	// uses, since the guarded post has no flag to lift it any more.
	funds_app::record_valuation(
		&h.nav,
		h.ledger.as_ref(),
		ValuationId::new(),
		service.clone(),
		remaining.checked_add(remaining).and_then(|d| d.checked_add(remaining)).unwrap(),
		"itest",
	)
	.await
	.unwrap();

	// Refused, not queued: nobody is waiting on this, and the fee units keep accumulating
	// at no cost until the fund is liquid again.
	let revenue_before = cash_of(&h, fee_claim()).await;
	let err = fee_app::settle_fee_shares(&h.settlements, h.ledger.as_ref(), &h.nav, &h.reds, &h.notify, service.clone(), None, UserId::new(), now_unix())
		.await
		.unwrap_err();
	assert!(matches!(err, domain::error::DomainError::Validation(_)), "got {err:?}");
	// And nothing was destroyed on the way to that refusal.
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service)).await, fee_units);
	assert_eq!(cash_of(&h, fee_claim()).await, revenue_before);
}

#[tokio::test]
async fn a_queued_redemption_is_reserved_before_the_manager_is_paid() {
	let _no_sweeping = no_sweeping().await;
	let _revenue = exclusive_revenue().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;
	backdate(&h, user, &service, YEAR).await;
	assess(&h, user, &service).await.expect("a year owes a fee");

	let fee_units = units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await;
	let held = units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await;

	// Mark the fund up first. The units are now worth more than the cash standing behind
	// them, which is the ordinary state of a fund holding anything other than cash.
	let claim = cash_of(&h, LedgerAccountKey::ServiceClaim(service.clone())).await;
	funds_app::record_valuation(&h.nav, h.ledger.as_ref(), ValuationId::new(), service.clone(), claim.checked_add(claim).unwrap(), "itest")
		.await
		.unwrap();

	// Now the investor asks to exit. `request_redemption` settles immediately only when
	// the claim covers the payout; here it does not, so the redemption genuinely QUEUES —
	// and note what that means: a queue exists precisely because the fund is already short
	// of what it owes this investor. The queue reserves units, never cash.
	funds_app::request_redemption(&fund_ports(&h), &h.reds, user, service.clone(), held, now_unix()).await.unwrap();
	h.relay.drain().await;

	// The claim still covers the fee many times over, so a gate reading only `available`
	// would pay the manager out of money already owed to a waiting investor — making a
	// shortfall the fund had already failed to cover worse. The holdback refuses it.
	let revenue_before = cash_of(&h, fee_claim()).await;
	let err = fee_app::settle_fee_shares(&h.settlements, h.ledger.as_ref(), &h.nav, &h.reds, &h.notify, service.clone(), None, UserId::new(), now_unix())
		.await
		.unwrap_err();
	let domain::error::DomainError::Validation(message) = &err else {
		panic!("expected a validation refusal, got {err:?}");
	};
	assert!(message.contains("queued redemptions"), "the refusal names the queue it is protecting: {message}");

	// Nothing moved on the way to the refusal — the units keep accumulating at no cost.
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, fee_units);
	assert_eq!(cash_of(&h, fee_claim()).await, revenue_before);

	// And once the queue is gone the same settlement goes through: the reserve is a
	// holdback against a real obligation, not a permanent freeze on the manager's fee.
	let queued = piggybank_core::ports::redemptions::RedemptionRepository::list_queued(&h.reds).await.unwrap();
	let mine = queued.iter().find(|q| q.service == service).expect("the redemption is queued");
	funds_app::cancel_redemption(&h.reds, &h.notify, mine.id, user).await.unwrap();
	h.relay.drain().await;
	fee_app::settle_fee_shares(&h.settlements, h.ledger.as_ref(), &h.nav, &h.reds, &h.notify, service.clone(), None, UserId::new(), now_unix())
		.await
		.expect("with the queue cleared the manager is paid");
}

#[tokio::test]
async fn the_sweeper_charges_every_due_position_and_records_a_statement() {
	let _sweeping = sweeping().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;
	backdate(&h, user, &service, YEAR).await;

	let sweeper = FeeSweeper::new(
		Arc::new(PgFeePolicies::new(h.pool.clone())),
		Arc::new(PgFeePolicyChanges::new(h.pool.clone())),
		Arc::new(PgPositionAccruals::new(h.pool.clone())),
		Arc::new(PgFeeAssessments::new(h.pool.clone())),
		h.ledger.clone(),
		Arc::new(PgNav::new(h.pool.clone())),
		h.notify.clone(),
		Arc::new(piggybank_core::infrastructure::outflow::PgOutflowPolicy::new(h.pool.clone())),
	);
	assert!(sweeper.sweep(now_unix()).await.unwrap() >= 1, "the backdated position is due");
	h.relay.drain().await;

	// The charge is on the investor's statement, with the figures that explain it.
	let statement = h.assessments.list_by_user(user).await.unwrap();
	let entry = statement.first().expect("one charge");
	assert_eq!(entry.service, service);
	assert_close(entry.management, usdt("20"), "a year of management on 1000 invested");
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await,
		shares("1000").checked_sub(entry.charged_units).unwrap(),
		"the statement's figure is the one the ledger actually moved"
	);

	// A second sweep does not charge THIS position again — the clocks moved with the
	// charge. The sweep's own return value counts every due position in the database,
	// which a sibling test's fixture can add to, so the assertion is on this investor's
	// statement rather than on that global count.
	sweeper.sweep(now_unix()).await.unwrap();
	h.relay.drain().await;
	assert_eq!(h.assessments.list_by_user(user).await.unwrap().len(), 1, "a swept position is not charged again");
}

#[tokio::test]
async fn a_zero_rate_policy_is_distinct_from_no_policy_and_also_charges_nothing() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;
	let free = FeePolicy::new(0, 0, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual).unwrap();
	install_policy(&h, &service, free).await;

	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;
	backdate(&h, user, &service, YEAR).await;

	assert!(assess(&h, user, &service).await.is_none());
	// The policy is still readable and still says what it says — "charges nothing" is a
	// configured answer, not a missing one.
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(free));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service, user)).await, shares("1000"));
}

#[tokio::test]
async fn a_top_up_is_never_billed_for_the_window_before_it_arrived() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;

	// A small position held for a year, then a hundredfold top-up.
	fund_user(&h, user, "100000").await;
	subscribe(&h, user, &service, "1000").await;
	backdate(&h, user, &service, YEAR + PERIOD_MARGIN).await;
	subscribe(&h, user, &service, "99000").await;

	// The management leg is `basis × rate × elapsed`, and the top-up moved the basis
	// without moving the clock. Charging now must bill 2% of the 1000 that was actually
	// held for the year — NOT 2% of the 100000 that exists at this instant.
	let charge = assess(&h, user, &service).await.expect("the carried year is collectable");
	assert_close(charge.due, usdt("20"), "a year on the original 1000, not on the topped-up 100000");
	// It arrives as carried debt: the accrual was settled the moment the basis moved,
	// which is the whole mechanism.
	assert_close(charge.debt_opening, usdt("20"), "settled at the top-up, collected here");
	assert_close(charge.management, usdt("0"), "no elapsed window remains after the top-up");
}

#[tokio::test]
async fn a_position_that_left_and_came_back_is_not_billed_for_its_dormancy() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;

	fund_user(&h, user, "20000").await;
	subscribe(&h, user, &service, "10000").await;
	// Exactly what a full redemption leaves behind: `reduce_cost_basis` computes
	// `cost_basis × (units − redeemed) / units`, which is zero when everything goes.
	// Doing it in SQL keeps the test on the branch under examination rather than on the
	// redemption saga, which has its own tests.
	sqlx::query("UPDATE fund_positions SET units = '0', cost_basis = '0' WHERE user_id = $1 AND service = $2")
		.bind(user.raw())
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	// A year of dormancy. The sweeper's queue skips unit-less rows, so nothing would have
	// advanced this clock on its own.
	backdate(&h, user, &service, YEAR + PERIOD_MARGIN).await;

	subscribe(&h, user, &service, "10000").await;
	// Before the fix this billed a full year of management fee on the returning capital —
	// 2% of 10000, or 200 USDT. What is owed now is the handful of seconds the returned
	// position has actually been held.
	let charge = assess(&h, user, &service).await;
	let due = charge.map(|charge| charge.due).unwrap_or(Usdt::ZERO);
	assert!(due < usdt("0.01"), "a dormant year must not be billed on the returning capital, got {due}");
}

#[tokio::test]
async fn a_second_assessor_for_the_same_window_charges_nothing() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;

	fund_user(&h, user, "10000").await;
	subscribe(&h, user, &service, "10000").await;
	backdate(&h, user, &service, YEAR + PERIOD_MARGIN).await;

	// Capture the clock both assessors would read, then let the first one charge.
	let stale = h.accruals.find(user, &service).await.unwrap().unwrap().accrued_at_unix;
	let first = assess(&h, user, &service).await.expect("the year is collectable");
	let units_after_first = units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await;

	// The second assessor read the same snapshot before the first committed — the shape
	// of two replicas sweeping the same position, which is a supported deployment (the
	// relay is a lock-enforced singleton precisely because a second core instance runs).
	// Its write must be refused, not merely serialized after the first.
	let charge = fees::assess(
		&FeePolicy::HOUSE,
		&snapshot_at(&h, user, &service, stale).await,
		first.high_water_mark,
		Trigger::Period,
		now_unix(),
	)
	.unwrap();
	let mut duplicate = FeeAssessment::record(FeeAssessmentId::new(), user, service.clone(), first.high_water_mark, Trigger::Period, charge).unwrap();
	let landed = h.assessments.charge(&mut duplicate, stale, now_unix()).await.unwrap();
	assert!(!landed, "a stale snapshot must not produce a second charge for the same window");

	h.relay.drain().await;
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await,
		units_after_first,
		"the investor was charged once, not twice"
	);
	// And the refused charge left no audit row behind — the whole transaction rolled back.
	assert_eq!(h.assessments.list_by_user(user).await.unwrap().len(), 1);
}

/// Rebuild the snapshot a losing assessor would have held: today's position, but the
/// accrual clock it read before the winner moved it.
async fn snapshot_at(h: &Harness, user: UserId, service: &ServiceId, accrued_at_unix: i64) -> domain::fees::PositionSnapshot {
	let accrual = h.accruals.find(user, service).await.unwrap().unwrap();
	let units = units_of(h, LedgerAccountKey::UserShares(service.clone(), user)).await;
	domain::fees::PositionSnapshot {
		units,
		collectable: units,
		cost_basis: accrual.cost_basis,
		high_water_mark: accrual.high_water_mark,
		debt: accrual.debt,
		accrued_at_unix,
		crystallized_at_unix: accrual.crystallized_at_unix,
	}
}

/// #255 as a regression pin, end to end: a holder whose every unit rests in a sell order is
/// not skipped by the assessment. The charge is recorded with nothing collected, the debt
/// is on the position, the clock has moved, and the ledger — holding, escrow, fee account,
/// supply — is exactly as it was.
#[tokio::test]
async fn a_holder_with_every_unit_in_a_resting_sell_is_assessed_not_skipped() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let book = Book::new(&h);
	let user = book.provisioned_user().await;
	let service = unique_service();
	open_fund(&h, &service).await;
	book.open(&h, &service).await;
	fund_user(&h, user, "5000").await;
	subscribe(&h, user, &service, "5000").await;
	let _ask = book.rest_sell(&h, user, &service, "5000").await;

	let holding_before = units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await;
	let escrow_before = units_of(&h, LedgerAccountKey::BookShares(service.clone(), user)).await;
	let fee_before = units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await;
	let outstanding_before = units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await;
	assert_eq!((holding_before, escrow_before), (Shares::ZERO, shares("5000")));

	backdate(&h, user, &service, YEAR + PERIOD_MARGIN).await;
	let owed_since = accrued_at(&h, user, &service).await;
	let charge = assess(&h, user, &service).await.expect("the assessment is not skipped");
	assert_close(charge.management, usdt("100"), "2% of the 5000 invested, all of it in the escrow");
	assert_eq!(charge.charged_units, Shares::ZERO);
	assert_close(charge.debt_carried, usdt("100"), "the whole year is now debt");
	assert!(charge.crystallized, "the period elapsed, so the performance leg crystallized (to nothing, at a flat NAV)");

	let position = h.accruals.find(user, &service).await.unwrap().unwrap();
	assert_close(position.debt, usdt("100"), "the debt is written on the position");
	assert!(position.accrued_at_unix > owed_since, "the accrual clock moved");
	assert!(position.crystallized_at_unix > owed_since, "and so did the period clock");

	let recorded = fee_app::list_assessments(&h.assessments, user).await.unwrap();
	assert_eq!(recorded.len(), 1);
	assert_eq!(recorded[0].charged_units, Shares::ZERO, "the audit row says nothing was collected");
	assert_eq!(recorded[0].charged_cash, Usdt::ZERO);
	assert_close(recorded[0].debt_carried, usdt("100"), "and how much was deferred");

	// Nothing moved on the ledger: no clawback was posted, because there was none to post.
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await, holding_before);
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), user)).await, escrow_before);
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, fee_before);
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, outstanding_before);

	// And none was even asked for: a wholly deferred charge raises no `Charged`, so
	// neither the audit log nor the relay's outbox has a row for this fund — the ledger
	// stayed put because there was nothing to relay, not because a zero transfer was
	// tried and shrugged off.
	let charged_events: i64 = sqlx::query_scalar(
		"SELECT COUNT(*) FROM (SELECT 1 FROM event_log WHERE aggregate = 'fee_assessment' AND payload->>'service' = $1 \
		 UNION ALL SELECT 1 FROM outbox WHERE aggregate = 'fee_assessment' AND payload->>'service' = $1) AS rows",
	)
	.bind(service.as_str())
	.fetch_one(&h.pool)
	.await
	.unwrap();
	assert_eq!(charged_events, 0, "a charge that collected nothing raises no Charged event");

	// The holder's statement still shows it — as a fee that deferred, worth nothing in
	// units, rather than choking the feed on a zero row the schema only just allowed.
	let feed = piggybank_core::infrastructure::operation_feed::PgOperationFeed::new(h.pool.clone());
	let page = piggybank_core::ports::operations::OperationFeed::list_by_user(&feed, user, piggybank_core::ports::operations::MAX_PAGE)
		.await
		.expect("the feed reads a zero-unit fee row");
	let fee_rows: Vec<_> = page.operations.iter().filter(|op| op.kind() == "fee").collect();
	assert_eq!(fee_rows.len(), 1, "the deferred charge is on the statement");
	let piggybank_core::ports::operations::Operation::FeeCharge {
		units: fed_units,
		cash: fed_cash,
		management: fed_management,
		deferred,
		..
	} = fee_rows[0]
	else {
		panic!("expected a fee row, got {}", fee_rows[0].kind());
	};
	assert_eq!((*fed_units, *fed_cash), (Shares::ZERO, Usdt::ZERO));
	assert_close(*fed_management, usdt("100"), "the feed carries what was owed, not only what was taken");
	assert!(*deferred, "the feed flags the charge as deferred");
}

/// The other way a holder's units can be theirs yet untouchable: a queued redemption
/// reserves them as a pending burn, so the holding's available balance is zero while its
/// posted balance is not. The fee is still owed on all of them — and, the fund having been
/// marked up to make the redemption queue, the performance leg is owed on all of them too.
/// Nothing is collected while the reserve stands; the debt is taken the moment the
/// redemption is cancelled and the units are free again.
#[tokio::test]
async fn units_reserved_by_a_queued_redemption_are_charged_as_debt_and_the_clock_moves() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;

	// Mark the fund up to NAV 2.0: the units are now worth more than the cash behind them,
	// so a full exit cannot settle and genuinely QUEUES — reserving every unit as a pending
	// burn. Posted stays 1000; available drops to nothing.
	let claim = cash_of(&h, LedgerAccountKey::ServiceClaim(service.clone())).await;
	funds_app::record_valuation(&h.nav, h.ledger.as_ref(), ValuationId::new(), service.clone(), claim.checked_add(claim).unwrap(), "itest")
		.await
		.unwrap();
	funds_app::request_redemption(&fund_ports(&h), &h.reds, user, service.clone(), shares("1000"), now_unix())
		.await
		.unwrap();
	h.relay.drain().await;
	let holding = h.ledger.balance(&LedgerAccountKey::UserShares(service.clone(), user)).await.unwrap();
	assert_eq!(
		(Shares::from_base_units(holding.posted), Shares::from_base_units(holding.available())),
		(shares("1000"), Shares::ZERO),
		"reserved, not burned"
	);

	backdate(&h, user, &service, YEAR + PERIOD_MARGIN).await;
	let owed_since = accrued_at(&h, user, &service).await;
	let charge = assess(&h, user, &service).await.expect("the year is owed on the reserved position and must be recorded");
	// Management: 2% of the 1000 invested = 20, or 10 units at NAV 2.0, leaving 990 in
	// scope. Performance: 20% of the 1.0 gain on those 990 = 198 — on the WHOLE position,
	// reserve included; a charge measured on the available balance would find no units to
	// have gained anything.
	assert_close(charge.management, usdt("20"), "a year of management on 1000 invested");
	assert_close(charge.performance, usdt("198"), "the gain on every unit the holder owns, reserved or not");
	assert_eq!(charge.charged_units, Shares::ZERO, "nothing was collectable");
	assert_close(charge.debt_carried, usdt("218"), "the whole charge is carried as debt");

	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await,
		shares("1000"),
		"the reserve was not drawn on"
	);
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, Shares::ZERO, "no units reached the fee account");
	let position = h.accruals.find(user, &service).await.unwrap().unwrap();
	assert_close(position.debt, usdt("218"), "the debt is on the position");
	assert!(position.accrued_at_unix > owed_since, "the clock moved: the year has been assessed");
	assert_eq!(position.high_water_mark, Nav::parse_decimal("2").unwrap(), "the mark ratcheted with the crystallization");

	// The investor changes their mind; the reserve is released and the debt is collected
	// — at NAV 2.0, so 109 units for the 218 owed — with only the seconds since on top.
	let queued = piggybank_core::ports::redemptions::RedemptionRepository::list_queued(&h.reds).await.unwrap();
	let mine = queued.iter().find(|q| q.service == service).expect("the redemption is queued");
	funds_app::cancel_redemption(&h.reds, &h.notify, mine.id, user).await.unwrap();
	h.relay.drain().await;
	let next = assess(&h, user, &service).await.expect("the carried debt is collected once the units are free");
	assert_close(next.debt_opening, usdt("218"), "the debt came in");
	assert!(next.performance.is_zero(), "the mark already sits at 2.0, so there is no new gain to charge");
	assert!(
		next.management < usdt("0.01"),
		"only the seconds since the deferred charge, not the year again: {}",
		next.management
	);
	assert_close(next.charged_cash, usdt("218"), "and went out in units");
	assert_close(
		Nav::parse_decimal("2")
			.unwrap()
			.value(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await)
			.unwrap(),
		usdt("1782"),
		"1000 units less 109 taken, at 2.0",
	);
}

/// Units parked in the book are still measured at the fund's mark: a holder whose whole
/// position rests in a sell order owes the performance leg on the gain of every unit,
/// exactly as if the order had never been placed. Pins the escrow addend in the
/// position read — a fee measured on the holding alone would see no units to have gained.
#[tokio::test]
async fn units_escrowed_by_a_resting_sell_still_owe_performance_on_their_gain() {
	let _no_sweeping = no_sweeping().await;
	let Some(h) = harness().await else { return };
	let book = Book::new(&h);
	let user = book.provisioned_user().await;
	let service = unique_service();
	open_fund(&h, &service).await;
	book.open(&h, &service).await;
	fund_user(&h, user, "1000").await;
	subscribe(&h, user, &service, "1000").await;
	let _ask = book.rest_sell(&h, user, &service, "1000").await;
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), user)).await,
		Shares::ZERO,
		"every unit is in the escrow"
	);

	// The fund doubles while the ask rests.
	let claim = cash_of(&h, LedgerAccountKey::ServiceClaim(service.clone())).await;
	funds_app::record_valuation(&h.nav, h.ledger.as_ref(), ValuationId::new(), service.clone(), claim.checked_add(claim).unwrap(), "itest")
		.await
		.unwrap();
	backdate(&h, user, &service, YEAR + PERIOD_MARGIN).await;

	let charge = assess(&h, user, &service).await.expect("the year is owed on the escrowed position");
	assert_close(charge.management, usdt("20"), "a year of management on 1000 invested");
	assert_close(
		charge.performance,
		usdt("198"),
		"20% of the 1.0 gain on the 990 units net of management — all of them in the book",
	);
	assert_eq!(charge.charged_units, Shares::ZERO);
	assert_close(charge.debt_carried, usdt("218"), "owed in full, collected not at all");
	assert_eq!(
		units_of(&h, LedgerAccountKey::BookShares(service.clone(), user)).await,
		shares("1000"),
		"the escrow is the book's, not the fee's"
	);
}
