//! Integration tests for the `fee` allocation as the holder of every fee (issue #245,
//! phase 1) — real Postgres **and** TigerBeetle (no mocks, per the project rules). They
//! run when `DATABASE_URL` is set and a TigerBeetle replica is reachable (`nix run .#db`
//! + `.#tb`), and skip otherwise.
//!
//! What these pin down is the ownership chain, end to end:
//!
//! 1. **An allocation fee is the `fee` allocation's, and the allocation is its people's.** A
//!    product's settled fee class and a taker fee land on `service:fee`, whose price is what
//!    it holds over its supply — so a person holding `fee` units sees their position rise as
//!    fees accrue and settle. A retained withdrawal fee is banking's own and goes to its
//!    treasury instead (D17), leaving the `fee` holders' price where it was.
//! 2. **A reserved allocation's cash is never queued for.** A redemption of `fee` units
//!    the claim cannot cover is refused outright; once fee units are settled into cash,
//!    the same redemption completes.
//! 3. **Hidden is not invisible to a holder.** The catalog never lists `fee`, but the
//!    person holding its units reads its title and its price like any position.
//!
//! Every test here reads the one platform-wide `service:fee` claim and its NAV, so the
//! suite runs serially under the shared outbox guard — the same rule `allocation_registry`
//! applies, for the same reason. Serial is not isolated, though: the claim, the supply and
//! the fee-unit holders are one set of accounts per binary, and the tests take the guard
//! in whatever order the runtime hands it out. So each test brackets its own effect
//! (before/after, never an absolute), puts value behind `fee` **before** it mints any of
//! its units (a grant into an empty allocation prices the next one at zero, and a
//! zero-NAV issuance is refused by the row's check), and sizes an "uncovered" ask off
//! the claim's balance it reads, not off the claim being empty.

use std::sync::Arc;

use domain::{
	allocations::{Allocation, AllocationAccess, AllocationIcon, AllocationId},
	auth::AuthSubject,
	balance::{LedgerAccountKey, Party, ServiceId, TransferCode, ValuationId},
	error::DomainError,
	fees::{FeePolicy, Trigger},
	issuance::{IdempotencyKey, UnitHolder},
	money::{Nav, Network, Shares, TxRef, Usdt, WalletAddress},
	users::{Email, UserId},
	withdrawals::WithdrawalId,
};
use piggybank_core::{
	application::{allocations as allocations_app, balance as balance_app, fees as fee_app, funds as funds_app, issuance as issuance_app, withdrawals as withdrawal_app},
	config::KycGate,
	infrastructure::{
		allocations::PgAllocations,
		consilium::PgConsilia,
		custody::StubCustody,
		deposits::PgDeposits,
		fee_policy_changes::PgFeePolicyChanges,
		fees::{PgFeeAssessments, PgFeePolicies, PgFeeSettlements, PgPositionAccruals},
		issuance::PgUnitIssuances,
		nav::PgNav,
		outflow::PgOutflowPolicy,
		positions::PgFundPositions,
		redemptions::PgRedemptions,
		relay::Relay,
		subscriptions::PgSubscriptions,
		users::PgUsers,
		withdrawals::PgWithdrawals,
	},
	ports::{
		AllocationRegistry, RedemptionRepository, UserRepository,
		fees::{FeePolicyChanges, FeeSettlements, PositionAccruals},
		ledger::{Ledger, LedgerTransfer},
		nav::NavMarks,
	},
};
use sqlx::PgPool;
use tokio::sync::{MutexGuard, Notify};
use uuid::Uuid;

mod common;

