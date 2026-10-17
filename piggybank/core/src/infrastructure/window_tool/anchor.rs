//! The test anchor of the window (plan §6.5 step 6): the `service_arb` holders — valeratrades
//! 13 000 and inintel44 3 250 units, Σ 16 250 — are live clients' money and must read the
//! same before and after every step.
//!
//! A reading is per holder and counts a holder's units wherever they are: on the live share
//! accounts (free and resting in a sell) **and** on the accounts the window already closed,
//! whose amounts it takes from the ledger by transfer id, never from the row alone. So the
//! number does not change when `close-share-accounts` moves units off the share ledger, and
//! it would change if any step lost, minted or re-attributed a unit.
//!
//! The expected shape is amounts, not names: banking knows holders by id and the people only
//! through concierge. The two amounts and their sum are what the owner gave; the holders'
//! ids are whatever the first reading finds, and every later reading must equal it.

use std::collections::BTreeMap;

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use domain::balance::{LedgerAccountKey, ServiceId};
use sqlx::PgPool;
use uuid::Uuid;

use super::units;
use crate::ports::ledger::{HoldingScope, Ledger};

/// 13 000 and 3 250 units in 18-dp base units.
const SERVICE_ARB_UNITS: [u128; 2] = [3_250_000_000_000_000_000_000, 13_000_000_000_000_000_000_000];

/// What the anchor must read: one allocation, and the amounts its holders hold.
#[derive(Clone, Debug)]
pub struct Anchor {
	service: ServiceId,
	/// Ascending, so two shapes compare as multisets.
	amounts: Vec<u128>,
}

impl Anchor {
	/// The production anchor: `service_arb`, 13 000 / 3 250 units.
	pub fn service_arb() -> Result<Self> {
		Ok(Self::new(ServiceId::parse("service_arb").map_err(|e| eyre!("{e}"))?, SERVICE_ARB_UNITS.to_vec()))
	}

	pub fn new(service: ServiceId, mut amounts: Vec<u128>) -> Self {
		amounts.sort_unstable();
		Self { service, amounts }
	}

	pub fn service(&self) -> &ServiceId {
		&self.service
	}
}

/// The anchor allocation's holders as the ledger says they are right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reading {
	/// Per holder (the banking `users.id`): live + closed units.
	pub by_holder: BTreeMap<Uuid, u128>,
	/// Units still on the live share accounts.
	pub live: u128,
	/// Units on accounts the window closed.
	pub closed: u128,
}

impl Reading {
	pub fn total(&self) -> u128 {
		self.live + self.closed
	}

	/// One line per holder and the sum, for the report.
	pub fn render(&self, anchor: &Anchor) -> String {
		let mut out = format!("anchor {}: ", anchor.service);
		for (holder, amount) in &self.by_holder {
			out.push_str(&format!("{holder} {}  ", units(*amount)));
		}
		out.push_str(&format!("Σ {} (live {}, closed {})", units(self.total()), units(self.live), units(self.closed)));
		out
	}

	/// The reading has exactly the shape the owner gave: the right amounts, no one else.
	pub fn matches(&self, anchor: &Anchor) -> Result<()> {
		let mut found: Vec<u128> = self.by_holder.values().copied().filter(|v| *v != 0).collect();
		found.sort_unstable();
		if found != anchor.amounts {
			bail!(
				"ANCHOR BROKEN: {} holders read {:?} units, expected {:?} (Σ {})",
				anchor.service,
				found.iter().map(|v| units(*v)).collect::<Vec<_>>(),
				anchor.amounts.iter().map(|v| units(*v)).collect::<Vec<_>>(),
				units(anchor.amounts.iter().sum())
			);
		}
		Ok(())
	}

	/// This reading is the same one `before` was: same holders, same amounts. Where the units
	/// sit (live or closed) may differ — that is what the closing step does.
	pub fn same_holders_as(&self, before: &Self) -> Result<()> {
		if self.by_holder != before.by_holder {
			bail!("ANCHOR CHANGED: holders were {:?}, now {:?}", before.by_holder, self.by_holder);
		}
		Ok(())
	}
}

