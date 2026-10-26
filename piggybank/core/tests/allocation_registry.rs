//! Integration tests for the allocation registry — real Postgres **and** TigerBeetle
//! (no mocks, per the project rules). They run when `DATABASE_URL` is set and a
//! TigerBeetle replica is reachable (e.g. `nix run .#db` + `.#tb`), and skip otherwise.
//!
//! The load-bearing behaviour here is the **gate**: before this registry existed,
//! `subscribe` accepted any well-formed slug and a service with no valuation bootstrapped
//! silently at the seed NAV — so a user could mint a fund by typing an unknown id. These
//! tests pin that door shut, and pin the one door that must stay open: a `closed`
//! allocation still redeems, so winding a product down never traps an investor.

use std::sync::Arc;

use domain::{
	allocations::{Allocation, AllocationAccess, AllocationBacking, AllocationEvent, AllocationIcon, AllocationId, AllocationState},
	auth::AuthSubject,
	balance::{LedgerAccountKey, ServiceId},
	error::DomainError,
	issuance::{IdempotencyKey, IssuanceSource, IssuanceState, UnitHolder, UnitIssuance, UnitIssuanceId},
	money::{Nav, Network, Shares, TxRef, Usdt},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{allocations as allocations_app, balance as balance_app, funds as funds_app, issuance as issuance_app},
	infrastructure::{
		allocations::PgAllocations, custody::StubCustody, deposits::PgDeposits, issuance::PgUnitIssuances, nav::PgNav, positions::PgFundPositions, redemptions::PgRedemptions, relay::Relay,
		subscriptions::PgSubscriptions, users::PgUsers,
	},
	ports::{
		AllocationRegistry, FundPositionReader, UnitIssuanceRepository, UserRepository,
		issuance::{IssueOutcome, StoredIssuance, UnitIssuanceRecord},
		ledger::Ledger,
	},
};
use sqlx::{
	AssertSqlSafe, PgPool,
	postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::sync::{MutexGuard, Notify};
use uuid::Uuid;

mod common;

struct Harness {
	pool: PgPool,
	allocations: PgAllocations,
	users: PgUsers,
	subs: PgSubscriptions,
	reds: PgRedemptions,
	issuances: PgUnitIssuances,
	positions: PgFundPositions,
	nav: PgNav,
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
	let ledger = common::seeded_ledger(&pool, "allocation-registry test").await?;

	let notify = Arc::new(Notify::new());
	Some(Harness {
		allocations: PgAllocations::new(pool.clone()),
		users: PgUsers::new(pool.clone()),
		subs: PgSubscriptions::new(pool.clone()),
		reds: PgRedemptions::new(pool.clone()),
		issuances: PgUnitIssuances::new(pool.clone()),
		positions: PgFundPositions::new(pool.clone()),
		nav: PgNav::new(pool.clone()),
		deposits: PgDeposits::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		ledger,
		notify,
		pool,
		_serial: serial,
	})
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
	ServiceId::parse(&format!("svc-{}", Uuid::new_v4())).unwrap()
}

fn now_unix() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

async fn register(h: &Harness, service: &ServiceId) -> Allocation {
	register_with_icon(h, service, AllocationIcon::default()).await
}

/// Open for business AND to everyone: the two operator decisions a product dealing with
/// the public needs. The lifecycle tests below are about `state`, so they take both.
async fn open_to_everyone(h: &Harness, service: &ServiceId) {
	h.allocations.open(service).await.unwrap();
	h.allocations.set_access(service, AllocationAccess::Invest).await.unwrap();
}

/// A real `users` row — grants reference the table, as they do in production.
async fn provisioned_user(h: &Harness) -> UserId {
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("u{}@example.com", Uuid::new_v4().simple())).unwrap();
	h.users.provision(subject, email, true).await.unwrap().id()
}

async fn register_with_icon(h: &Harness, service: &ServiceId, icon: AllocationIcon) -> Allocation {
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "EV Trading", "Systematic crypto", icon).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	allocation
}

/// Credit `amount` to the user's unified claim so a subscribe has money to move.
async fn fund_user(h: &Harness, user: UserId, amount: &str) {
	let tx_ref = TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	balance_app::record_deposit(&h.deposits, &h.notify, tx_ref, user, Network::Bep20, usdt(amount)).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

async fn subscribe(h: &Harness, user: UserId, service: &ServiceId, amount: &str) -> Result<(), domain::error::DomainError> {
	funds_app::subscribe(&fund_ports(h), &h.subs, user, service.clone(), usdt(amount), now_unix()).await.map(|_| ())
}

/// An operator's in-kind mint through the use case, keyed by `key`.
async fn issue(h: &Harness, service: &ServiceId, holder: UnitHolder, units: &str, cost_basis: Option<&str>, key: &str) -> Result<UnitIssuanceRecord, DomainError> {
	issuance_app::issue_units(
		&fund_ports(h),
		&h.issuances,
		&h.users,
		issuance_app::IssueUnitsRequest {
			service: service.clone(),
			holder,
			units: shares(units),
			cost_basis: cost_basis.map(usdt),
			idempotency_key: IdempotencyKey::parse(key).unwrap(),
		},
		now_unix(),
	)
	.await
}

/// The mint into a RESERVED allocation — no RPC reaches it; the operator's `issue_units`
/// refuses it.
async fn grant(h: &Harness, service: &ServiceId, holder: UnitHolder, units: &str, cost_basis: Option<&str>, key: &str) -> Result<UnitIssuanceRecord, DomainError> {
	issuance_app::grant_units(
		&fund_ports(h),
		&h.issuances,
		&h.users,
		issuance_app::IssueUnitsRequest {
			service: service.clone(),
			holder,
			units: shares(units),
			cost_basis: cost_basis.map(usdt),
			idempotency_key: IdempotencyKey::parse(key).unwrap(),
		},
		now_unix(),
	)
	.await
}

/// The person the asset behind an in-kind product belongs to — minted the 80 % of a
/// product registered against it. What the company holder used to be in these scenarios
/// (#245): the platform's stake in an asset is a person's, and the `fee` allocation holds
/// a product's units only as its fee class, which the accrual mints and nobody issues.
async fn asset_owner(h: &Harness) -> UnitHolder {
	UnitHolder::User(provisioned_user(h).await)
}

/// The person inside an [`asset_owner`] holder.
fn user_of(holder: &UnitHolder) -> UserId {
	match holder {
		UnitHolder::User(user) => *user,
		other => panic!("not a person: {other:?}"),
	}
}

/// The cap table's units for one holder, summed over its lines.
fn units_held(holders: &issuance_app::UnitHoldersView, holder: &UnitHolder) -> Shares {
	holders
		.holders
		.iter()
		.filter(|line| &line.holder == holder)
		.fold(Shares::ZERO, |acc, line| acc.checked_add(line.units).unwrap())
}

/// The cap table's units for one class of holder — a person, the fee allocation, or the
/// retired company stake — summed over its lines, so the assertions below read as the
/// three-line summary the wire still carries.
fn units_held_by(holders: &issuance_app::UnitHoldersView, pick: fn(&UnitHolder) -> bool) -> Shares {
	holders
		.holders
		.iter()
		.filter(|line| pick(&line.holder))
		.fold(Shares::ZERO, |acc, line| acc.checked_add(line.units).unwrap())
}

fn investor_units(holders: &issuance_app::UnitHoldersView) -> Shares {
	units_held_by(holders, |holder| matches!(holder, UnitHolder::User(_)))
}

fn fee_units(holders: &issuance_app::UnitHoldersView) -> Shares {
	units_held_by(holders, |holder| matches!(holder, UnitHolder::Allocation(a) if *a == ServiceId::fee()))
}

async fn units_of(h: &Harness, key: LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

#[tokio::test]
async fn an_unregistered_service_cannot_be_subscribed_into() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	fund_user(&h, user, "100").await;

	// The regression this whole feature exists for: a well-formed but unknown slug used
	// to mint a fund at the seed NAV. It must now be refused outright.
	let err = subscribe(&h, user, &service, "100").await.unwrap_err();
	assert!(matches!(err, domain::error::DomainError::NotFound { entity: "allocation", .. }), "got {err:?}");

	// And it must be refused BEFORE any money moves — no claim spent, no service claim born.
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let user_claim = h.ledger.balance(&LedgerAccountKey::UserClaim(user)).await.unwrap();
	assert_eq!(Usdt::from_base_units(user_claim.available()), usdt("100"), "the user's balance is untouched");
	let service_claim = h.ledger.balance(&LedgerAccountKey::ServiceClaim(service.clone())).await.unwrap();
	assert_eq!(Usdt::from_base_units(service_claim.posted), Usdt::ZERO, "no phantom fund claim was created");
}

#[tokio::test]
async fn a_draft_allocation_takes_no_money_until_opened() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	fund_user(&h, user, "100").await;
	register(&h, &service).await;

	// Registered but still `draft` — listing and funding are separate operator decisions.
	assert!(subscribe(&h, user, &service, "50").await.is_err(), "a draft must not accept money");

	open_to_everyone(&h, &service).await;
	subscribe(&h, user, &service, "50").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let held = h.ledger.balance(&LedgerAccountKey::UserShares(service.clone(), user)).await.unwrap();
	assert_eq!(Shares::from_base_units(held.posted), shares("50"), "units minted at the seed NAV once open");
}