const YEAR: i64 = 365 * 24 * 60 * 60;

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
	policies: PgFeePolicies,
	changes: PgFeePolicyChanges,
	consilia: PgConsilia,
	accruals: PgPositionAccruals,
	assessments: PgFeeAssessments,
	settlements: PgFeeSettlements,
	withdrawals: PgWithdrawals,
	outflow: PgOutflowPolicy,
	ledger: Arc<dyn Ledger>,
	relay: Relay,
	notify: Arc<Notify>,
	/// Held for the test's whole life — declared last so it is released after the relay.
	_serial: MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "ownership fee test").await?;
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
		policies: PgFeePolicies::new(pool.clone()),
		changes: PgFeePolicyChanges::new(pool.clone()),
		consilia: PgConsilia::new(pool.clone()),
		accruals: PgPositionAccruals::new(pool.clone()),
		assessments: PgFeeAssessments::new(pool.clone()),
		settlements: PgFeeSettlements::new(pool.clone()),
		withdrawals: PgWithdrawals::new(pool.clone()),
		outflow: PgOutflowPolicy::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		ledger,
		notify,
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

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn shares(decimal: &str) -> Shares {
	Shares::parse_decimal(decimal).unwrap()
}

fn nav(decimal: &str) -> Nav {
	Nav::parse_decimal(decimal).unwrap()
}

fn unique_service() -> ServiceId {
	ServiceId::parse(&format!("own-{}", Uuid::new_v4())).unwrap()
}

fn now_unix() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn fee_claim() -> LedgerAccountKey {
	LedgerAccountKey::ServiceClaim(ServiceId::fee())
}

async fn cash_of(h: &Harness, key: LedgerAccountKey) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

