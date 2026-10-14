//! Integration tests for the treasury and the reconciliation under the ownership model
//! (issue #245, phase 1) — real Postgres **and** TigerBeetle (no mocks, per the project
//! rules). They run when `DATABASE_URL` is set and a TigerBeetle replica is reachable
//! (`nix run .#db` + `.#tb`), and skip otherwise.
//!
//! The invariant every step of the ownership work is held to: **every unit of value on
//! every ledger is at a holder**, and `Σ holders == total custody` is *checked*, never
//! derived. What these pin down:
//!
//! 1. **The treasury lists every allocation with its holders**, the hidden reserved ones
//!    included, and what people hold directly is the sum of their claims — not the
//!    remainder `custody − fund − fee` the screen used to show.
//! 2. **A leftover without a holder is reported, not swallowed.** Cash on an allocation
//!    nobody holds units of — the `fee` claim's state between the first release and the
//!    data migration — is a warning and a counter, never an alert and never hidden in a
//!    remainder.
//! 3. **Per-allocation supply reconciles** through every move that touches units:
//!    subscribe, fee charge, settle, redeem, in-kind issue, retire.
//!
//! Every test reads platform-wide sums (`Σ user claims`, the `fee` claim, the
//! reconciliation report), so the suite runs serially under the shared outbox guard, and
//! each test brackets its own effect against what it read before — never an absolute.

use std::sync::Arc;

use domain::{
	allocations::{Allocation, AllocationAccess, AllocationIcon, AllocationId},
	auth::AuthSubject,
	balance::{LedgerAccountKey, Party, ServiceId, TransferCode},
	fees::{FeePolicy, Trigger},
	issuance::{IdempotencyKey, UnitHolder},
	money::{Network, Shares, TxRef, Usdt},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{balance as balance_app, fees as fee_app, funds as funds_app, issuance as issuance_app, ownership as ownership_app},
	infrastructure::{
		allocations::PgAllocations,
		consilium::PgConsilia,
		custody::StubCustody,
		deposits::PgDeposits,
		fee_policy_changes::PgFeePolicyChanges,
		fees::{PgFeeAssessments, PgFeePolicies, PgFeeSettlements, PgPositionAccruals},
		issuance::PgUnitIssuances,
		nav::PgNav,
		reconciliation::{ReconReport, Reconciliation},
		redemptions::PgRedemptions,
		relay::Relay,
		subscriptions::PgSubscriptions,
		telemetry,
		users::PgUsers,
	},
	ports::{
		AllocationRegistry, UserRepository,
		fees::{FeePolicyChanges, PositionAccruals},
		ledger::{Ledger, LedgerTransfer},
	},
};
use sqlx::PgPool;
use tokio::sync::{MutexGuard, Notify};
use uuid::Uuid;

mod common;

const YEAR: i64 = 365 * 24 * 60 * 60;

struct Harness {
	pool: PgPool,
	allocations: Arc<PgAllocations>,
	users: PgUsers,
	subs: PgSubscriptions,
	reds: PgRedemptions,
	issuances: PgUnitIssuances,
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
	/// Held for the test's whole life — declared last so it is released after the relay.
	_serial: MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "treasury ownership test").await?;
	let notify = Arc::new(Notify::new());
	Some(Harness {
		allocations: Arc::new(PgAllocations::new(pool.clone())),
		users: PgUsers::new(pool.clone()),
		subs: PgSubscriptions::new(pool.clone()),
		reds: PgRedemptions::new(pool.clone()),
		issuances: PgUnitIssuances::new(pool.clone()),
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
		_serial: serial,
	})
}

fn fund_ports(h: &Harness) -> funds_app::FundPorts<'_> {
	funds_app::FundPorts {
		allocations: h.allocations.as_ref(),
		ledger: h.ledger.as_ref(),
		nav: &h.nav,
		relay: &h.notify,
	}
}

fn treasury_ports(h: &Harness) -> balance_app::TreasuryPorts<'_> {
	balance_app::TreasuryPorts {
		ledger: h.ledger.as_ref(),
		custody: &StubCustody,
		allocations: h.allocations.as_ref(),
		nav: &h.nav,
	}
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn shares(decimal: &str) -> Shares {
	Shares::parse_decimal(decimal).unwrap()
}

fn unique_service() -> ServiceId {
	ServiceId::parse(&format!("trs-{}", Uuid::new_v4())).unwrap()
}

fn now_unix() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

async fn cash_of(h: &Harness, key: LedgerAccountKey) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