#[tokio::test]
async fn closing_stops_new_money_but_never_traps_an_investor() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	fund_user(&h, user, "100").await;
	register(&h, &service).await;
	open_to_everyone(&h, &service).await;
	subscribe(&h, user, &service, "100").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	h.allocations.close(&service).await.unwrap();
	assert!(subscribe(&h, user, &service, "1").await.is_err(), "a closed allocation refuses new subscriptions");

	// The asymmetry that matters: the investor can still get out. Refusing here would
	// lock 100 units of real money inside a wound-down product.
	let redemption = funds_app::request_redemption(&fund_ports(&h), &h.reds, user, service.clone(), shares("100"), now_unix())
		.await
		.expect("a closed allocation must still redeem");
	assert_eq!(redemption.service(), &service);
}

#[tokio::test]
async fn registering_twice_is_a_conflict_not_a_silent_reset() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;
	h.allocations.open(&service).await.unwrap();

	let mut duplicate = Allocation::register(AllocationId::new(), service.clone(), "Impostor", "", AllocationIcon::Venture).unwrap();
	let err = h.allocations.register(&mut duplicate).await.unwrap_err();
	assert!(matches!(err, domain::error::DomainError::Conflict(_)), "got {err:?}");

	// The live product kept its identity and, crucially, its open state.
	let current = h.allocations.find(&service).await.unwrap().unwrap();
	assert_eq!(current.title(), "EV Trading");
	assert_eq!(current.state(), AllocationState::Open);
	assert_eq!(current.icon(), AllocationIcon::default(), "the impostor's icon did not overwrite the live card");
}

#[tokio::test]
async fn transitions_persist_and_are_idempotent() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;

	assert_eq!(h.allocations.open(&service).await.unwrap().state(), AllocationState::Open);
	assert_eq!(h.allocations.open(&service).await.unwrap().state(), AllocationState::Open, "re-opening is a no-op");
	assert_eq!(h.allocations.close(&service).await.unwrap().state(), AllocationState::Closed);
	assert_eq!(h.allocations.close(&service).await.unwrap().state(), AllocationState::Closed, "re-closing is a no-op");
	assert_eq!(h.allocations.open(&service).await.unwrap().state(), AllocationState::Open, "closed reopens");

	let updated = h.allocations.update_details(&service, " Renamed ", " New summary ", Some(AllocationIcon::Yield)).await.unwrap();
	assert_eq!(updated.title(), "Renamed");
	assert_eq!(updated.summary(), "New summary");
	assert_eq!(updated.state(), AllocationState::Open, "a details edit leaves state alone");
}

#[tokio::test]
async fn transitions_on_an_unregistered_service_are_not_found() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	assert!(h.allocations.find(&service).await.unwrap().is_none());
	for result in [
		h.allocations.open(&service).await,
		h.allocations.close(&service).await,
		h.allocations.update_details(&service, "x", "", Some(AllocationIcon::default())).await,
	] {
		assert!(matches!(result, Err(domain::error::DomainError::NotFound { entity: "allocation", .. })));
	}
}

#[tokio::test]
async fn the_catalog_hides_drafts_and_closed_from_investors() {
	let Some(h) = harness().await else { return };
	let draft = unique_service();
	let open = unique_service();
	let closed = unique_service();
	for service in [&draft, &open, &closed] {
		register(&h, service).await;
	}
	h.allocations.open(&open).await.unwrap();
	h.allocations.open(&closed).await.unwrap();
	h.allocations.close(&closed).await.unwrap();

	let listed: Vec<String> = h
		.allocations
		.list_for(UserId::new(), false)
		.await
		.unwrap()
		.iter()
		.map(|r| r.allocation.service().to_string())
		.collect();
	assert!(listed.contains(&open.to_string()), "an open allocation is in the investor catalog");
	assert!(!listed.contains(&draft.to_string()), "a draft is hidden");
	assert!(!listed.contains(&closed.to_string()), "a closed allocation is hidden");

	let all: Vec<String> = h
		.allocations
		.list_for(UserId::new(), true)
		.await
		.unwrap()
		.iter()
		.map(|r| r.allocation.service().to_string())
		.collect();
	for service in [&draft, &open, &closed] {
		assert!(all.contains(&service.to_string()), "include_unlisted surfaces {service}");
	}
}

#[tokio::test]
async fn registration_is_audited_but_never_reaches_the_relay() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let allocation = register(&h, &service).await;

	// The fact is in the append-only audit log…
	let logged: i64 = sqlx::query_scalar("SELECT count(*) FROM event_log WHERE aggregate = 'allocation' AND aggregate_id = $1")
		.bind(allocation.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(logged, 1);

	// …and deliberately NOT in the outbox: an allocation moves no value, so the relay has
	// no ledger op for it and would park the row.
	let relayed: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE aggregate = 'allocation'").fetch_one(&h.pool).await.unwrap();
	assert_eq!(relayed, 0, "allocation events must never enter the outbox");
}

#[tokio::test]
async fn a_subscription_past_the_unit_cap_is_refused_before_any_money_moves() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	fund_user(&h, user, "200").await;
	register(&h, &service).await;
	open_to_everyone(&h, &service).await;
	// Sized to a hundred units — the whole point of the cap is that "open" and "unbounded"
	// are different things.
	h.allocations.set_unit_cap(&service, shares("100")).await.unwrap();

	// At the seed NAV of 1.0 this mints exactly the cap. Landing on it is allowed.
	subscribe(&h, user, &service, "100").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let err = subscribe(&h, user, &service, "1").await.unwrap_err();
	assert!(matches!(err, domain::error::DomainError::Validation(ref m) if m.contains("unit cap")), "got {err:?}");

	// Refused before the ledger, like the registry gate above it: the remaining 100 USDT is
	// still the user's, and the fund did not quietly issue a 101st unit.
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let claim = h.ledger.balance(&LedgerAccountKey::UserClaim(user)).await.unwrap();
	assert_eq!(Usdt::from_base_units(claim.available()), usdt("100"), "the refused subscription spent nothing");
	let outstanding = h.ledger.balance(&LedgerAccountKey::SharesOutstanding(service.clone())).await.unwrap();
	assert_eq!(Shares::from_base_units(outstanding.posted), shares("100"), "supply stopped at the cap");
}

#[tokio::test]
async fn narrowing_the_cap_below_the_issued_supply_stops_issuance_without_trapping_anyone() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	fund_user(&h, user, "100").await;
	register(&h, &service).await;
	open_to_everyone(&h, &service).await;
	subscribe(&h, user, &service, "100").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// An operator decides the product has run further than intended and pulls the cap in
	// under what is already out. Legal, and it means "no more units" — not "some units are
	// now invalid".
	h.allocations.set_unit_cap(&service, shares("10")).await.unwrap();
	assert!(subscribe(&h, user, &service, "1").await.is_err(), "no further issuance");

	// The same asymmetry the lifecycle gate has: the holder still gets out in full.
	funds_app::request_redemption(&fund_ports(&h), &h.reds, user, service.clone(), shares("100"), now_unix())
		.await
		.expect("a narrowed cap must never block a redemption");
}

#[tokio::test]
async fn the_cap_defaults_on_registration_persists_and_is_reported_with_the_nav() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;
	h.allocations.open(&service).await.unwrap();

	// Registration lands on the default rather than on "unset", so the registry always has
	// a number to show and to refuse against.
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().unit_cap(), domain::allocations::DEFAULT_UNIT_CAP);

	h.allocations.set_unit_cap(&service, shares("1000")).await.unwrap();
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().unit_cap(), shares("1000"), "the new cap survives a reload");
	// Zero is refused — it would be indistinguishable from unset while silently closing the
	// product to new money.
	assert!(h.allocations.set_unit_cap(&service, Shares::ZERO).await.is_err());

	// And the read side the screens use reports the headroom, so a client never offers
	// capacity the subscribe gate would then refuse.
	let investor = provisioned_user(&h).await;
	let view = funds_app::fund_nav_view(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), investor, false, now_unix())
		.await
		.unwrap();
	assert_eq!(view.unit_cap, shares("1000"));
	assert_eq!(view.remaining_capacity, shares("1000"), "nothing issued yet, so the whole cap is available");
}

// ── in-kind issuance ────────────────────────────────────────────────────────

#[tokio::test]
async fn units_issued_in_kind_land_on_the_holder_and_in_the_supply_with_no_cash_leg() {
	// The service_arb shape: 20% to a named investor, 80% to the person who owns the asset,
	// and not a cent of cash behind either — the product is registered against an asset
	// they already own.
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let investor = provisioned_user(&h).await;
	let owner = asset_owner(&h).await;
	register(&h, &service).await;

	let to_investor = issue(&h, &service, UnitHolder::User(investor), "3250", Some("3250"), "investor").await.unwrap();
	let to_company = issue(&h, &service, owner.clone(), "13000", Some("13000"), "company").await.unwrap();
	assert_eq!(to_investor.issuance.state(), IssuanceState::Queued, "recorded, not yet on the ledger");
	assert!(to_investor.applied_at.is_none());
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), investor)).await, shares("3250"));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user_of(&owner))).await, shares("13000"));
	assert_eq!(
		units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await,
		shares("16250"),
		"supply is the sum of both holders"
	);
	// No cash moved: the fund's claim is untouched and the investor spent nothing.
	assert_eq!(
		Usdt::from_base_units(h.ledger.balance(&LedgerAccountKey::ServiceClaim(service.clone())).await.unwrap().posted),
		Usdt::ZERO
	);
	assert_eq!(Usdt::from_base_units(h.ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap().posted), Usdt::ZERO);

	// The relay stamped both rows applied once the mint posted — never before.
	for record in [&to_investor, &to_company] {
		let applied = h.issuances.find_by_id(record.issuance.id()).await.unwrap().and_then(StoredIssuance::live).unwrap();
		assert_eq!(applied.issuance.state(), IssuanceState::Applied);
		assert!(applied.applied_at.is_some());
	}
	// Each person's projection carries the stated basis.
	let position = h.positions.find(investor, &service).await.unwrap().expect("a position for the investor");
	assert_eq!(position.cost_basis, usdt("3250"));
	assert_eq!(position.high_water_mark, Nav::SEED, "issued at the seed NAV, so the mark is 1.0");
	assert_eq!(
		h.positions.find(user_of(&owner), &service).await.unwrap().expect("a position for the owner").cost_basis,
		usdt("13000")
	);

	// The cap table the operator reads before opening the product.
	let holders = issuance_app::unit_holders(&h.allocations, h.ledger.as_ref(), &h.issuances, service.clone()).await.unwrap();
	assert_eq!(holders.units_outstanding, shares("16250"));
	// Every line is a holder read from the ledger, largest first — nobody is a remainder.
	assert_eq!(
		holders.holders,
		vec![
			issuance_app::UnitHolding {
				holder: owner.clone(),
				units: shares("13000")
			},
			issuance_app::UnitHolding {
				holder: UnitHolder::User(investor),
				units: shares("3250")
			},
		]
	);
	assert_eq!(fee_units(&holders), Shares::ZERO, "nothing was minted into the fee allocation");
	// And what the investor's own screen shows.
	let view = funds_app::fund_nav_view(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), investor, false, now_unix())
		.await
		.unwrap();
	assert_eq!(view.units_outstanding, shares("16250"));
}