async fn units_of(h: &Harness, key: LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

async fn fee_nav(h: &Harness) -> Nav {
	funds_app::nav_of(&h.nav, h.ledger.as_ref(), &ServiceId::fee()).await.unwrap().nav
}

/// A real `users` row at KYC tier 1 — an issuance names it, a withdrawal is gated on it.
async fn person(h: &Harness) -> UserId {
	let subject = AuthSubject::parse(&format!("own-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("o{}@example.com", Uuid::new_v4().simple())).unwrap();
	let user = h.users.provision(subject, email, true).await.unwrap().id();
	common::set_kyc_level(&h.pool, user, 1).await;
	user
}

/// A registered, open product with the house 2-and-20 terms, admitting every investor.
async fn open_fund(h: &Harness, service: &ServiceId) {
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "EV Trading", "Systematic crypto", AllocationIcon::default()).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	h.allocations.open(service).await.unwrap();
	h.allocations.set_access(service, AllocationAccess::Invest).await.unwrap();
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
			let tx_ref = TxRef::parse(&format!("own-{}", Uuid::new_v4())).unwrap();
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

async fn fund_user(h: &Harness, user: UserId, amount: &str) {
	fund_party(h, Party::User(user), amount).await;
}

/// Put `amount` of cash behind the `fee` allocation, posted straight onto its claim — what
/// a settled fee class leaves there, minus the product, the investor and the year it takes
/// to earn one. A test that only needs the allocation to be *worth something* before it
/// mints units calls this instead of restaging the whole fee chain.
async fn fund_fee_claim(h: &Harness, amount: &str) {
	fund_party(h, Party::Service(ServiceId::fee()), amount).await;
}

/// Units worth no less than `cash` at `price`. `Shares::from_cash` floors, so its answer can
/// be worth up to one base unit of price *less* than `cash`; one more base unit of shares
/// closes that gap whatever the price is — `price.value(result) >= cash`.
fn units_worth_at_least(cash: Usdt, price: Nav) -> Shares {
	Shares::from_base_units(Shares::from_cash(cash, price).unwrap().base_units() + 1)
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
/// and an executed `HolderGrant` use (the operator's `issue_units` refuses a reserved
/// allocation) — priced at the allocation's own computed NAV.
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

async fn settle_fee_shares(h: &Harness, service: &ServiceId) -> Usdt {
	let settlement = fee_app::settle_fee_shares(&h.settlements, h.ledger.as_ref(), &h.nav, &h.reds, &h.notify, service.clone(), None, UserId::new(), now_unix())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	settlement.cash()
}

/// `holder`'s position in `service` as `ListPositions` shows it.
async fn position_of(h: &Harness, holder: UserId, service: &ServiceId) -> Option<funds_app::PositionView> {
	funds_app::list_positions(&h.positions, h.ledger.as_ref(), &h.nav, holder)
		.await
		.unwrap()
		.into_iter()
		.find(|position| &position.service == service)
}

/// The product's fee class settles into the `fee` allocation's claim, and the people
/// holding `fee` units own it: their price is the fee class at the product's NAV while
/// the units are held, and the same value in cash once they are settled. Marking the
/// product up raises the fee holders' NAV before a single USDT has moved.
#[tokio::test]
async fn settled_fee_cash_lands_in_the_fee_allocation_and_its_holders_nav_rises() {
	let Some(h) = harness().await else { return };
	let (investor, owner) = (UserId::new(), person(&h).await);
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, investor, "1000").await;
	subscribe(&h, investor, &service, "1000").await;
	let fee_class = charge_a_year(&h, investor, &service).await;
	assert!(fee_class > Shares::ZERO, "the charge accumulated fee units");
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, fee_class);

	// The allocation's value is what it holds: the fee class at the product's NAV plus its
	// cash — read from the ledger, whatever the claim and the marks did before this test.
	let fee_units_before = units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await;
	let claim_before = cash_of(&h, fee_claim()).await;
	let value_before = funds_app::nav_of(&h.nav, h.ledger.as_ref(), &ServiceId::fee()).await.unwrap().aum.expect("a computed value");
	assert!(
		value_before >= claim_before.checked_add(Nav::SEED.value(fee_class).unwrap()).unwrap(),
		"the fee class is counted at the seed NAV"
	);

	// A person is granted as many `fee` units as the allocation is worth in USDT — the
	// migration's shape: to the first holder, one unit is then worth one USDT more or
	// less. A sibling's holders dilute that, so the price is checked as value over the
	// supply that results, not as a figure.
	let granted = Shares::from_cash(value_before, Nav::SEED).unwrap();
	grant_fee_units(&h, owner, &granted.to_decimal_string()).await;
	let supply = fee_units_before.checked_add(granted).unwrap();
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await, supply);
	let nav_at_grant = fee_nav(&h).await;
	assert_eq!(nav_at_grant, Nav::from_aum(value_before, supply).unwrap());

	// Mark the product up 2x (through the owners' writer, past the single-poster guard):
	// the fee class is now worth twice as much, and so are the fee holders' units.
	let aum = cash_of(&h, LedgerAccountKey::ServiceClaim(service.clone())).await;
	funds_app::record_valuation(&h.nav, h.ledger.as_ref(), ValuationId::new(), service.clone(), aum.checked_add(aum).unwrap(), "itest")
		.await
		.unwrap();
	let marked_up = fee_nav(&h).await;
	assert!(marked_up > nav_at_grant, "the fee holders' NAV rises with the product's mark: {nav_at_grant} -> {marked_up}");
	let owner_position = position_of(&h, owner, &ServiceId::fee()).await.expect("the grant projected a position");
	assert_eq!(owner_position.nav, marked_up, "the position card prices at the computed NAV");
	assert_eq!(owner_position.value, marked_up.value(granted).unwrap());

	// Settle: the product buys its fee class back at today's NAV. Cash lands on the fee
	// allocation's claim — not on the retired revenue account — and the holders' NAV is
	// unchanged, because the units left and their exact value arrived.
	let retired_before = h.ledger.cash_invariant().await.unwrap().retired_claims;
	let cash = settle_fee_shares(&h, &service).await;
	assert_eq!(cash, nav("2").value(fee_class).unwrap(), "priced at the day's NAV");
	assert_eq!(cash_of(&h, fee_claim()).await, claim_before.checked_add(cash).unwrap(), "the cash is the fee allocation's");
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, Shares::ZERO);
	assert_eq!(
		h.ledger.cash_invariant().await.unwrap().retired_claims,
		retired_before,
		"nothing new lands on the retired revenue claim"
	);
	let settled = fee_nav(&h).await;
	assert!(
		settled.base_units().abs_diff(marked_up.base_units()) <= 1,
		"settling crystallizes value, it does not move it: {marked_up} -> {settled}"
	);

	// The cap tables read the chain the same way: the product's fee class was the fee
	// allocation's line, and the fee allocation's supply is the person's.
	let fee_holders = issuance_app::unit_holders(&h.allocations, h.ledger.as_ref(), &h.issuances, ServiceId::fee()).await.unwrap();
	let mine = fee_holders
		.holders
		.iter()
		.find(|line| line.holder == UnitHolder::User(owner))
		.expect("the owner is a holder of fee");
	assert_eq!(mine.units, granted);
}