async fn units_of(h: &Harness, key: LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

/// A real `users` row at KYC tier 1 — an issuance names it.
async fn person(h: &Harness) -> UserId {
	let subject = AuthSubject::parse(&format!("trs-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("t{}@example.com", Uuid::new_v4().simple())).unwrap();
	let user = h.users.provision(subject, email, true).await.unwrap().id();
	common::set_kyc_level(&h.pool, user, 1).await;
	user
}

/// A registered, open product admitting every investor — with the house 2-and-20 terms
/// when `with_fees`, so a year on a holding owes a charge.
async fn open_product(h: &Harness, service: &ServiceId, with_fees: bool) {
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "EV Trading", "Systematic crypto", AllocationIcon::default()).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	h.allocations.open(service).await.unwrap();
	h.allocations.set_access(service, AllocationAccess::Invest).await.unwrap();
	if !with_fees {
		return;
	}
	let ports = fee_app::FeePolicyPorts {
		policies: &h.policies,
		changes: &h.changes,
		allocations: h.allocations.as_ref(),
		consilia: &h.consilia,
		approval_url_base: "https://example.test/approve",
		governance_mail_wired: true,
	};
	let request = fee_app::PolicyChangeRequest {
		service: service.clone(),
		policy: FeePolicy::HOUSE,
		requested_effective_from_unix: 0,
		reason: String::new(),
	};
	let change = fee_app::schedule_policy(&ports, UserId::new(), request, now_unix()).await.unwrap();
	assert!(h.changes.promote(change.id, now_unix()).await.unwrap(), "a fund with no holders takes new terms at once");
}

/// New custody credited to `party`: a person's chain deposit, or — for an allocation,
/// which no deposit may credit since #245 — the same `Dr wallet / Cr claim` posted straight
/// to the ledger, standing in for a settled fee or a realised return.
async fn fund_party(h: &Harness, party: Party, amount: &str) {
	match party {
		Party::User(user) => {
			let tx_ref = TxRef::parse(&format!("trs-{}", Uuid::new_v4())).unwrap();
			balance_app::record_deposit(&h.deposits, &h.notify, tx_ref, user, Network::Bep20, usdt(amount)).await.unwrap();
			common::drain_to_quiescence(&h.relay, &h.pool).await;
		}
		allocation_or_treasury => {
			let transfer = LedgerTransfer {
				id: Uuid::new_v4().as_u128(),
				debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
				credit: allocation_or_treasury.claim_key(),
				amount: usdt(amount).base_units(),
				code: TransferCode::Deposit,
				reference: 0,
			};
			h.ledger.post(&transfer).await.unwrap();
		}
	}
}

/// Subscribe and wait for the position projection — the accrual clocks live on it, and
/// the backdating below must not be overwritten by a projection landing late.
async fn subscribe(h: &Harness, user: UserId, service: &ServiceId, amount: &str) {
	funds_app::subscribe(&fund_ports(h), &h.subs, user, service.clone(), usdt(amount), now_unix()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	for _ in 0..100 {
		if h.accruals.find(user, service).await.unwrap().is_some_and(|accrual| !accrual.cost_basis.is_zero()) {
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(50)).await;
	}
	panic!("the subscription's position projection never landed");
}

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

/// A year of 2 % on `investor`'s holding, charged into the product's fee class.
async fn charge_a_year(h: &Harness, investor: UserId, service: &ServiceId) -> Shares {
	backdate(h, investor, service, YEAR).await;
	let assessment = fee_app::assess_position(
		&h.policies,
		&h.accruals,
		&h.assessments,
		h.ledger.as_ref(),
		&h.nav,
		&h.notify,
		investor,
		service.clone(),
		Trigger::Period,
		now_unix(),
	)
	.await
	.unwrap()
	.expect("a year owes a fee");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assessment.charge().charged_units
}

/// Mint `units` of the `fee` allocation to `holder` — through the door the data migration
/// and an executed `HolderGrant` use — priced at the allocation's own computed NAV.
async fn grant_fee_units(h: &Harness, holder: UserId, units: &str) {
	issuance_app::grant_units(
		&fund_ports(h),
		&h.issuances,
		&h.users,
		issuance_app::IssueUnitsRequest {
			service: ServiceId::fee(),
			holder: UnitHolder::User(holder),
			units: shares(units),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse(&format!("grant-{}", Uuid::new_v4())).unwrap(),
		},
		now_unix(),
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

async fn scan(h: &Harness) -> ReconReport {
	Reconciliation::new(h.pool.clone(), h.ledger.clone(), h.allocations.clone())
		.scan()
		.await
		.expect("reconciliation scan")
}

async fn ownership(h: &Harness, service: &ServiceId) -> ownership_app::AllocationOwnership {
	ownership_app::allocation_ownership(h.ledger.as_ref(), service.clone()).await.unwrap()
}

fn line<'a>(treasury: &'a balance_app::Treasury, service: &ServiceId) -> &'a balance_app::AllocationTreasury {
	treasury
		.allocations
		.iter()
		.find(|a| &a.service == service)
		.unwrap_or_else(|| panic!("the treasury does not list '{service}'"))
}

/// The treasury is the registry's list, every row with its claim, its supply and its
/// holders read off the ledger — a product with its investor, and the hidden `fee` and
/// `fund` allocations exactly the same way, with the person a grant seated as `fee`'s
/// holder. The admin revenue screen is the same line for `fee`, not a second reading.
#[tokio::test]
async fn the_treasury_lists_every_allocation_with_its_holders_including_the_hidden_ones() {
	let Some(h) = harness().await else { return };
	let (investor, owner) = (person(&h).await, person(&h).await);
	let product = unique_service();
	open_product(&h, &product, false).await;
	fund_party(&h, Party::User(investor), "100").await;
	subscribe(&h, investor, &product, "60").await;
	// Value first, units second: a grant into an empty allocation would price the next
	// one at zero.
	fund_party(&h, Party::Service(ServiceId::fee()), "10").await;
	grant_fee_units(&h, owner, "10").await;

	let treasury = balance_app::treasury(&treasury_ports(&h)).await.unwrap();

	let line_product = line(&treasury, &product);
	assert_eq!(line_product.title, "EV Trading");
	assert_eq!(line_product.access, AllocationAccess::Invest);
	assert_eq!(line_product.claim.posted, usdt("60"), "the subscription's cash is the product's");
	assert_eq!(line_product.units_outstanding, shares("60"), "60 units at the seed NAV");
	assert_eq!(
		line_product.holders,
		vec![issuance_app::UnitHolding {
			holder: UnitHolder::User(investor),
			units: shares("60"),
		}]
	);

	let line_fee = line(&treasury, &ServiceId::fee());
	assert_eq!(line_fee.access, AllocationAccess::Hidden, "hidden from the catalog, listed by the treasury");
	assert_eq!(line_fee.claim.posted, cash_of(&h, LedgerAccountKey::ServiceClaim(ServiceId::fee())).await);
	assert!(
		line_fee.holders.iter().any(|holding| holding.holder == UnitHolder::User(owner) && holding.units == shares("10")),
		"the granted person holds fee: {:?}",
		line_fee.holders
	);
	assert_eq!(line_fee.units_outstanding, units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await);
	assert!(line_fee.nav.aum.is_some(), "a reserved allocation's value is computed");

	let line_fund = line(&treasury, &ServiceId::fund());
	assert_eq!(line_fund.access, AllocationAccess::Hidden);

	// The revenue screen reads the same line.
	let revenue = balance_app::fee_allocation(h.allocations.as_ref(), h.ledger.as_ref(), &h.nav).await.unwrap();
	assert_eq!(revenue.claim, line_fee.claim);
	assert_eq!(revenue.holders, line_fee.holders);
	assert_eq!(revenue.units_outstanding, line_fee.units_outstanding);
}

/// `held_for_clients` on the wire is `Treasury::held_by_users`: the sum of what people
/// hold directly. A deposit raises it by exactly the deposit; a subscription moves the
/// cash to the product's claim and lowers it by exactly that, with custody unchanged —
/// where the old remainder `custody − fund − fee` would not have moved; and cash landing
/// on the `fee` claim raises custody, not what people hold.
#[tokio::test]
async fn held_by_users_is_the_sum_of_user_claims_and_not_a_remainder() {
	let Some(h) = harness().await else { return };
	let investor = person(&h).await;
	let product = unique_service();
	open_product(&h, &product, false).await;
	let before = balance_app::treasury(&treasury_ports(&h)).await.unwrap();

	fund_party(&h, Party::User(investor), "100").await;
	let deposited = balance_app::treasury(&treasury_ports(&h)).await.unwrap();
	assert_eq!(deposited.held_by_users, before.held_by_users.checked_add(usdt("100")).unwrap());
	assert_eq!(deposited.total_custody, before.total_custody.checked_add(usdt("100")).unwrap());

	subscribe(&h, investor, &product, "40").await;
	let subscribed = balance_app::treasury(&treasury_ports(&h)).await.unwrap();
	assert_eq!(subscribed.total_custody, deposited.total_custody, "a subscription moves no custody");
	assert_eq!(
		subscribed.held_by_users,
		deposited.held_by_users.checked_sub(usdt("40")).unwrap(),
		"the cash is the product's now"
	);
	assert_eq!(line(&subscribed, &product).claim.posted, usdt("40"));

	fund_party(&h, Party::Service(ServiceId::fee()), "5").await;
	let fee_funded = balance_app::treasury(&treasury_ports(&h)).await.unwrap();
	assert_eq!(fee_funded.held_by_users, subscribed.held_by_users, "the fee allocation's cash is not people's");
	assert_eq!(fee_funded.total_custody, subscribed.total_custody.checked_add(usdt("5")).unwrap());

	// The figure is the same sum the reconciliation attributes to people.
	let cash = h.ledger.cash_invariant().await.unwrap();
	assert_eq!(fee_funded.held_by_users, Usdt::from_base_units(cash.user_claims));
	assert_eq!(cash.custody, cash.claims, "conservation holds through all of it");
	assert_eq!(cash.unclassified(), 0);
}

/// banking's own treasury (D17) is a line of its own: not an allocation, not people's — the
/// withdrawal fees it keeps, with their own posted / reserved / available split.
#[tokio::test]
async fn the_treasury_shows_banking_s_own_account_on_a_line_of_its_own() {
	let Some(h) = harness().await else { return };
	let before = balance_app::treasury(&treasury_ports(&h)).await.unwrap();
	fund_party(&h, Party::Treasury, "7").await;
	let after = balance_app::treasury(&treasury_ports(&h)).await.unwrap();
	assert_eq!(after.banking_treasury.posted, before.banking_treasury.posted.checked_add(usdt("7")).unwrap());
	assert_eq!(after.banking_treasury.available, before.banking_treasury.available.checked_add(usdt("7")).unwrap());
	assert_eq!(after.held_by_users, before.held_by_users, "banking's money is not people's");
	assert_eq!(after.held_in_compartments, before.held_in_compartments);
	assert_eq!(
		balance_app::banking_treasury(h.ledger.as_ref()).await.unwrap(),
		after.banking_treasury,
		"GetFundRevenue reads the same line"
	);
	let cash = h.ledger.cash_invariant().await.unwrap();
	assert_eq!(cash.custody, cash.claims);
	assert_eq!(cash.unclassified(), 0, "the treasury is a named part of the claims");
}

/// Cash on an allocation nobody holds units of — the `fee` claim's state until the data
/// migration seats its holders, or a product credited before its first subscription —
/// is reported by name and counted, and the scan stays clean: nothing is lost, the
/// value is simply not yet anyone's. Seating a holder makes it theirs and the report
/// stops naming the allocation.
#[tokio::test]
async fn a_leftover_without_a_holder_is_reported_not_swallowed() {
	let Some(h) = harness().await else { return };
	let product = unique_service();
	open_product(&h, &product, false).await;
	fund_party(&h, Party::Service(product.clone()), "25").await;
	assert!(units_of(&h, LedgerAccountKey::SharesOutstanding(product.clone())).await.is_zero());

	let before = telemetry::unheld_allocation_value();
	let report = scan(&h).await;
	assert!(report.unheld.contains(&product), "the product holds 25 USDT nobody owns: {:?}", report.unheld);
	assert!(report.units_drift.is_empty(), "unheld value is not drift: {:?}", report.units_drift);
	assert!(report.clean(), "unheld value is a warning, never an alert: {report:?}");
	assert!(telemetry::unheld_allocation_value() > before, "each finding counts");
	// Whatever the sibling tests left on the `fee` claim, it is reported the same way
	// while nobody holds `fee`, and never as drift.
	let fee = ownership(&h, &ServiceId::fee()).await;
	assert_eq!(report.unheld.contains(&ServiceId::fee()), fee.is_unheld());

	// A holder seated by an in-kind issue: the same 25 USDT is now theirs.
	let holder = person(&h).await;
	issuance_app::issue_units(
		&fund_ports(&h),
		&h.issuances,
		&h.users,
		issuance_app::IssueUnitsRequest {
			service: product.clone(),
			holder: UnitHolder::User(holder),
			units: shares("25"),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse(&format!("issue-{}", Uuid::new_v4())).unwrap(),
		},
		now_unix(),
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let after = scan(&h).await;
	assert!(!after.unheld.contains(&product), "a seated holder owns the cash: {:?}", after.unheld);
	assert!(after.units_drift.is_empty());
}

/// Through every move that touches units — a subscription, an in-kind issue, a year's
/// fee charge, its settlement, a redemption, a retirement — the supply is what the
/// holders add up to, on the product and on the `fee` allocation that holds its fee
/// class, and the reconciliation finds no drift.
#[tokio::test]
async fn per_allocation_units_reconcile_after_grant_redeem_and_fee_charge() {
	let Some(h) = harness().await else { return };
	let (investor, insider, owner) = (person(&h).await, person(&h).await, person(&h).await);
	let product = unique_service();
	open_product(&h, &product, true).await;
	fund_party(&h, Party::User(investor), "1000").await;
	subscribe(&h, investor, &product, "1000").await;

	let reconciled = |o: &ownership_app::AllocationOwnership, step: &str| {
		assert!(
			o.units_reconcile(),
			"{step}: '{}' outstanding {} != held {} ({:?})",
			o.service,
			o.units_outstanding.to_decimal_string(),
			o.held_units().to_decimal_string(),
			o.holders
		);
	};
	reconciled(&ownership(&h, &product).await, "after the subscription");

	// A year of fees: units move from the investor to the product's fee class — the `fee`
	// allocation's holding — and the supply does not move.
	let charged = charge_a_year(&h, investor, &product).await;
	assert!(charged > Shares::ZERO);
	let charged_picture = ownership(&h, &product).await;
	reconciled(&charged_picture, "after the fee charge");
	assert_eq!(charged_picture.units_outstanding, shares("1000"), "a charge is a move between holders");
	assert!(
		charged_picture
			.holders
			.iter()
			.any(|line| line.holder == UnitHolder::Allocation(ServiceId::fee()) && line.units == charged),
		"the fee allocation holds the fee class: {:?}",
		charged_picture.holders
	);
	let fee_picture = ownership(&h, &ServiceId::fee()).await;
	reconciled(&fee_picture, "fee after the charge");
	assert!(fee_picture.product_units.iter().any(|(service, units)| service == &product && *units == charged));

	// Seat a holder of `fee` (value first) and settle the fee class into cash.
	grant_fee_units(&h, owner, "10").await;
	fee_app::settle_fee_shares(&h.settlements, h.ledger.as_ref(), &h.nav, &h.reds, &h.notify, product.clone(), None, UserId::new(), now_unix())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	reconciled(&ownership(&h, &product).await, "after the settlement");
	let fee_settled = ownership(&h, &ServiceId::fee()).await;
	reconciled(&fee_settled, "fee after the settlement");
	assert!(fee_settled.holders.iter().any(|line| line.holder == UnitHolder::User(owner)));

	// A redemption burns the investor's units.
	funds_app::request_redemption(&fund_ports(&h), &h.reds, investor, product.clone(), shares("100"), now_unix())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	reconciled(&ownership(&h, &product).await, "after the redemption");

	// An in-kind issue to a second person: supply grows by what they hold. After the
	// redemption on purpose — a mint with no cash behind it flips the product to
	// `in_kind`, and a redemption out of an in-kind product is refused.
	issuance_app::issue_units(
		&fund_ports(&h),
		&h.issuances,
		&h.users,
		issuance_app::IssueUnitsRequest {
			service: product.clone(),
			holder: UnitHolder::User(insider),
			units: shares("100"),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse(&format!("issue-{}", Uuid::new_v4())).unwrap(),
		},
		now_unix(),
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let issued = ownership(&h, &product).await;
	reconciled(&issued, "after the issue");
	assert_eq!(issued.holders.len(), 2, "the investor and the insider: {:?}", issued.holders);

	// A retirement burns the insider's.
	issuance_app::retire_units(
		&fund_ports(&h),
		&h.issuances,
		&h.users,
		issuance_app::RetireUnitsRequest {
			service: product.clone(),
			holder: UnitHolder::User(insider),
			units: shares("100"),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse(&format!("retire-{}", Uuid::new_v4())).unwrap(),
			force: true,
		},
		now_unix(),
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let retired = ownership(&h, &product).await;
	reconciled(&retired, "after the retirement");
	assert!(retired.holders.iter().all(|line| line.holder != UnitHolder::User(insider)), "an emptied holder is not listed");

	let report = scan(&h).await;
	assert!(report.units_drift.is_empty(), "no allocation drifted: {:?}", report.units_drift);
	assert!(report.clean(), "{report:?}");
}
