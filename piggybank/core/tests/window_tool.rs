//! Integration tests for the migration-window tool — real Postgres **and** TigerBeetle (no
//! mocks). They run when `DATABASE_URL` is set and a TigerBeetle replica is reachable, and
//! skip otherwise (a failure under CI).
//!
//! One scenario walks the window in the order the runbook does, on the production shapes:
//! `service_arb` with two holders (13 000 / 3 250 units, no cash, a fee class to burn), a
//! cash-holding product, and cash on the reserved `fee` allocation. Pinned: every step is a
//! dry run until `--yes`, every step is idempotent, the anchor reads the same after each, the
//! cash totals never move, and what lands where is what plan §6.5 says.

use std::sync::Arc;

use domain::{
	allocations::{Allocation, AllocationIcon, AllocationId},
	auth::AuthSubject,
	balance::{LedgerAccountKey, ServiceId, TransferCode},
	issuance::{IdempotencyKey, UnitHolder},
	money::{Network, Shares},
	scopes::{ScopeAccountLabel, ScopeId},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{funds as funds_app, issuance as issuance_app},
	infrastructure::{
		allocations::PgAllocations,
		custody::StubCustody,
		issuance::PgUnitIssuances,
		nav::PgNav,
		operations,
		relay::Relay,
		users::PgUsers,
		window_export,
		window_tool::{Anchor, Mode, SingleRun, Window, burn, close, drain, maintenance, reconcile, remap},
	},
	ports::{
		AllocationRegistry, UserRepository,
		ledger::{HoldingScope, Ledger, LedgerTransfer},
	},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

const UNIT: u128 = 1_000_000_000_000_000_000;

async fn user(users: &PgUsers) -> UserId {
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("u{}@example.com", Uuid::new_v4().simple())).unwrap();
	users.provision(subject, email, true).await.unwrap().id()
}

fn svc(slug: &str) -> ServiceId {
	ServiceId::parse(slug).unwrap()
}

fn scope_account(scope: &str, label: &str) -> LedgerAccountKey {
	LedgerAccountKey::ScopeAccount(ScopeId::parse(scope).unwrap(), ScopeAccountLabel::parse(label).unwrap())
}

async fn posted(ledger: &dyn Ledger, key: &LedgerAccountKey) -> u128 {
	ledger.balance(key).await.unwrap().posted
}

/// Cash arriving on a claim from custody, the way a deposit does: balanced, so the cash
/// invariant is untouched.
async fn fund_claim(ledger: &dyn Ledger, claim: &ServiceId, whole_usdt: u128) {
	ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: LedgerAccountKey::ServiceClaim(claim.clone()),
			amount: whole_usdt * UNIT,
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.unwrap();
}

struct World {
	pool: PgPool,
	ledger: Arc<dyn Ledger>,
	relay: Relay,
	notify: Arc<Notify>,
	allocations: PgAllocations,
	users: PgUsers,
	issuances: PgUnitIssuances,
	nav: PgNav,
}

impl World {
	async fn issue(&self, service: &ServiceId, holder: UnitHolder, whole_units: &str) {
		let ports = funds_app::FundPorts {
			allocations: &self.allocations,
			ledger: self.ledger.as_ref(),
			nav: &self.nav,
			relay: &self.notify,
		};
		let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
		issuance_app::issue_units(
			&ports,
			&self.issuances,
			&self.users,
			issuance_app::IssueUnitsRequest {
				service: service.clone(),
				holder,
				units: Shares::parse_decimal(whole_units).unwrap(),
				cost_basis: None,
				idempotency_key: IdempotencyKey::parse(&Uuid::new_v4().to_string()).unwrap(),
			},
			now,
		)
		.await
		.unwrap();
	}

	async fn register(&self, slug: &str) -> ServiceId {
		let service = svc(slug);
		let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "T", "window tool test", AllocationIcon::default()).unwrap();
		self.allocations.register(&mut allocation).await.unwrap();
		service
	}

	async fn relay_drain(&self) {
		common::drain_to_quiescence(&self.relay, &self.pool).await;
	}
}

