//! Holds on users' compartments and the money-event stream (`scope.v1.ScopeLedgerService`
//! Reserve/Capture/Void/GetHold, `scope.v1.MoneyEvents.Pull`) — real Postgres and TigerBeetle.
//! Every query of the hold store and the event log runs here: placing a hold under the
//! compartment's lock, capturing and voiding it under its row lock, answering its keys, the
//! relay's linked chains, the reaper's release of an expired hold, and a client's pull by cursor.
//!
//! Users consent through real mandates ([`consent`]); the caps themselves are
//! `scope_mandates.rs`'s. Each test works in scopes, clients and users of its own.

use std::sync::Arc;

use domain::{
	balance::{LedgerAccountKey, TransferCode},
	error::DomainError,
	money::{Network, Usdt},
	scope_ledger::{ClientId, Grant, Grants, HoldState, IDEMPOTENCY_CONFLICT, LedgerRefusal, MandateOperation, MoneyEventKind, SubjectRef, TransferReason},
	scopes::{ScopeAccountLabel, ScopeId, ScopeState},
	users::UserId,
};
use piggybank_core::{
	application::{
		scope_ledger::{self as ledger_app, CaptureRequest, ListHoldsRequest, ReserveRequest, ScopeLedgerPorts, TransferError, TransferRequest, VoidRequest},
		scope_mandates::{self as mandates_app, GrantRequest, MandatePorts},
	},
	infrastructure::{compartments::PgCompartments, relay::Relay, scope_holds, scope_ledger::PgScopeLedger},
	ports::{
		ledger::{Ledger, LedgerBalance, LedgerTransfer},
		scope_ledger::{CaptureTarget, EventAccount, RecordedHold},
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

fn mandate_ports<'a>(h: &'a Harness, grants: &'a Grants) -> MandatePorts<'a> {
	MandatePorts {
		grants,
		store: &h.store,
		scopes: &h.scopes,
		relay: &h.notify,
	}
}

/// `user`'s mandate for `client` in `scope`, its caps out of the way: RESERVE, and BOOK_FILL
/// when `book_fill`.
async fn consent(h: &Harness, grants: &Grants, client: &ClientId, scope: &ScopeId, user: UserId, book_fill: bool) -> RecordedMandate {
	let mut operations = vec![MandateOperation::Reserve];
	if book_fill {
		operations.push(MandateOperation::BookFill);
	}
	mandates_app::grant(
		&mandate_ports(h, grants),
		user,
		GrantRequest {
			scope: scope.clone(),
			client: client.clone(),
			cap_per_op: usdt("1000"),
			cap_24h: usdt("1000"),
			cap_outstanding: None,
			cap_book_fill_24h: book_fill.then(|| usdt("1000")),
			operations,
			expires_at: None,
			idempotency_key: key(),
		},
	)
	.await
	.unwrap()
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
	ledger_app::open_account(&ports(h, &grants), &client, &scope, &label("alloc:x"), &key()).await.unwrap();
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
	let (user_id, user) = user_with(&h, &scope, "10").await;
	let p = ports(&h, &grants);

	// The user has consented to nothing yet.
	assert_eq!(
		refusal(ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "1", &key())).await),
		LedgerRefusal::MandateRequired
	);
	consent(&h, &grants, &client, &scope, user_id, false).await;
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
		refusal(ledger_app::reserve(&ports(&h, &accounts_only), &client, reserve_request(&scope, &user, "1", &key())).await),
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

/// `idempotency_conflict`: the same key under another term refuses, whichever call it is, and
/// leaves the hold as the first call made it.
#[tokio::test]
async fn a_key_reused_with_other_terms_is_refused_by_reserve_capture_and_void() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user_id, user) = user_with(&h, &scope, "10").await;
	let (_, neighbour) = user_with(&h, &scope, "10").await;
	consent(&h, &grants, &client, &scope, user_id, false).await;
	let p = ports(&h, &grants);
	let conflict = |result: Result<RecordedHold, TransferError>| match domain_err(result) {
		DomainError::Conflict(why) => assert_eq!(why, IDEMPOTENCY_CONFLICT),
		other => panic!("expected the idempotency conflict, got {other:?}"),
	};

	let k = key();
	let asked = reserve_request(&scope, &user, "6", &k);
	let hold = ledger_app::reserve(&p, &client, asked.clone()).await.unwrap();
	for other in [
		ReserveRequest { amount: usdt("5"), ..asked.clone() },
		ReserveRequest { ttl_seconds: 60, ..asked.clone() },
		ReserveRequest {
			auth_subject: neighbour.clone(),
			..asked.clone()
		},
		ReserveRequest {
			subject_ref: subject(),
			..asked.clone()
		},
	] {
		conflict(ledger_app::reserve(&p, &client, other).await);
	}
	assert_eq!(ledger_app::reserve(&p, &client, asked).await.unwrap().hold.id(), hold.hold.id());

	let capture = capture_request(&scope, &hold, CaptureTarget::Account(label("alloc:x")), Some("2"));
	let captured = ledger_app::capture(&p, &client, capture.clone()).await.unwrap();
	for other in [
		CaptureRequest {
			amount: Some(usdt("3")),
			..capture.clone()
		},
		CaptureRequest { amount: None, ..capture.clone() },
		CaptureRequest {
			to: CaptureTarget::User(neighbour.clone()),
			..capture.clone()
		},
		CaptureRequest {
			subject_ref: subject(),
			..capture.clone()
		},
	] {
		conflict(ledger_app::capture(&p, &client, other).await);
	}
	let repeat = ledger_app::capture(&p, &client, capture).await.unwrap();
	assert_eq!(repeat.hold.progress().captured, captured.hold.progress().captured, "a repeat is the original capture");

	let release = void_request(&scope, &hold);
	ledger_app::void(&p, &client, release.clone()).await.unwrap();
	conflict(
		ledger_app::void(
			&p,
			&client,
			VoidRequest {
				subject_ref: subject(),
				..release.clone()
			},
		)
		.await,
	);
	ledger_app::void(&p, &client, release).await.expect("the same void again is a repeat");

	let held = ledger_app::get_hold(&p, &client, &scope, hold.hold.id().raw()).await.unwrap();
	assert_eq!(held.hold.progress().captured, usdt("2"), "none of the refused calls captured anything");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