/// A retained withdrawal fee is banking's own (D17): it lands in banking's treasury,
/// `ScopeAccount(main, treasury)`, and the `fee` allocation — its claim and its holders'
/// NAV — does not move.
#[tokio::test]
async fn a_retained_withdrawal_fee_lands_in_the_treasury_not_the_fee_allocation() {
	let Some(h) = harness().await else { return };
	let (user, owner) = (person(&h).await, person(&h).await);
	fund_user(&h, user, "100").await;
	// Value first, units second: the holder's price is what stands behind the units.
	fund_fee_claim(&h, "10").await;
	grant_fee_units(&h, owner, "10").await;
	let claim_before = cash_of(&h, fee_claim()).await;
	let quote_before = funds_app::nav_of(&h.nav, h.ledger.as_ref(), &ServiceId::fee()).await.unwrap();
	let treasury = domain::balance::FeePayee::treasury().claim_key();
	let treasury_before = cash_of(&h, treasury.clone()).await;

	let network = Network::Bep20;
	let ports = withdrawal_app::WithdrawalPorts {
		withdrawals: &h.withdrawals,
		ledger: h.ledger.as_ref(),
		custody: &StubCustody,
		relay: &h.notify,
	};
	let gates = withdrawal_app::AdmissionGates {
		policy: &h.outflow,
		configured: &Network::ALL,
		kyc: KycGate::ENFORCED,
	};
	let destination = WalletAddress::parse(network, "0x52908400098527886E0F7030069857D2E4169EE7").unwrap();
	let withdrawal = withdrawal_app::request_withdrawal(&ports, &gates, WithdrawalId::new(), user, network, destination, usdt("50"))
		.await
		.unwrap();
	let fee = withdrawal.fee();
	assert!(!fee.is_zero(), "the flat fee is what this test is about");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	withdrawal_app::settle_withdrawal(&h.withdrawals, &h.notify, withdrawal.id(), TxRef::parse(&format!("own-{}", Uuid::new_v4())).unwrap())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	assert_eq!(cash_of(&h, treasury).await, treasury_before.checked_add(fee).unwrap(), "the retained fee is banking's treasury's");
	assert_eq!(cash_of(&h, fee_claim()).await, claim_before, "the fee allocation's claim did not move");
	assert_eq!(fee_nav(&h).await, quote_before.nav, "nor did its holders' NAV");
}

