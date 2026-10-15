//! Holds on users' compartments and the money-event stream (`scope.v1.ScopeLedgerService`
//! Reserve/Capture/Void/GetHold, `scope.v1.MoneyEvents.Pull`) — real Postgres and TigerBeetle.
//! Every query of the hold store and the event log runs here: placing a hold under the
//! compartment's lock, capturing and voiding it under its row lock, answering its keys, the
//! relay's linked chains, the reaper's release of an expired hold, and a client's pull by cursor.
//!
//! Mandates are not granted by users yet, so the tests consent with [`Consent`]; production
//! wires `NoMandates` and the first test shows what that means. Each test works in scopes,
//! clients and users of its own.

use std::sync::Arc;

use async_trait::async_trait;
use domain::{
	balance::{LedgerAccountKey, TransferCode},
	error::DomainError,
	money::{Network, Usdt},
	scope_ledger::{ClientId, Grant, Grants, HoldState, LedgerRefusal, MandateOperation, MoneyEventKind, SubjectRef, TransferReason},
	scopes::{ScopeAccountLabel, ScopeId, ScopeState},
	users::UserId,
};
use piggybank_core::{
	application::scope_ledger::{self as ledger_app, CaptureRequest, NoMandates, ReserveRequest, ScopeLedgerPorts, TransferError, TransferRequest, VoidRequest},
	infrastructure::{compartments::PgCompartments, relay::Relay, scope_holds, scope_ledger::PgScopeLedger},
	ports::{
		ledger::{Ledger, LedgerBalance, LedgerTransfer},
		scope_ledger::{CaptureTarget, EventAccount, MandateBook, RecordedHold},
	},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

/// A user's consent as a test grants it: every hold is under one mandate, which lists
/// BOOK_FILL only when the test says so.
struct Consent {
	mandate: Uuid,
	book_fill: bool,
}

#[async_trait]
impl MandateBook for Consent {
	async fn authorize(&self, _: &ClientId, _: &ScopeId, _: UserId, operation: MandateOperation, _: Usdt) -> Result<Option<Uuid>, DomainError> {
		Ok((operation == MandateOperation::Reserve).then_some(self.mandate))
	}

	async fn lists(&self, mandate: Uuid, operation: MandateOperation) -> Result<bool, DomainError> {
		Ok(mandate == self.mandate && (operation != MandateOperation::BookFill || self.book_fill))
	}
}

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
	let ledger = common::seeded_ledger(&pool, "scope hold tests").await?;
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

fn ports<'a>(h: &'a Harness, grants: &'a Grants, mandates: &'a dyn MandateBook) -> ScopeLedgerPorts<'a> {
	ScopeLedgerPorts {
		grants,
		store: &h.store,
		holds: &h.store,
		mandates,
		events: &h.store,
		scopes: &h.scopes,
		ledger: h.ledger.as_ref(),
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

fn fresh_client() -> ClientId {
	ClientId::parse(&format!("c-{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap()
}

fn subject() -> SubjectRef {
	SubjectRef::parse(&format!("subscription:{}", Uuid::new_v4())).unwrap()
}

fn consent() -> Consent {
	Consent {
		mandate: Uuid::new_v4(),
		book_fill: false,
	}
}

/// A scope of this test's own, active, default policy, banking's fee collector open.
async fn own_scope(h: &Harness) -> ScopeId {
	let id = ScopeId::parse(&format!("h_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	sqlx::query("INSERT INTO scopes (id, display_name) VALUES ($1, 'Hold scope')")
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

/// A user with `amount` in their compartment in `scope`, straight on the ledger (custody grows
/// with it, so the cash invariant holds). Returns the user and their `auth_subject`.
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
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: LedgerAccountKey::ScopeClaim(user, scope.clone()),
			amount: usdt(amount).base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.unwrap();
	(user, auth_subject)
}

async fn compartment(h: &Harness, user: UserId, scope: &ScopeId) -> LedgerBalance {
	h.ledger.balance(&LedgerAccountKey::ScopeClaim(user, scope.clone())).await.unwrap()
}

async fn account(h: &Harness, scope: &ScopeId, raw: &str) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(&LedgerAccountKey::ScopeAccount(scope.clone(), label(raw))).await.unwrap().posted)
}

/// A client granted `alloc:` with the compartments flag in a fresh scope, owning `alloc:x`.
async fn holder(h: &Harness) -> (ClientId, ScopeId, Grants) {
	let client = fresh_client();
	let scope = own_scope(h).await;
	let grants = Grants::new(vec![Grant {
		client: client.clone(),
		scope: scope.clone(),
		label_prefix: "alloc:".into(),
		compartments: true,
	}])
	.unwrap();
	ledger_app::open_account(&ports(h, &grants, &NoMandates), &client, &scope, &label("alloc:x"), &key())
		.await
		.unwrap();
	(client, scope, grants)
}

fn reserve_request(scope: &ScopeId, auth_subject: &str, amount: &str, idempotency_key: &str) -> ReserveRequest {
	ReserveRequest {
		scope: scope.clone(),
		auth_subject: auth_subject.to_owned(),
		amount: usdt(amount),
		ttl_seconds: 0,
		subject_ref: SubjectRef::parse("subscription:4f0c2d1e-0000-4000-8000-0123456789ab").unwrap(),
		idempotency_key: idempotency_key.to_owned(),
	}
}

fn capture_request(scope: &ScopeId, hold: &RecordedHold, to: CaptureTarget, amount: Option<&str>) -> CaptureRequest {
	CaptureRequest {
		scope: scope.clone(),
		hold_id: hold.hold.id().raw(),
		to,
		amount: amount.map(usdt),
		subject_ref: subject(),
		idempotency_key: key(),
	}
}

fn void_request(scope: &ScopeId, hold: &RecordedHold) -> VoidRequest {
	VoidRequest {
		scope: scope.clone(),
		hold_id: hold.hold.id().raw(),
		subject_ref: subject(),
		idempotency_key: key(),
	}
}

fn refusal<T: std::fmt::Debug>(result: Result<T, TransferError>) -> LedgerRefusal {
	match result {
		Err(TransferError::Refused(refusal)) => refusal,
		other => panic!("expected a refusal, got {other:?}"),
	}
}

fn domain_err<T: std::fmt::Debug>(result: Result<T, TransferError>) -> DomainError {
	match result {
		Err(TransferError::Domain(err)) => err,
		other => panic!("expected a domain error, got {other:?}"),
	}
}

#[tokio::test]
async fn a_hold_needs_the_compartments_grant_a_known_scope_and_a_mandate() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (_, user) = user_with(&h, &scope, "10").await;
	let mandates = consent();

	// Production's book: nobody has consented to anything yet.
	let none = ports(&h, &grants, &NoMandates);
	assert_eq!(
		refusal(ledger_app::reserve(&none, &client, reserve_request(&scope, &user, "1", &key())).await),
		LedgerRefusal::MandateRequired
	);

	let p = ports(&h, &grants, &mandates);
	// An unknown user reads like one who never consented, so subjects cannot be probed.
	let stranger = format!("itest-{}", Uuid::new_v4());
	assert_eq!(
		refusal(ledger_app::reserve(&p, &client, reserve_request(&scope, &stranger, "1", &key())).await),
		LedgerRefusal::MandateRequired
	);
	for elsewhere in [ScopeId::main(), own_scope(&h).await] {
		assert_eq!(
			refusal(ledger_app::reserve(&p, &client, reserve_request(&elsewhere, &user, "1", &key())).await),
			LedgerRefusal::NotGranted
		);
	}
	let accounts_only = Grants::new(vec![Grant {
		client: client.clone(),
		scope: scope.clone(),
		label_prefix: "alloc:".into(),
		compartments: false,
	}])
	.unwrap();
	assert_eq!(
		refusal(ledger_app::reserve(&ports(&h, &accounts_only, &mandates), &client, reserve_request(&scope, &user, "1", &key())).await),
		LedgerRefusal::NotGranted,
		"accounts without the compartments flag never reach a user"
	);
	assert_eq!(
		refusal(ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "10.000000000000000001", &key())).await),
		LedgerRefusal::InsufficientFunds
	);
	assert!(
		sqlx::query_scalar::<_, i64>("SELECT count(*) FROM scope_holds WHERE scope = $1")
			.bind(scope.as_str())
			.fetch_one(&h.pool)
			.await
			.unwrap() == 0
	);
}