#[tokio::test]
async fn a_repeated_idempotency_key_returns_the_same_issuance_and_mints_once() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let owner = asset_owner(&h).await;
	register(&h, &service).await;

	let first = issue(&h, &service, owner.clone(), "100", None, "retry-me").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// The console re-sends after a timeout: same key, same request — same row, no second mint.
	let again = issue(&h, &service, owner.clone(), "100", None, "retry-me").await.unwrap();
	assert_eq!(again.issuance.id(), first.issuance.id());
	assert_eq!(again.issuance.state(), IssuanceState::Applied, "the repeat reads the row as it stands now");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), user_of(&owner))).await,
		shares("100"),
		"one mint, not two"
	);
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("100"));

	// The same key for a DIFFERENT request is the mistake the key exists to catch.
	let err = issue(&h, &service, owner.clone(), "101", None, "retry-me").await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	// Keys are per product: another service may reuse the string freely.
	let other = unique_service();
	register(&h, &other).await;
	issue(&h, &other, owner, "5", None, "retry-me").await.unwrap();
}

#[tokio::test]
async fn an_issuance_defaults_its_cost_basis_to_units_times_nav() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let investor = provisioned_user(&h).await;
	let owner = asset_owner(&h).await;
	register(&h, &service).await;
	// Seed the supply so a valuation can be posted, then mark the fund at NAV 1.25.
	issue(&h, &service, owner, "800", Some("0"), "seed").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), usdt("1000"), "itest", now_unix())
		.await
		.unwrap();

	let record = issue(&h, &service, UnitHolder::User(investor), "200", None, "at-mark").await.unwrap();
	assert_eq!(record.issuance.nav(), Nav::parse_decimal("1.25").unwrap());
	assert_eq!(record.issuance.cost_basis(), usdt("250"), "200 units at 1.25");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let position = h.positions.find(investor, &service).await.unwrap().unwrap();
	assert_eq!(position.cost_basis, usdt("250"));
	assert_eq!(position.high_water_mark, Nav::parse_decimal("1.25").unwrap());
	// The owner's explicit zero basis was taken as given.
	let seed = h
		.issuances
		.find_by_key(&service, &IdempotencyKey::parse("seed").unwrap())
		.await
		.unwrap()
		.and_then(StoredIssuance::live)
		.unwrap();
	assert_eq!(seed.issuance.cost_basis(), Usdt::ZERO);
}

#[tokio::test]
async fn an_issuance_is_gated_by_the_registry_the_holder_and_the_cap() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let investor = provisioned_user(&h).await;
	let owner = asset_owner(&h).await;

	// Unregistered: refused before the ledger, like a subscription.
	let err = issue(&h, &service, owner.clone(), "10", None, "unregistered").await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { entity: "allocation", .. }), "got {err:?}");

	// Registered in `draft` is enough — a product is normally seeded before it opens.
	register(&h, &service).await;
	issue(&h, &service, owner.clone(), "10", None, "draft-ok").await.unwrap();

	// A user nobody can sign in as is refused: units minted to them could never be redeemed.
	let err = issue(&h, &service, UnitHolder::User(UserId::new()), "10", None, "nobody").await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { entity: "user", .. }), "got {err:?}");
	issue(&h, &service, UnitHolder::User(investor), "10", None, "somebody").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// The cap is the same gate a subscription runs, in-flight mints included: 20 issued
	// against a cap of 25 leaves room for 5, not 6.
	h.allocations.set_unit_cap(&service, shares("25")).await.unwrap();
	let err = issue(&h, &service, owner.clone(), "6", None, "over").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("unit cap")), "got {err:?}");
	issue(&h, &service, owner, "5", None, "fits").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("25"));
	// The refused issuance left no row behind to retry into.
	assert!(h.issuances.find_by_key(&service, &IdempotencyKey::parse("over").unwrap()).await.unwrap().is_none());
}

#[tokio::test]
async fn capping_at_the_issued_supply_closes_the_product_to_further_units() {
	// The service_arb registration end to end: 20/80 issued, then the cap set to exactly
	// what was issued, so the product is fully subscribed and no one can buy in — not an
	// investor, and not another issuance.
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let investor = provisioned_user(&h).await;
	let owner = asset_owner(&h).await;
	register(&h, &service).await;
	issue(&h, &service, UnitHolder::User(investor), "3250", Some("3250"), "investor").await.unwrap();
	issue(&h, &service, owner.clone(), "13000", Some("13000"), "company").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	h.allocations.set_unit_cap(&service, shares("16250")).await.unwrap();
	let view = funds_app::fund_nav_view(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), investor, false, now_unix())
		.await
		.unwrap();
	assert_eq!(view.remaining_capacity, Shares::ZERO, "cap == issued means nothing is left");

	// Opened to everyone, and still no subscription fits.
	open_to_everyone(&h, &service).await;
	let buyer = UserId::new();
	fund_user(&h, buyer, "1").await;
	let err = subscribe(&h, buyer, &service, "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("unit cap")), "got {err:?}");
	assert!(issue(&h, &service, owner, "1", None, "one-more").await.is_err());

	// The asymmetry every gate here has: the investor still gets out — once the fund
	// holds cash for the units. The mint flipped the product to `in_kind`; the operator
	// declares the cash is there, and the cap gate still stands aside.
	h.allocations.set_backing(&service, AllocationBacking::Cash).await.unwrap();
	funds_app::request_redemption(&fund_ports(&h), &h.reds, investor, service.clone(), shares("3250"), now_unix())
		.await
		.expect("a fully subscribed product must never block a redemption");
}

#[tokio::test]
async fn an_issuance_is_logged_and_reaches_the_relay_as_its_own_kind() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;
	let record = issue(&h, &service, asset_owner(&h).await, "1", None, "logged").await.unwrap();
	let id = record.issuance.id().raw();

	let (logged, relayed): (i64, i64) = sqlx::query_as(
		"SELECT (SELECT COUNT(*) FROM event_log WHERE aggregate = 'unit_issuance' AND aggregate_id = $1), \
		 (SELECT COUNT(*) FROM outbox WHERE aggregate = 'unit_issuance' AND aggregate_id = $1 AND kind = 'issuances')",
	)
	.bind(id)
	.fetch_one(&h.pool)
	.await
	.unwrap();
	assert_eq!(logged, 1, "the issuance is an audit fact");
	assert_eq!(relayed, 1, "and, unlike a registry event, a money move the relay must post");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let dispatched: bool = sqlx::query_scalar("SELECT dispatched_at IS NOT NULL FROM outbox WHERE aggregate_id = $1")
		.bind(id)
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert!(dispatched, "the relay drained it rather than parking an unknown kind");
}

// The company holder, its mint and its hand-over are gone from the vocabulary (#245): the
// schema refuses them since 0047, and the two production rows are pinned as history by
// `tests/ownership_contract.rs`, over a database migrated to 0046 first.