async fn place(h: &Harness, grants: &Grants, client: &ClientId, scope: &ScopeId, auth_subject: &str, subject_ref: SubjectRef) -> RecordedHold {
	let request = ReserveRequest {
		subject_ref,
		..reserve_request(scope, auth_subject, "1", &key())
	};
	ledger_app::reserve(&ports(h, grants), client, request).await.unwrap()
}

fn order_ref() -> SubjectRef {
	SubjectRef::parse(&format!("order:{}", Uuid::new_v4())).unwrap()
}

fn list_request(scope: Option<&ScopeId>, prefix: &str, page_size: u32, after: Option<Uuid>) -> ListHoldsRequest {
	ListHoldsRequest {
		scope: scope.cloned(),
		subject_prefix: prefix.to_owned(),
		page_size,
		after,
	}
}

/// The orphan the listing exists for: a hold whose answer was lost. Every query of the listing
/// runs here — the scope filter, the `subject_ref` prefix, the keyset page and the active-only
/// filter — and a neighbouring client's holds must never show.
#[tokio::test]
async fn a_client_lists_only_its_own_active_holds_filtered_and_paged() {
	let Some(h) = harness().await else { return };
	let (client, first, _) = holder(&h).await;
	let second = own_scope(&h).await;
	let neighbour = fresh_client();
	let grants = Grants::new(vec![
		Grant {
			client: client.clone(),
			scope: first.clone(),
			label_prefix: "alloc:".into(),
			compartments: true,
		},
		Grant {
			client: client.clone(),
			scope: second.clone(),
			label_prefix: "alloc:".into(),
			compartments: true,
		},
		Grant {
			client: neighbour.clone(),
			scope: first.clone(),
			label_prefix: "panel:".into(),
			compartments: true,
		},
	])
	.unwrap();
	let p = ports(&h, &grants);
	let (user_id, user) = user_with(&h, &first, "100").await;
	let (other_id, other) = user_with(&h, &second, "100").await;
	consent(&h, &grants, &client, &first, user_id, false).await;
	consent(&h, &grants, &client, &second, other_id, false).await;
	consent(&h, &grants, &neighbour, &first, user_id, false).await;

	let mut orders = Vec::new();
	for _ in 0..3 {
		orders.push(place(&h, &grants, &client, &first, &user, order_ref()).await);
	}
	let elsewhere = place(&h, &grants, &client, &second, &other, order_ref()).await;
	let subscription = place(&h, &grants, &client, &first, &user, subject()).await;
	let voided = place(&h, &grants, &client, &first, &user, order_ref()).await;
	ledger_app::void(&p, &client, void_request(&first, &voided)).await.unwrap();
	let theirs = place(&h, &grants, &neighbour, &first, &user, order_ref()).await;
	let captured = place(&h, &grants, &client, &first, &user, order_ref()).await;
	ledger_app::capture(&p, &client, capture_request(&first, &captured, CaptureTarget::Account(label("alloc:x")), None))
		.await
		.unwrap();

	let ids = |page: &ledger_app::HoldPage| {
		let mut ids: Vec<Uuid> = page.holds.iter().map(|held| held.hold.id().raw()).collect();
		ids.sort_unstable();
		ids
	};
	let sorted = |holds: &[&RecordedHold]| {
		let mut ids: Vec<Uuid> = holds.iter().map(|held| held.hold.id().raw()).collect();
		ids.sort_unstable();
		ids
	};
	let order_refs: Vec<&RecordedHold> = orders.iter().collect();

	// Across every granted scope: the three orders, the other scope's, the subscription — and
	// neither the voided and the captured, nor the neighbour's.
	let everything = ledger_app::list_holds(&p, &client, list_request(None, "", 0, None)).await.unwrap();
	let mut expected = order_refs.clone();
	expected.extend([&elsewhere, &subscription]);
	assert_eq!(ids(&everything), sorted(&expected));
	assert!(everything.next.is_none());
	assert!(everything.holds.iter().all(|held| held.hold.state() == HoldState::Active && held.hold.terms().client == client));
	assert!(!ids(&everything).contains(&theirs.hold.id().raw()), "another client's hold is not in the list");

	// One scope, then a prefix inside it.
	let in_first = ledger_app::list_holds(&p, &client, list_request(Some(&first), "", 0, None)).await.unwrap();
	let mut expected = order_refs.clone();
	expected.push(&subscription);
	assert_eq!(ids(&in_first), sorted(&expected));
	let only_orders = ledger_app::list_holds(&p, &client, list_request(Some(&first), "order:", 0, None)).await.unwrap();
	assert_eq!(ids(&only_orders), sorted(&order_refs));
	let one_order = ledger_app::list_holds(&p, &client, list_request(Some(&first), orders[1].hold.terms().subject_ref.as_str(), 0, None))
		.await
		.unwrap();
	assert_eq!(ids(&one_order), sorted(&[&orders[1]]));
	assert!(
		ledger_app::list_holds(&p, &client, list_request(Some(&first), "invoice:", 0, None))
			.await
			.unwrap()
			.holds
			.is_empty(),
		"no hold of that kind"
	);

	// Pages walk the whole set once, in hold id order, and the last one has no successor.
	let mut walked = Vec::new();
	let mut after = None;
	let mut pages = 0;
	loop {
		let page = ledger_app::list_holds(&p, &client, list_request(None, "", 2, after)).await.unwrap();
		pages += 1;
		assert!(page.holds.len() <= 2);
		walked.extend(page.holds.iter().map(|held| held.hold.id().raw()));
		match page.next {
			Some(next) => after = Some(next),
			None => break,
		}
	}
	assert_eq!(pages, 3, "five holds, two to a page");
	assert!(walked.windows(2).all(|pair| pair[0] < pair[1]), "ascending by hold id, no repeats");
	assert_eq!(walked, ids(&everything));

	// The neighbour sees its own and nothing of the client's.
	let own = ledger_app::list_holds(&p, &neighbour, list_request(None, "", 0, None)).await.unwrap();
	assert_eq!(ids(&own), sorted(&[&theirs]));

	// Scopes outside the grants are refused like everywhere; a client with none lists nothing.
	for elsewhere_scope in [ScopeId::main(), own_scope(&h).await] {
		assert_eq!(
			refusal(ledger_app::list_holds(&p, &client, list_request(Some(&elsewhere_scope), "", 0, None)).await),
			LedgerRefusal::NotGranted
		);
	}
	assert!(ledger_app::list_holds(&p, &fresh_client(), list_request(None, "", 0, None)).await.unwrap().holds.is_empty());
	assert!(matches!(
		domain_err(ledger_app::list_holds(&p, &client, list_request(None, &"x".repeat(129), 0, None)).await),
		DomainError::Validation(_)
	));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn a_hold_locks_the_compartment_captures_in_parts_and_releases_the_rest() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = holder(&h).await;
	let (user_id, user) = user_with(&h, &scope, "10").await;
	let mandate = consent(&h, &grants, &client, &scope, user_id, false).await;
	let p = ports(&h, &grants);

	let k = key();
	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &user, "6", &k)).await.unwrap();
	assert_eq!((hold.hold.state(), hold.hold.remaining()), (HoldState::Active, usdt("6")));
	assert_eq!(hold.hold.terms().mandate_id, mandate.mandate.id);
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
	let (user_id, user) = user_with(&h, &scope, "5").await;
	consent(&h, &grants, &client, &scope, user_id, false).await;
	let p = ports(&h, &grants);
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
	let p = ports(&h, &grants);
	consent(&h, &grants, &client, &scope, buyer_id, false).await;
	let plain = ledger_app::reserve(&p, &client, reserve_request(&scope, &buyer, "1", &key())).await.unwrap();
	// Granting again replaces the mandate for new holds; the standing one keeps its own.
	consent(&h, &grants, &client, &scope, buyer_id, true).await;
	let hold = ledger_app::reserve(&p, &client, reserve_request(&scope, &buyer, "3", &key())).await.unwrap();
	assert_eq!(
		refusal(ledger_app::capture(&p, &client, capture_request(&scope, &plain, CaptureTarget::User(seller.clone()), Some("1"))).await),
		LedgerRefusal::MandateRequired,
		"the hold's own mandate decides, and it never listed BOOK_FILL"
	);
	ledger_app::void(&p, &client, void_request(&scope, &plain)).await.unwrap();
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
	consent(&h, &grants, &client, &scope, user_id, false).await;
	let p = ports(&h, &grants);
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
	consent(&h, &grants, &client, &scope, user_id, false).await;
	let p = ports(&h, &grants);
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
	let (user_id, user) = user_with(&h, &scope, "10").await;
	let mandate = consent(&h, &grants, &client, &scope, user_id, false).await;
	let p = ports(&h, &grants);
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
			MoneyEventKind::MandateGranted,
			MoneyEventKind::HoldPlaced,
			MoneyEventKind::HoldCaptured,
			MoneyEventKind::Transferred,
			// banking's route of the swept fee out of `fee`, shown to the client that swept it.
			MoneyEventKind::Transferred
		]
	);
	assert!(all.events.windows(2).all(|pair| pair[0].position < pair[1].position));
	assert_eq!(all.next_position, all.events[5].position);
	let granted = &all.events[1];
	assert_eq!(
		(granted.ref_id.as_str(), granted.auth_subject.as_str(), granted.initiator_client.as_str()),
		(mandate.mandate.id.to_string().as_str(), user.as_str(), ""),
		"the user granted it; no client is its initiator"
	);
	let captured = &all.events[3];
	assert_eq!((captured.ref_id.as_str(), captured.aggregate_version), (hold.hold.id().to_string().as_str(), 2));
	assert_eq!((captured.amount, captured.hold_remaining), (Some(usdt("3")), Some(usdt("1"))));
	assert_eq!(captured.to, Some(EventAccount::Account(label("alloc:x"))));
	assert_eq!(captured.auth_subject, user);
	let transferred = &all.events[4];
	let shown: Vec<&str> = transferred.balances.iter().map(|entry| entry.label.as_str()).collect();
	assert_eq!(shown, ["alloc:x"], "banking's fee collector never shows in a client's balances");
	assert_eq!(transferred.balances[0].balance, usdt("2"));
	let routed = &all.events[5];
	assert_eq!(
		(routed.initiator_client.as_str(), routed.from.clone(), routed.counterparty_scope.as_str()),
		("", Some(EventAccount::Account(ScopeAccountLabel::fee())), "main")
	);
	assert!(routed.balances.is_empty(), "both ends are banking's");

	let page = ledger_app::pull(&p, &client, None, all.events[1].position, 2).await.unwrap();
	assert_eq!(
		page.events.iter().map(|event| event.kind).collect::<Vec<_>>(),
		[MoneyEventKind::HoldPlaced, MoneyEventKind::HoldCaptured]
	);
	assert_eq!(page.next_position, all.events[3].position);
	let rest = ledger_app::pull(&p, &client, None, page.next_position, 10).await.unwrap();
	assert_eq!(rest.events.len(), 2, "the sweep and its route");
	let past_end = ledger_app::pull(&p, &client, None, rest.next_position, 10).await.unwrap();
	assert!(past_end.events.is_empty());
	assert_eq!(past_end.next_position, rest.next_position, "nothing scanned: the cursor stays");

	let (stranger, stranger_scope, stranger_grants) = holder(&h).await;
	let theirs = ports(&h, &stranger_grants);
	assert_eq!(refusal(ledger_app::pull(&theirs, &stranger, Some(&scope), 0, 10).await), LedgerRefusal::NotGranted);
	assert_eq!(refusal(ledger_app::pull(&theirs, &stranger, Some(&ScopeId::main()), 0, 10).await), LedgerRefusal::NotGranted);
	let own = ledger_app::pull(&theirs, &stranger, None, 0, 10).await.unwrap();
	assert!(own.events.iter().all(|event| event.scope == stranger_scope), "another client's scope never leaks into the stream");
}
