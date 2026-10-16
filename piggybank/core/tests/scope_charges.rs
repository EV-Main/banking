//! `ScopeLedgerService.Charge` against real Postgres and TigerBeetle: the mandate's caps and
//! liveness, the freezes, the grant and the credited account's owner, idempotency by key, two
//! charges racing for one cap or one balance, and the posting discipline (a charge is announced
//! only once the relay has posted it, and a ledger refusal leaves nothing behind).
//!
//! Each test works in scopes, clients and users of its own.

use std::sync::Arc;

use domain::{
	balance::{LedgerAccountKey, TransferCode},
	error::DomainError,
	money::{Network, Usdt},
	scope_ledger::{ClientId, Grant, Grants, LedgerRefusal, MandateOperation, MoneyEventKind, SubjectRef},
	scopes::{MoveState, ScopeAccountLabel, ScopeId},
	users::UserId,
};
use piggybank_core::{
	application::{
		scope_charges::{self as charges_app, ChargeRequest},
		scope_ledger::{self as ledger_app, ScopeLedgerPorts, TransferError},
		scope_mandates::{self as mandates_app, GrantRequest, MandatePorts},
	},
	infrastructure::{compartments::PgCompartments, relay::Relay, scope_ledger::PgScopeLedger},
	ports::{
		ledger::{Ledger, LedgerBalance, LedgerTransfer},
		scope_charges::RecordedCharge,
		scope_mandates::RecordedMandate,
	},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

struct Harness {
	pool: PgPool,
	ledger: Arc<dyn Ledger>,
	store: PgScopeLedger,
	scopes: PgCompartments,
	relay: Relay,
	notify: Arc<Notify>,
	_serial: tokio::sync::MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "scope mandate tests").await?;
	let notify = Arc::new(Notify::new());
	Some(Harness {
		store: PgScopeLedger::new(pool.clone()),
		scopes: PgCompartments::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(piggybank_core::infrastructure::custody::StubCustody), notify.clone()),
		pool,
		ledger,
		notify,
		_serial: serial,
	})
}

fn ports<'a>(h: &'a Harness, grants: &'a Grants) -> ScopeLedgerPorts<'a> {
	ScopeLedgerPorts {
		grants,
		store: &h.store,
		holds: &h.store,
		events: &h.store,
		scopes: &h.scopes,
		ledger: h.ledger.as_ref(),
		relay: &h.notify,
	}
}

fn mandate_ports<'a>(h: &'a Harness, grants: &'a Grants) -> MandatePorts<'a> {
	MandatePorts {
		grants,
		store: &h.store,
		scopes: &h.scopes,
		relay: &h.notify,
	}
}

fn usdt(raw: &str) -> Usdt {
	Usdt::parse_decimal(raw).unwrap()
}

fn label(raw: &str) -> ScopeAccountLabel {
	ScopeAccountLabel::parse(raw).unwrap()
}

fn key() -> String {
	Uuid::new_v4().to_string()
}

fn subject_ref() -> SubjectRef {
	SubjectRef::parse(&format!("subscription:{}", Uuid::new_v4())).unwrap()
}