#[tokio::test]
async fn a_hold_locks_the_compartment_captures_in_parts_and_releases_the_rest() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user_id, user) = user_with(&h, &scope, "10").await;
	let mandates = consent();
	let p = ports(&h, &grants, &mandates);

	let k = key();
	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "6", &k)).await.unwrap();
	assert_eq!((hold.hold.state(), hold.hold.remaining()), (HoldState::Active, usdt("6")));
	assert_eq!(hold.hold.terms().mandate_id, mandates.mandate);
	// Before the relay applied it, the hold is already spoken for.
	assert_eq!(
		refusal(ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "4.5", &key())).await),
		LedgerRefusal::InsufficientFunds
	);
	let repeat = ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "6", &k)).await.unwrap();
	assert_eq!(repeat.hold.id(), hold.hold.id(), "a repeat is the original hold");
	assert!(matches!(
		domain_err(ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "5", &k)).await),
		DomainError::Conflict(_)
	));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let locked = compartment(&h, user_id, &scope).await;
	assert_eq!((locked.posted, locked.locked), (usdt("10").base_units(), usdt("6").base_units()));

	let partial = ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), Some("2")))
		.await
		.unwrap();
	assert_eq!((partial.hold.progress().captured, partial.hold.remaining()), (usdt("2"), usdt("4")));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let after = compartment(&h, user_id, &scope).await;
	assert_eq!((after.posted, after.locked), (usdt("8").base_units(), usdt("4").base_units()), "2 left, 4 reserved anew");
	assert_eq!(account(&h, &scope, "alloc:x").await, usdt("2"));
	assert_eq!(
		refusal(ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), Some("4.1"))).await),
		LedgerRefusal::InsufficientFunds
	);
	assert_eq!(
		refusal(ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("fee")), Some("1"))).await),
		LedgerRefusal::NotAccountOwner,
		"banking's collector is never a capture's destination"
	);

	let voided = ledger_app::void(&p, &client, void_request(&scope, &hold)).await.unwrap();
	assert_eq!((voided.hold.state(), voided.hold.remaining()), (HoldState::Voided, Usdt::ZERO));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let released = compartment(&h, user_id, &scope).await;
	assert_eq!((released.posted, released.locked), (usdt("8").base_units(), 0));
	let again = ledger_app::void(&p, &client, void_request(&scope, &hold)).await.unwrap();
	assert_eq!(again.hold.progress().version, voided.hold.progress().version, "a void of a voided hold changes nothing");
	assert_eq!(
		refusal(ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), None)).await),
		LedgerRefusal::HoldNotActive
	);
	let read = ledger_app::get_hold(&p, &client, &scope, hold.hold.id().raw()).await.unwrap();
	assert_eq!((read.hold.state(), read.hold.progress().captured, read.auth_subject), (HoldState::Voided, usdt("2"), user));
}