/// A holder of `fee` units exits like any investor — priced at the allocation's NAV,
/// paid out of its claim — except that a shortfall is **refused, never queued**: nobody
/// tops the fee allocation up on request. Settling the product's fee class puts the cash
/// there, and the same redemption then completes at once.
#[tokio::test]
async fn a_redemption_of_fee_units_the_allocations_cash_cannot_cover_is_refused_not_queued() {
	let Some(h) = harness().await else { return };
	let (investor, owner) = (UserId::new(), person(&h).await);
	let service = unique_service();
	open_fund(&h, &service).await;
	// A large position, so the year's fee class (2 %) is worth far more than the cash a
	// sibling test settles into the claim — the shortfall below is by construction.
	fund_user(&h, investor, "1000000").await;
	subscribe(&h, investor, &service, "1000000").await;
	let fee_class = charge_a_year(&h, investor, &service).await;

	// The owner is granted as many units as the allocation is worth: their stake is backed
	// by the product's fee class plus whatever cash the claim already holds, less what the
	// siblings' holders own of it.
	let value = funds_app::nav_of(&h.nav, h.ledger.as_ref(), &ServiceId::fee()).await.unwrap().aum.unwrap();
	let claim_before = cash_of(&h, fee_claim()).await;
	let granted = Shares::from_cash(value, Nav::SEED).unwrap();
	grant_fee_units(&h, owner, &granted.to_decimal_string()).await;
	let price = fee_nav(&h).await;
	// Ask for more than the claim can pay right now — read, not assumed empty: the part of
	// the value that is still the product's fee units.
	let available = Usdt::from_base_units(h.ledger.balance(&fee_claim()).await.unwrap().available());
	let uncovered = units_worth_at_least(available.checked_add(usdt("1")).unwrap(), price);
	assert!(
		uncovered <= granted,
		"the owner's stake ({granted}) outweighs the claim's cash ({available}) — the fee class is the bulk of the value"
	);

	let err = funds_app::request_redemption(&fund_ports(&h), &h.reds, owner, ServiceId::fee(), uncovered, now_unix())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("cannot cover")), "refused as a shortfall: {err:?}");
	assert!(h.reds.list_by_user(owner).await.unwrap().is_empty(), "refused, not queued: no redemption row at all");
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(ServiceId::fee(), owner)).await, granted, "nothing was reserved");

	// The product settles its fee class into the allocation's claim; now the cash is there.
	let cash = settle_fee_shares(&h, &service).await;
	assert_eq!(cash, Nav::SEED.value(fee_class).unwrap());
	let redemption = funds_app::request_redemption(&fund_ports(&h), &h.reds, owner, ServiceId::fee(), uncovered, now_unix())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let redemption = h.reds.find_by_id(redemption.id()).await.unwrap().unwrap();
	assert_eq!(redemption.state(), domain::redemptions::RedemptionState::Completed, "covered, so settled at once");
	assert_eq!(redemption.nav(), Some(price), "priced at the allocation's own NAV");
	let paid = redemption.cash().unwrap();
	assert_eq!(cash_of(&h, LedgerAccountKey::UserClaim(owner)).await, paid, "paid into the person's claim");
	assert_eq!(
		cash_of(&h, fee_claim()).await,
		claim_before.checked_add(cash).unwrap().checked_sub(paid).unwrap(),
		"out of the allocation's claim"
	);
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(ServiceId::fee(), owner)).await, granted.checked_sub(uncovered).unwrap());
}

/// The catalog never lists `fee`, and a stranger asking for it is told it does not exist —
/// but the person holding its units reads its title and its price like any position they
/// own, through the same reads the position screen makes.
#[tokio::test]
async fn a_holder_of_the_hidden_fee_allocation_sees_their_position_with_its_title_and_price() {
	let Some(h) = harness().await else { return };
	let (owner, stranger) = (person(&h).await, person(&h).await);
	// Value first, units second — a position priced at zero is no position to read.
	fund_fee_claim(&h, "10").await;
	grant_fee_units(&h, owner, "10").await;

	// Not in anyone's catalog, holder or not.
	for user in [owner, stranger] {
		let catalog = allocations_app::list_for(&h.allocations, user, false).await.unwrap();
		assert!(!catalog.iter().any(|record| record.allocation.service().is_reserved()), "a reserved allocation is never listed");
		let err = allocations_app::get_for(&h.allocations, &ServiceId::fee(), user, false).await.unwrap_err();
		assert!(matches!(err, DomainError::NotFound { .. }), "the plain visibility gate still hides it: {err:?}");
	}

	// The holder reads it: the position, its title, its price.
	let position = position_of(&h, owner, &ServiceId::fee()).await.expect("the fee units are a position");
	assert_eq!(position.units, shares("10"));
	assert_eq!(position.nav, fee_nav(&h).await);
	let record = funds_app::allocation_for_holder(&h.allocations, h.ledger.as_ref(), &ServiceId::fee(), owner, false)
		.await
		.expect("a holder reads the hidden allocation");
	assert_eq!(record.allocation.title(), "Fee allocation");
	assert_eq!(record.caller_access, AllocationAccess::Hidden, "with their honest access level, not a courtesy");
	let view = funds_app::fund_nav_view(&h.allocations, &h.nav, h.ledger.as_ref(), ServiceId::fee(), owner, false, now_unix())
		.await
		.expect("a holder reads the hidden allocation's price");
	assert_eq!(view.nav, position.nav);
	assert!(!view.stale, "a computed price over cash is never stale");

	// The stranger holds nothing and reads nothing — the probe answers as an unknown slug.
	assert!(position_of(&h, stranger, &ServiceId::fee()).await.is_none());
	let err = funds_app::allocation_for_holder(&h.allocations, h.ledger.as_ref(), &ServiceId::fee(), stranger, false)
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { .. }), "{err:?}");
	let Err(err) = funds_app::fund_nav_view(&h.allocations, &h.nav, h.ledger.as_ref(), ServiceId::fee(), stranger, false, now_unix()).await else {
		panic!("a stranger must not read the hidden allocation's price")
	};
	assert!(matches!(err, DomainError::NotFound { .. }), "{err:?}");
}