/// Read the anchor allocation's holders. Read-only.
pub async fn read(pool: &PgPool, ledger: &dyn Ledger, anchor: &Anchor) -> Result<Reading> {
	let mut by_holder: BTreeMap<Uuid, u128> = BTreeMap::new();
	let mut live = 0u128;
	for (key, posted) in ledger
		.share_holdings(&HoldingScope::Product(anchor.service.clone()))
		.await
		.wrap_err("read the anchor allocation's holdings")?
	{
		let user = match &key {
			// Units resting in a sell are still the holder's.
			LedgerAccountKey::UserShares(_, user) | LedgerAccountKey::BookShares(_, user) => user.raw(),
			// The fee class is the `fee` allocation's, not a holder's: not part of the anchor.
			_ => continue,
		};
		if posted == 0 {
			continue;
		}
		*by_holder.entry(user).or_default() += posted;
		live = live.checked_add(posted).ok_or_else(|| eyre!("Σ live units of {} overflow u128", anchor.service))?;
	}

	let mut closed = 0u128;
	let rows: Vec<(Uuid, String, Vec<u8>)> = sqlx::query_as("SELECT user_id, units, transfer_id FROM window_closures WHERE service = $1 AND user_id IS NOT NULL")
		.bind(anchor.service.as_str())
		.fetch_all(pool)
		.await
		.wrap_err("read window_closures")?;
	for (user, recorded, transfer_id) in rows {
		let recorded: u128 = recorded.parse().wrap_err_with(|| format!("window_closures units `{recorded}` is not an integer"))?;
		let id = u128::from_be_bytes(transfer_id.as_slice().try_into().map_err(|_| eyre!("window_closures transfer_id is not 16 bytes"))?);
		// The ledger is authoritative: a row that claims units the transfer did not move is
		// not evidence of anything.
		let on_ledger = ledger.transfer_amount(id).await.wrap_err("read a closing transfer")?;
		if on_ledger != Some(recorded) {
			bail!("window_closures says {user} had {recorded} units closed, the ledger's transfer says {on_ledger:?}");
		}
		*by_holder.entry(user).or_default() += recorded;
		closed = closed.checked_add(recorded).ok_or_else(|| eyre!("Σ closed units of {} overflow u128", anchor.service))?;
	}
	Ok(Reading { by_holder, live, closed })
}

#[cfg(test)]
mod tests {
	use super::*;

	fn reading(holders: &[(u128, u128)]) -> Reading {
		let by_holder: BTreeMap<Uuid, u128> = holders.iter().map(|(who, units)| (Uuid::from_u128(*who), *units)).collect();
		let live = by_holder.values().sum();
		Reading { by_holder, live, closed: 0 }
	}

	// The anchor is a shape: the two amounts, in any order, and nobody else with units.
	#[test]
	fn the_anchor_accepts_the_two_amounts_and_nothing_else() {
		let anchor = Anchor::service_arb().unwrap();
		let [small, big] = SERVICE_ARB_UNITS;
		reading(&[(1, big), (2, small)]).matches(&anchor).unwrap();
		reading(&[(2, small), (1, big)]).matches(&anchor).unwrap();
		assert!(reading(&[(1, big)]).matches(&anchor).is_err(), "a holder missing");
		assert!(reading(&[(1, big), (2, small), (3, 1)]).matches(&anchor).is_err(), "a third holder with a unit");
		assert!(reading(&[(1, big), (2, small - 1)]).matches(&anchor).is_err(), "a base unit lost");
		assert!(reading(&[(1, big), (2, big)]).matches(&anchor).is_err(), "the amounts swapped for others");
		reading(&[(1, big), (2, small), (3, 0)]).matches(&anchor).unwrap();
	}

	#[test]
	fn the_sum_the_owner_gave_is_sixteen_thousand_two_hundred_fifty() {
		assert_eq!(units(SERVICE_ARB_UNITS.iter().sum()), "16250");
	}

	// Where the units sit may change across a step; whose they are may not.
	#[test]
	fn moving_units_from_live_to_closed_keeps_the_holders() {
		let before = reading(&[(1, 10), (2, 5)]);
		let mut after = before.clone();
		after.closed = after.live;
		after.live = 0;
		after.same_holders_as(&before).unwrap();
		let mut stolen = before.clone();
		stolen.by_holder.insert(Uuid::from_u128(1), 9);
		assert!(stolen.same_holders_as(&before).is_err());
	}
}