#[tokio::test]
async fn the_window_walks_service_arb_through_every_step_with_the_anchor_unmoved() {
	let _serial = common::outbox_serial().await;
	let Some(pool) = common::pool().await else { return };
	let Some(ledger) = common::seeded_ledger(&pool, "window-tool test").await else { return };
	let notify = Arc::new(Notify::new());
	let world = World {
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		allocations: PgAllocations::new(pool.clone()),
		users: PgUsers::new(pool.clone()),
		issuances: PgUnitIssuances::new(pool.clone()),
		nav: PgNav::new(pool.clone()),
		pool: pool.clone(),
		ledger: ledger.clone(),
		notify,
	};

	// The production shapes: service_arb with its two clients and no cash, a fee class that
	// is the `fee` allocation's, and a product that does hold cash.
	let service_arb = world.register("service_arb").await;
	let cash_product = world.register(&format!("svc-{}", Uuid::new_v4())).await;
	let (valera, inintel, investor) = (user(&world.users).await, user(&world.users).await, user(&world.users).await);
	let charged = user(&world.users).await;
	world.issue(&service_arb, UnitHolder::User(valera), "13000").await;
	world.issue(&service_arb, UnitHolder::User(inintel), "3250").await;
	world.issue(&service_arb, UnitHolder::User(charged), "500").await;
	world.issue(&cash_product, UnitHolder::User(investor), "40").await;
	world.issue(&cash_product, UnitHolder::User(charged), "3").await;
	world.relay_drain().await;
	// A fee is units moving from a holder to the product's fee class; no mint is allowed to
	// make one by hand. After it `charged` holds nothing, so the anchor is the two clients.
	for (product, units) in [(&service_arb, 500), (&cash_product, 3)] {
		ledger
			.post(&LedgerTransfer {
				id: Uuid::new_v4().as_u128(),
				debit: LedgerAccountKey::FeeShares(product.clone()),
				credit: LedgerAccountKey::UserShares(product.clone(), charged),
				amount: units * UNIT,
				code: TransferCode::FeeClawback,
				reference: 0,
			})
			.await
			.unwrap();
	}
	fund_claim(ledger.as_ref(), &cash_product, 1000).await;
	fund_claim(ledger.as_ref(), &ServiceId::fee(), 200).await;

	let window = Window {
		pool: pool.clone(),
		ledger: ledger.clone(),
		anchor: Anchor::new(service_arb.clone(), vec![3250 * UNIT, 13_000 * UNIT]),
	};
	let cash0 = ledger.cash_invariant().await.unwrap();

	// Not in maintenance: the steps refuse rather than run on a hub that is taking writes.
	assert!(
		drain::run(
			&window,
			Mode::DryRun,
			drain::Options {
				release_queued_redemptions: false,
				wait: std::time::Duration::ZERO
			}
		)
		.await
		.is_err()
	);
	assert!(remap::run(&window, Mode::DryRun, remap::Options::default()).await.is_err());

	// One tool at a time.
	let first = SingleRun::acquire(&pool).await.unwrap();
	assert!(SingleRun::acquire(&pool).await.is_err(), "a second run must be refused while the first holds the lock");
	drop(first);

	// Step 1: maintenance, idempotent.
	assert!(maintenance::on(&window).await.unwrap().ok);
	let again = maintenance::on(&window).await.unwrap();
	assert!(again.ok && again.text.contains("already on"), "{}", again.text);
	let status = maintenance::status(&window).await.unwrap();
	assert!(status.ok && status.text.contains("16250"), "{}", status.text);

	// Step 1: drain waits out the outbox. A mint recorded now is undispatched, so the window
	// is not drained until the relay has it.
	let late = user(&world.users).await;
	world.issue(&cash_product, UnitHolder::User(late), "1").await;
	let options = drain::Options {
		release_queued_redemptions: false,
		wait: std::time::Duration::ZERO,
	};
	let pending = drain::run(&window, Mode::Apply, options).await.unwrap();
	assert!(
		!pending.ok && pending.text.contains("outbox undispatched 1") && pending.text.contains("NOT DRAINED"),
		"{}",
		pending.text
	);
	world.relay_drain().await;
	let drained = drain::run(&window, Mode::Apply, options).await.unwrap();
	assert!(drained.ok && drained.text.contains("DRAINED"), "{}", drained.text);

	// Before the export: the fee class of service_arb is burnt. A dry run changes nothing.
	let fee_class = LedgerAccountKey::FeeShares(service_arb.clone());
	assert_eq!(posted(ledger.as_ref(), &fee_class).await, 500 * UNIT);
	let dry = burn::run(&window, Mode::DryRun, std::time::Duration::ZERO).await.unwrap();
	assert!(dry.ok && dry.text.contains("500"), "{}", dry.text);
	assert_eq!(posted(ledger.as_ref(), &fee_class).await, 500 * UNIT, "a dry run burns nothing");
	// Applied but not yet posted by a relay: reported as queued, not as done.
	let queued = burn::run(&window, Mode::Apply, std::time::Duration::ZERO).await.unwrap();
	assert!(!queued.ok && queued.text.contains("not applied yet"), "{}", queued.text);
	let again = burn::run(&window, Mode::Apply, std::time::Duration::ZERO).await.unwrap();
	assert!(again.text.contains("retirement recorded"), "the same request again finds its row, {}", again.text);
	world.relay_drain().await;
	let done = burn::run(&window, Mode::Apply, std::time::Duration::ZERO).await.unwrap();
	assert!(done.ok, "{}", done.text);
	assert_eq!(posted(ledger.as_ref(), &fee_class).await, 0);
	assert_eq!(
		posted(ledger.as_ref(), &LedgerAccountKey::SharesOutstanding(service_arb.clone())).await,
		16_250 * UNIT,
		"supply is the clients' units alone"
	);
	let issuance_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM unit_issuances WHERE service = $1 AND source = 'retire'")
		.bind(service_arb.as_str())
		.fetch_one(&pool)
		.await
		.unwrap();
	assert_eq!(issuance_rows, 1, "one retirement, however many times the step ran");
	// The cash product's fee class is not an sa product's: untouched.
	assert_eq!(posted(ledger.as_ref(), &LedgerAccountKey::FeeShares(cash_product.clone())).await, 3 * UNIT);

	// Step 2: the export, then the stale-file guard of step 5.
	let mut file = Vec::new();
	window_export::export(&pool, ledger.as_ref(), "0", false, &mut file).await.unwrap();
	let early_export = close::parse_export(&String::from_utf8(file).unwrap()).unwrap();
	let later = user(&world.users).await;
	world.issue(&cash_product, UnitHolder::User(later), "1").await;
	world.relay_drain().await;
	let stale = close::run(&window, Mode::Apply, &early_export).await.unwrap_err();
	assert!(format!("{stale:#}").contains("does not list"), "{stale:#}");
	assert_eq!(
		posted(ledger.as_ref(), &LedgerAccountKey::SharesOutstanding(service_arb.clone())).await,
		16_250 * UNIT,
		"a refused close writes nothing"
	);

	let mut file = Vec::new();
	window_export::export(&pool, ledger.as_ref(), "0", false, &mut file).await.unwrap();
	let export = close::parse_export(&String::from_utf8(file).unwrap()).unwrap();

	// Step 4: remap. The dry run moves nothing; the treasury carve-out cannot exceed `fee`.
	let treasury_before = posted(ledger.as_ref(), &scope_account("main", "treasury")).await;
	let dry = remap::run(&window, Mode::DryRun, remap::Options { treasury_from_fee: 50 * UNIT }).await.unwrap();
	assert!(dry.ok && dry.text.contains("sa/alloc:service_arb") && dry.text.contains("main/treasury 50"), "{}", dry.text);
	assert_eq!(posted(ledger.as_ref(), &LedgerAccountKey::ServiceClaim(cash_product.clone())).await, 1000 * UNIT);
	let too_much = remap::run(&window, Mode::DryRun, remap::Options { treasury_from_fee: 201 * UNIT }).await.unwrap_err();
	assert!(format!("{too_much:#}").contains("more than service:fee holds"), "{too_much:#}");

	let remapped = remap::run(&window, Mode::Apply, remap::Options { treasury_from_fee: 50 * UNIT }).await.unwrap();
	assert!(remapped.ok, "{}", remapped.text);
	let invest_alloc = |slug: &str| scope_account("invest", &format!("alloc:{slug}"));
	assert_eq!(posted(ledger.as_ref(), &LedgerAccountKey::ServiceClaim(cash_product.clone())).await, 0);
	assert_eq!(posted(ledger.as_ref(), &invest_alloc(cash_product.as_str())).await, 1000 * UNIT);
	assert_eq!(posted(ledger.as_ref(), &LedgerAccountKey::ServiceClaim(ServiceId::fee())).await, 0);
	assert_eq!(posted(ledger.as_ref(), &invest_alloc("fee")).await, 150 * UNIT);
	assert_eq!(posted(ledger.as_ref(), &scope_account("main", "treasury")).await - treasury_before, 50 * UNIT);
	assert_eq!(
		posted(ledger.as_ref(), &scope_account("sa", "alloc:service_arb")).await,
		0,
		"service_arb has no cash, and its account exists"
	);
	let owners: Vec<(String, String, String)> = sqlx::query_as("SELECT scope, label, owner_client FROM scope_accounts WHERE label LIKE 'alloc:%' ORDER BY scope, label")
		.fetch_all(&pool)
		.await
		.unwrap();
	assert!(owners.iter().all(|(_, _, owner)| owner == "invest"), "{owners:?}");
	assert!(owners.iter().any(|(scope, label, _)| scope == "sa" && label == "alloc:service_arb"), "{owners:?}");
	let cash1 = ledger.cash_invariant().await.unwrap();
	assert_eq!(
		(cash1.custody, cash1.claims),
		(cash0.custody, cash0.claims),
		"the remap changes whose claim it is, never how much"
	);
	let rerun = remap::run(&window, Mode::Apply, remap::Options::default()).await.unwrap();
	assert!(rerun.ok && rerun.text.contains("Σ to move 0"), "{}", rerun.text);
	assert_eq!(posted(ledger.as_ref(), &invest_alloc("fee")).await, 150 * UNIT, "a rerun moves nothing");

	// Step 5: close. The dry run writes nothing; the real one empties every share account
	// against the supply, and the anchor reads the same units from the closure record.
	let before = window.read_anchor().await.unwrap();
	assert_eq!((before.live, before.closed), (16_250 * UNIT, 0));
	let dry = close::run(&window, Mode::DryRun, &export).await.unwrap();
	assert!(dry.ok && dry.text.contains("dry run"), "{}", dry.text);
	let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM window_closures").fetch_one(&pool).await.unwrap();
	assert_eq!(rows, 0);
	let closed = close::run(&window, Mode::Apply, &export).await.unwrap();
	assert!(closed.ok, "{}", closed.text);
	for service in [&service_arb, &cash_product] {
		assert!(ledger.share_holdings(&HoldingScope::Product(service.clone())).await.unwrap().iter().all(|(_, units)| *units == 0));
		assert_eq!(posted(ledger.as_ref(), &LedgerAccountKey::SharesOutstanding(service.clone())).await, 0);
	}
	let after = window.read_anchor().await.unwrap();
	assert_eq!((after.live, after.closed), (0, 16_250 * UNIT));
	assert_eq!(after.total(), before.total());
	after.same_holders_as(&before).unwrap();
	let rows: Vec<(String, i32, String)> = sqlx::query_as("SELECT kind, code, units FROM window_closures WHERE service = $1 ORDER BY units::numeric")
		.bind(service_arb.as_str())
		.fetch_all(&pool)
		.await
		.unwrap();
	assert_eq!(
		rows,
		[
			("user_shares".to_owned(), 60, (3250 * UNIT).to_string()),
			("user_shares".to_owned(), 60, (13_000 * UNIT).to_string())
		]
	);
	let cash2 = ledger.cash_invariant().await.unwrap();
	assert_eq!((cash2.custody, cash2.claims), (cash0.custody, cash0.claims));
	let rerun = close::run(&window, Mode::Apply, &export).await.unwrap();
	assert!(rerun.ok && rerun.text.contains("close 0 account(s)"), "{}", rerun.text);

	// A crash between the ledger and the rows: the rows are rebuilt from the ledger, and a
	// dry run says so without writing them.
	sqlx::query("DELETE FROM window_closures WHERE service = $1")
		.bind(service_arb.as_str())
		.execute(&pool)
		.await
		.unwrap();
	assert!(
		window.read_anchor().await.is_err(),
		"with the records gone the closed units are not visible, and the anchor says so"
	);
	let dry = close::run(&window, Mode::DryRun, &export).await.unwrap();
	assert!(dry.text.contains("have no row") && dry.text.contains("--yes writes them"), "{}", dry.text);
	let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM window_closures WHERE service = $1")
		.bind(service_arb.as_str())
		.fetch_one(&pool)
		.await
		.unwrap();
	assert_eq!(rows, 0, "a dry run writes nothing");
	let repaired = close::run(&window, Mode::Apply, &export).await.unwrap();
	assert!(repaired.ok && repaired.text.contains("written from the ledger"), "{}", repaired.text);
	window.read_anchor().await.unwrap().same_holders_as(&before).unwrap();

	// Step 6: the window is over.
	let last = reconcile::run(&window, true).await.unwrap();
	assert!(last.ok, "{}", last.text);

	operations::set_read_only(&pool, false).await.unwrap();
}