#[tokio::test]
async fn a_hold_is_its_placers_alone() {
	let Some(h) = harness().await else { return };
	let (client, scope, mut grants_list) = {
		let (client, scope, _) = holder(&h).await;
		let list = vec![Grant {
			client: client.clone(),
			scope: scope.clone(),
			label_prefix: "alloc:".into(),
			compartments: true,
		}];
		(client, scope, list)
	};
	let other = fresh_client();
	grants_list.push(Grant {
		client: other.clone(),
		scope: scope.clone(),
		label_prefix: "panel:".into(),
		compartments: true,
	});
	let grants = Grants::new(grants_list).unwrap();
	let (_, user) = user_with(&h, &scope, "5").await;
	let mandates = consent();
	let p = ports(&h, &grants, &mandates);
	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "1", &key())).await.unwrap();

	let not_found = |err: DomainError| assert!(matches!(err, DomainError::NotFound { entity: "hold", .. }), "{err:?}");
	not_found(domain_err(ledger_app::get_hold(&p, &other, &scope, hold.hold.id().raw()).await));
	not_found(domain_err(ledger_app::void(&p, &other, void_request(&scope, &hold)).await));
	ledger_app::open_account(&p, &other, &scope, &label("panel:a"), &key()).await.unwrap();
	not_found(domain_err(
		ledger_app::capture(&p, &other, capture_request(&scope, &hold, CaptureTarget::Account(label("panel:a")), None)).await,
	));
	not_found(domain_err(ledger_app::get_hold(&p, &client, &scope, Uuid::new_v4()).await));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn a_capture_into_another_user_needs_book_fill_and_a_holder_not_frozen() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (buyer_id, buyer) = user_with(&h, &scope, "10").await;
	let (seller_id, seller) = user_with(&h, &scope, "0.000000000000000001").await;
	let mandates = Consent {
		mandate: Uuid::new_v4(),
		book_fill: true,
	};
	let p = ports(&h, &grants, &mandates);
	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &buyer, "3", &key())).await.unwrap();

	let without = consent();
	assert_eq!(
		refusal(
			ledger_app::capture(
				&ports(&h, &grants, &without),
				&client,
				capture_request(&scope, &hold, CaptureTarget::User(seller.clone()), Some("1"))
			)
			.await
		),
		LedgerRefusal::MandateRequired,
		"the hold's own mandate decides, and this book does not know it"
	);
	assert!(matches!(
		domain_err(ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::User(buyer.clone()), Some("1"))).await),
		DomainError::Validation(_)
	));
	ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::User(seller.clone()), Some("1")))
		.await
		.unwrap();

	sqlx::query("UPDATE users SET frozen = TRUE WHERE id = $1").bind(buyer_id.raw()).execute(&h.pool).await.unwrap();
	assert_eq!(
		refusal(ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::User(seller.clone()), Some("1"))).await),
		LedgerRefusal::UserFrozen
	);
	// Into the client's own account a frozen holder's hold still settles: that is the
	// purchase the user already consented to, not a payment to someone else.
	ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), Some("1")))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(compartment(&h, seller_id, &scope).await.posted, usdt("1.000000000000000001").base_units());
	let buyer_after = compartment(&h, buyer_id, &scope).await;
	assert_eq!((buyer_after.posted, buyer_after.locked), (usdt("8").base_units(), usdt("1").base_units()));
}

