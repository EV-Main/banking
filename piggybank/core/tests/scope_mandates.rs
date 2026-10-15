//! Users' mandates and what a hold may draw under them — real Postgres and TigerBeetle. Every
//! query of the mandate store runs here: granting (with its key, its replacement of the current
//! mandate and its money event), revoking (voiding the client's holds in the same
//! transaction), listing with usage; and the hold store's reads under its locks — the current
//! mandate `FOR UPDATE`, the client's usage, a mandate's book fills. Also banking#514: a
//! reservation the ledger refused voids its hold, a money event that cannot be written parks
//! its row alone, Capture against Capture on one hold, and `hold_clearing` against the holds.
//!
//! Each test works in scopes, clients and users of its own.

use std::sync::Arc;

use domain::{
	balance::{LedgerAccountKey, TransferCode},
	error::DomainError,
	money::{Network, Usdt},
	scope_ledger::{ClientId, Grant, Grants, HoldState, LedgerRefusal, MandateOperation, MoneyEventKind, SubjectRef},
	scopes::{ScopeAccountLabel, ScopeId},
	users::UserId,
};
use piggybank_core::{
	application::{
		scope_ledger::{self as ledger_app, CaptureRequest, ReserveRequest, ScopeLedgerPorts, TransferError, VoidRequest},
		scope_mandates::{self as mandates_app, GrantRequest, MandatePorts},
	},
	infrastructure::{allocations::PgAllocations, compartments::PgCompartments, outbox, reconciliation::Reconciliation, relay::Relay, scope_ledger::PgScopeLedger},
	ports::{
		ledger::{Ledger, LedgerBalance, LedgerTransfer},
		scope_ledger::{CaptureTarget, RecordedHold},
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

async fn now(h: &Harness) -> i64 {
	sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::BIGINT").fetch_one(&h.pool).await.unwrap()
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

fn grant_request(client: &ClientId, scope: &ScopeId, per_op: &str, per_24h: &str, outstanding: &str) -> GrantRequest {
	GrantRequest {
		scope: scope.clone(),
		client: client.clone(),
		cap_per_op: usdt(per_op),
		cap_24h: usdt(per_24h),
		cap_outstanding: Some(usdt(outstanding)),
		cap_book_fill_24h: None,
		operations: vec![MandateOperation::Reserve],
		expires_at: None,
		idempotency_key: key(),
	}
}

async fn grant_mandate(h: &Harness, grants: &Grants, user: UserId, request: GrantRequest) -> RecordedMandate {
	mandates_app::grant(&mandate_ports(h, grants), user, request).await.unwrap()
}

fn reserve_request(scope: &ScopeId, auth_subject: &str, amount: &str) -> ReserveRequest {
	ReserveRequest {
		scope: scope.clone(),
		auth_subject: auth_subject.to_owned(),
		amount: usdt(amount),
		ttl_seconds: 0,
		subject_ref: subject_ref(),
		idempotency_key: key(),
	}
}

fn capture_request(scope: &ScopeId, hold: &RecordedHold, to: CaptureTarget, amount: Option<&str>) -> CaptureRequest {
	CaptureRequest {
		scope: scope.clone(),
		hold_id: hold.hold.id().raw(),
		to,
		amount: amount.map(usdt),
		subject_ref: subject_ref(),
		idempotency_key: key(),
	}
}

fn refusal<T: std::fmt::Debug>(result: Result<T, TransferError>) -> LedgerRefusal {
	match result {
		Err(TransferError::Refused(refusal)) => refusal,
		other => panic!("expected a refusal, got {other:?}"),
	}
}

async fn hold_state(h: &Harness, hold: &RecordedHold) -> (String, String, bool) {
	sqlx::query_as("SELECT state, captured, ledger_refused_at IS NOT NULL FROM scope_holds WHERE id = $1")
		.bind(hold.hold.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

/// The outbox rows of one hold, oldest first: `(seq, parked, dispatched, last_error)`.
async fn hold_rows(h: &Harness, hold: &RecordedHold) -> Vec<(i64, bool, bool, Option<String>)> {
	sqlx::query_as("SELECT seq, parked_at IS NOT NULL, dispatched_at IS NOT NULL, last_error FROM outbox WHERE aggregate_id = $1 ORDER BY seq")
		.bind(hold.hold.id().raw())
		.fetch_all(&h.pool)
		.await
		.unwrap()
}

#[tokio::test]
async fn a_mandate_is_granted_only_to_a_client_banking_lets_near_compartments() {
	let Some(h) = harness().await else { return };
	let (client, scope, _) = holder(&h).await;
	let (user, _) = user_with(&h, &scope, "1").await;
	let accounts_only = fresh_client();
	let unknown_scope = ScopeId::parse(&format!("m_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	let grants = Grants::new(vec![
		grant(&client, &scope, "alloc:", true),
		grant(&accounts_only, &scope, "panel:", false),
		grant(&client, &unknown_scope, "alloc:", true),
	])
	.unwrap();
	let p = mandate_ports(&h, &grants);
	for (request, why) in [
		(grant_request(&accounts_only, &scope, "1", "1", "1"), "no compartments flag"),
		(grant_request(&fresh_client(), &scope, "1", "1", "1"), "no grant at all"),
		(grant_request(&client, &ScopeId::main(), "1", "1", "1"), "main"),
		(grant_request(&client, &scope, "2", "1", "2"), "cap_24h below cap_per_op"),
	] {
		let err = mandates_app::grant(&p, user, request).await.expect_err(why);
		assert!(matches!(err, DomainError::Validation(_)), "{why}: {err:?}");
	}
	let err = mandates_app::grant(&p, user, grant_request(&client, &unknown_scope, "1", "1", "1")).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { entity: "scope", .. }), "{err:?}");
	let err = mandates_app::grant(
		&p,
		user,
		GrantRequest {
			expires_at: Some(now(&h).await - 1),
			..grant_request(&client, &scope, "1", "1", "1")
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "a mandate over before it starts: {err:?}");

	let first = grant_request(&client, &scope, "5", "10", "10");
	let granted = grant_mandate(&h, &grants, user, first.clone()).await;
	let repeat = grant_mandate(&h, &grants, user, first.clone()).await;
	assert_eq!(repeat.mandate.id, granted.mandate.id, "a repeat of the key is the original mandate");
	let err = mandates_app::grant(
		&p,
		user,
		GrantRequest {
			cap_per_op: usdt("6"),
			..first.clone()
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");

	let replacement = grant_mandate(&h, &grants, user, grant_request(&client, &scope, "1", "2", "2")).await;
	let live = mandates_app::list(&p, user).await.unwrap();
	assert_eq!(live.iter().map(|m| m.mandate.id).collect::<Vec<_>>(), [replacement.mandate.id], "granting again replaces");
	assert!(
		grant_mandate(&h, &grants, user, first).await.mandate.replaced_at.is_some(),
		"the original key still answers, with the mandate as it stands now"
	);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn caps_hold_under_a_race_and_over_a_rolling_day() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user, subject) = user_with(&h, &scope, "100").await;
	let p = ports(&h, &grants);
	grant_mandate(&h, &grants, user, grant_request(&client, &scope, "3", "100", "6")).await;
	assert_eq!(
		refusal(ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "3.000000000000000001")).await),
		LedgerRefusal::MandateRequired,
		"past cap_per_op"
	);
	// Four holds at once against room for two: the compartment's lock and the mandate's row
	// lock let exactly two through.
	let (a, b, c, d) = tokio::join!(
		ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "3")),
		ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "3")),
		ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "3")),
		ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "3")),
	);
	let results = [a, b, c, d];
	let placed: Vec<&RecordedHold> = results.iter().filter_map(|result| result.as_ref().ok()).collect();
	assert_eq!(placed.len(), 2, "cap_outstanding 6 admits two holds of 3: {results:?}");
	for result in results.iter().filter(|result| result.is_err()) {
		assert!(matches!(result, Err(TransferError::Refused(LedgerRefusal::MandateRequired))), "{result:?}");
	}
	// Captured money no longer stands: room for a third.
	ledger_app::capture(&p, &client, capture_request(&scope, placed[0], CaptureTarget::Account(label("alloc:x")), None))
		.await
		.unwrap();
	ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "3")).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// The rolling 24 h, on a second client whose mandate caps the day, not what stands.
	let other = fresh_client();
	let both = Grants::new(vec![grant(&client, &scope, "alloc:", true), grant(&other, &scope, "panel:", true)]).unwrap();
	let q = ports(&h, &both);
	grant_mandate(&h, &both, user, grant_request(&other, &scope, "3", "6", "9")).await;
	let (a, b, c) = tokio::join!(
		ledger_app::reserve(&q, &other, reserve_request(&scope, &subject, "3")),
		ledger_app::reserve(&q, &other, reserve_request(&scope, &subject, "3")),
		ledger_app::reserve(&q, &other, reserve_request(&scope, &subject, "3")),
	);
	let day = [a, b, c];
	assert_eq!(day.iter().filter(|result| result.is_ok()).count(), 2, "cap_24h 6 admits two holds of 3: {day:?}");
	let voided = day.iter().find_map(|result| result.as_ref().ok()).unwrap();
	ledger_app::void(
		&q,
		&other,
		VoidRequest {
			scope: scope.clone(),
			hold_id: voided.hold.id().raw(),
			subject_ref: subject_ref(),
			idempotency_key: key(),
		},
	)
	.await
	.unwrap();
	ledger_app::reserve(&q, &other, reserve_request(&scope, &subject, "3"))
		.await
		.expect("a released hold no longer counts against the day");
	assert_eq!(
		refusal(ledger_app::reserve(&q, &other, reserve_request(&scope, &subject, "3")).await),
		LedgerRefusal::MandateRequired
	);
	// A day later the window has room again, but a hold without TTL still stands against
	// cap_outstanding: two of 3 standing, one more of 3 fits 9, a fourth does not.
	sqlx::query("UPDATE scope_holds SET created_at = created_at - interval '25 hours' WHERE client_id = $1")
		.bind(other.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	ledger_app::reserve(&q, &other, reserve_request(&scope, &subject, "3")).await.unwrap();
	assert_eq!(
		refusal(ledger_app::reserve(&q, &other, reserve_request(&scope, &subject, "3")).await),
		LedgerRefusal::MandateRequired,
		"what stands is capped whatever its age"
	);
	let listed = mandates_app::list(&mandate_ports(&h, &both), user).await.unwrap();
	let shown = listed.iter().find(|m| m.mandate.terms.client == other).unwrap();
	assert_eq!((shown.usage.drawn_24h, shown.usage.outstanding), (usdt("3"), usdt("9")));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn a_hold_is_drawn_only_on_the_callers_own_mandate_in_its_own_scope() {
	let Some(h) = harness().await else { return };
	let (client, scope, _) = holder(&h).await;
	let other = fresh_client();
	let elsewhere = own_scope(&h).await;
	let grants = Grants::new(vec![
		grant(&client, &scope, "alloc:", true),
		grant(&other, &scope, "panel:", true),
		grant(&client, &elsewhere, "alloc:", true),
	])
	.unwrap();
	let (user, subject) = user_with(&h, &scope, "10").await;
	move_on_ledger(&h, LedgerAccountKey::CryptoWallet(Network::Bep20), LedgerAccountKey::ScopeClaim(user, elsewhere.clone()), "10").await;
	grant_mandate(&h, &grants, user, grant_request(&client, &scope, "5", "5", "5")).await;
	let p = ports(&h, &grants);
	assert_eq!(
		refusal(ledger_app::reserve(&p, &other, reserve_request(&scope, &subject, "1")).await),
		LedgerRefusal::MandateRequired,
		"another service of the same scope"
	);
	assert_eq!(
		refusal(ledger_app::reserve(&p, &client, reserve_request(&elsewhere, &subject, "1")).await),
		LedgerRefusal::MandateRequired,
		"the same service in another scope"
	);
	ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "1")).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn an_expiring_mandate_bounds_its_holds_and_then_ends() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user, subject) = user_with(&h, &scope, "10").await;
	let until = now(&h).await + 3600;
	let mandate = grant_mandate(
		&h,
		&grants,
		user,
		GrantRequest {
			expires_at: Some(until),
			..grant_request(&client, &scope, "5", "10", "10")
		},
	)
	.await;
	let p = ports(&h, &grants);
	let open = ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "1")).await.unwrap();
	assert_eq!(open.hold.terms().expires_at, Some(until), "no TTL is the mandate's expiry");
	let long = ledger_app::reserve(
		&p,
		&client,
		ReserveRequest {
			ttl_seconds: 7200,
			..reserve_request(&scope, &subject, "1")
		},
	)
	.await
	.unwrap();
	assert_eq!(long.hold.terms().expires_at, Some(until), "a TTL past the mandate is cut to it");
	let short = ledger_app::reserve(
		&p,
		&client,
		ReserveRequest {
			ttl_seconds: 60,
			..reserve_request(&scope, &subject, "1")
		},
	)
	.await
	.unwrap();
	assert!(short.hold.terms().expires_at.is_some_and(|at| at < until));
	sqlx::query("UPDATE scope_mandates SET expires_at = now() - interval '1 second' WHERE id = $1")
		.bind(mandate.mandate.id)
		.execute(&h.pool)
		.await
		.unwrap();
	assert_eq!(
		refusal(ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "1")).await),
		LedgerRefusal::MandateRequired
	);
	assert!(mandates_app::list(&mandate_ports(&h, &grants), user).await.unwrap().is_empty(), "an expired mandate is not live");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn a_revoke_ends_the_mandate_and_voids_the_clients_holds_at_once() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user, subject) = user_with(&h, &scope, "10").await;
	let p = ports(&h, &grants);
	let mp = mandate_ports(&h, &grants);
	let old = grant_mandate(&h, &grants, user, grant_request(&client, &scope, "5", "10", "10")).await;
	let under_old = ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "2")).await.unwrap();
	let current = grant_mandate(&h, &grants, user, grant_request(&client, &scope, "5", "10", "10")).await;
	let under_current = ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "3")).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(compartment(&h, user, &scope).await.locked, usdt("5").base_units());

	let stranger = user_with(&h, &scope, "0.000000000000000001").await.0;
	let err = mandates_app::revoke(&mp, stranger, current.mandate.id).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { entity: "mandate", .. }), "another user's mandate: {err:?}");
	let replaced = mandates_app::revoke(&mp, user, old.mandate.id).await.unwrap();
	assert!(replaced.mandate.revoked_at.is_none(), "a replaced mandate is returned as it is");

	let revoked = mandates_app::revoke(&mp, user, current.mandate.id).await.unwrap();
	assert!(revoked.mandate.revoked_at.is_some());
	for hold in [&under_old, &under_current] {
		assert_eq!(hold_state(&h, hold).await.0, "voided", "every hold of the client, whichever mandate");
	}
	assert_eq!(
		refusal(ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "1")).await),
		LedgerRefusal::MandateRequired
	);
	let again = mandates_app::revoke(&mp, user, current.mandate.id).await.unwrap();
	assert_eq!(again.mandate.revoked_at, revoked.mandate.revoked_at, "a second revoke changes nothing");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(compartment(&h, user, &scope).await.locked, 0, "the money is back with the user");
	let events = ledger_app::pull(&p, &client, Some(&scope), 0, 100).await.unwrap().events;
	let tail: Vec<MoneyEventKind> = events.iter().rev().take(3).rev().map(|event| event.kind).collect();
	assert_eq!(tail[0], MoneyEventKind::MandateRevoked);
	assert_eq!(tail[1..], [MoneyEventKind::HoldVoided, MoneyEventKind::HoldVoided]);
}