/// H-2 / M-2 of the #245 security review: one `AllocationManage` holder could grant
/// themselves `invest` on `fee` or `fund` and subscribe cash into the owners' money, or
/// close, re-back or cap a reserved allocation and lock its holders in. Every
/// operator write on a reserved allocation is refused, and a subscription into one is
/// refused before the catalog is even consulted — so what 0045 wrote stays as written.
#[tokio::test]
async fn a_reserved_allocation_is_not_an_operators_to_manage_or_buy_into() {
	let Some(h) = harness().await else { return };
	let admin = provisioned_user(&h).await;
	fund_user(&h, admin, "1000").await;
	for reserved in [ServiceId::fee(), ServiceId::fund()] {
		let before = h.allocations.find(&reserved).await.unwrap().expect("0045 wrote the reserved row");
		let forbidden = |err: DomainError, what: &str| {
			assert!(matches!(err, DomainError::Forbidden(ref m) if m.contains("reserved")), "{reserved} {what}: got {err:?}");
		};
		// Letting themselves in: the grant is refused, and so is the subscription it would
		// have opened — with or without the grant.
		forbidden(
			allocations_app::grant_access(&h.allocations, &reserved, admin, AllocationAccess::Invest, admin)
				.await
				.unwrap_err(),
			"grant_access",
		);
		assert!(h.allocations.list_grants(&reserved).await.unwrap().is_empty(), "{reserved}: no grant was written");
		forbidden(subscribe(&h, admin, &reserved, "100").await.unwrap_err(), "subscribe");
		// Even a grant written past the use case (a hand edit of the registry) buys nothing.
		h.allocations.grant_access(&reserved, admin, AllocationAccess::Invest, admin).await.unwrap();
		forbidden(subscribe(&h, admin, &reserved, "100").await.unwrap_err(), "subscribe after a direct grant");
		h.allocations.revoke_access(&reserved, admin, admin).await.unwrap();
		assert_eq!(
			sqlx::query_scalar::<_, i64>("SELECT count(*) FROM subscriptions WHERE user_id = $1 AND service = $2")
				.bind(admin.raw())
				.bind(reserved.as_str())
				.fetch_one(&h.pool)
				.await
				.unwrap(),
			0,
			"{reserved}: nothing was opened"
		);
		// Locking the holders in, or out: state, access, backing, cap and presentation.
		forbidden(allocations_app::set_access(&h.allocations, &reserved, AllocationAccess::Invest).await.unwrap_err(), "set_access");
		forbidden(allocations_app::close(&h.allocations, &reserved).await.unwrap_err(), "close");
		forbidden(allocations_app::open(&h.allocations, &reserved).await.unwrap_err(), "open");
		forbidden(
			allocations_app::set_backing(&h.allocations, &reserved, AllocationBacking::InKind).await.unwrap_err(),
			"set_backing",
		);
		forbidden(allocations_app::set_unit_cap(&h.allocations, &reserved, shares("1")).await.unwrap_err(), "set_unit_cap");
		forbidden(allocations_app::update_details(&h.allocations, &reserved, "Mine", "", None).await.unwrap_err(), "update_details");
		forbidden(allocations_app::revoke_access(&h.allocations, &reserved, admin, admin).await.unwrap_err(), "revoke_access");
		let after = h.allocations.find(&reserved).await.unwrap().unwrap();
		assert_eq!(
			(after.state(), after.access(), after.backing(), after.unit_cap(), after.title()),
			(before.state(), before.access(), before.backing(), before.unit_cap(), before.title())
		);
		assert_eq!(
			(after.state(), after.access(), after.backing()),
			(AllocationState::Open, AllocationAccess::Hidden, AllocationBacking::Cash),
			"{reserved}: as 0045 wrote it"
		);
	}
	// The same admin, with the same cash, still buys into an ordinary open product.
	let product = unique_service();
	register(&h, &product).await;
	open_to_everyone(&h, &product).await;
	subscribe(&h, admin, &product, "100").await.expect("a product is bought as before");
}

#[tokio::test]
async fn the_fee_allocation_holds_a_products_fee_class_and_nothing_holds_a_reserved_one_but_people() {
	// #245: an allocation may hold units of a product only when it is reserved and the
	// product is not. The fee allocation's holding IS the product's fee class — the same
	// `shares_fee:<svc>` account a clawback moves units into — so the cap table reads it
	// as fee units, and it needs no `users` row and no position. Nobody ISSUES it: the
	// accrual mints it as a charge earns it, so the operator's mint is refused (L-1) and
	// the row is staged here past the use case, as the accrual's own record is.
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;
	let fee = UnitHolder::Allocation(ServiceId::fee());
	let err = issue(&h, &service, fee.clone(), "800", Some("0"), "fee-class").await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(ref m) if m.contains("fee accrual")), "got {err:?}");
	assert!(
		h.issuances.find_by_key(&service, &IdempotencyKey::parse("fee-class").unwrap()).await.unwrap().is_none(),
		"nothing was written"
	);
	let mut fee_class = UnitIssuance::issue(
		UnitIssuanceId::new(),
		service.clone(),
		fee.clone(),
		shares("800"),
		Nav::SEED,
		Some(Usdt::ZERO),
		IdempotencyKey::parse("fee-class").unwrap(),
	)
	.unwrap();
	let IssueOutcome::Recorded(record) = h.issuances.issue(&mut fee_class).await.unwrap() else {
		panic!("a fresh key records")
	};
	assert_eq!(record.issuance.holder(), &fee);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, shares("800"));
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("800"));
	let holders = issuance_app::unit_holders(&h.allocations, h.ledger.as_ref(), &h.issuances, service.clone()).await.unwrap();
	assert_eq!(fee_units(&holders), shares("800"));
	assert_eq!(investor_units(&holders), Shares::ZERO);
	// The row round-trips through its own column, not the user one.
	let (kind, user, holder_service): (String, Option<Uuid>, Option<String>) = sqlx::query_as("SELECT holder_kind, holder_id, holder_service FROM unit_issuances WHERE id = $1")
		.bind(record.issuance.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!((kind.as_str(), user, holder_service.as_deref()), ("allocation", None, Some("fee")));
	let reread = h.issuances.find_by_id(record.issuance.id()).await.unwrap().and_then(StoredIssuance::live).unwrap();
	assert_eq!(reread.issuance.holder(), &fee);
	assert_eq!(reread.issuance.state(), IssuanceState::Applied);

	// The graph has one direction, and the operator's mint never points an allocation at a
	// product at all: a product cannot hold; a reserved allocation cannot be held by
	// anything but a person; and, in phase 1, the fund allocation holds nothing.
	let err = issue(&h, &service, UnitHolder::Allocation(unique_service()), "1", None, "product-holds").await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(ref m) if m.contains("fee accrual")), "got {err:?}");
	let err = grant(&h, &ServiceId::fee(), fee.clone(), "1", None, "fee-holds-fee").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("held by people")), "got {err:?}");
	let err = grant(&h, &ServiceId::fund(), fee, "1", None, "fee-holds-fund").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("held by people")), "got {err:?}");
	let err = issue(&h, &service, UnitHolder::Allocation(ServiceId::fund()), "1", None, "fund-holds").await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(ref m) if m.contains("fee accrual")), "got {err:?}");
	// A person may hold the reserved allocations — migration 0045 registered them, hidden —
	// but the operator's mint is refused outright, and a product is not granted.
	let owner = provisioned_user(&h).await;
	let err = issue(&h, &ServiceId::fee(), UnitHolder::User(owner), "10", Some("10"), "operator-seeds-holder")
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(ref m) if m.contains("never issued by hand")), "got {err:?}");
	let err = grant(&h, &service, UnitHolder::User(owner), "10", Some("10"), "grant-a-product").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("not a reserved allocation")), "got {err:?}");
	for reserved in [ServiceId::fee(), ServiceId::fund()] {
		let row = h.allocations.find(&reserved).await.unwrap().expect("0045 wrote the reserved row");
		assert_eq!(row.access(), AllocationAccess::Hidden);
		assert_eq!(row.state(), AllocationState::Open);
		grant(&h, &reserved, UnitHolder::User(owner), "10", Some("10"), "seed-holder").await.unwrap();
	}
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(ServiceId::fee(), owner)).await, shares("10"));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(ServiceId::fund(), owner)).await, shares("10"));
	// And the reserved slugs can never be registered again over those rows.
	let err = allocations_app::register(&h.allocations, ServiceId::fee(), "Mine", "", AllocationIcon::Fund).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("reserved")), "got {err:?}");
}

#[tokio::test]
async fn a_valuation_cannot_be_posted_for_an_unregistered_service() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	// The other door into a phantom fund: an AUM post would write a valuation history for
	// a service no registry entry backs.
	let err = funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), usdt("100"), "op", now_unix())
		.await
		.unwrap_err();
	assert!(matches!(err, domain::error::DomainError::NotFound { entity: "allocation", .. }), "got {err:?}");
}

#[tokio::test]
async fn the_icon_is_stored_survives_a_reload_and_is_editable() {
	let Some(h) = harness().await else { return };
	let service = unique_service();

	// Registration carries the operator's pick all the way to the column — the point of
	// the field is that the catalog card is a decision, not a letter derived from the title.
	register_with_icon(&h, &service, AllocationIcon::RealEstate).await;
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().icon(), AllocationIcon::RealEstate);

	// It is presentation, so editing it is an ordinary details edit and leaves the
	// lifecycle alone — a live product can be restyled without touching money flow.
	h.allocations.open(&service).await.unwrap();
	let updated = h
		.allocations
		.update_details(&service, "EV Arbitrage", "Cross-venue basis", Some(AllocationIcon::Arbitrage))
		.await
		.unwrap();
	assert_eq!(updated.icon(), AllocationIcon::Arbitrage);
	assert_eq!(updated.state(), AllocationState::Open);
	assert_eq!(
		h.allocations.find(&service).await.unwrap().unwrap().icon(),
		AllocationIcon::Arbitrage,
		"the new icon survives a reload"
	);

	// The catalog read carries it too — that projection is what the wallet host renders.
	let listed = h.allocations.list_for(UserId::new(), true).await.unwrap();
	let row = listed.iter().find(|r| r.allocation.service() == &service).expect("the registered product is in the catalog");
	assert_eq!(row.allocation.icon(), AllocationIcon::Arbitrage);

	// A registration that names nothing lands on the default rather than on NULL, so the
	// client always has a glyph to draw.
	let plain = unique_service();
	register(&h, &plain).await;
	assert_eq!(h.allocations.find(&plain).await.unwrap().unwrap().icon(), AllocationIcon::Fund);
}