#[tokio::test]
async fn a_frozen_scope_or_user_takes_no_new_hold() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user_id, user) = user_with(&h, &scope, "5").await;
	let mandates = consent();
	let p = ports(&h, &grants, &mandates);
	sqlx::query("UPDATE users SET frozen = TRUE WHERE id = $1").bind(user_id.raw()).execute(&h.pool).await.unwrap();
	assert_eq!(
		refusal(ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "1", &key())).await),
		LedgerRefusal::UserFrozen
	);
	sqlx::query("UPDATE users SET frozen = FALSE WHERE id = $1").bind(user_id.raw()).execute(&h.pool).await.unwrap();
	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "1", &key())).await.unwrap();
	sqlx::query("UPDATE scopes SET state = $2 WHERE id = $1")
		.bind(scope.as_str())
		.bind(ScopeState::Frozen.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	assert_eq!(
		refusal(ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "1", &key())).await),
		LedgerRefusal::ScopeFrozen
	);
	// A release is a credit back to the user; a freeze does not keep money locked.
	ledger_app::void(&p, &client, void_request(&scope, &hold)).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(compartment(&h, user_id, &scope).await.locked, 0);
}

#[tokio::test]
async fn a_hold_past_its_ttl_is_released_by_the_reaper() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user_id, user) = user_with(&h, &scope, "5").await;
	let mandates = consent();
	let p = ports(&h, &grants, &mandates);
	let hold = ledger_app::reserve(
		&p,
		&client,
		ReserveRequest {
			ttl_seconds: 600,
			..reserve_request(&scope, &user, "2", &key())
		},
	)
	.await
	.unwrap();
	assert!(hold.hold.terms().expires_at.is_some_and(|at| at > hold.created_at));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	sqlx::query("UPDATE scope_holds SET expires_at = now() - interval '1 second' WHERE id = $1")
		.bind(hold.hold.id().raw())
		.execute(&h.pool)
		.await
		.unwrap();
	assert_eq!(
		refusal(ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), None)).await),
		LedgerRefusal::HoldNotActive,
		"due is not capturable, even before the reaper got to it"
	);
	assert!(scope_holds::expire_due_holds(&h.pool).await.unwrap() >= 1);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let read = ledger_app::get_hold(&p, &client, &scope, hold.hold.id().raw()).await.unwrap();
	assert_eq!(read.hold.state(), HoldState::Expired);
	assert_eq!(compartment(&h, user_id, &scope).await.locked, 0);
}

