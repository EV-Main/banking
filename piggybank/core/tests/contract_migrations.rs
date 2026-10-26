//! The v2.21 contract migrations — `0071_units_contract.sql` and
//! `0072_governance_contract.sql` — over the history v2.20 left behind. Real Postgres and
//! TigerBeetle, no mocks.
//!
//! The rule they enforce: every row in a live table parses with the v2.21 domain types
//! (`tb_accounts` excepted). What only the removed unit and owner-governance paths
//! understand moves to schema `retired`. The code that wrote that history is gone, so it
//! is seeded with raw SQL on a database migrated to 0070; the live rows go in through the
//! hub's own adapters wherever one still exists.
//!
//! Pinned:
//! 1. `retired.*` holds every retired row, and every live read path loads what is left;
//!    every `event_log` and `outbox` row deserializes as the event its kind names.
//! 2. The narrowed CHECKs refuse the retired shapes.
//! 3. The documented rollbacks restore the 0070 database exactly, and re-applying the files
//!    lands on the same contract; what v2.21 wrote in between survives both ways.
//! 4. Each preflight condition — live state of a removed path — refuses the deploy before
//!    anything changes.
//!
//! Each test owns a scratch database, `<binary db>_<suffix>`, recreated on every run: the
//! binary's usual clone is already migrated past 0070.

use std::sync::Arc;

use domain::{
	auth::AuthSubject,
	balance::{LedgerEvent, Party},
	money::{Network, TxRef, Usdt, WalletAddress},
	payments::{PaymentApproval, PaymentDestination, PaymentEvent, PaymentId, PaymentOrder, PaymentReason, PaymentTerms},
	scope_charges::ChargeEvent,
	scope_invoices::InvoiceEvent,
	scope_ledger::{HoldEvent, ScopeLedgerEvent},
	scope_operations::ScopePayoutEvent,
	scopes::CompartmentEvent,
	users::{Email, UserEvent, UserId},
	withdrawals::{WithdrawalEvent, WithdrawalId, WithdrawalSource},
};
use piggybank_core::{
	infrastructure::{
		deposits::PgDeposits, operation_feed::PgOperationFeed, outbox, payments::PgPayments, reconciliation::Reconciliation, scope_notifications::ScopeNotificationWorker, users::PgUsers,
		withdrawals::PgWithdrawals,
	},
	ports::{
		Deposits, OperationFeed, PaymentFeed, PaymentRepository, UserRepository, WithdrawalRepository,
		payments::{ApprovalSeat, PaymentApprovalStore, PaymentFilter, SessionSeat},
		scope_notifications::{NoticeError, ScopeNotice, ScopeNotifier},
	},
};
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

mod common;

const BEFORE: i64 = 70;
const HEAD: i64 = 72;

const UNITS_CONTRACT: &str = include_str!("../migrations/0071_units_contract.sql");
const GOVERNANCE_CONTRACT: &str = include_str!("../migrations/0072_governance_contract.sql");

/// The rollback a migration's `REVERSIBILITY:` header documents, read out of the file, so the
/// round trip runs exactly what an operator would paste.
fn rollback(migration: &str) -> String {
	let sql: Vec<&str> = migration
		.lines()
		.skip_while(|line| !line.starts_with("-- REVERSIBILITY:"))
		.skip_while(|line| *line != "--   BEGIN;")
		.take_while(|line| line.starts_with("--   "))
		.map(|line| &line["--   ".len()..])
		.collect();
	assert_eq!(sql.last(), Some(&"COMMIT;"), "the documented rollback is one transaction");
	sql.join("\n")
}

/// Every table either migration moves or archives out of, by its live name.
const TOUCHED: &[&str] = &[
	"allocation_access_grants",
	"allocations",
	"book_orders",
	"book_policies",
	"book_revisions",
	"book_trades",
	"fee_assessments",
	"fee_policies",
	"fee_policy_changes",
	"fee_settlements",
	"fund_positions",
	"fund_valuations",
	"fund_wallets",
	"redemptions",
	"subscriptions",
	"unit_issuances",
	"window_closures",
	"consilium",
	"consilium_voter",
	"consilium_mail",
	"payment_approval",
	"payment_consent",
	"governance_roster_change",
	"payments",
	"payment_concierge_approval",
	"payment_session_consent",
	"scope_notification_outbox",
	"deposits",
	"event_log",
	"outbox",
];

/// The tables that leave `public` whole.
const WHOLLY_RETIRED: &[&str] = &[
	"allocation_access_grants",
	"allocations",
	"book_orders",
	"book_policies",
	"book_revisions",
	"book_trades",
	"fee_assessments",
	"fee_policies",
	"fee_policy_changes",
	"fee_settlements",
	"fund_positions",
	"fund_valuations",
	"fund_wallets",
	"redemptions",
	"subscriptions",
	"unit_issuances",
	"window_closures",
	"consilium",
	"consilium_voter",
	"consilium_mail",
	"payment_approval",
	"payment_consent",
	"governance_roster_change",
];

const CHECK_VIOLATION: &str = "23514";

fn now() -> i64 {
	i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()).unwrap()
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn bytes32() -> Vec<u8> {
	Uuid::new_v4().as_bytes().repeat(2)
}

/// Replace the database path segment of a `postgres://…/db[?p]` URL.
fn with_database(url: &str, name: &str) -> String {
	let (base, query) = url.split_once('?').map_or((url, None), |(base, query)| (base, Some(query)));
	let authority_start = base.find("://").map_or(0, |i| i + 3);
	let rebuilt = match base[authority_start..].find('/') {
		Some(slash) => format!("{}/{name}", &base[..authority_start + slash]),
		None => format!("{base}/{name}"),
	};
	query.map_or(rebuilt.clone(), |query| format!("{rebuilt}?{query}"))
}

fn database_of(url: &str) -> String {
	let base = url.split_once('?').map_or(url, |(base, _)| base);
	base.rsplit('/').next().expect("a database URL names a database").to_owned()
}