/// A reserved allocation's price is computed, never posted, and it charges no fee of its
/// own — both asks are refused as validation, before anything is written.
#[tokio::test]
async fn a_reserved_allocation_takes_no_mark_and_no_fee_policy() {
	let Some(h) = harness().await else { return };
	for reserved in [ServiceId::fee(), ServiceId::fund()] {
		let err = funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), reserved.clone(), usdt("100"), "itest", now_unix())
			.await
			.unwrap_err();
		assert!(matches!(err, DomainError::Validation(ref m) if m.contains("reserved")), "{reserved}: {err:?}");
		assert!(h.nav.current(&reserved).await.unwrap().is_none(), "{reserved}: no mark was written");

		let ports = fee_app::FeePolicyPorts {
			policies: &h.policies,
			changes: &h.changes,
			allocations: &h.allocations,
			consilia: &h.consilia,
			approval_url_base: "https://example.test/approve",
			governance_mail_wired: true,
		};
		let request = fee_app::PolicyChangeRequest {
			service: reserved.clone(),
			policy: FeePolicy::HOUSE,
			requested_effective_from_unix: 0,
			reason: String::new(),
		};
		let err = fee_app::schedule_policy(&ports, UserId::new(), request, now_unix()).await.unwrap_err();
		assert!(matches!(err, DomainError::Validation(ref m) if m.contains("reserved")), "{reserved}: {err:?}");
		assert!(h.changes.pending(&reserved).await.unwrap().is_none(), "{reserved}: no change was scheduled");
	}
	// With nothing outstanding the price is the seed NAV — the first units issued are
	// worth exactly what stands behind them, and nothing ever divides by zero.
	let fund = funds_app::nav_of(&h.nav, h.ledger.as_ref(), &ServiceId::fund()).await.unwrap();
	if units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fund())).await.is_zero() {
		assert_eq!(fund.nav, Nav::SEED);
	}
	assert_eq!(fund.posted_at_unix, 0, "cash only: never stale");
}