fn fresh_client() -> ClientId {
	ClientId::parse(&format!("m-{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap()
}

/// A scope of this test's own, active, default policy, banking's fee collector open.
async fn own_scope(h: &Harness) -> ScopeId {
	let id = ScopeId::parse(&format!("m_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	sqlx::query("INSERT INTO scopes (id, display_name) VALUES ($1, 'Mandate scope')")
		.bind(id.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("INSERT INTO scope_policies (scope) VALUES ($1)").bind(id.as_str()).execute(&h.pool).await.unwrap();
	sqlx::query("INSERT INTO scope_accounts (scope, label, owner_client) VALUES ($1, 'fee', 'banking')")
		.bind(id.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	id
}

fn grant(client: &ClientId, scope: &ScopeId, prefix: &str, compartments: bool) -> Grant {
	Grant {
		client: client.clone(),
		scope: scope.clone(),
		label_prefix: prefix.to_owned(),
		compartments,
	}
}

/// A user with `amount` in their compartment in `scope`, straight on the ledger (custody grows
/// with it, so the cash invariant holds).
async fn user_with(h: &Harness, scope: &ScopeId, amount: &str) -> (UserId, String) {
	let user = UserId::new();
	let auth_subject = format!("itest-{}", Uuid::new_v4());
	sqlx::query("INSERT INTO users (id, auth_subject, email) VALUES ($1, $2, $3)")
		.bind(user.raw())
		.bind(&auth_subject)
		.bind(format!("{}@example.com", Uuid::new_v4().simple()))
		.execute(&h.pool)
		.await
		.unwrap();
	move_on_ledger(h, LedgerAccountKey::CryptoWallet(Network::Bep20), LedgerAccountKey::ScopeClaim(user, scope.clone()), amount).await;
	(user, auth_subject)
}

async fn move_on_ledger(h: &Harness, debit: LedgerAccountKey, credit: LedgerAccountKey, amount: &str) {
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit,
			credit,
			amount: usdt(amount).base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.unwrap();
}

async fn compartment(h: &Harness, user: UserId, scope: &ScopeId) -> LedgerBalance {
	h.ledger.balance(&LedgerAccountKey::ScopeClaim(user, scope.clone())).await.unwrap()
}

/// One client, granted `alloc:` with the compartments flag in a fresh scope, owning `alloc:x`.
async fn holder(h: &Harness) -> (ClientId, ScopeId, Grants) {
	let client = fresh_client();
	let scope = own_scope(h).await;
	let grants = Grants::new(vec![grant(&client, &scope, "alloc:", true)]).unwrap();
	ledger_app::open_account(&ports(h, &grants), &client, &scope, &label("alloc:x"), &key()).await.unwrap();
	(client, scope, grants)
}

fn charge_request(scope: &ScopeId, auth_subject: &str, to: &str, amount: &str) -> ChargeRequest {
	ChargeRequest {
		scope: scope.clone(),
		auth_subject: auth_subject.to_owned(),
		amount: usdt(amount),
		to: label(to),
		memo: "Monthly plan".to_owned(),
		subject_ref: subject_ref(),
		idempotency_key: key(),
	}
}

fn mandate_request(client: &ClientId, scope: &ScopeId, per_op: &str, per_24h: &str, operations: Vec<MandateOperation>) -> GrantRequest {
	GrantRequest {
		scope: scope.clone(),
		client: client.clone(),
		cap_per_op: usdt(per_op),
		cap_24h: usdt(per_24h),
		cap_outstanding: None,
		cap_book_fill_24h: None,
		operations,
		expires_at: None,
		idempotency_key: key(),
	}
}

async fn grant_mandate(h: &Harness, grants: &Grants, user: UserId, request: GrantRequest) -> RecordedMandate {
	mandates_app::grant(&mandate_ports(h, grants), user, request).await.unwrap()
}

fn refusal<T: std::fmt::Debug>(result: Result<T, TransferError>) -> LedgerRefusal {
	match result {
		Err(TransferError::Refused(refusal)) => refusal,
		other => panic!("expected a refusal, got {other:?}"),
	}
}

/// A charging client with `alloc:x` open, a user holding `funds` in their compartment, and a
/// CHARGE mandate of `per_op` / `per_24h` for the client.
async fn charging(h: &Harness, funds: &str, per_op: &str, per_24h: &str) -> (ClientId, ScopeId, Grants, UserId, String) {
	let (client, scope, grants) = holder(h).await;
	let (user, subject) = user_with(h, &scope, funds).await;
	grant_mandate(h, &grants, user, mandate_request(&client, &scope, per_op, per_24h, vec![MandateOperation::Charge])).await;
	(client, scope, grants, user, subject)
}

async fn charge_row(h: &Harness, charge: &RecordedCharge) -> (String, Option<String>) {
	sqlx::query_as("SELECT state, failure_reason FROM scope_charges WHERE id = $1")
		.bind(charge.charge.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

async fn account_balance(h: &Harness, scope: &ScopeId, name: &str) -> u128 {
	h.ledger.balance(&LedgerAccountKey::ScopeAccount(scope.clone(), label(name))).await.unwrap().posted
}

#[tokio::test]
async fn a_charge_is_one_posted_transfer_announced_only_once_posted() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants, user, subject) = charging(&h, "100", "100", "200").await;
	let p = ports(&h, &grants);

	let request = charge_request(&scope, &subject, "alloc:x", "40");
	let charged = charges_app::charge(&p, &client, request.clone()).await.unwrap();
	let terms = charged.charge.terms();
	assert_eq!((terms.amount, terms.to.as_str(), terms.memo.as_str()), (usdt("40"), "alloc:x", "Monthly plan"));
	assert_eq!(charged.state, MoveState::Pending);
	assert_eq!(charge_row(&h, &charged).await.0, "pending");

	// Recorded, not posted: nothing moved yet, nothing announced, but the 40 is promised away.
	assert_eq!(compartment(&h, user, &scope).await.posted, usdt("100").base_units());
	assert!(events_of(&h, &client, &grants, &scope).await.iter().all(|e| e.kind != MoneyEventKind::Charged));
	assert_eq!(
		refusal(charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "70")).await),
		LedgerRefusal::InsufficientFunds,
		"100 less the 40 promised away leaves 60"
	);
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	assert_eq!(compartment(&h, user, &scope).await.posted, usdt("60").base_units());
	assert_eq!(account_balance(&h, &scope, "alloc:x").await, usdt("40").base_units());
	assert_eq!(charge_row(&h, &charged).await.0, "posted");
	let events = events_of(&h, &client, &grants, &scope).await;
	let announced: Vec<_> = events.iter().filter(|e| e.kind == MoneyEventKind::Charged).collect();
	let [event] = announced.as_slice() else { panic!("one CHARGED event: {events:?}") };
	assert_eq!(event.ref_id, charged.charge.id().to_string());
	assert_eq!((event.auth_subject.as_str(), event.amount), (subject.as_str(), Some(usdt("40"))));
	assert_eq!(event.subject_ref, terms.subject_ref.as_str());
	assert_eq!(event.initiator_client, client.as_str());
	assert_eq!(event.balances.len(), 1, "only the client's own account shows");
	assert_eq!(event.balances[0].balance, usdt("40"));

	// The user sees the draw against the mandate.
	let listed = mandates_app::list(&mandate_ports(&h, &grants), user).await.unwrap();
	assert_eq!(listed[0].usage.drawn_24h, usdt("40"));
	assert_eq!(listed[0].usage.outstanding, Usdt::ZERO, "a charge holds nothing");
}

#[tokio::test]
async fn every_cap_and_every_missing_consent_is_mandate_required() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants, user, subject) = charging(&h, "100", "30", "50").await;
	let p = ports(&h, &grants);

	assert_eq!(
		refusal(charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "30.000000000000000001")).await),
		LedgerRefusal::MandateRequired,
		"past cap_per_op"
	);
	charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "30")).await.unwrap();
	assert_eq!(
		refusal(charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "20.000000000000000001")).await),
		LedgerRefusal::MandateRequired,
		"past cap_24h with the first charge counted"
	);
	charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "20")).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(
		refusal(charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "0.000001")).await),
		LedgerRefusal::MandateRequired,
		"the day is spent, posted or not"
	);
	// A day later the window has room again.
	sqlx::query("UPDATE scope_charges SET created_at = created_at - interval '25 hours' WHERE client_id = $1")
		.bind(client.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "30")).await.unwrap();

	// A mandate that does not list CHARGE, a user with none, and a subject banking has no wallet
	// for all read alike.
	let (reserve_only, reserve_scope, reserve_grants) = holder(&h).await;
	let (reserve_user, reserve_subject) = user_with(&h, &reserve_scope, "10").await;
	grant_mandate(
		&h,
		&reserve_grants,
		reserve_user,
		mandate_request(&reserve_only, &reserve_scope, "10", "10", vec![MandateOperation::Reserve]),
	)
	.await;
	let (none_client, none_scope, none_grants) = holder(&h).await;
	let (_, none_subject) = user_with(&h, &none_scope, "10").await;
	for (grants, client, scope, subject, why) in [
		(&reserve_grants, &reserve_only, &reserve_scope, reserve_subject.as_str(), "operation not listed"),
		(&none_grants, &none_client, &none_scope, none_subject.as_str(), "no mandate"),
		(&none_grants, &none_client, &none_scope, "nobody-at-all", "unknown subject"),
	] {
		assert_eq!(
			refusal(charges_app::charge(&ports(&h, grants), client, charge_request(scope, subject, "alloc:x", "1")).await),
			LedgerRefusal::MandateRequired,
			"{why}"
		);
	}

	// Revoked, and expired: the same answer.
	let (c2, s2, g2, u2, sub2) = charging(&h, "10", "10", "10").await;
	let mandate = mandates_app::list(&mandate_ports(&h, &g2), u2).await.unwrap().remove(0);
	sqlx::query("UPDATE scope_mandates SET expires_at = now() - interval '1 second' WHERE id = $1")
		.bind(mandate.mandate.id)
		.execute(&h.pool)
		.await
		.unwrap();
	assert_eq!(
		refusal(charges_app::charge(&ports(&h, &g2), &c2, charge_request(&s2, &sub2, "alloc:x", "1")).await),
		LedgerRefusal::MandateRequired,
		"expired"
	);
	let (c3, s3, g3, u3, sub3) = charging(&h, "10", "10", "10").await;
	let mandate = mandates_app::list(&mandate_ports(&h, &g3), u3).await.unwrap().remove(0);
	mandates_app::revoke(&mandate_ports(&h, &g3), u3, mandate.mandate.id).await.unwrap();
	assert_eq!(
		refusal(charges_app::charge(&ports(&h, &g3), &c3, charge_request(&s3, &sub3, "alloc:x", "1")).await),
		LedgerRefusal::MandateRequired,
		"revoked"
	);
	let _ = user;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn freezes_grants_and_account_ownership_refuse_a_charge() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants, user, subject) = charging(&h, "100", "50", "100").await;
	let p = ports(&h, &grants);

	sqlx::query("UPDATE users SET frozen = TRUE WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
	assert_eq!(
		refusal(charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "1")).await),
		LedgerRefusal::UserFrozen
	);
	sqlx::query("UPDATE users SET frozen = FALSE WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();

	for (state, expected) in [("frozen", LedgerRefusal::ScopeFrozen), ("closed", LedgerRefusal::ScopeClosed)] {
		sqlx::query("UPDATE scopes SET state = $2 WHERE id = $1")
			.bind(scope.as_str())
			.bind(state)
			.execute(&h.pool)
			.await
			.unwrap();
		assert_eq!(refusal(charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "1")).await), expected);
	}
	sqlx::query("UPDATE scopes SET state = 'active' WHERE id = $1")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();

	// banking's own accounts, someone else's, and none at all.
	let rival = fresh_client();
	let both = Grants::new(vec![grant(&client, &scope, "alloc:", true), grant(&rival, &scope, "panel:", true)]).unwrap();
	ledger_app::open_account(&ports(&h, &both), &rival, &scope, &label("panel:till"), &key()).await.unwrap();
	for to in ["fee", "treasury", "panel:till", "alloc:nope"] {
		assert_eq!(
			refusal(charges_app::charge(&ports(&h, &both), &client, charge_request(&scope, &subject, to, "1")).await),
			LedgerRefusal::NotAccountOwner,
			"{to}"
		);
	}

	// No compartments flag, no grant in the scope, main.
	let accounts_only = Grants::new(vec![grant(&client, &scope, "alloc:", false)]).unwrap();
	assert_eq!(
		refusal(charges_app::charge(&ports(&h, &accounts_only), &client, charge_request(&scope, &subject, "alloc:x", "1")).await),
		LedgerRefusal::NotGranted
	);
	assert_eq!(
		refusal(charges_app::charge(&ports(&h, &accounts_only), &rival, charge_request(&scope, &subject, "alloc:x", "1")).await),
		LedgerRefusal::NotGranted
	);
	assert_eq!(
		refusal(charges_app::charge(&p, &client, charge_request(&ScopeId::main(), &subject, "alloc:x", "1")).await),
		LedgerRefusal::NotGranted
	);
	assert_eq!(
		refusal(charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "100.000000000000000001")).await),
		LedgerRefusal::MandateRequired,
		"past the mandate before the balance"
	);

	// Nothing above reached the ledger or the table.
	let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM scope_charges WHERE client_id = $1")
		.bind(client.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(rows, 0);
	assert_eq!(compartment(&h, user, &scope).await.posted, usdt("100").base_units());
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn a_key_is_one_charge_and_other_terms_under_it_conflict() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants, user, subject) = charging(&h, "100", "50", "100").await;
	let p = ports(&h, &grants);

	let request = charge_request(&scope, &subject, "alloc:x", "10");
	let first = charges_app::charge(&p, &client, request.clone()).await.unwrap();
	let again = charges_app::charge(&p, &client, request.clone()).await.unwrap();
	assert_eq!(first.charge.id(), again.charge.id(), "the original charge");
	let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM scope_charges WHERE client_id = $1")
		.bind(client.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(rows, 1);

	let (_, other_subject) = user_with(&h, &scope, "5").await;
	ledger_app::open_account(&p, &client, &scope, &label("alloc:y"), &key()).await.unwrap();
	for (different, why) in [
		(
			ChargeRequest {
				amount: usdt("11"),
				..request.clone()
			},
			"amount",
		),
		(
			ChargeRequest {
				to: label("alloc:y"),
				..request.clone()
			},
			"to",
		),
		(
			ChargeRequest {
				memo: "Other".into(),
				..request.clone()
			},
			"memo",
		),
		(
			ChargeRequest {
				auth_subject: other_subject,
				..request.clone()
			},
			"user",
		),
		(
			ChargeRequest {
				subject_ref: subject_ref(),
				..request.clone()
			},
			"subject_ref",
		),
	] {
		match charges_app::charge(&p, &client, different).await {
			Err(TransferError::Domain(DomainError::Conflict(_))) => {}
			other => panic!("{why}: expected a conflict, got {other:?}"),
		}
	}

	// The original answer outlives what changed since: a revoked mandate, a frozen scope.
	let mandate = mandates_app::list(&mandate_ports(&h, &grants), user).await.unwrap().remove(0);
	mandates_app::revoke(&mandate_ports(&h, &grants), user, mandate.mandate.id).await.unwrap();
	sqlx::query("UPDATE scopes SET state = 'frozen' WHERE id = $1")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	let after = charges_app::charge(&p, &client, request).await.unwrap();
	assert_eq!(after.charge.id(), first.charge.id());
	// ...but a caller with no grant left in the scope gets none of it.
	let nobody = Grants::new(vec![]).unwrap();
	assert_eq!(
		refusal(charges_app::charge(&ports(&h, &nobody), &client, charge_request(&scope, &subject, "alloc:x", "1")).await),
		LedgerRefusal::NotGranted
	);
	sqlx::query("UPDATE scopes SET state = 'active' WHERE id = $1")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(compartment(&h, user, &scope).await.posted, usdt("90").base_units(), "debited once");
}