#[tokio::test]
async fn a_book_fill_is_capped_by_the_holds_mandate_over_a_day() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (buyer, buyer_subject) = user_with(&h, &scope, "10").await;
	let (_, seller) = user_with(&h, &scope, "0.000000000000000001").await;
	grant_mandate(
		&h,
		&grants,
		buyer,
		GrantRequest {
			cap_book_fill_24h: Some(usdt("2")),
			operations: vec![MandateOperation::Reserve, MandateOperation::BookFill],
			..grant_request(&client, &scope, "5", "10", "10")
		},
	)
	.await;
	let p = ports(&h, &grants);
	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &buyer_subject, "5")).await.unwrap();
	ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::User(seller.clone()), Some("1.5")))
		.await
		.unwrap();
	assert_eq!(
		refusal(ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::User(seller.clone()), Some("1"))).await),
		LedgerRefusal::MandateRequired,
		"1.5 + 1 is past the day's BOOK_FILL cap of 2"
	);
	ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), Some("1")))
		.await
		.expect("into the client's own account the BOOK_FILL cap does not apply");
	sqlx::query("UPDATE scope_book_fills SET created_at = created_at - interval '25 hours' WHERE hold_id = $1")
		.bind(hold.hold.id().raw())
		.execute(&h.pool)
		.await
		.unwrap();
	ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::User(seller), Some("2")))
		.await
		.expect("a day later the cap has room again");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn two_captures_of_one_hold_cannot_both_take_the_rest() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user, subject) = user_with(&h, &scope, "10").await;
	grant_mandate(&h, &grants, user, grant_request(&client, &scope, "5", "10", "10")).await;
	let p = ports(&h, &grants);
	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "5")).await.unwrap();
	let to = || CaptureTarget::Account(label("alloc:x"));
	let (a, b) = tokio::join!(
		ledger_app::capture(&p, &client, capture_request(&scope, &hold, to(), Some("3"))),
		ledger_app::capture(&p, &client, capture_request(&scope, &hold, to(), Some("3"))),
	);
	let outcomes = [a, b];
	assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1, "{outcomes:?}");
	let loser = outcomes.into_iter().find(|result| result.is_err()).unwrap();
	assert_eq!(refusal(loser), LedgerRefusal::InsufficientFunds, "the second sees only 2 left");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let after = compartment(&h, user, &scope).await;
	assert_eq!((after.posted, after.locked), (usdt("7").base_units(), usdt("2").base_units()));
}