#[tokio::test]
async fn a_row_written_by_a_pod_that_predates_the_icon_column_reads_back_as_the_default() {
	let Some(h) = harness().await else { return };
	let service = unique_service();

	// Byte for byte the INSERT the CURRENTLY DEPLOYED code runs: it names its columns,
	// and `icon` is not among them. That is the claim migration 0027's header makes about
	// rolling deploys, and until now nothing checked it — the previous test wrote
	// `icon = DEFAULT`, which is a literal 'fund' spelled differently, so it proved
	// `parse("fund") == Fund` (already a unit test) and could not fail for the reason it
	// was named after.
	sqlx::query("INSERT INTO allocations (id, service, title, summary, state, unit_cap) VALUES ($1, $2, $3, $4, $5, $6)")
		.bind(Uuid::new_v4())
		.bind(service.as_str())
		.bind("Legacy Fund")
		.bind("Registered by a pod that had never heard of icons")
		.bind("open")
		.bind(domain::allocations::DEFAULT_UNIT_CAP.base_units().to_string())
		.execute(&h.pool)
		.await
		.expect("the old INSERT must keep working — a migration that breaks it breaks the rolling deploy");

	let stored: String = sqlx::query_scalar("SELECT icon FROM allocations WHERE service = $1")
		.bind(service.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(stored, "fund", "the column DEFAULT is what fills the gap the old writer leaves");
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().icon(), AllocationIcon::Fund);
	// And it is in the catalog the wallet host renders, not just readable one row at a time.
	let listed = h.allocations.list_for(UserId::new(), true).await.unwrap();
	let row = listed.iter().find(|r| r.allocation.service() == &service).expect("the legacy row is in the catalog");
	assert_eq!(row.allocation.icon(), AllocationIcon::Fund);
}

#[tokio::test]
async fn every_icon_the_domain_knows_is_accepted_by_the_column() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;

	// The pair nothing covered: `domain_icons_match_the_wire_contract` compares the domain
	// enum with the wire contract and never touches SQL, so a variant added to the enum,
	// to `icon::ALL` and to the client but FORGOTTEN in a migration compiled, unit-tested
	// green, and first showed up in front of an operator — as a CHECK violation rolling
	// back the whole `update_details` transaction, taking the title edit with it and
	// answering `internal` where a validation error belonged.
	for icon in AllocationIcon::ALL {
		sqlx::query("UPDATE allocations SET icon = $2 WHERE service = $1")
			.bind(service.as_str())
			.bind(icon.as_str())
			.execute(&h.pool)
			.await
			.unwrap_or_else(|err| panic!("the column refuses {icon:?}, which the domain calls legal — migration 0027 is missing it: {err}"));
		assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().icon(), icon, "{icon:?} did not survive the round trip");
	}
}

#[tokio::test]
async fn the_database_refuses_an_icon_outside_the_vocabulary() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;

	// The CHECK in 0027 is the backstop under `AllocationIcon::parse`: a hand-written
	// UPDATE cannot leave a row outside the vocabulary this build ships artwork for.
	let err = sqlx::query("UPDATE allocations SET icon = 'rocket' WHERE service = $1")
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap_err();
	// Identified by SQLSTATE and constraint name rather than by
	// `err.to_string().contains("icon")`, which any error naming the column would have
	// satisfied — a NOT NULL violation, a type mismatch, a typo in the query text.
	let db_err = err.as_database_error().expect("a server-side error, not a client-side one");
	assert_eq!(db_err.code().as_deref(), Some("23514"), "23514 is check_violation; got {err}");
	assert_eq!(db_err.constraint(), Some("allocations_icon_check"), "refused by some other constraint: {err}");
}

#[tokio::test]
async fn an_icon_from_a_wider_vocabulary_reads_back_as_the_default_instead_of_failing() {
	let Some(h) = harness().await else { return };
	let service = unique_service();

	// Widening the vocabulary is an ordinary migration: ship it, let an operator pick the
	// new value, then roll the release back — and this build is reading a string it cannot
	// parse. Strict parsing on the read path turned that into a failing `list()` (the
	// whole catalog) and a failing `find()`, which is the gate every subscribe and redeem
	// passes through. A presentation column must not be able to stop money.
	//
	// Producing such a row needs a table whose CHECK is wider than this build's enum, and
	// that table is built in a throwaway schema rather than by dropping the real
	// constraint: `allocations` is shared with every other test in this binary, and one
	// running while the CHECK was off would assert against a schema that no longer
	// enforces anything — a vacuous pass, which is the exact failure this whole area is
	// about. `LIKE ... INCLUDING DEFAULTS` copies the columns, their NOT NULLs and the
	// `icon` default, and deliberately not the CHECK.
	let url = common::database_url().await.unwrap();
	let schema = format!("icon_probe_{}", Uuid::new_v4().simple());
	sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}"))).execute(&h.pool).await.unwrap();
	sqlx::query(AssertSqlSafe(format!("CREATE TABLE {schema}.allocations (LIKE public.allocations INCLUDING DEFAULTS)")))
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query(AssertSqlSafe(format!(
		"INSERT INTO {schema}.allocations (id, service, title, summary, state, unit_cap, icon) VALUES ($1, $2, $3, $4, 'open', $5, 'hologram')"
	)))
	.bind(Uuid::new_v4())
	.bind(service.as_str())
	.bind("EV Hologram")
	.bind("Registered by a build that knew one more icon")
	.bind(domain::allocations::DEFAULT_UNIT_CAP.base_units().to_string())
	.execute(&h.pool)
	.await
	.unwrap();

	// The adapter's SQL names `allocations` unqualified, so a pool whose `search_path`
	// leads with the probe schema reads the wider table through the ordinary code path —
	// the same `find`/`list` the money plane calls, not a re-implementation of them.
	let options = url.parse::<PgConnectOptions>().unwrap().options([("search_path", format!("{schema},public"))]);
	let probe = PgPoolOptions::new().max_connections(2).connect_with(options).await.unwrap();
	let allocations = PgAllocations::new(probe.clone());

	let found = allocations.find(&service).await;
	let listed = allocations.list_for(UserId::new(), true).await;
	probe.close().await;
	sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE"))).execute(&h.pool).await.unwrap();

	let found = found.expect("an unparseable icon must not fail the read every subscribe depends on");
	assert_eq!(
		found.expect("the row is there").icon(),
		AllocationIcon::Fund,
		"an icon this build cannot draw degrades to the default, matching what the client already does"
	);
	let listed = listed.expect("nor may it fail the catalog read for every other product");
	let row = listed.iter().find(|r| r.allocation.service() == &service).expect("the row is still listed, wearing the default");
	assert_eq!(row.allocation.icon(), AllocationIcon::Fund);
}

#[tokio::test]
async fn an_update_that_names_no_icon_keeps_the_one_the_operator_picked() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register_with_icon(&h, &service, AllocationIcon::RealEstate).await;

	// `None` is what an `UpdateAllocation` with the `optional` field unset reaches the
	// registry as: an older consumer repo, a browser tab on a stale bundle, a pod still
	// running the previous release. Before the field carried presence they all sent the
	// proto3 default — an empty string, indistinguishable from a deliberate clear — and a
	// plain title edit silently restyled a live product back to `fund`.
	let renamed = h.allocations.update_details(&service, "EV Property", "Income-producing property", None).await.unwrap();
	assert_eq!(renamed.title(), "EV Property");
	assert_eq!(renamed.icon(), AllocationIcon::RealEstate, "an absent icon left the operator's pick alone");
	assert_eq!(
		h.allocations.find(&service).await.unwrap().unwrap().icon(),
		AllocationIcon::RealEstate,
		"and the column was not overwritten either"
	);

	// `Some` still sets it, so the field has not become unwritable…
	let restyled = h.allocations.update_details(&service, "EV Property", "", Some(AllocationIcon::Venture)).await.unwrap();
	assert_eq!(restyled.icon(), AllocationIcon::Venture);
	// …and `Some(default)` is how a reset to the neutral glyph stays reachable, which is
	// exactly what absence must NOT mean.
	let reset = h.allocations.update_details(&service, "EV Property", "", Some(AllocationIcon::default())).await.unwrap();
	assert_eq!(reset.icon(), AllocationIcon::Fund);
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().icon(), AllocationIcon::Fund);
}

// ── access: who sees a product, and who may put money in ─────────────────────

#[tokio::test]
async fn a_registration_lands_on_view_and_setting_access_is_idempotent_and_audited() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let allocation = register(&h, &service).await;

	// "Closed by default": the column and the aggregate agree on `view`.
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().access(), AllocationAccess::View);
	let stored: String = sqlx::query_scalar("SELECT access FROM allocations WHERE service = $1")
		.bind(service.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(stored, "view");

	assert_eq!(h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap().access(), AllocationAccess::Invest);
	assert_eq!(
		h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap().access(),
		AllocationAccess::Invest,
		"re-setting is a no-op"
	);
	assert_eq!(
		h.allocations.find(&service).await.unwrap().unwrap().access(),
		AllocationAccess::Invest,
		"the level survives a reload"
	);

	// Registered + one real change = two audit facts; the repeat left none.
	let logged: i64 = sqlx::query_scalar("SELECT count(*) FROM event_log WHERE aggregate = 'allocation' AND aggregate_id = $1")
		.bind(allocation.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(logged, 2, "an idempotent re-set must not spam the log");
	assert!(h.allocations.set_access(&unique_service(), AllocationAccess::Invest).await.is_err(), "unregistered is NotFound");
}

#[tokio::test]
async fn every_access_level_the_domain_knows_is_accepted_by_the_column() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;
	// The CHECK in 0033 spells the same strings as the enum; a level the domain knows
	// but the column refuses would surface as `internal` on an operator's command.
	for level in [AllocationAccess::Hidden, AllocationAccess::View, AllocationAccess::Invest] {
		h.allocations
			.set_access(&service, level)
			.await
			.unwrap_or_else(|err| panic!("the column refuses {level:?}, which the domain calls legal — migration 0033 is missing it: {err}"));
		assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().access(), level);
	}
	let err = sqlx::query("UPDATE allocations SET access = 'public' WHERE service = $1")
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap_err();
	let db_err = err.as_database_error().expect("a server-side error");
	assert_eq!(db_err.code().as_deref(), Some("23514"), "23514 is check_violation; got {err}");
	assert_eq!(db_err.constraint(), Some("allocations_access_check"), "refused by some other constraint: {err}");
}