#[tokio::test]
async fn two_charges_racing_for_one_cap_or_one_balance_let_one_through() {
	let Some(h) = harness().await else { return };

	// The cap: room for one of 30 in a day of 40.
	let (client, scope, grants, _, subject) = charging(&h, "100", "30", "40").await;
	let p = ports(&h, &grants);
	let (a, b) = tokio::join!(
		charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "30")),
		charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "30")),
	);
	let results = [a, b];
	assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "{results:?}");
	let loser = results.into_iter().find(|r| r.is_err()).unwrap();
	assert_eq!(refusal(loser), LedgerRefusal::MandateRequired);

	// The balance: 10 in the compartment, two charges of 6 under generous caps.
	let (client, scope, grants, user, subject) = charging(&h, "10", "50", "500").await;
	let p = ports(&h, &grants);
	let (a, b) = tokio::join!(
		charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "6")),
		charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "6")),
	);
	let results = [a, b];
	assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "{results:?}");
	let loser = results.into_iter().find(|r| r.is_err()).unwrap();
	assert_eq!(refusal(loser), LedgerRefusal::InsufficientFunds);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(compartment(&h, user, &scope).await.posted, usdt("4").base_units());
}

#[tokio::test]
async fn a_charge_the_ledger_refuses_leaves_nothing_behind() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants, user, subject) = charging(&h, "10", "10", "10").await;
	let p = ports(&h, &grants);
	let charged = charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "10")).await.unwrap();

	// Another flow takes the money straight off the ledger before the relay posts the charge.
	move_on_ledger(&h, LedgerAccountKey::ScopeClaim(user, scope.clone()), LedgerAccountKey::CryptoWallet(Network::Bep20), "10").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let (state, reason) = charge_row(&h, &charged).await;
	assert_eq!((state.as_str(), reason.as_deref()), ("failed", Some("insufficient_funds")));
	assert_eq!(account_balance(&h, &scope, "alloc:x").await, 0);
	assert!(
		events_of(&h, &client, &grants, &scope).await.iter().all(|e| e.kind != MoneyEventKind::Charged),
		"a refused charge is never announced"
	);
	// A failed charge no longer counts against the day...
	let listed = mandates_app::list(&mandate_ports(&h, &grants), user).await.unwrap();
	assert_eq!(listed[0].usage.drawn_24h, Usdt::ZERO);
	// ...nor against the compartment, and a repeat of its key still answers the original.
	let repeat = charges_app::charge(
		&p,
		&client,
		ChargeRequest {
			idempotency_key: key(),
			..charge_request(&scope, &subject, "alloc:x", "1")
		},
	)
	.await;
	assert_eq!(refusal(repeat), LedgerRefusal::InsufficientFunds, "the compartment is empty now, and nothing stays promised");
}