/// banking#514, 1: the ledger refuses the reservation — the compartment was emptied under it —
/// so the hold must stop answering captures, in the transaction that parks it, and stay
/// unplaced if an operator unparks it later.
#[tokio::test]
async fn a_reservation_the_ledger_refused_voids_its_hold() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user, subject) = user_with(&h, &scope, "5").await;
	grant_mandate(&h, &grants, user, grant_request(&client, &scope, "5", "10", "10")).await;
	let p = ports(&h, &grants);
	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "4")).await.unwrap();
	let early = ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), Some("1")))
		.await
		.unwrap();
	assert_eq!(early.hold.progress().captured, usdt("1"), "recorded, not yet on the ledger");
	move_on_ledger(&h, LedgerAccountKey::ScopeClaim(user, scope.clone()), LedgerAccountKey::CryptoWallet(Network::Bep20), "5").await;
	h.relay.drain().await;

	let rows = hold_rows(&h, &hold).await;
	assert!(
		rows.iter().all(|(_, parked, dispatched, _)| *parked && !dispatched),
		"the reserve and the capture behind it park: {rows:?}"
	);
	assert_eq!(hold_state(&h, &hold).await, ("voided".to_owned(), "0".to_owned(), true));
	assert_eq!(
		refusal(ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), None)).await),
		LedgerRefusal::HoldNotActive
	);
	let read = ledger_app::get_hold(&p, &client, &scope, hold.hold.id().raw()).await.unwrap();
	assert_eq!((read.hold.state(), read.hold.progress().captured), (HoldState::Voided, Usdt::ZERO));

	// The money comes back and an operator unparks the reservation: it is a refused intent,
	// not one owed — nothing is placed.
	move_on_ledger(&h, LedgerAccountKey::CryptoWallet(Network::Bep20), LedgerAccountKey::ScopeClaim(user, scope.clone()), "5").await;
	assert!(outbox::unpark(&h.pool, rows[0].0).await.unwrap());
	h.relay.drain().await;
	assert!(hold_rows(&h, &hold).await[0].2, "dispatched as a no-op");
	assert_eq!(compartment(&h, user, &scope).await.locked, 0);
	let placed = ledger_app::pull(&p, &client, Some(&scope), 0, 100)
		.await
		.unwrap()
		.events
		.into_iter()
		.filter(|event| event.ref_id == hold.hold.id().to_string())
		.count();
	assert_eq!(placed, 0, "the service never hears of a hold the ledger never placed");
}