/// A fresh database migrated to [`BEFORE`] and no further — v2.20's schema — or `None` where
/// the suite skips (no `DATABASE_URL` locally).
async fn database_at_v2_20(suffix: &str) -> Option<PgPool> {
	let clone_url = common::database_url().await?;
	let scratch = format!("{}_{suffix}", database_of(&clone_url));
	assert!(scratch.len() <= 63, "{scratch} does not fit a Postgres identifier");
	let mut admin = PgConnection::connect(&clone_url).await.expect("connect to the binary's database");
	sqlx::query(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {scratch} WITH (FORCE)")))
		.execute(&mut admin)
		.await
		.expect("drop the previous run's scratch database");
	sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {scratch}")))
		.execute(&mut admin)
		.await
		.expect("create the scratch database");
	admin.close().await.expect("close the admin connection");
	let pool = PgPoolOptions::new()
		.max_connections(8)
		.connect(&with_database(&clone_url, &scratch))
		.await
		.expect("connect to the scratch database");
	sqlx::migrate!().run_to(BEFORE, &pool).await.expect("migrate the scratch database to 0070");
	Some(pool)
}

async fn exec(pool: &PgPool, sql: &str) {
	sqlx::raw_sql(AssertSqlSafe(sql.to_owned()))
		.execute(pool)
		.await
		.unwrap_or_else(|err| panic!("seed statement failed: {err}\n{sql}"));
}

async fn applied_version(pool: &PgPool) -> i64 {
	sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
		.fetch_one(pool)
		.await
		.expect("read the applied version")
}

async fn person(pool: &PgPool, name: &str) -> UserId {
	let users = PgUsers::new(pool.clone());
	let tag = Uuid::new_v4();
	let subject = AuthSubject::parse(&format!("contract-{name}-{tag}")).unwrap();
	let email = Email::parse(&format!("{name}-{}@example.test", tag.simple())).unwrap();
	let id = users.provision(subject, email, true).await.expect("provision a person").id();
	// A consent notice is addressed by the mirrored concierge id; `open` refuses without one.
	sqlx::query("UPDATE users SET concierge_user_id = $2 WHERE id = $1")
		.bind(id.raw())
		.bind(Uuid::new_v4())
		.execute(pool)
		.await
		.expect("mirror the concierge id");
	id
}

/// Where an event row stands in the outbox, if it is in it at all.
#[derive(Clone, Copy)]
enum Relayed {
	No,
	Dispatched,
}

/// One event as the hub writes it: always into `event_log`, into `outbox` too when relayed.
async fn event(pool: &PgPool, aggregate: &str, aggregate_id: Uuid, kind: &str, payload: &Value, relayed: Relayed) -> Uuid {
	let event_id = Uuid::new_v4();
	sqlx::query("INSERT INTO event_log (event_id, aggregate, aggregate_id, kind, payload) VALUES ($1, $2, $3, $4, $5)")
		.bind(event_id)
		.bind(aggregate)
		.bind(aggregate_id)
		.bind(kind)
		.bind(payload)
		.execute(pool)
		.await
		.expect("insert an event_log row");
	if let Relayed::Dispatched = relayed {
		sqlx::query("INSERT INTO outbox (event_id, aggregate, aggregate_id, kind, payload, dispatched_at) VALUES ($1, $2, $3, $4, $5, now())")
			.bind(event_id)
			.bind(aggregate)
			.bind(aggregate_id)
			.bind(kind)
			.bind(payload)
			.execute(pool)
			.await
			.expect("insert an outbox row");
	}
	event_id
}

fn to_value<T: serde::Serialize>(event: &T) -> Value {
	serde_json::to_value(event).expect("an event serializes")
}

/// A payment event as v2.21 writes it, between two people; `patch` turns it into the shape
/// a removed path wrote.
fn payment_event(event: PaymentEvent, patch: impl FnOnce(&mut Value)) -> Value {
	let mut value = to_value(&event);
	patch(&mut value);
	value
}

fn terms(from: Party, to: Party) -> PaymentTerms {
	PaymentTerms::new(from, PaymentDestination::Internal(to), usdt("10"), PaymentReason::new("a documented reason").unwrap()).unwrap()
}

fn service(id: &str) -> Value {
	json!({ "kind": "service", "id": id })
}

/// What v2.20 left behind, by id.
struct History {
	investor: UserId,
	other: UserId,
	/// Decided by a consilium: `service:fund` → investor.
	consilium_payment: Uuid,
	consilium: Uuid,
	/// Decided by an emailed consent: investor → other.
	emailed_payment: Uuid,
	/// Into an allocation, on a session seat: investor → `service:service_arb`.
	service_payment: Uuid,
	/// Out of an allocation, on a concierge seat that reported its outcome: `service:fund` → other.
	concierge_service_payment: Uuid,
	/// Stays: the treasury's, on its concierge seat.
	treasury_payment: Uuid,
	/// Stays: investor → other, on a session seat.
	session_payment: Uuid,
	seed_deposit: String,
	live_deposit: TxRef,
	withdrawal: WithdrawalId,
	/// event_ids of the rows that must end up in `retired.event_log`.
	retired_events: Vec<Uuid>,
}

async fn seed_history(pool: &PgPool) -> History {
	let admin = person(pool, "admin").await;
	let investor = person(pool, "investor").await;
	let other = person(pool, "other").await;
	let third = person(pool, "third").await;
	let mut retired_events = Vec::new();

	// The consilium path: a closed `payment` consilium that moved `service:fund` money.
	let consilium_payment = Uuid::new_v4();
	let consilium = Uuid::new_v4();
	sqlx::query(
		"INSERT INTO payments (id, state, from_kind, from_id, to_kind, to_id, amount, reason, payload_hash, initiator_user_id, expires_at, decided_at) \
		 VALUES ($1, 'executed', 'service', 'fund', 'user', $2, '10000000000000000000', 'fund to an investor', $3, $4, now() + interval '7 days', now())",
	)
	.bind(consilium_payment)
	.bind(investor.to_string())
	.bind(bytes32())
	.bind(admin.raw())
	.execute(pool)
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO consilium (id, kind, state, terms, payload_hash, initiator_user_id, owner_count, threshold, expires_at, decided_at, source_claim, executed_payment_id) \
		 VALUES ($1, 'payment', 'executed', '{}', $2, $3, 3, 2, now() + interval '7 days', now(), 'fund', $4)",
	)
	.bind(consilium)
	.bind(bytes32())
	.bind(admin.raw())
	.bind(consilium_payment)
	.execute(pool)
	.await
	.unwrap();
	sqlx::query("INSERT INTO payment_approval (payment_id, consilium_id) VALUES ($1, $2)")
		.bind(consilium_payment)
		.bind(consilium)
		.execute(pool)
		.await
		.unwrap();
	for voter in [investor, other, third] {
		sqlx::query(
			"INSERT INTO consilium_voter (consilium_id, user_id, initiator_user_id, decision, decided_at, used_at, token_hash, code_hash, expires_at) \
			 VALUES ($1, $2, $3, 'approve', now(), now(), $4, $5, now() + interval '7 days')",
		)
		.bind(consilium)
		.bind(voter.raw())
		.bind(admin.raw())
		.bind(bytes32())
		.bind(bytes32())
		.execute(pool)
		.await
		.unwrap();
	}
	for (voter, kind) in [(investor, "payment_approval"), (other, "payout_outcome")] {
		sqlx::query("INSERT INTO consilium_mail (consilium_id, user_id, kind, dedupe_key, payload, sent_at) VALUES ($1, $2, $3, $4, '{}', now())")
			.bind(consilium)
			.bind(voter.raw())
			.bind(kind)
			.bind(format!("{consilium}:{voter}"))
			.execute(pool)
			.await
			.unwrap();
	}
	let id = PaymentId::from_raw(consilium_payment);
	let opened = payment_event(
		PaymentEvent::Opened {
			payment_id: id,
			terms: terms(Party::Treasury, Party::User(investor)),
			payload_hash: "0".repeat(64),
			initiator: admin,
			requirement: PaymentApproval::OwnersApproval,
			tier: domain::payments::PaymentTier::Internal,
			expires_at: now(),
		},
		|v| {
			v["terms"]["from"] = service("fund");
			v["requirement"] = json!({ "requirement": "owner_consilium" });
		},
	);
	retired_events.push(event(pool, "payment", consilium_payment, "payments", &opened, Relayed::No).await);
	let settled = payment_event(
		PaymentEvent::Settled {
			payment_id: id,
			from: Party::Treasury,
			to: Party::User(investor),
			amount: usdt("10"),
			at: now(),
		},
		|v| v["from"] = service("fund"),
	);
	retired_events.push(event(pool, "payment", consilium_payment, "payments", &settled, Relayed::Dispatched).await);
	retired_events.push(event(pool, "consilium", consilium, "consilium", &json!({ "type": "executed" }), Relayed::Dispatched).await);

	// The emailed-consent path: a refused order, its seat and its outcome mail.
	let emailed_payment = Uuid::new_v4();
	sqlx::query(
		"INSERT INTO payments (id, state, from_kind, from_id, to_kind, to_id, amount, reason, payload_hash, initiator_user_id, expires_at, decided_at) \
		 VALUES ($1, 'rejected', 'user', $2, 'user', $3, '10000000000000000000', 'to a friend', $4, $5, now() + interval '7 days', now())",
	)
	.bind(emailed_payment)
	.bind(investor.to_string())
	.bind(other.to_string())
	.bind(bytes32())
	.bind(investor.raw())
	.execute(pool)
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO payment_consent (payment_id, subject_user_id, decision, decided_at, used_at, token_hash, code_hash, expires_at, subject_token_version_at_open, subject_email_hash_at_open) \
		 VALUES ($1, $2, 'reject', now(), now(), $3, $4, now() + interval '7 days', 0, $5)",
	)
	.bind(emailed_payment)
	.bind(investor.raw())
	.bind(bytes32())
	.bind(bytes32())
	.bind(bytes32())
	.execute(pool)
	.await
	.unwrap();
	sqlx::query("INSERT INTO consilium_mail (payment_id, user_id, kind, dedupe_key, payload, sent_at) VALUES ($1, $2, 'payment_outcome', $3, '{}', now())")
		.bind(emailed_payment)
		.bind(investor.raw())
		.bind(format!("{emailed_payment}:outcome"))
		.execute(pool)
		.await
		.unwrap();
	let id = PaymentId::from_raw(emailed_payment);
	let opened = to_value(&PaymentEvent::Opened {
		payment_id: id,
		terms: terms(Party::User(investor), Party::User(other)),
		payload_hash: "0".repeat(64),
		initiator: investor,
		requirement: PaymentApproval::SubjectConsent(investor),
		tier: domain::payments::PaymentTier::Internal,
		expires_at: now(),
	});
	retired_events.push(event(pool, "payment", emailed_payment, "payments", &opened, Relayed::No).await);
	let rejected = to_value(&PaymentEvent::Rejected { payment_id: id, at: now() });
	retired_events.push(event(pool, "payment", emailed_payment, "payments", &rejected, Relayed::No).await);

	// A payment into an allocation, decided on a live seat.
	let service_payment = Uuid::new_v4();
	sqlx::query(
		"INSERT INTO payments (id, state, from_kind, from_id, to_kind, to_id, amount, reason, payload_hash, initiator_user_id, expires_at, decided_at) \
		 VALUES ($1, 'executed', 'user', $2, 'service', 'service_arb', '10000000000000000000', 'into a product', $3, $2::uuid, now() + interval '7 days', now())",
	)
	.bind(service_payment)
	.bind(investor.to_string())
	.bind(bytes32())
	.execute(pool)
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO payment_session_consent (payment_id, subject_user_id, decision, decided_at, subject_token_version_at_open, subject_email_hash_at_open) \
		 VALUES ($1, $2, 'approve', now(), 0, $3)",
	)
	.bind(service_payment)
	.bind(investor.raw())
	.bind(bytes32())
	.execute(pool)
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO scope_notification_outbox (user_id, topic, kind, title, body, link, dedupe_key, payment_id, sent_at) \
		 VALUES ($1, 'bank:money-movement', 'payment_consent', 'Confirm a payment', '', '/wallet/payment', $2, $3, now())",
	)
	.bind(investor.raw())
	.bind(format!("{service_payment}:consent"))
	.bind(service_payment)
	.execute(pool)
	.await
	.unwrap();
	let id = PaymentId::from_raw(service_payment);
	let opened = payment_event(
		PaymentEvent::Opened {
			payment_id: id,
			terms: terms(Party::User(investor), Party::User(other)),
			payload_hash: "0".repeat(64),
			initiator: investor,
			requirement: PaymentApproval::SubjectConsent(investor),
			tier: domain::payments::PaymentTier::Internal,
			expires_at: now(),
		},
		|v| {
			v["terms"]["to"]["target"] = service("service_arb");
			v["tier"] = json!("service");
		},
	);
	retired_events.push(event(pool, "payment", service_payment, "payments", &opened, Relayed::No).await);
	let reserved = to_value(&PaymentEvent::Reserved {
		payment_id: id,
		from: Party::User(investor),
		amount: usdt("10"),
		at: now(),
	});
	retired_events.push(event(pool, "payment", service_payment, "payments", &reserved, Relayed::Dispatched).await);
	let settled = payment_event(
		PaymentEvent::Settled {
			payment_id: id,
			from: Party::User(investor),
			to: Party::User(other),
			amount: usdt("10"),
			at: now(),
		},
		|v| v["to"] = service("service_arb"),
	);
	retired_events.push(event(pool, "payment", service_payment, "payments", &settled, Relayed::Dispatched).await);

	// Out of an allocation, decided on concierge's seat and reported back.
	let concierge_service_payment = Uuid::new_v4();
	exec(
		pool,
		&format!(
			"INSERT INTO payments (id, state, from_kind, from_id, to_kind, to_id, amount, reason, payload_hash, initiator_user_id, expires_at, decided_at) \
			 VALUES ('{concierge_service_payment}', 'executed', 'service', 'fund', 'user', '{other}', '10000000000000000000', 'a payout', '\\x{}', '{admin}', now() + interval '7 days', now());
			 INSERT INTO payment_concierge_approval (payment_id, destination, payload_hash, approval_id, reported_at) \
			 VALUES ('{concierge_service_payment}', 'user:{other}', '{}', 'approval-1', now());",
			"00".repeat(32),
			"ab".repeat(32),
		),
	)
	.await;

	// The two orders that stay, written by the live adapter. v2.20 named the treasury's
	// requirement `owner_consilium`; the migration renames it.
	let payments = PgPayments::new(pool.clone());
	let order = |from: Party, initiator: UserId| {
		let terms = terms(from, Party::User(other));
		PaymentOrder::open(PaymentId::new(), terms, [7u8; 32], initiator, now())
	};
	let mut treasury_order = order(Party::Treasury, admin);
	payments.open(&mut treasury_order, ApprovalSeat::Concierge).await.expect("the treasury's order opens");
	payments.cancel(treasury_order.id(), admin, now()).await.expect("and is withdrawn");
	exec(
		pool,
		&format!(
			"UPDATE event_log SET payload = jsonb_set(payload, '{{requirement,requirement}}', '\"owner_consilium\"') \
			 WHERE aggregate_id = '{}' AND payload ? 'requirement'",
			treasury_order.id().raw()
		),
	)
	.await;
	let (token_version, email): (i64, String) = sqlx::query_as("SELECT GREATEST(concierge_token_version, token_version), email FROM users WHERE id = $1")
		.bind(investor.raw())
		.fetch_one(pool)
		.await
		.unwrap();
	let mut session_order = order(Party::User(investor), investor);
	let seat = ApprovalSeat::Session(SessionSeat {
		subject: investor,
		token_version_at_open: u64::try_from(token_version).unwrap(),
		email_hash_at_open: {
			use sha2::{Digest, Sha256};
			Sha256::digest(email.as_bytes()).into()
		},
	});
	payments.open(&mut session_order, seat).await.expect("the investor's order opens");
	payments.cancel(session_order.id(), investor, now()).await.expect("and is withdrawn");

	// The unit tables, one row each where a row has anything to say.
	let allocation = Uuid::new_v4();
	let buy = Uuid::new_v4();
	let sell = Uuid::new_v4();
	exec(
		pool,
		&format!(
			"INSERT INTO allocations (id, service, title, state) VALUES ('{allocation}', 'service_arb', 'Service Arb', 'open');
			 INSERT INTO allocation_access_grants (service, user_id, level, granted_by) VALUES ('service_arb', '{investor}', 'invest', '{admin}');
			 INSERT INTO subscriptions (id, user_id, service, cash, nav, units) VALUES ('{}', '{investor}', 'service_arb', '1000', '1', '1000');
			 INSERT INTO redemptions (id, user_id, service, units, nav, cash, state) VALUES ('{}', '{investor}', 'service_arb', '10', '1', '10', 'completed');
			 INSERT INTO fee_assessments (id, user_id, service, trigger_kind, nav, management, performance, debt_opening, charged_units, charged_cash, debt_carried, high_water_mark)
			   VALUES ('{}', '{investor}', 'service_arb', 'period', '1', '0', '0', '0', '0', '0', '0', '1');
			 INSERT INTO fund_positions (user_id, service) VALUES ('{investor}', 'service_arb');
			 INSERT INTO fund_valuations (id, service, aum, units_outstanding, nav, posted_by) VALUES ('{}', 'service_arb', '1000', '1000', '1', 'operator');
			 INSERT INTO unit_issuances (id, service, holder_kind, holder_id, units, nav, cost_basis, idempotency_key, state, applied_at)
			   VALUES ('{}', 'service_arb', 'user', '{investor}', '5', '1', '5', 'seed', 'applied', now());
			 INSERT INTO book_orders (id, service, user_id, client_order_id, side, kind, tif, price, size, filled, notional_filled, reserved, state, revision)
			   VALUES ('{buy}', 'service_arb', '{investor}', 'b1', 'buy', 'limit', 'gtc', '1', '1', '1', '1', '1', 'filled', 1),
			          ('{sell}', 'service_arb', '{other}', 's1', 'sell', 'limit', 'gtc', '1', '1', '1', '1', '1', 'filled', 1);
			 INSERT INTO book_trades (id, service, buyer_id, seller_id, buy_order_id, sell_order_id, taker_side, price, size, notional, fee)
			   VALUES ('{}', 'service_arb', '{investor}', '{other}', '{buy}', '{sell}', 'buy', '1', '1', '1', '0');
			 INSERT INTO window_closures (logical_key, service, kind, user_id, code, units, transfer_id)
			   VALUES ('shares:service_arb:{investor}', 'service_arb', 'user_shares', '{investor}', 60, '1', '\\x{}');
			 INSERT INTO governance_roster_change (user_id, from_role, to_role) VALUES ('{other}', 'investor', 'owner');
			 INSERT INTO fund_wallets (network, token, address) VALUES ('bep20', 'usdt', '0xfund');",
			Uuid::new_v4(),
			Uuid::new_v4(),
			Uuid::new_v4(),
			Uuid::new_v4(),
			Uuid::new_v4(),
			Uuid::new_v4(),
			Uuid::new_v4().simple(),
		),
	)
	.await;
	for kind in ["allocations", "book", "fees", "issuances", "redemptions", "subscriptions"] {
		retired_events.push(event(pool, kind, Uuid::new_v4(), kind, &json!({ "type": "seeded" }), Relayed::Dispatched).await);
	}

	// `balance`: the seed's history onto the retired fund claim, and an ordinary deposit.
	let network = Network::Bep20;
	retired_events.push(
		event(
			pool,
			"deposit",
			Uuid::new_v4(),
			"balance",
			&json!({ "type": "capital_seeded", "network": "bep20", "amount": "1000" }),
			Relayed::Dispatched,
		)
		.await,
	);
	let seed_deposit = format!("0xseed{}", Uuid::new_v4().simple());
	let mut seeded = to_value(&LedgerEvent::Deposited {
		party: Party::User(investor),
		network,
		amount: usdt("1000"),
	});
	seeded["party"] = json!({ "kind": "piggybank" });
	let seed_event = event(pool, "deposit", Uuid::new_v4(), "balance", &seeded, Relayed::Dispatched).await;
	retired_events.push(seed_event);
	// The rows v1.0 wrote before 0047: its CHECK is NOT VALID, so they stand beside it.
	exec(
		pool,
		&format!(
			"ALTER TABLE deposits DROP CONSTRAINT deposits_party_is_a_user;
			 INSERT INTO deposits (tx_ref, party_kind, party_id, network, amount, event_id) VALUES ('{seed_deposit}', 'piggybank', NULL, 'bep20', '1000', '{seed_event}');
			 ALTER TABLE deposits ADD CONSTRAINT deposits_party_is_a_user CHECK (party_kind = 'user') NOT VALID;"
		),
	)
	.await;
	let live_deposit = TxRef::parse(&format!("0xlive{}", Uuid::new_v4().simple())).unwrap();
	assert!(
		PgDeposits::new(pool.clone()).record(live_deposit.clone(), investor, network, usdt("250")).await.unwrap(),
		"the deposit is new"
	);
	exec(pool, "UPDATE outbox SET dispatched_at = now() WHERE dispatched_at IS NULL").await;

	// A completed withdrawal and its events — two settles in shapes v2.21 cannot read truly.
	let withdrawal = WithdrawalId::new();
	let address = WalletAddress::parse(network, "0x52908400098527886E0F7030069857D2E4169EE7").unwrap();
	let tx_ref = TxRef::parse(&format!("0xout{}", Uuid::new_v4().simple())).unwrap();
	sqlx::query(
		"INSERT INTO withdrawals (id, source, user_id, network, address, amount, fee, state, tx_ref) \
		 VALUES ($1, 'user', $2, 'bep20', $3, '5000000000000000000', '1000000000000000000', 'completed', $4)",
	)
	.bind(withdrawal.raw())
	.bind(investor.raw())
	.bind(address.as_str())
	.bind(tx_ref.as_str())
	.execute(pool)
	.await
	.unwrap();
	let source = WithdrawalSource::User(investor);
	let (amount, fee) = (usdt("5"), usdt("1"));
	let requested = to_value(&WithdrawalEvent::Requested {
		withdrawal_id: withdrawal,
		source,
		network,
		address: address.clone(),
		amount,
		fee,
	});
	event(pool, "withdrawal", withdrawal.raw(), "withdrawals", &requested, Relayed::Dispatched).await;
	let dispatched = to_value(&WithdrawalEvent::Dispatched {
		withdrawal_id: withdrawal,
		source,
		network,
		address,
		amount,
		fee,
	});
	event(pool, "withdrawal", withdrawal.raw(), "withdrawals", &dispatched, Relayed::Dispatched).await;
	let settled = to_value(&WithdrawalEvent::Settled {
		withdrawal_id: withdrawal,
		source,
		network,
		amount,
		fee,
		tx_ref,
	});
	let with_payee = |payee: Value| {
		let mut event = settled.clone();
		event["payee"] = payee;
		event
	};
	let treasury = json!({ "kind": "scope_account", "id": { "scope": "main", "label": "treasury" } });
	event(pool, "withdrawal", withdrawal.raw(), "withdrawals", &with_payee(treasury), Relayed::Dispatched).await;
	retired_events.push(event(pool, "withdrawal", withdrawal.raw(), "withdrawals", &with_payee(service("fee")), Relayed::Dispatched).await);
	retired_events.push(event(pool, "withdrawal", withdrawal.raw(), "withdrawals", &settled, Relayed::Dispatched).await);

	History {
		investor,
		other,
		consilium_payment,
		consilium,
		emailed_payment,
		service_payment,
		concierge_service_payment,
		treasury_payment: treasury_order.id().raw(),
		session_payment: session_order.id().raw(),
		seed_deposit,
		live_deposit,
		withdrawal,
		retired_events,
	}
}

