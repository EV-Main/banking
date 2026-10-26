//! Boundary tests for the fee charge, through the crate's public API: the edge #255 was found
//! at — what a charge does when the position it is owed on and the holding it can be taken
//! from are not the same number.

use domain::{
	fees::{CrystallizationPeriod, FeePolicy, ManagementBasis, PositionSnapshot, Trigger, assess},
	money::{Nav, Shares, Usdt},
};

const YEAR: i64 = 365 * 24 * 60 * 60;

fn position(collectable: &str) -> PositionSnapshot {
	PositionSnapshot {
		units: Shares::parse_decimal("1000").unwrap(),
		collectable: Shares::parse_decimal(collectable).unwrap(),
		cost_basis: Usdt::parse_decimal("1000").unwrap(),
		high_water_mark: Nav::parse_decimal("1").unwrap(),
		debt: Usdt::ZERO,
		accrued_at_unix: 0,
		crystallized_at_unix: 0,
	}
}

#[test]
fn a_position_with_nothing_collectable_is_charged_in_full_and_owes_it_all_as_debt() {
	// Every unit in a resting sell: the year's 2% is owed on the whole position, none of
	// it can be taken now, and the charge is NOT empty — it must be recorded, or the
	// holder defers the fee for as long as the ask rests.
	let charge = assess(&FeePolicy::HOUSE, &position("0"), Nav::parse_decimal("1").unwrap(), Trigger::Period, YEAR).unwrap();
	assert_eq!(charge.due, Usdt::parse_decimal("20").unwrap());
	assert_eq!(charge.charged_units, Shares::ZERO);
	assert_eq!(charge.charged_cash, Usdt::ZERO);
	assert_eq!(charge.debt_carried, charge.due, "the whole charge is carried, none written off");
	assert!(!charge.is_empty(), "owed but uncollectable is still a charge");
}

#[test]
fn a_charge_is_empty_exactly_when_nothing_is_owed() {
	// A zero policy owes nothing however long the clock ran…
	let none = FeePolicy::new(0, 0, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual).unwrap();
	let charge = assess(&none, &position("1000"), Nav::parse_decimal("2").unwrap(), Trigger::Period, YEAR).unwrap();
	assert!(charge.due.is_zero());
	assert!(charge.is_empty());
	// …and the house policy owes nothing when the clock did not run at all.
	let charge = assess(&FeePolicy::HOUSE, &position("1000"), Nav::parse_decimal("1").unwrap(), Trigger::Period, 0).unwrap();
	assert!(charge.due.is_zero());
	assert!(charge.is_empty());
}

#[test]
fn a_partly_collectable_position_gives_up_what_it_can_and_carries_the_rest() {
	// 995 of 1000 units escrowed: 5 are free against a 20-unit charge at NAV 1.0.
	let charge = assess(&FeePolicy::HOUSE, &position("5"), Nav::parse_decimal("1").unwrap(), Trigger::Period, YEAR).unwrap();
	assert_eq!(charge.due, Usdt::parse_decimal("20").unwrap());
	assert_eq!(charge.charged_units, Shares::parse_decimal("5").unwrap(), "capped by the free holding, not by the position");
	assert_eq!(charge.charged_cash, Usdt::parse_decimal("5").unwrap());
	assert_eq!(charge.debt_carried, Usdt::parse_decimal("15").unwrap());
	assert!(!charge.is_empty());
}

#[test]
fn the_market_value_basis_is_measured_on_the_whole_position_not_the_free_part() {
	// 1000 units at NAV 2.0 is 2000 of market value, every unit of it in a resting sell.
	// The 2% is owed on the 2000 — an order moves units into escrow, it does not shrink
	// what they are worth — and none of it can be taken now.
	let market = FeePolicy::new(200, 0, 0, ManagementBasis::MarketValue, CrystallizationPeriod::Annual).unwrap();
	let charge = assess(&market, &position("0"), Nav::parse_decimal("2").unwrap(), Trigger::Period, YEAR).unwrap();
	assert_eq!(charge.management, Usdt::parse_decimal("40").unwrap(), "2% of 1000 units × NAV 2.0, escrow included");
	assert_eq!(charge.charged_units, Shares::ZERO);
	assert_eq!(charge.debt_carried, Usdt::parse_decimal("40").unwrap());
}

#[test]
fn the_performance_leg_is_measured_on_the_whole_position_not_the_free_part() {
	// Mark 1.0 → NAV 2.0 on 1000 units, none of them collectable. Management: 2% of the
	// 1000 invested = 20, or 10 units at 2.0, leaving 990 in scope. Performance: 20% of the
	// 1.0 gain on those 990 = 198. Owed in full, collected not at all.
	let charge = assess(&FeePolicy::HOUSE, &position("0"), Nav::parse_decimal("2").unwrap(), Trigger::Period, YEAR).unwrap();
	assert_eq!(charge.management, Usdt::parse_decimal("20").unwrap());
	assert_eq!(
		charge.performance,
		Usdt::parse_decimal("198").unwrap(),
		"the gain is on every unit the holder owns, escrowed or not"
	);
	assert_eq!(charge.due, Usdt::parse_decimal("218").unwrap());
	assert_eq!(charge.charged_units, Shares::ZERO);
	assert_eq!(charge.debt_carried, Usdt::parse_decimal("218").unwrap());
	assert_eq!(charge.high_water_mark, Nav::parse_decimal("2").unwrap(), "the mark ratchets even though nothing was collected");
}

#[test]
fn a_year_deferred_into_debt_and_collected_a_day_later_is_billed_once() {
	const DAY: i64 = 24 * 60 * 60;
	// Pass one: the year is owed, nothing is collectable, all of it becomes debt.
	let deferred = assess(&FeePolicy::HOUSE, &position("0"), Nav::parse_decimal("1").unwrap(), Trigger::Period, YEAR).unwrap();
	assert_eq!(deferred.debt_carried, Usdt::parse_decimal("20").unwrap());

	// Pass two, a day later: the units are home, the clocks were moved by pass one, and
	// the debt rides in. What accrues on top is one DAY of management — not the year again.
	let mut returned = position("1000");
	returned.debt = deferred.debt_carried;
	returned.high_water_mark = deferred.high_water_mark;
	returned.accrued_at_unix = YEAR;
	returned.crystallized_at_unix = YEAR;
	let collected = assess(&FeePolicy::HOUSE, &returned, Nav::parse_decimal("1").unwrap(), Trigger::Period, YEAR + DAY).unwrap();
	assert_eq!(collected.debt_opening, Usdt::parse_decimal("20").unwrap());
	assert!(
		collected.management > Usdt::parse_decimal("0.05").unwrap() && collected.management < Usdt::parse_decimal("0.06").unwrap(),
		"a day of 2% on 1000 is ~0.0548, got {}",
		collected.management
	);
	assert_eq!(collected.due, collected.debt_opening.checked_add(collected.management).unwrap());
	assert_eq!(
		collected.charged_units,
		Shares::from_cash(collected.due, Nav::parse_decimal("1").unwrap()).unwrap(),
		"the debt and the day are taken together"
	);
	assert!(
		collected.debt_carried < Usdt::parse_decimal("0.000000000000000002").unwrap(),
		"nothing but the sub-unit residue is carried"
	);
	// Across both passes the management billed is one year plus one day — the deferred
	// year was carried as debt, not re-accrued.
	let billed = deferred.management.checked_add(collected.management).unwrap();
	assert!(billed < Usdt::parse_decimal("20.06").unwrap(), "one year and one day, got {billed}");
}