/// banking#514, 2: a money event that can never be written parks its own row after bounded
/// retries — the relay keeps applying what is behind it — and an unpark once the cause is gone
/// writes the missing event.
#[tokio::test]
async fn a_money_event_that_cannot_be_written_parks_alone() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user, subject) = user_with(&h, &scope, "10").await;
	grant_mandate(&h, &grants, user, grant_request(&client, &scope, "5", "10", "10")).await;
	let p = ports(&h, &grants);
	let broken = ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "1")).await.unwrap();
	let behind = ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "2")).await.unwrap();
	// The hold's row is what its money event is written from; out of the way, the event cannot
	// be. One attempt short of the bound, so this drain is the last try.
	let hidden = Uuid::new_v4();
	sqlx::query("UPDATE scope_holds SET id = $2 WHERE id = $1")
		.bind(broken.hold.id().raw())
		.bind(hidden)
		.execute(&h.pool)
		.await
		.unwrap();
	let seq = hold_rows(&h, &broken).await[0].0;
	sqlx::query("UPDATE outbox SET attempts = 24 WHERE seq = $1").bind(seq).execute(&h.pool).await.unwrap();
	for _ in 0..3 {
		h.relay.drain().await;
	}
	let (_, parked, dispatched, last_error) = hold_rows(&h, &broken).await.remove(0);
	assert!(parked && !dispatched, "parked, not dispatched");
	assert!(last_error.as_deref().is_some_and(|reason| reason.starts_with("money event:")), "{last_error:?}");
	let compensated: bool = sqlx::query_scalar("SELECT compensated_at IS NOT NULL FROM outbox WHERE seq = $1")
		.bind(seq)
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert!(!compensated, "the ledger applied it whole; nothing is owed back");
	assert!(hold_rows(&h, &behind).await.iter().all(|(_, parked, dispatched, _)| *dispatched && !parked), "the relay went on");
	assert_eq!(compartment(&h, user, &scope).await.locked, usdt("3").base_units(), "both reservations are on the ledger");

	sqlx::query("UPDATE scope_holds SET id = $1 WHERE id = $2")
		.bind(broken.hold.id().raw())
		.bind(hidden)
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(outbox::unpark(&h.pool, seq).await.unwrap());
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(compartment(&h, user, &scope).await.locked, usdt("3").base_units(), "replayed, not placed twice");
	let placed = ledger_app::pull(&p, &client, Some(&scope), 0, 100)
		.await
		.unwrap()
		.events
		.into_iter()
		.filter(|event| event.ref_id == broken.hold.id().to_string() && event.kind == MoneyEventKind::HoldPlaced)
		.count();
	assert_eq!(placed, 1);
}

