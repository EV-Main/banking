//! In maintenance the fee sweeper charges nothing — real Postgres **and** TigerBeetle (no
//! mocks). The migration window's cut must not move under a background job; skips without
//! `DATABASE_URL` and a replica, fails under CI.

use std::{collections::HashMap, sync::Arc};

use domain::{
	allocations::{Allocation, AllocationAccess, AllocationIcon, AllocationId},
	auth::AuthSubject,
	balance::ServiceId,
	issuance::{IdempotencyKey, UnitHolder},
	money::Shares,
	users::Email,
};
use piggybank_core::{
	application::{funds as funds_app, issuance as issuance_app},
	infrastructure::{
		allocations::PgAllocations,
		custody::StubCustody,
		fee_policy_changes::PgFeePolicyChanges,
		fee_sweeper::FeeSweeper,
		fees::{PgFeeAssessments, PgFeePolicies, PgPositionAccruals},
		issuance::PgUnitIssuances,
		nav::PgNav,
		operations,
		outflow::PgOutflowPolicy,
		relay::Relay,
		users::PgUsers,
	},
	ports::{AllocationRegistry, UserRepository},
};
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

#[tokio::test]
async fn the_sweeper_charges_nothing_while_the_hub_is_in_maintenance() {
	let _serial = common::outbox_serial().await;
	let Some(pool) = common::pool().await else { return };
	let Some(ledger) = common::seeded_ledger(&pool, "window sweeper test").await else { return };
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone());
	let (allocations, users, issuances, nav) = (
		PgAllocations::new(pool.clone()),
		PgUsers::new(pool.clone()),
		PgUnitIssuances::new(pool.clone()),
		PgNav::new(pool.clone()),
	);

	let service = ServiceId::parse(&format!("sweep-{}", Uuid::new_v4())).unwrap();
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "T", "sweeper test", AllocationIcon::default()).unwrap();
	allocations.register(&mut allocation).await.unwrap();
	allocations.open(&service).await.unwrap();
	allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	sqlx::query(
		"INSERT INTO fee_policies (service, management_bps, performance_bps, hurdle_bps, basis, crystallization, updated_by) VALUES ($1, 200, 0, 0, 'invested_capital', 'annual', 'test')",
	)
	.bind(service.as_str())
	.execute(&pool)
	.await
	.unwrap();

	let holder = users
		.provision(
			AuthSubject::parse(&format!("sw-{}", Uuid::new_v4())).unwrap(),
			Email::parse(&format!("s{}@example.com", Uuid::new_v4().simple())).unwrap(),
			true,
		)
		.await
		.unwrap()
		.id();
	let ports = funds_app::FundPorts {
		allocations: &allocations,
		ledger: ledger.as_ref(),
		nav: &nav,
		relay: &notify,
	};
	let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
	issuance_app::issue_units(
		&ports,
		&issuances,
		&users,
		issuance_app::IssueUnitsRequest {
			service: service.clone(),
			holder: UnitHolder::User(holder),
			units: Shares::parse_decimal("1000").unwrap(),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse("sweeper-test").unwrap(),
		},
		now,
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&relay, &pool).await;
	let year = 365.0 * 24.0 * 3600.0;
	sqlx::query("UPDATE fund_positions SET fees_accrued_at = now() - make_interval(secs => $2), crystallized_at = now() - make_interval(secs => $2) WHERE user_id = $1")
		.bind(holder.raw())
		.bind(year)
		.execute(&pool)
		.await
		.unwrap();

	let sweeper = FeeSweeper::new(
		Arc::new(PgFeePolicies::new(pool.clone())),
		Arc::new(PgFeePolicyChanges::new(pool.clone())),
		Arc::new(PgPositionAccruals::new(pool.clone())),
		Arc::new(PgFeeAssessments::new(pool.clone())),
		ledger.clone(),
		Arc::new(PgNav::new(pool.clone())),
		notify.clone(),
		Arc::new(PgOutflowPolicy::new(pool.clone())),
	);
	let charges = || async {
		sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fee_assessments WHERE user_id = $1")
			.bind(holder.raw())
			.fetch_one(&pool)
			.await
			.unwrap()
	};

	operations::set_read_only(&pool, true).await.unwrap();
	assert_eq!(sweeper.sweep(now).await.unwrap(), 0, "maintenance: no charge");
	assert_eq!(sweeper.promote(now, &mut HashMap::new()).await.unwrap(), 0);
	assert_eq!(charges().await, 0, "and nothing was assessed");

	operations::set_read_only(&pool, false).await.unwrap();
	assert!(sweeper.sweep(now).await.unwrap() >= 1, "the same position is due once maintenance is off");
	assert_eq!(charges().await, 1);
}