/// H-3 of the #245 security review. A reserved allocation takes no mark of its own: its
/// price is the marks of the products it holds. So the redeem cooldown that binds a
/// product's poster must bind them on `fee` too — otherwise a fee holder marks the product
/// up and cashes their `fee` units out at the price they set, the very move the cooldown
/// exists to stop. Someone who did not mark redeems as before, and the poster is free once
/// the mark has aged out.
#[tokio::test]
async fn a_fee_holder_who_marked_a_product_cannot_redeem_fee_units_for_seven_days() {
	let Some(h) = harness().await else { return };
	let (investor, poster, bystander) = (UserId::new(), person(&h).await, person(&h).await);
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, investor, "1000").await;
	subscribe(&h, investor, &service, "1000").await;
	let fee_class = charge_a_year(&h, investor, &service).await;
	assert!(fee_class > Shares::ZERO, "the fee allocation holds the product's fee class");
	// Cash behind the allocation, so the one-unit redemptions below are covered and the
	// only thing standing between the poster and their cash is the cooldown.
	fund_fee_claim(&h, "100").await;
	grant_fee_units(&h, poster, "10").await;
	grant_fee_units(&h, bystander, "10").await;

	// The poster marks the product under their own id — the direct RPC's writer.
	let aum = cash_of(&h, LedgerAccountKey::ServiceClaim(service.clone())).await;
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), aum, &poster.to_string(), now_unix())
		.await
		.unwrap();

	let err = funds_app::request_redemption(&fund_ports(&h), &h.reds, poster, ServiceId::fee(), shares("1"), now_unix())
		.await
		.expect_err("the poster's fee units are priced off their own mark");
	assert!(
		matches!(err, DomainError::Precondition(ref m) if m.contains("posted a valuation") && m.contains(service.as_str())),
		"refused by the cooldown, naming the product: {err:?}"
	);
	assert!(h.reds.list_by_user(poster).await.unwrap().is_empty(), "refused, not queued");
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(ServiceId::fee(), poster)).await, shares("10"), "nothing was reserved");

	// A holder who did not mark anything redeems as before — covered, so settled at once.
	let redemption = funds_app::request_redemption(&fund_ports(&h), &h.reds, bystander, ServiceId::fee(), shares("1"), now_unix())
		.await
		.expect("the cooldown binds the poster, not the allocation");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(
		h.reds.find_by_id(redemption.id()).await.unwrap().unwrap().state(),
		domain::redemptions::RedemptionState::Completed
	);

	// Seven days on, the mark has aged out and the poster deals again.
	sqlx::query("UPDATE fund_valuations SET posted_at = posted_at - interval '8 days' WHERE service = $1 AND posted_by = $2")
		.bind(service.as_str())
		.bind(poster.to_string())
		.execute(&h.pool)
		.await
		.unwrap();
	// The aged mark makes the product's price stale, which would refuse the deal for a
	// different reason; a fresh mark by somebody else restores it without touching the
	// poster's cooldown.
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), aum, "another-operator", now_unix())
		.await
		.unwrap();
	funds_app::request_redemption(&fund_ports(&h), &h.reds, poster, ServiceId::fee(), shares("1"), now_unix())
		.await
		.expect("an aged-out mark no longer binds its poster");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

/// M-1 of the #245 security review. Settling a product's fee class prices it at the
/// product's dealing NAV and turns it into the `fee` holders' cash — so an administrator
/// who marks the product up and settles at once has converted their own mark into cash
/// one step before the redeem cooldown would have caught them. The same cooldown binds
/// the settler; another administrator settles as before.
#[tokio::test]
async fn the_admin_who_marked_a_product_cannot_settle_its_fee_class_for_seven_days() {
	let Some(h) = harness().await else { return };
	let (investor, poster, colleague) = (UserId::new(), UserId::new(), UserId::new());
	let service = unique_service();
	open_fund(&h, &service).await;
	fund_user(&h, investor, "1000").await;
	subscribe(&h, investor, &service, "1000").await;
	let fee_class = charge_a_year(&h, investor, &service).await;
	assert!(fee_class > Shares::ZERO);
	let claim_before = cash_of(&h, fee_claim()).await;

	let aum = cash_of(&h, LedgerAccountKey::ServiceClaim(service.clone())).await;
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), aum, &poster.to_string(), now_unix())
		.await
		.unwrap();

	let err = fee_app::settle_fee_shares(&h.settlements, h.ledger.as_ref(), &h.nav, &h.reds, &h.notify, service.clone(), None, poster, now_unix())
		.await
		.expect_err("the poster must not crystallize their own mark");
	assert!(
		matches!(err, DomainError::Precondition(ref m) if m.contains("posted a valuation")),
		"refused by the cooldown: {err:?}"
	);
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, fee_class, "the fee class is untouched");
	assert_eq!(cash_of(&h, fee_claim()).await, claim_before, "and no cash moved");
	assert!(h.settlements.list_by_service(&service).await.unwrap().is_empty(), "no settlement was recorded");

	let settlement = fee_app::settle_fee_shares(&h.settlements, h.ledger.as_ref(), &h.nav, &h.reds, &h.notify, service.clone(), None, colleague, now_unix())
		.await
		.expect("an administrator who did not mark the product settles it");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(settlement.units(), fee_class);
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, Shares::ZERO);
	assert_eq!(cash_of(&h, fee_claim()).await, claim_before.checked_add(settlement.cash()).unwrap());
}