/// Checklist 14: what the ledger holds on `hold_clearing` is what the active holds say, and
/// nothing is posted to it.
#[tokio::test]
async fn hold_clearing_matches_the_active_holds() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user, subject) = user_with(&h, &scope, "10").await;
	grant_mandate(&h, &grants, user, grant_request(&client, &scope, "5", "10", "10")).await;
	let p = ports(&h, &grants);
	let recon = Reconciliation::new(h.pool.clone(), h.ledger.clone(), Arc::new(PgAllocations::new(h.pool.clone())));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let before = recon.scan().await.unwrap().hold_clearing.expect("compared on a quiet pass");
	assert!(before.holds(), "{before:?}");

	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &subject, "4")).await.unwrap();
	assert_eq!(recon.scan().await.unwrap().hold_clearing, None, "not compared while the hold is on its way");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), Some("1")))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let after = recon.scan().await.unwrap().hold_clearing.unwrap();
	assert!(after.holds(), "{after:?}");
	assert_eq!(after.expected - before.expected, usdt("3").base_units());

	// A reservation the ledger holds and Postgres does not know of is drift.
	sqlx::query("UPDATE scope_holds SET state = 'voided' WHERE id = $1")
		.bind(hold.hold.id().raw())
		.execute(&h.pool)
		.await
		.unwrap();
	let drifted = recon.scan().await.unwrap();
	assert!(!drifted.hold_clearing.unwrap().holds() && !drifted.clean());
	sqlx::query("UPDATE scope_holds SET state = 'active' WHERE id = $1")
		.bind(hold.hold.id().raw())
		.execute(&h.pool)
		.await
		.unwrap();
}