async fn events_of(h: &Harness, client: &ClientId, grants: &Grants, scope: &ScopeId) -> Vec<piggybank_core::ports::scope_ledger::MoneyEventRecord> {
	ledger_app::pull(&ports(h, grants), client, Some(scope), 0, 500).await.unwrap().events
}

#[tokio::test]
async fn dust_is_refused_and_a_pending_charge_shrinks_what_others_may_spend() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user, subject) = user_with(&h, &scope, "100").await;
	grant_mandate(
		&h,
		&grants,
		user,
		mandate_request(&client, &scope, "100", "200", vec![MandateOperation::Charge, MandateOperation::Reserve]),
	)
	.await;
	let p = ports(&h, &grants);

	match charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "0.000000999999999999")).await {
		Err(TransferError::Domain(DomainError::Validation(_))) => {}
		other => panic!("dust is invalid: {other:?}"),
	}
	charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "0.000001"))
		.await
		.expect("the minimum is a charge");

	// 70 pending, not yet posted: a Reserve of 40 no longer fits the 30 left.
	charges_app::charge(&p, &client, charge_request(&scope, &subject, "alloc:x", "70")).await.unwrap();
	let reserve = ledger_app::ReserveRequest {
		scope: scope.clone(),
		auth_subject: subject.clone(),
		amount: usdt("40"),
		ttl_seconds: 0,
		subject_ref: subject_ref(),
		idempotency_key: key(),
	};
	assert_eq!(refusal(ledger_app::reserve(&p, &client, reserve).await), LedgerRefusal::InsufficientFunds);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn a_charge_waiting_past_its_window_is_refused_not_carried_out() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants, user, subject) = charging(&h, "50", "50", "100").await;
	let p = ports(&h, &grants);
	let request = charge_request(&scope, &subject, "alloc:x", "20");
	let stale = charges_app::charge(&p, &client, request.clone()).await.unwrap();
	sqlx::query("UPDATE scope_charges SET created_at = now() - interval '2 hours' WHERE id = $1")
		.bind(stale.charge.id().raw())
		.execute(&h.pool)
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let (state, reason) = charge_row(&h, &stale).await;
	assert_eq!((state.as_str(), reason.as_deref()), ("failed", Some("expired")));
	assert_eq!(compartment(&h, user, &scope).await.posted, usdt("50").base_units(), "nothing moved");
	assert!(events_of(&h, &client, &grants, &scope).await.iter().all(|e| e.kind != MoneyEventKind::Charged));

	// The key still answers the original charge, not a second one.
	let again = charges_app::charge(&p, &client, request).await.unwrap();
	assert_eq!(again.charge.id(), stale.charge.id());
	assert_eq!(again.state, MoveState::Failed);
}