#[tokio::test]
async fn the_effective_level_is_the_higher_of_default_and_grant() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;
	let investor = provisioned_user(&h).await;
	let operator = provisioned_user(&h).await;

	// No grant: the default is what the investor holds.
	assert_eq!(h.allocations.find_for(&service, investor).await.unwrap().unwrap().caller_access, AllocationAccess::View);

	// A grant raises above the default…
	let grant = h.allocations.grant_access(&service, investor, AllocationAccess::Invest, operator).await.unwrap();
	assert_eq!((grant.user_id, grant.level, grant.granted_by), (investor, AllocationAccess::Invest, operator));
	assert!(grant.granted_at > 0, "the grant is dated by the database");
	assert_eq!(h.allocations.find_for(&service, investor).await.unwrap().unwrap().caller_access, AllocationAccess::Invest);
	// …for that investor only.
	assert_eq!(h.allocations.find_for(&service, operator).await.unwrap().unwrap().caller_access, AllocationAccess::View);

	// A grant never lowers: raising the default past it leaves the investor at the default.
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	h.allocations.grant_access(&service, investor, AllocationAccess::View, operator).await.unwrap();
	assert_eq!(h.allocations.find_for(&service, investor).await.unwrap().unwrap().caller_access, AllocationAccess::Invest);

	// Lowering the default all the way down leaves the `view` grant standing.
	h.allocations.set_access(&service, AllocationAccess::Hidden).await.unwrap();
	assert_eq!(h.allocations.find_for(&service, investor).await.unwrap().unwrap().caller_access, AllocationAccess::View);
	assert_eq!(h.allocations.find_for(&service, operator).await.unwrap().unwrap().caller_access, AllocationAccess::Hidden);

	// The caller-agnostic read carries the default alone — the redeem gate wants nothing else.
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().access(), AllocationAccess::Hidden);
}

#[tokio::test]
async fn grants_overwrite_revoke_idempotently_and_are_audited() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let allocation = register(&h, &service).await;
	let investor = provisioned_user(&h).await;
	let operator = provisioned_user(&h).await;
	let events = |aggregate_id: Uuid| {
		let pool = h.pool.clone();
		async move {
			sqlx::query_scalar::<_, i64>("SELECT count(*) FROM event_log WHERE aggregate = 'allocation' AND aggregate_id = $1")
				.bind(aggregate_id)
				.fetch_one(&pool)
				.await
				.unwrap()
		}
	};
	let baseline = events(allocation.id().raw()).await;

	// `hidden` is not a level a grant may carry — refused as bad input, nothing written.
	let err = h.allocations.grant_access(&service, investor, AllocationAccess::Hidden, operator).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "{err:?}");
	assert!(h.allocations.list_grants(&service).await.unwrap().is_empty());

	h.allocations.grant_access(&service, investor, AllocationAccess::View, operator).await.unwrap();
	h.allocations.grant_access(&service, investor, AllocationAccess::View, operator).await.unwrap();
	assert_eq!(events(allocation.id().raw()).await, baseline + 1, "a repeat grant at the same level leaves no fact");

	// A repeat at a different level overwrites — one row, new level, one more fact.
	let grant = h.allocations.grant_access(&service, investor, AllocationAccess::Invest, operator).await.unwrap();
	assert_eq!(grant.level, AllocationAccess::Invest);
	let grants = h.allocations.list_grants(&service).await.unwrap();
	assert_eq!(grants.len(), 1);
	assert_eq!(grants[0].level, AllocationAccess::Invest);
	assert_eq!(events(allocation.id().raw()).await, baseline + 2);

	// Revoke: once with a fact, again without — and the investor is back at the default.
	h.allocations.revoke_access(&service, investor, operator).await.unwrap();
	h.allocations.revoke_access(&service, investor, operator).await.unwrap();
	assert!(h.allocations.list_grants(&service).await.unwrap().is_empty());
	assert_eq!(events(allocation.id().raw()).await, baseline + 3, "revoking nothing is unlogged");
	assert_eq!(h.allocations.find_for(&service, investor).await.unwrap().unwrap().caller_access, AllocationAccess::View);

	// Grants are audit facts, never relay work.
	let relayed: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE aggregate = 'allocation'").fetch_one(&h.pool).await.unwrap();
	assert_eq!(relayed, 0);

	// An unregistered product has no grants to list, grant or revoke — NotFound, not empty.
	let ghost = unique_service();
	assert!(matches!(h.allocations.list_grants(&ghost).await, Err(DomainError::NotFound { entity: "allocation", .. })));
	assert!(matches!(
		h.allocations.grant_access(&ghost, investor, AllocationAccess::View, operator).await,
		Err(DomainError::NotFound { .. })
	));
	assert!(matches!(h.allocations.revoke_access(&ghost, investor, operator).await, Err(DomainError::NotFound { .. })));
}

#[tokio::test]
async fn the_catalog_hides_a_hidden_product_until_the_investor_is_granted() {
	let Some(h) = harness().await else { return };
	let hidden = unique_service();
	let viewable = unique_service();
	let investable = unique_service();
	for service in [&hidden, &viewable, &investable] {
		register(&h, service).await;
		h.allocations.open(service).await.unwrap();
	}
	h.allocations.set_access(&hidden, AllocationAccess::Hidden).await.unwrap();
	h.allocations.set_access(&investable, AllocationAccess::Invest).await.unwrap();
	let investor = provisioned_user(&h).await;
	let operator = provisioned_user(&h).await;
	let catalog = |include_unlisted: bool| {
		let allocations = &h.allocations;
		async move {
			allocations
				.list_for(investor, include_unlisted)
				.await
				.unwrap()
				.into_iter()
				.map(|r| (r.allocation.service().to_string(), r.caller_access))
				.collect::<Vec<_>>()
		}
	};

	let listed = catalog(false).await;
	assert!(listed.contains(&(viewable.to_string(), AllocationAccess::View)), "a `view` product is in the catalog, locked");
	assert!(listed.contains(&(investable.to_string(), AllocationAccess::Invest)));
	assert!(!listed.iter().any(|(s, _)| *s == hidden.to_string()), "a hidden product is not");

	// A `view` grant is enough to surface it — and the row says what the investor holds.
	h.allocations.grant_access(&hidden, investor, AllocationAccess::View, operator).await.unwrap();
	assert!(catalog(false).await.contains(&(hidden.to_string(), AllocationAccess::View)), "granted `view`, so listed");
	// The grant is per investor: the operator, holding none, still does not see it.
	let operators_view: Vec<String> = h
		.allocations
		.list_for(operator, false)
		.await
		.unwrap()
		.iter()
		.map(|r| r.allocation.service().to_string())
		.collect();
	assert!(!operators_view.contains(&hidden.to_string()));

	// The manager's unfiltered list carries every product, and still the HONEST level
	// for the caller rather than a courtesy `invest`.
	let all = catalog(true).await;
	assert!(all.contains(&(hidden.to_string(), AllocationAccess::View)));
	assert!(all.contains(&(viewable.to_string(), AllocationAccess::View)));
	let for_operator = h.allocations.list_for(operator, true).await.unwrap();
	let row = for_operator
		.iter()
		.find(|r| r.allocation.service() == &hidden)
		.expect("include_unlisted surfaces the hidden product");
	assert_eq!(row.caller_access, AllocationAccess::Hidden, "the manager's own effective level, not their permission");
}

#[tokio::test]
async fn a_hidden_product_is_not_found_for_an_investor_but_readable_by_a_manager() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;
	h.allocations.open(&service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Hidden).await.unwrap();
	let investor = provisioned_user(&h).await;
	let operator = provisioned_user(&h).await;

	// The application-layer read is what `GetAllocation` runs: hidden answers exactly as
	// an unregistered slug does, so the catalog cannot be probed for locked products.
	let err = allocations_app::get_for(&h.allocations, &service, investor, false).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { entity: "allocation", .. }), "{err:?}");
	// `unrestricted` is the AllocationManage view — everything, with the honest level.
	let record = allocations_app::get_for(&h.allocations, &service, investor, true).await.unwrap();
	assert_eq!(record.caller_access, AllocationAccess::Hidden);
	assert!(record.created_at > 0 && record.updated_at >= record.created_at, "a read carries the DB-stamped timestamps");

	// A `view` grant makes it readable to that investor without any manager privilege.
	h.allocations.grant_access(&service, investor, AllocationAccess::View, operator).await.unwrap();
	let record = allocations_app::get_for(&h.allocations, &service, investor, false).await.unwrap();
	assert_eq!(record.caller_access, AllocationAccess::View);
	assert_eq!(
		record.allocation.access(),
		AllocationAccess::Hidden,
		"the product's default is reported beside the caller's level"
	);
}

#[tokio::test]
async fn the_nav_of_a_hidden_product_is_not_found_for_an_investor_but_readable_by_a_manager() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;
	h.allocations.open(&service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Hidden).await.unwrap();
	let investor = provisioned_user(&h).await;
	let operator = provisioned_user(&h).await;

	// The price route used to resolve the allocation caller-agnostically, so a hidden
	// product answered with its NAV, cap and headroom to anyone holding the slug — and,
	// worse, answered differently from an unregistered one. It now runs the same gate
	// `GetAllocation` does.
	let err = funds_app::fund_nav_view(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), investor, false, now_unix())
		.await
		.err()
		.expect("a hidden product's NAV is not readable without a grant");
	assert!(matches!(err, DomainError::NotFound { entity: "allocation", .. }), "{err:?}");
	// `unrestricted` is the AllocationManage view — the manager reads every product's price.
	funds_app::fund_nav_view(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), investor, true, now_unix())
		.await
		.expect("a manager reads the NAV of a hidden product");

	// A `view` grant is enough to read the price: `view` withholds only the subscribe.
	h.allocations.grant_access(&service, investor, AllocationAccess::View, operator).await.unwrap();
	let view = funds_app::fund_nav_view(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), investor, false, now_unix())
		.await
		.expect("a `view` grant reads the NAV without any manager privilege");
	assert_eq!(view.service, service);
}