#[tokio::test]
async fn money_events_show_each_client_its_own_by_cursor() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (_, user) = user_with(&h, &scope, "10").await;
	let mandates = consent();
	let p = ports(&h, &grants, &mandates);
	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "4", &key())).await.unwrap();
	ledger_app::capture(&p, &client, capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), Some("3")))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	ledger_app::transfer(
		&p,
		&client,
		TransferRequest {
			scope: scope.clone(),
			from: label("alloc:x"),
			to: ScopeAccountLabel::fee(),
			amount: usdt("1"),
			reason: TransferReason::FeeSweep,
			subject_ref: subject(),
			memo: String::new(),
			idempotency_key: key(),
		},
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let all = ledger_app::pull(&p, &client, Some(&scope), 0, 0).await.unwrap();
	let kinds: Vec<MoneyEventKind> = all.events.iter().map(|event| event.kind).collect();
	assert_eq!(
		kinds,
		[
			MoneyEventKind::AccountOpened,
			MoneyEventKind::HoldPlaced,
			MoneyEventKind::HoldCaptured,
			MoneyEventKind::Transferred
		]
	);
	assert!(all.events.windows(2).all(|pair| pair[0].position < pair[1].position));
	assert_eq!(all.next_position, all.events[3].position);
	let captured = &all.events[2];
	assert_eq!((captured.ref_id.as_str(), captured.aggregate_version), (hold.hold.id().to_string().as_str(), 2));
	assert_eq!((captured.amount, captured.hold_remaining), (Some(usdt("3")), Some(usdt("1"))));
	assert_eq!(captured.to, Some(EventAccount::Account(label("alloc:x"))));
	assert_eq!(captured.auth_subject, user);
	let transferred = &all.events[3];
	let shown: Vec<&str> = transferred.balances.iter().map(|entry| entry.label.as_str()).collect();
	assert_eq!(shown, ["alloc:x"], "banking's fee collector never shows in a client's balances");
	assert_eq!(transferred.balances[0].balance, usdt("2"));

	let page = ledger_app::pull(&p, &client, None, all.events[0].position, 2).await.unwrap();
	assert_eq!(
		page.events.iter().map(|event| event.kind).collect::<Vec<_>>(),
		[MoneyEventKind::HoldPlaced, MoneyEventKind::HoldCaptured]
	);
	assert_eq!(page.next_position, all.events[2].position);
	let rest = ledger_app::pull(&p, &client, None, page.next_position, 10).await.unwrap();
	assert_eq!(rest.events.len(), 1);
	let past_end = ledger_app::pull(&p, &client, None, rest.next_position, 10).await.unwrap();
	assert!(past_end.events.is_empty());
	assert_eq!(past_end.next_position, rest.next_position, "nothing scanned: the cursor stays");

	let (stranger, stranger_scope, stranger_grants) = holder(&h).await;
	let theirs = ports(&h, &stranger_grants, &mandates);
	assert_eq!(refusal(ledger_app::pull(&theirs, &stranger, Some(&scope), 0, 10).await), LedgerRefusal::NotGranted);
	assert_eq!(refusal(ledger_app::pull(&theirs, &stranger, Some(&ScopeId::main()), 0, 10).await), LedgerRefusal::NotGranted);
	let own = ledger_app::pull(&theirs, &stranger, None, 0, 10).await.unwrap();
	assert!(own.events.iter().all(|event| event.scope == stranger_scope), "another client's scope never leaks into the stream");
}