/// Everything the round trip must restore, in one comparable value: where each touched
/// table lives and an md5 over its rows, every constraint on them, and the columns renamed.
#[derive(Debug, PartialEq)]
struct Picture {
	retired_schema: bool,
	tables: Vec<(String, String, i64, String)>,
	constraints: Vec<(String, String, String, bool)>,
	columns: Vec<(String, String)>,
}

async fn picture(pool: &PgPool) -> Picture {
	let retired_schema: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'retired')")
		.fetch_one(pool)
		.await
		.unwrap();
	let placed: Vec<(String, String)> =
		sqlx::query_as("SELECT schemaname::text, tablename::text FROM pg_tables WHERE schemaname IN ('public', 'retired') AND tablename = ANY($1) ORDER BY 1, 2")
			.bind(TOUCHED)
			.fetch_all(pool)
			.await
			.unwrap();
	let mut tables = Vec::new();
	for (schema, table) in placed {
		let (count, digest): (i64, String) = sqlx::query_as(AssertSqlSafe(format!(
			"SELECT count(*), md5(coalesce(string_agg(t::text, '|' ORDER BY t::text), '')) FROM {schema}.{table} t"
		)))
		.fetch_one(pool)
		.await
		.unwrap();
		tables.push((schema, table, count, digest));
	}
	let constraints = sqlx::query_as(
		"SELECT c.conrelid::regclass::text, c.conname::text, pg_get_constraintdef(c.oid), c.convalidated FROM pg_constraint c \
		 JOIN pg_class r ON r.oid = c.conrelid JOIN pg_namespace n ON n.oid = r.relnamespace \
		 WHERE n.nspname IN ('public', 'retired') AND r.relname = ANY($1) ORDER BY 1, 2",
	)
	.bind(TOUCHED)
	.fetch_all(pool)
	.await
	.unwrap();
	let columns = sqlx::query_as(
		"SELECT table_name::text, column_name::text FROM information_schema.columns \
		 WHERE table_schema = 'public' AND table_name IN ('payments', 'payment_concierge_approval') ORDER BY 1, 2",
	)
	.fetch_all(pool)
	.await
	.unwrap();
	Picture {
		retired_schema,
		tables,
		constraints,
		columns,
	}
}

