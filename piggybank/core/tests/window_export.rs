//! Integration tests for the migration-window export — real Postgres **and** TigerBeetle
//! (no mocks). They run when `DATABASE_URL` is set and a TigerBeetle replica is
//! reachable, and skip otherwise.
//!
//! Pinned here: the `service_arb` shape (13 000 / 3 250 units, no cash) comes out of the
//! file exactly, the supply balances against its holders, and the export writes nothing.

use std::sync::Arc;

use domain::{
	allocations::{Allocation, AllocationIcon, AllocationId},
	auth::AuthSubject,
	balance::ServiceId,
	issuance::{IdempotencyKey, UnitHolder},
	money::Shares,
	users::{Email, UserId},
	window_export::{HoldingKind, Record, SCHEMA_VERSION},
};
use piggybank_core::{
	application::{funds as funds_app, issuance as issuance_app},
	infrastructure::{allocations::PgAllocations, custody::StubCustody, issuance::PgUnitIssuances, nav::PgNav, relay::Relay, users::PgUsers, window_export},
	ports::{AllocationRegistry, UserRepository},
};
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

async fn user(users: &PgUsers) -> UserId {
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("u{}@example.com", Uuid::new_v4().simple())).unwrap();
	users.provision(subject, email, true).await.unwrap().id()
}

async fn counts(pool: &sqlx::PgPool) -> (i64, i64) {
	let accounts = sqlx::query_scalar("SELECT count(*) FROM tb_accounts").fetch_one(pool).await.unwrap();
	let outbox = sqlx::query_scalar("SELECT count(*) FROM outbox").fetch_one(pool).await.unwrap();
	(accounts, outbox)
}

#[tokio::test]
async fn the_service_arb_shape_is_exported_exactly_and_nothing_is_written() {
	let _serial = common::outbox_serial().await;
	let Some(pool) = common::pool().await else { return };
	let Some(ledger) = common::seeded_ledger(&pool, "window-export test").await else { return };

	let allocations = PgAllocations::new(pool.clone());
	let users = PgUsers::new(pool.clone());
	let issuances = PgUnitIssuances::new(pool.clone());
	let nav = PgNav::new(pool.clone());
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone());

	let service = ServiceId::parse(&format!("svc-{}", Uuid::new_v4())).unwrap();
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "SA", "export test", AllocationIcon::default()).unwrap();
	allocations.register(&mut allocation).await.unwrap();
	let (investor, owner) = (user(&users).await, user(&users).await);
	let ports = funds_app::FundPorts {
		allocations: &allocations,
		ledger: ledger.as_ref(),
		nav: &nav,
		relay: &notify,
	};
	let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
	for (holder, units, key) in [(investor, "3250", "investor"), (owner, "13000", "owner")] {
		issuance_app::issue_units(
			&ports,
			&issuances,
			&users,
			issuance_app::IssueUnitsRequest {
				service: service.clone(),
				holder: UnitHolder::User(holder),
				units: Shares::parse_decimal(units).unwrap(),
				cost_basis: None,
				idempotency_key: IdempotencyKey::parse(key).unwrap(),
			},
			now,
		)
		.await
		.unwrap();
	}
	common::drain_to_quiescence(&relay, &pool).await;

	let before = counts(&pool).await;
	let mut out = Vec::new();
	// The shared test database is never in maintenance, hence the rehearsal flag.
	let summary = window_export::export(&pool, ledger.as_ref(), "0", true, &mut out).await.unwrap();
	assert_eq!(counts(&pool).await, before, "the export must not create accounts or outbox rows");

	let records: Vec<Record> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
	let Some(Record::Header(header)) = records.first() else {
		panic!("first line must be the header")
	};
	assert_eq!((header.schema_version, header.live), (SCHEMA_VERSION, true));
	let Some(Record::Trailer(trailer)) = records.last() else {
		panic!("last line must be the trailer")
	};
	assert_eq!(trailer, &summary.trailer);
	assert_eq!(trailer.counts.values().sum::<u64>() as usize, records.len(), "the trailer counts every line, itself included");

	let ours = |s: &String| s == service.as_str();
	let mut held: Vec<(String, String)> = records
		.iter()
		.filter_map(|r| match r {
			Record::ShareHolding(h) if ours(&h.service) && h.kind == HoldingKind::UserShares => Some((h.user_id.clone().unwrap(), h.units.clone())),
			_ => None,
		})
		.collect();
	held.sort();
	let mut expected = vec![
		(investor.raw().to_string(), "3250000000000000000000".to_owned()),
		(owner.raw().to_string(), "13000000000000000000000".to_owned()),
	];
	expected.sort();
	assert_eq!(held, expected);
	let total = trailer.allocations.iter().find(|t| ours(&t.service)).expect("a total for the allocation");
	assert_eq!((total.held.as_str(), total.balanced), ("16250000000000000000000", true));
}