#[tokio::test]
async fn a_row_written_by_a_pod_that_predates_the_access_column_lands_locked() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	// The INSERT the currently deployed code runs names its columns and knows no
	// `access`; the column default must land it on `view` — visible, locked — so a
	// product registered mid-rollout lets nobody in by accident.
	sqlx::query("INSERT INTO allocations (id, service, title, summary, state, unit_cap, icon) VALUES ($1, $2, $3, $4, $5, $6, $7)")
		.bind(Uuid::new_v4())
		.bind(service.as_str())
		.bind("Legacy Fund")
		.bind("Registered by a pod that had never heard of access")
		.bind("open")
		.bind(domain::allocations::DEFAULT_UNIT_CAP.base_units().to_string())
		.bind("fund")
		.execute(&h.pool)
		.await
		.expect("the old INSERT must keep working — a migration that breaks it breaks the rolling deploy");
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().access(), AllocationAccess::View);
	let user = UserId::new();
	fund_user(&h, user, "10").await;
	let err = subscribe(&h, user, &service, "10").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(_)), "listed but locked: {err:?}");
}

#[tokio::test]
async fn a_queued_mint_is_reported_beside_the_settled_supply_until_the_relay_posts_it() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	register(&h, &service).await;

	// The operator's console offered "pin cap to issued" off `units_outstanding`, which
	// is the ledger's settled figure. A mint recorded seconds earlier is not in it yet,
	// so the pinned cap sat below where the supply landed once the relay caught up.
	let owner = asset_owner(&h).await;
	let record = issue(&h, &service, owner.clone(), "13000", Some("13000"), "queued-then-applied").await.unwrap();
	assert_eq!(record.issuance.state(), IssuanceState::Queued);
	let holders = issuance_app::unit_holders(&h.allocations, h.ledger.as_ref(), &h.issuances, service.clone()).await.unwrap();
	assert_eq!(holders.units_outstanding, Shares::ZERO, "nothing has posted yet");
	assert_eq!(holders.queued_units, shares("13000"), "the recorded mint is the signal to wait");

	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let holders = issuance_app::unit_holders(&h.allocations, h.ledger.as_ref(), &h.issuances, service.clone()).await.unwrap();
	assert_eq!(holders.units_outstanding, shares("13000"));
	assert_eq!(units_held(&holders, &owner), shares("13000"));
	assert_eq!(holders.queued_units, Shares::ZERO, "applied rows drop out of the queue");
}

// ── retiring units, and what stands behind them ──────────────────────────────

/// An operator burning `units` out of `holder`'s account through the use case.
async fn retire(h: &Harness, service: &ServiceId, holder: UnitHolder, units: &str, key: &str, force: bool) -> Result<UnitIssuanceRecord, DomainError> {
	issuance_app::retire_units(
		&fund_ports(h),
		&h.issuances,
		&h.users,
		issuance_app::RetireUnitsRequest {
			service: service.clone(),
			holder,
			units: shares(units),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse(key).unwrap(),
			force,
		},
		now_unix(),
	)
	.await
}

/// How many issuance rows — mints, hand-overs and retirements alike — stand for `service`.
async fn issuance_rows(h: &Harness, service: &ServiceId) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM unit_issuances WHERE service = $1")
		.bind(service.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

/// The `BackingChanged` facts logged for one allocation.
async fn backing_changes(h: &Harness, allocation: &Allocation) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM event_log WHERE aggregate = 'allocation' AND aggregate_id = $1 AND payload->>'type' = 'backing_changed'")
		.bind(allocation.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

/// The projection's own `(units, cost_basis)` for one holder — the denominator and
/// numerator a redemption settle reduces against.
async fn projected(h: &Harness, user: UserId, service: &ServiceId) -> (Shares, Usdt) {
	let (units, basis): (String, String) = sqlx::query_as("SELECT units, cost_basis FROM fund_positions WHERE user_id = $1 AND service = $2")
		.bind(user.raw())
		.bind(service.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	(Shares::from_base_units(units.parse().unwrap()), Usdt::from_base_units(basis.parse().unwrap()))
}

#[tokio::test]
async fn retiring_units_on_a_closed_product_burns_them_out_of_the_holder_and_the_supply() {
	// The mint reversed: units leave the holder, the supply shrinks by exactly that, and
	// not a cent moves — for an investor and for the asset's owner alike.
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let investor = provisioned_user(&h).await;
	let owner = asset_owner(&h).await;
	let allocation = register(&h, &service).await;
	issue(&h, &service, UnitHolder::User(investor), "3250", Some("3250"), "investor").await.unwrap();
	issue(&h, &service, owner.clone(), "13000", Some("13000"), "company").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	h.allocations.close(&service).await.unwrap();

	let from_investor = retire(&h, &service, UnitHolder::User(investor), "1000", "retire-investor", false).await.unwrap();
	let from_company = retire(&h, &service, owner.clone(), "3000", "retire-company", false).await.unwrap();
	assert_eq!(from_investor.issuance.source(), IssuanceSource::Retire);
	assert_eq!(from_investor.issuance.units(), shares("1000"), "the row carries the magnitude; the source is the direction");
	assert_eq!(from_investor.issuance.cost_basis(), usdt("1000"), "the written-off basis defaults to units × NAV");
	assert_eq!(from_investor.issuance.state(), IssuanceState::Queued, "recorded, not yet burnt");
	assert_eq!(issuance_rows(&h, &service).await, 4, "one history: two mints, two retirements");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), investor)).await, shares("2250"));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user_of(&owner))).await, shares("10000"));
	assert_eq!(
		units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await,
		shares("12250"),
		"supply shrank by both burns"
	);
	assert_eq!(
		Usdt::from_base_units(h.ledger.balance(&LedgerAccountKey::ServiceClaim(service.clone())).await.unwrap().posted),
		Usdt::ZERO,
		"no cash leg"
	);
	let holders = issuance_app::unit_holders(&h.allocations, h.ledger.as_ref(), &h.issuances, service.clone()).await.unwrap();
	assert_eq!(
		(holders.units_outstanding, units_held(&holders, &owner), units_held(&holders, &UnitHolder::User(investor))),
		(shares("12250"), shares("10000"), shares("2250"))
	);

	// The relay stamped both rows applied with the source intact, and the investor's
	// projection shed units and basis pro rata — the mark stays where the mint put it.
	for record in [&from_investor, &from_company] {
		let applied = h.issuances.find_by_id(record.issuance.id()).await.unwrap().and_then(StoredIssuance::live).unwrap();
		assert_eq!(applied.issuance.state(), IssuanceState::Applied);
		assert_eq!(applied.issuance.source(), IssuanceSource::Retire, "the source survives the round trip");
		assert!(applied.applied_at.is_some());
	}
	assert_eq!(projected(&h, investor, &service).await, (shares("2250"), usdt("2250")));
	let position = h.positions.find(investor, &service).await.unwrap().unwrap();
	assert_eq!(position.high_water_mark, Nav::SEED, "nothing was realised at any price");
	// The product's backing is not the retirement's business: still what the mint made it.
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().backing(), AllocationBacking::InKind);
	assert_eq!(backing_changes(&h, &allocation).await, 1, "only the first mint flipped it");
}

#[tokio::test]
async fn retiring_units_out_of_a_live_product_needs_force() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let owner = asset_owner(&h).await;
	register(&h, &service).await;
	issue(&h, &service, owner.clone(), "100", Some("0"), "seed").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// A draft and an open product both refuse: a closed door first, or an explicit
	// override — and the refusal is a precondition the operator can lift, not bad input.
	for state in [AllocationState::Draft, AllocationState::Open] {
		if state == AllocationState::Open {
			h.allocations.open(&service).await.unwrap();
		}
		let err = retire(&h, &service, owner.clone(), "40", "burn", false).await.unwrap_err();
		assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("close it before retiring")), "{state:?}: {err:?}");
	}
	assert_eq!(issuance_rows(&h, &service).await, 1, "a refused retirement writes nothing");
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user_of(&owner))).await, shares("100"));

	// The override is the operator saying "on a live product, yes".
	retire(&h, &service, owner.clone(), "40", "burn", true).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), user_of(&owner))).await, shares("60"));
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("60"));
	assert_eq!(
		h.allocations.find(&service).await.unwrap().unwrap().state(),
		AllocationState::Open,
		"force retires; it does not close"
	);
}