/// Every row of a live event table, read as the event its kind names. An unknown kind is a
/// failure: the live kinds are exactly what the relay plans and the hub writes.
async fn every_event_parses(pool: &PgPool, table: &str) -> usize {
	let rows: Vec<(i64, String, String)> = sqlx::query_as(AssertSqlSafe(format!("SELECT seq, kind, payload::text FROM {table} ORDER BY seq")))
		.fetch_all(pool)
		.await
		.unwrap();
	for (seq, kind, payload) in &rows {
		let parsed = match kind.as_str() {
			"users" => serde_json::from_str::<UserEvent>(payload).map(drop),
			"balance" => serde_json::from_str::<LedgerEvent>(payload).map(drop),
			"withdrawals" => serde_json::from_str::<WithdrawalEvent>(payload).map(drop),
			"payments" => serde_json::from_str::<PaymentEvent>(payload).map(drop),
			"compartments" => serde_json::from_str::<CompartmentEvent>(payload).map(drop),
			"scope_ledger" => serde_json::from_str::<ScopeLedgerEvent>(payload).map(drop),
			"scope_holds" => serde_json::from_str::<HoldEvent>(payload).map(drop),
			"scope_payouts" => serde_json::from_str::<ScopePayoutEvent>(payload).map(drop),
			"scope_invoices" => serde_json::from_str::<InvoiceEvent>(payload).map(drop),
			"scope_charges" => serde_json::from_str::<ChargeEvent>(payload).map(drop),
			other => panic!("{table} seq {seq}: kind `{other}` is no live kind: {payload}"),
		};
		parsed.unwrap_or_else(|err| panic!("{table} seq {seq} ({kind}) does not parse: {err}: {payload}"));
	}
	rows.len()
}