#[tokio::test]
async fn a_retirement_shares_the_issuance_key_space_and_burns_once() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let owner = asset_owner(&h).await;
	register(&h, &service).await;
	issue(&h, &service, owner.clone(), "100", Some("0"), "seed").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	h.allocations.close(&service).await.unwrap();

	let first = retire(&h, &service, owner.clone(), "40", "burn", false).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// The console re-sends after a timeout: same key, same request — same row, no second burn.
	let again = retire(&h, &service, owner.clone(), "40", "burn", false).await.unwrap();
	assert_eq!(again.issuance.id(), first.issuance.id());
	assert_eq!(again.issuance.state(), IssuanceState::Applied, "the repeat reads the row as it stands now");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), user_of(&owner))).await,
		shares("60"),
		"one burn, not two"
	);
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("60"));

	// The same key for a different amount, a mint's key reused for a retirement, or the
	// reverse: one key space per product, because the three move the supply differently.
	let err = retire(&h, &service, owner.clone(), "41", "burn", false).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	let err = retire(&h, &service, owner.clone(), "100", "seed", false).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "a mint's key is not a retirement's retry: {err:?}");
	let err = issue(&h, &service, owner.clone(), "40", None, "burn").await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "a retirement's key is not a mint's retry: {err:?}");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(issuance_rows(&h, &service).await, 2);
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("60"));

	// The widened `source` CHECK (0039) took `retire` for the row above and still refuses
	// a value nothing in the vocabulary names.
	let err = sqlx::query("UPDATE unit_issuances SET source = 'burn' WHERE id = $1")
		.bind(first.issuance.id().raw())
		.execute(&h.pool)
		.await
		.unwrap_err();
	assert_eq!(err.as_database_error().and_then(|e| e.constraint()), Some("unit_issuances_source_check"), "{err}");
}

#[tokio::test]
async fn a_retirement_is_gated_by_the_registry_the_holder_and_what_is_available() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let investor = provisioned_user(&h).await;

	// Unregistered: refused before the ledger, like a mint.
	let err = retire(&h, &service, UnitHolder::User(investor), "10", "unregistered", false).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { entity: "allocation", .. }), "got {err:?}");

	register(&h, &service).await;
	issue(&h, &service, UnitHolder::User(investor), "100", Some("100"), "seed").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	h.allocations.close(&service).await.unwrap();
	// A user nobody can sign in as holds nothing to retire.
	let err = retire(&h, &service, UnitHolder::User(UserId::new()), "10", "nobody", false).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { entity: "user", .. }), "got {err:?}");
	// More than the holder has is refused on the Read-First, with nothing written.
	let err = retire(&h, &service, UnitHolder::User(investor), "101", "over", false).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("available units")), "got {err:?}");
	assert_eq!(issuance_rows(&h, &service).await, 1);
	// The fee allocation holds no fee class of this product yet, so there is nothing to burn.
	let err = retire(&h, &service, UnitHolder::Allocation(ServiceId::fee()), "1", "fee-holds-nothing", false).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "got {err:?}");

	// Units a redemption has reserved are spoken for and do not count. A closed product
	// still redeems; the fund has no cash, so the redemption queues and the relay locks
	// the units as a pending burn — `available()` drops, `posted` does not.
	h.allocations.set_backing(&service, AllocationBacking::Cash).await.unwrap();
	funds_app::request_redemption(&fund_ports(&h), &h.reds, investor, service.clone(), shares("30"), now_unix())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let holding = h.ledger.balance(&LedgerAccountKey::UserShares(service.clone(), investor)).await.unwrap();
	assert_eq!(Shares::from_base_units(holding.posted), shares("100"), "the burn is only reserved");
	assert_eq!(Shares::from_base_units(holding.available()), shares("70"));
	let err = retire(&h, &service, UnitHolder::User(investor), "71", "locked", false).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("reserved by a redemption")), "got {err:?}");
	// Exactly what is free fits.
	retire(&h, &service, UnitHolder::User(investor), "70", "all-free", false).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let holding = h.ledger.balance(&LedgerAccountKey::UserShares(service.clone(), investor)).await.unwrap();
	assert_eq!(Shares::from_base_units(holding.posted), shares("30"), "only the reserved units remain");
	assert_eq!(Shares::from_base_units(holding.available()), Shares::ZERO);
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("30"));
}

#[tokio::test]
async fn the_first_in_kind_mint_marks_the_product_in_kind_and_nothing_else_touches_it() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let investor = provisioned_user(&h).await;
	let allocation = register(&h, &service).await;
	assert_eq!(
		h.allocations.find(&service).await.unwrap().unwrap().backing(),
		AllocationBacking::Cash,
		"a registration is cash-backed"
	);
	let stored: String = sqlx::query_scalar("SELECT backing FROM allocations WHERE service = $1")
		.bind(service.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(stored, "cash");

	// The first mint flips it, and leaves one fact behind…
	let owner = asset_owner(&h).await;
	issue(&h, &service, owner.clone(), "100", Some("0"), "seed").await.unwrap();
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().backing(), AllocationBacking::InKind);
	assert_eq!(backing_changes(&h, &allocation).await, 1);
	let payload: String = sqlx::query_scalar("SELECT payload::text FROM event_log WHERE aggregate = 'allocation' AND aggregate_id = $1 AND payload->>'type' = 'backing_changed'")
		.bind(allocation.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	let event: AllocationEvent = serde_json::from_str(&payload).unwrap();
	assert!(
		matches!(
			event,
			AllocationEvent::BackingChanged {
				backing: AllocationBacking::InKind,
				..
			}
		),
		"{event:?}"
	);
	// …the second mint leaves none.
	issue(&h, &service, UnitHolder::User(investor), "10", None, "again").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().backing(), AllocationBacking::InKind);
	assert_eq!(backing_changes(&h, &allocation).await, 1, "a repeat is idempotent and unlogged");

	// Only the operator takes it back — and the next mint flips it again, because the
	// units it creates are once more ones the fund holds no cash for.
	h.allocations.set_backing(&service, AllocationBacking::Cash).await.unwrap();
	h.allocations.set_backing(&service, AllocationBacking::Cash).await.unwrap();
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().backing(), AllocationBacking::Cash);
	assert_eq!(backing_changes(&h, &allocation).await, 2, "re-setting the backing it holds raises nothing");
	issue(&h, &service, owner, "1", Some("0"), "once-more").await.unwrap();
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().backing(), AllocationBacking::InKind);
	assert_eq!(backing_changes(&h, &allocation).await, 3);
	// Backing is an audit fact like every other registry event, never relay work.
	let relayed: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE aggregate = 'allocation' AND aggregate_id = $1")
		.bind(allocation.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(relayed, 0);
	assert!(h.allocations.set_backing(&unique_service(), AllocationBacking::Cash).await.is_err(), "unregistered is NotFound");
}

#[tokio::test]
async fn a_redemption_is_refused_on_an_in_kind_product_until_the_operator_declares_cash() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	let investor = provisioned_user(&h).await;
	register(&h, &service).await;
	issue(&h, &service, UnitHolder::User(investor), "100", Some("100"), "seed").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	open_to_everyone(&h, &service).await;

	// The state gate passes (open), the holder has the units, and still: the fund holds
	// no cash for them, so the answer is a precondition pointing at the book — and it
	// comes before any redemption is recorded or any unit reserved.
	let err = funds_app::request_redemption(&fund_ports(&h), &h.reds, investor, service.clone(), shares("50"), now_unix())
		.await
		.unwrap_err();
	assert!(
		matches!(err, DomainError::Precondition(ref m) if m.contains("not backed by fund cash") && m.contains("book")),
		"got {err:?}"
	);
	let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM redemptions WHERE service = $1")
		.bind(service.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(recorded, 0, "refused before anything is written");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(
		Shares::from_base_units(h.ledger.balance(&LedgerAccountKey::UserShares(service.clone(), investor)).await.unwrap().available()),
		shares("100")
	);

	// The operator declares the fund holds cash for the units; the same request passes
	// (and queues, since the claim is still empty — the ordinary treasury path).
	h.allocations.set_backing(&service, AllocationBacking::Cash).await.unwrap();
	funds_app::request_redemption(&fund_ports(&h), &h.reds, investor, service.clone(), shares("50"), now_unix())
		.await
		.expect("a cash-backed product redeems");
	// And the gate is on backing alone: a closed cash-backed product still lets holders out.
	h.allocations.close(&service).await.unwrap();
	funds_app::request_redemption(&fund_ports(&h), &h.reds, investor, service.clone(), shares("50"), now_unix())
		.await
		.expect("closed never traps an investor");
}

#[tokio::test]
async fn a_row_written_by_a_pod_that_predates_the_backing_column_reads_as_cash() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	// The INSERT the currently deployed code runs names its columns and knows no
	// `backing`; the column default must land it on `cash`, which is what every product
	// registered before in-kind mints existed has always been.
	sqlx::query("INSERT INTO allocations (id, service, title, summary, state, unit_cap, icon, access) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
		.bind(Uuid::new_v4())
		.bind(service.as_str())
		.bind("Legacy Fund")
		.bind("Registered by a pod that had never heard of backing")
		.bind("open")
		.bind(domain::allocations::DEFAULT_UNIT_CAP.base_units().to_string())
		.bind("fund")
		.bind("invest")
		.execute(&h.pool)
		.await
		.expect("the old INSERT must keep working — a migration that breaks it breaks the rolling deploy");
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().backing(), AllocationBacking::Cash);
	// A subscriber into such a product can get out — the legacy path end to end.
	let user = UserId::new();
	fund_user(&h, user, "10").await;
	subscribe(&h, user, &service, "10").await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	funds_app::request_redemption(&fund_ports(&h), &h.reds, user, service.clone(), shares("10"), now_unix())
		.await
		.expect("a cash-backed legacy row redeems");

	// The CHECK spells the same strings as the enum, and refuses anything else.
	for backing in [AllocationBacking::Cash, AllocationBacking::InKind] {
		h.allocations
			.set_backing(&service, backing)
			.await
			.unwrap_or_else(|err| panic!("the column refuses {backing:?}, which the domain calls legal — migration 0039 is missing it: {err}"));
	}
	let err = sqlx::query("UPDATE allocations SET backing = 'asset' WHERE service = $1")
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap_err();
	let db_err = err.as_database_error().expect("a server-side error");
	assert_eq!(db_err.code().as_deref(), Some("23514"), "23514 is check_violation; got {err}");
	assert_eq!(db_err.constraint(), Some("allocations_backing_check"), "refused by some other constraint: {err}");
}