async fn ids_in(pool: &PgPool, sql: &str) -> Vec<Uuid> {
	sqlx::query_scalar(AssertSqlSafe(sql.to_owned()))
		.fetch_all(pool)
		.await
		.unwrap_or_else(|err| panic!("{sql}: {err}"))
}

fn sorted(mut ids: Vec<Uuid>) -> Vec<Uuid> {
	ids.sort();
	ids
}

async fn sqlstate_of(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> Option<String> {
	match result {
		Ok(_) => None,
		Err(sqlx::Error::Database(err)) => err.code().map(|code| code.into_owned()),
		Err(other) => panic!("not a database refusal: {other}"),
	}
}

#[tokio::test]
async fn the_contract_archives_v2_20_history_and_every_live_row_parses() {
	let _serial = common::outbox_serial().await;
	let Some(pool) = database_at_v2_20("c71a").await else { return };
	let Some(ledger) = common::seeded_ledger(&pool, "contract migrations").await else {
		return;
	};
	let history = seed_history(&pool).await;
	let before = picture(&pool).await;

	sqlx::migrate!().run_to(HEAD, &pool).await.expect("the contract applies over v2.20's history");
	assert_eq!(applied_version(&pool).await, HEAD, "0071 and 0072 are recorded as applied");
	let after = picture(&pool).await;

	// (a) The archive holds what only the removed paths understood.
	let in_public: Vec<String> = sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' AND tablename = ANY($1)")
		.bind(WHOLLY_RETIRED)
		.fetch_all(&pool)
		.await
		.unwrap();
	assert!(in_public.is_empty(), "left in public: {in_public:?}");
	let in_retired: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE schemaname = 'retired' AND tablename = ANY($1)")
		.bind(WHOLLY_RETIRED)
		.fetch_one(&pool)
		.await
		.unwrap();
	assert_eq!(in_retired, i64::try_from(WHOLLY_RETIRED.len()).unwrap(), "every removed path's table is in retired");
	let consilium: Vec<Uuid> = ids_in(&pool, "SELECT id FROM retired.consilium").await;
	assert_eq!(consilium, vec![history.consilium]);
	let voters: i64 = sqlx::query_scalar("SELECT count(*) FROM retired.consilium_voter").fetch_one(&pool).await.unwrap();
	let mails: i64 = sqlx::query_scalar("SELECT count(*) FROM retired.consilium_mail").fetch_one(&pool).await.unwrap();
	assert_eq!((voters, mails), (3, 3), "the voters and every governance mail, outcome mail included");
	assert_eq!(
		sorted(ids_in(&pool, "SELECT id FROM retired.payments").await),
		sorted(vec![
			history.consilium_payment,
			history.emailed_payment,
			history.service_payment,
			history.concierge_service_payment
		])
	);
	assert_eq!(ids_in(&pool, "SELECT payment_id FROM retired.payment_session_consent").await, vec![history.service_payment]);
	assert_eq!(ids_in(&pool, "SELECT payment_id FROM retired.scope_notification_outbox").await, vec![history.service_payment]);
	assert_eq!(
		ids_in(&pool, "SELECT payment_id FROM retired.payment_concierge_approval").await,
		vec![history.concierge_service_payment]
	);
	let seed: Vec<String> = sqlx::query_scalar("SELECT tx_ref FROM retired.deposits").fetch_all(&pool).await.unwrap();
	assert_eq!(seed, vec![history.seed_deposit.clone()]);
	assert_eq!(
		sorted(ids_in(&pool, "SELECT event_id FROM retired.event_log").await),
		sorted(history.retired_events.clone()),
		"exactly the retired events left the event log"
	);
	let retired_outbox = ids_in(&pool, "SELECT event_id FROM retired.outbox").await;
	assert!(
		retired_outbox.iter().all(|id| history.retired_events.contains(id)),
		"the outbox gave up only retired events: {retired_outbox:?}"
	);
	let live_outbox = ids_in(&pool, "SELECT event_id FROM outbox").await;
	assert!(
		live_outbox.iter().all(|id| !history.retired_events.contains(id)),
		"every relayed retired event left the outbox: {live_outbox:?}"
	);

	// (b) Every live read path loads what is left.
	let payments = PgPayments::new(pool.clone());
	let listed = payments.list(&PaymentFilter::default(), 100).await.expect("the payments list loads");
	assert_eq!(
		sorted(listed.iter().map(|view| view.order.id().raw()).collect()),
		sorted(vec![history.treasury_payment, history.session_payment])
	);
	let treasury = payments.find(PaymentId::from_raw(history.treasury_payment)).await.unwrap().expect("the treasury's order is live");
	assert_eq!(treasury.order.requirement(), PaymentApproval::OwnersApproval);
	payments.find(PaymentId::from_raw(history.session_payment)).await.unwrap().expect("the investor's order is live");
	for gone in [history.consilium_payment, history.emailed_payment, history.service_payment, history.concierge_service_payment] {
		assert!(payments.find(PaymentId::from_raw(gone)).await.unwrap().is_none(), "{gone} is archived, not live");
	}
	let withdrawals = PgWithdrawals::new(pool.clone());
	withdrawals.find_by_id(history.withdrawal).await.unwrap().expect("the withdrawal is live");
	assert_eq!(withdrawals.list_by_user(history.investor).await.unwrap().len(), 1);
	withdrawals.list_actionable().await.expect("the actionable list loads");
	let deposits = PgDeposits::new(pool.clone());
	assert_eq!(deposits.list_by_user(history.investor).await.unwrap().len(), 1, "the live deposit, not the seed");
	let live: Vec<String> = sqlx::query_scalar("SELECT tx_ref FROM deposits").fetch_all(&pool).await.unwrap();
	assert_eq!(live, vec![history.live_deposit.as_str().to_owned()], "the gate keeps the live deposit");
	let feed = PgOperationFeed::new(pool.clone()).list_by_user(history.investor, 50).await.expect("the operations feed loads");
	assert_eq!(feed.operations.len(), 2, "one deposit and one withdrawal");
	let users = PgUsers::new(pool.clone());
	for user in [history.investor, history.other] {
		users.find_by_id(user).await.unwrap().expect("the user loads");
	}
	assert!(payments.awaiting_execution().await.unwrap().is_empty());
	assert!(payments.awaiting_session_consent(history.investor).await.unwrap().is_empty());
	payments
		.find_for_session_subject(PaymentId::from_raw(history.session_payment), history.investor)
		.await
		.unwrap()
		.expect("the investor reads their order");
	assert!(payments.unproposed(0, 100).await.unwrap().is_empty(), "the archived seats left the approval scans");
	assert!(payments.due_to_check(100).await.unwrap().is_empty());
	assert!(payments.due_to_report(100).await.unwrap().is_empty());
	assert!(payments.due_to_withdraw(100).await.unwrap().is_empty());
	let notices = ScopeNotificationWorker::new(pool.clone(), Arc::new(NoNoticeDue));
	assert_eq!(notices.drain().await.expect("the notice outbox reads"), 0);
	assert!(outbox::next_batch(&pool, 100).await.unwrap().is_empty(), "nothing left to drain");
	assert!(outbox::parked_rows(&pool).await.unwrap().is_empty());
	outbox::pipeline_depth(&pool).await.expect("the readiness probe reads the outbox");
	Reconciliation::new(pool.clone(), Arc::clone(&ledger))
		.scan()
		.await
		.expect("the reconciliation scan reads every table it joins");
	assert!(every_event_parses(&pool, "event_log").await > 0);
	assert!(every_event_parses(&pool, "outbox").await > 0);

	// (c) The narrowed CHECKs refuse the retired shapes.
	let initiator = history.investor.raw();
	let refused = [
		sqlx::query(
			"INSERT INTO payments (id, state, from_kind, from_id, to_kind, to_id, amount, reason, payload_hash, initiator_user_id, expires_at) \
			 VALUES ($1, 'pending', 'service', 'fund', 'user', $2, '1', 'r', $3, $4, now() + interval '1 day')",
		)
		.bind(Uuid::new_v4())
		.bind(history.other.to_string())
		.bind(bytes32())
		.bind(initiator)
		.execute(&pool)
		.await,
		sqlx::query(
			"INSERT INTO payments (id, state, from_kind, from_id, to_kind, to_id, amount, reason, payload_hash, initiator_user_id, expires_at) \
			 VALUES ($1, 'pending', 'user', $2, 'service', 'service_arb', '1', 'r', $3, $4, now() + interval '1 day')",
		)
		.bind(Uuid::new_v4())
		.bind(history.investor.to_string())
		.bind(bytes32())
		.bind(initiator)
		.execute(&pool)
		.await,
		sqlx::query("INSERT INTO deposits (tx_ref, party_kind, party_id, network, amount, event_id) VALUES ($1, 'piggybank', NULL, 'bep20', '1', $2)")
			.bind(format!("0xnew{}", Uuid::new_v4().simple()))
			.bind(Uuid::new_v4())
			.execute(&pool)
			.await,
	];
	for (i, result) in refused.into_iter().enumerate() {
		assert_eq!(
			sqlstate_of(result).await.as_deref(),
			Some(CHECK_VIOLATION),
			"insert #{i} of a retired shape must be refused by a CHECK"
		);
	}
	let valid = |name: &str| after.constraints.iter().find(|(_, n, ..)| n == name).unwrap_or_else(|| panic!("{name} exists")).3;
	for name in ["deposits_party_is_a_user", "payments_from_kind_check", "payments_to_kind_check"] {
		assert!(valid(name), "{name} is validated over every live row");
	}

	// (d) The documented rollbacks restore v2.20 exactly; the files re-apply over it.
	roll_back(&pool).await;
	assert_eq!(picture(&pool).await, before, "rolled back, the database is v2.20's again");
	sqlx::migrate!().run_to(HEAD, &pool).await.expect("the contract re-applies after its rollback");
	assert_eq!(picture(&pool).await, after, "apply → roll back → apply lands on the same contract");

	// (e) A settle v2.21 wrote, which names no payee, reads as v2.20's after a rollback and
	// stays live through a re-apply.
	let settled = to_value(&WithdrawalEvent::Settled {
		withdrawal_id: history.withdrawal,
		source: WithdrawalSource::User(history.investor),
		network: Network::Bep20,
		amount: usdt("5"),
		fee: usdt("1"),
		tx_ref: TxRef::parse(&format!("0xv221{}", Uuid::new_v4().simple())).unwrap(),
	});
	assert!(settled.get("payee").is_none(), "v2.21 writes no payee");
	let v2_21_settle = event(&pool, "withdrawal", history.withdrawal.raw(), "withdrawals", &settled, Relayed::Dispatched).await;
	roll_back(&pool).await;
	let treasury = json!({ "kind": "scope_account", "id": { "scope": "main", "label": "treasury" } });
	for table in ["event_log", "outbox"] {
		let payee: Option<Value> = sqlx::query_scalar(AssertSqlSafe(format!("SELECT payload->'payee' FROM {table} WHERE event_id = $1")))
			.bind(v2_21_settle)
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(payee, Some(treasury.clone()), "{table}: v2.20's Settled requires the payee it always wrote");
	}
	sqlx::migrate!().run_to(HEAD, &pool).await.expect("the contract re-applies over v2.21's settle");
	for table in ["event_log", "outbox"] {
		let live: bool = sqlx::query_scalar(AssertSqlSafe(format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE event_id = $1)")))
			.bind(v2_21_settle)
			.fetch_one(&pool)
			.await
			.unwrap();
		assert!(live, "{table}: the settle paid the treasury, so it stays live");
		every_event_parses(&pool, table).await;
	}
}

async fn roll_back(pool: &PgPool) {
	sqlx::raw_sql(AssertSqlSafe(rollback(GOVERNANCE_CONTRACT))).execute(pool).await.expect("0072's rollback applies");
	sqlx::raw_sql(AssertSqlSafe(rollback(UNITS_CONTRACT))).execute(pool).await.expect("0071's rollback applies");
	assert_eq!(applied_version(pool).await, BEFORE, "the rollbacks take their versions with them");
}

/// The notice outbox after the contract: nothing of an archived payment is still owed.
struct NoNoticeDue;

#[async_trait::async_trait]
impl ScopeNotifier for NoNoticeDue {
	async fn emit(&self, _: Uuid, notice: &ScopeNotice) -> Result<(), NoticeError> {
		panic!("no notice is due after the contract, yet {notice:?} was handed over")
	}
}

/// One piece of live state of a removed path, how to plant it, how to take it away again,
/// and the words its refusal must carry.
struct LiveState {
	what: &'static str,
	seed: String,
	cleanup: String,
	refusal: &'static str,
}

fn outbox_row(kind: &str, payload: &str, stamps: &str) -> String {
	format!(
		"INSERT INTO outbox (event_id, aggregate, aggregate_id, kind, payload{}) VALUES ('{}', 'x', '{}', '{kind}', '{payload}'{});",
		if stamps.is_empty() { "" } else { ", parked_at" },
		Uuid::new_v4(),
		Uuid::new_v4(),
		if stamps.is_empty() { "" } else { ", now()" },
	)
}

fn live_states(investor: UserId, admin: UserId) -> Vec<LiveState> {
	let id = Uuid::new_v4();
	let payment = |state: &str, from: (&str, String), to: (&str, String)| {
		format!(
			"INSERT INTO payments (id, state, from_kind, from_id, to_kind, to_id, amount, reason, payload_hash, initiator_user_id, expires_at, decided_at) \
			 VALUES ('{id}', '{state}', '{}', '{}', '{}', '{}', '1', 'r', '\\x{}', '{admin}', now() + interval '7 days', {});",
			from.0,
			from.1,
			to.0,
			to.1,
			"00".repeat(32),
			if state == "pending" { "NULL" } else { "now()" },
		)
	};
	let consilium = |state: &str| {
		format!(
			"INSERT INTO consilium (id, kind, state, terms, payload_hash, initiator_user_id, owner_count, threshold, expires_at, decided_at, source_claim) \
			 VALUES ('{id}', 'valuation_override', '{state}', '{{}}', '\\x{}', '{admin}', 3, 2, now() + interval '7 days', {}, 'fund');",
			"00".repeat(32),
			if state == "open" { "NULL" } else { "now()" },
		)
	};
	let consent = format!(
		"INSERT INTO payment_consent (payment_id, subject_user_id, token_hash, code_hash, expires_at, subject_token_version_at_open, subject_email_hash_at_open) \
		 VALUES ('{id}', '{investor}', '\\x{}', '\\x{}', now() + interval '7 days', 0, '\\x{}');",
		"01".repeat(32),
		"02".repeat(32),
		"03".repeat(32),
	);
	let user = |u: UserId| u.to_string();
	let concierge_seat = |withdrawn: &str| {
		format!(
			"INSERT INTO payment_concierge_approval (payment_id, destination, payload_hash, approval_id, withdrawn_at) VALUES ('{id}', 'user:{investor}', '{}', 'approval-1', {withdrawn});",
			"ab".repeat(32)
		)
	};
	let payment_event = format!(
		"INSERT INTO outbox (event_id, aggregate, aggregate_id, kind, payload) VALUES ('{}', 'payment', '{id}', 'payments', '{{\"type\":\"settled\"}}');",
		Uuid::new_v4()
	);
	let clear_events = "DELETE FROM outbox;".to_owned();
	let clear_payment = format!("DELETE FROM payments WHERE id = '{id}';");
	vec![
		LiveState {
			what: "an open book order",
			seed: format!(
				"INSERT INTO book_orders (id, service, user_id, client_order_id, side, kind, tif, price, size, reserved, revision) \
				 VALUES ('{id}', 'svc', '{investor}', 'o1', 'buy', 'limit', 'gtc', '1', '1', '1', 1);"
			),
			cleanup: format!("DELETE FROM book_orders WHERE id = '{id}';"),
			refusal: "open book order",
		},
		LiveState {
			what: "a queued redemption",
			seed: format!("INSERT INTO redemptions (id, user_id, service, units, state) VALUES ('{id}', '{investor}', 'svc', '1', 'queued');"),
			cleanup: format!("DELETE FROM redemptions WHERE id = '{id}';"),
			refusal: "queued redemption",
		},
		LiveState {
			what: "a queued issuance",
			seed: format!(
				"INSERT INTO unit_issuances (id, service, holder_kind, holder_id, units, nav, cost_basis, idempotency_key) \
				 VALUES ('{id}', 'svc', 'user', '{investor}', '1', '1', '1', 'q');"
			),
			cleanup: format!("DELETE FROM unit_issuances WHERE id = '{id}';"),
			refusal: "queued unit issuance",
		},
		LiveState {
			what: "a scheduled fee policy change",
			seed: format!(
				"INSERT INTO fee_policy_changes (id, service, version, management_bps, performance_bps, hurdle_bps, basis, crystallization, state, requirement, effective_from, requested_by, scheduled_at) \
				 VALUES ('{id}', 'svc', 1, 100, 1000, 0, 'invested_capital', 'monthly', 'scheduled', 'admin', now() + interval '30 days', 'operator', now());"
			),
			cleanup: format!("DELETE FROM fee_policy_changes WHERE id = '{id}';"),
			refusal: "fee policy change",
		},
		LiveState {
			what: "a pending payment out of an allocation",
			seed: payment("pending", ("service", "fund".into()), ("user", user(investor))),
			cleanup: clear_payment.clone(),
			refusal: "open payment(s) naming an allocation",
		},
		LiveState {
			what: "an undispatched subscription",
			seed: outbox_row("subscriptions", r#"{"type":"subscribed"}"#, ""),
			cleanup: clear_events.clone(),
			refusal: "outbox row(s) of a removed unit path",
		},
		LiveState {
			what: "a parked book event",
			seed: outbox_row("book", r#"{"type":"traded"}"#, "parked"),
			cleanup: clear_events.clone(),
			refusal: "outbox row(s) of a removed unit path",
		},
		LiveState {
			what: "an undispatched settle paying the fee allocation",
			seed: outbox_row("withdrawals", r#"{"type":"settled","payee":{"kind":"service","id":"fee"}}"#, ""),
			cleanup: clear_events.clone(),
			refusal: "outbox row(s) of a removed unit path",
		},
		LiveState {
			what: "an undispatched deposit onto the retired fund claim",
			seed: outbox_row("balance", r#"{"type":"deposited","party":{"kind":"piggybank"},"network":"bep20","amount":"1"}"#, ""),
			cleanup: clear_events.clone(),
			refusal: "outbox row(s) of a removed unit path",
		},
		LiveState {
			what: "an undispatched event of a payment into an allocation",
			seed: format!("{}{payment_event}", payment("executed", ("user", user(investor)), ("service", "svc".into()))),
			cleanup: format!("{clear_events}{clear_payment}"),
			refusal: "outbox row(s) of a removed unit path",
		},
		LiveState {
			what: "a governance mail about a session-seated payment into an allocation",
			seed: format!(
				"{}INSERT INTO consilium_mail (payment_id, user_id, kind, dedupe_key, payload, sent_at) VALUES ('{id}', '{investor}', 'payment_outcome', 'm0', '{{}}', now());",
				payment("executed", ("user", user(investor)), ("service", "svc".into())),
			),
			cleanup: clear_payment.clone(),
			refusal: "governance mail(s) about a concierge- or session-seated payment",
		},
		LiveState {
			what: "a concierge seat of an executed allocation payout that owes its outcome",
			seed: format!("{}{}", payment("executed", ("service", "fund".into()), ("user", user(investor))), concierge_seat("NULL")),
			cleanup: clear_payment.clone(),
			refusal: "still owe concierge an outcome or a withdrawal",
		},
		LiveState {
			what: "a concierge seat of a cancelled allocation payout that owes its withdrawal",
			seed: format!("{}{}", payment("cancelled", ("service", "fund".into()), ("user", user(investor))), concierge_seat("NULL")),
			cleanup: clear_payment.clone(),
			refusal: "still owe concierge an outcome or a withdrawal",
		},
		LiveState {
			what: "an unsent notice about a payment into an allocation",
			seed: format!(
				"{}INSERT INTO scope_notification_outbox (user_id, topic, kind, title, body, link, dedupe_key, payment_id) \
				 VALUES ('{investor}', 'bank:money-movement', 'payment_outcome', 'A payment', '', '/wallet', 'n1', '{id}');",
				payment("executed", ("user", user(investor)), ("service", "svc".into())),
			),
			cleanup: clear_payment.clone(),
			refusal: "notice(s) about a payment naming an allocation not yet sent",
		},
		LiveState {
			what: "a payment that stays live with no seat",
			seed: payment("rejected", ("user", user(investor)), ("user", user(admin))),
			cleanup: clear_payment.clone(),
			refusal: "without exactly one concierge or session seat",
		},
		LiveState {
			what: "an executed treasury payment into an allocation",
			seed: payment("executed", ("treasury", "main:treasury".into()), ("service", "svc".into())),
			cleanup: clear_payment.clone(),
			refusal: "executed treasury payment",
		},
		LiveState {
			what: "an open consilium",
			seed: consilium("open"),
			cleanup: format!("DELETE FROM consilium WHERE id = '{id}';"),
			refusal: "open or executing consilium",
		},
		LiveState {
			what: "an approved consilium awaiting execution",
			seed: consilium("approved"),
			cleanup: format!("DELETE FROM consilium WHERE id = '{id}';"),
			refusal: "open or executing consilium",
		},
		LiveState {
			what: "a pending payment on an emailed consent",
			seed: format!("{}{consent}", payment("pending", ("user", user(investor)), ("user", user(admin)))),
			cleanup: clear_payment.clone(),
			refusal: "waiting on a consilium or emailed-consent seat",
		},
		LiveState {
			what: "a pending payment on a consilium seat",
			seed: format!(
				"{}{}INSERT INTO payment_approval (payment_id, consilium_id) VALUES ('{id}', '{id}');",
				payment("pending", ("treasury", "main:treasury".into()), ("user", user(investor))),
				consilium("cancelled"),
			),
			cleanup: format!("DELETE FROM payment_approval WHERE payment_id = '{id}'; DELETE FROM consilium WHERE id = '{id}'; {clear_payment}"),
			refusal: "waiting on a consilium or emailed-consent seat",
		},
		LiveState {
			what: "an unsent governance mail",
			seed: format!(
				"{}INSERT INTO consilium_mail (consilium_id, user_id, kind, dedupe_key, payload) VALUES ('{id}', '{investor}', 'payout_outcome', 'm1', '{{}}');",
				consilium("cancelled"),
			),
			cleanup: format!("DELETE FROM consilium WHERE id = '{id}';"),
			refusal: "governance mail(s) not yet sent",
		},
		LiveState {
			what: "an undispatched consilium event",
			seed: outbox_row("consilium", r#"{"type":"opened"}"#, ""),
			cleanup: clear_events.clone(),
			refusal: "outbox row(s) of the old approval path",
		},
		LiveState {
			what: "an undispatched event of a payment on an emailed consent",
			seed: format!(
				"{}INSERT INTO payment_consent (payment_id, subject_user_id, decision, decided_at, used_at, token_hash, code_hash, expires_at, subject_token_version_at_open, subject_email_hash_at_open) \
				 VALUES ('{id}', '{investor}', 'reject', now(), now(), '\\x{}', '\\x{}', now() + interval '7 days', 0, '\\x{}');{payment_event}",
				payment("rejected", ("user", user(investor)), ("user", user(admin))),
				"01".repeat(32),
				"02".repeat(32),
				"03".repeat(32),
			),
			cleanup: format!("{clear_events}{clear_payment}"),
			refusal: "outbox row(s) of the old approval path",
		},
		LiveState {
			what: "a payment seated on both an old and a new path",
			seed: format!(
				"{}INSERT INTO payment_consent (payment_id, subject_user_id, decision, decided_at, used_at, token_hash, code_hash, expires_at, subject_token_version_at_open, subject_email_hash_at_open) \
				 VALUES ('{id}', '{investor}', 'reject', now(), now(), '\\x{}', '\\x{}', now() + interval '7 days', 0, '\\x{}');\
				 INSERT INTO payment_session_consent (payment_id, subject_user_id, decision, decided_at, subject_token_version_at_open, subject_email_hash_at_open) \
				 VALUES ('{id}', '{investor}', 'reject', now(), 0, '\\x{}');",
				payment("rejected", ("user", user(investor)), ("user", user(admin))),
				"01".repeat(32),
				"02".repeat(32),
				"03".repeat(32),
				"04".repeat(32),
			),
			cleanup: clear_payment,
			refusal: "seated on both an old and a new approval path",
		},
	]
}

#[tokio::test]
async fn the_preflight_refuses_live_state_of_a_removed_path() {
	let _serial = common::outbox_serial().await;
	let Some(pool) = database_at_v2_20("c71b").await else { return };
	let investor = person(&pool, "investor").await;
	let admin = person(&pool, "admin").await;
	exec(
		&pool,
		&format!("INSERT INTO allocations (id, service, title, state) VALUES ('{}', 'svc', 'Svc', 'open');", Uuid::new_v4()),
	)
	.await;

	for state in live_states(investor, admin) {
		exec(&pool, &state.seed).await;
		// A connection of its own, closed after: sqlx defers the failed migration's ROLLBACK to
		// the connection's next use, so a pooled one would sit on its locks.
		let mut conn = pool.acquire().await.expect("a connection").detach();
		let refused = sqlx::migrate!().run_to(HEAD, &mut conn).await.expect_err(state.what);
		conn.close().await.expect("close the refused migration's connection");
		assert!(
			refused.to_string().contains(state.refusal),
			"{}: the refusal names it ({:?}), got: {refused}",
			state.what,
			state.refusal
		);
		assert_eq!(applied_version(&pool).await, BEFORE, "{}: nothing applied before the refusal", state.what);
		exec(&pool, &state.cleanup).await;
	}

	sqlx::migrate!().run_to(HEAD, &pool).await.expect("with no live state left, the contract applies");
	assert_eq!(applied_version(&pool).await, HEAD);
}
