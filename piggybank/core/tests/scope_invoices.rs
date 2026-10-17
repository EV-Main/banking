//! Invoices a scope issues and a user pays (`scope.v1.InvoiceService`,
//! `banking.v1.InvoicePaymentService`) — real Postgres and TigerBeetle. Every query of the
//! invoice store runs here: issuing under the client's key, cancelling and paying under the
//! invoice's row lock, the payer's key, the relay's one posted leg, the reaper's expiry, and the
//! events the issuer pulls. Each test works in scopes, clients and users of its own.

use std::sync::Arc;

use domain::{
	balance::{LedgerAccountKey, TransferCode},
	error::DomainError,
	money::{Network, Usdt},
	scope_invoices::{InvoiceRefusal, InvoiceState},
	scope_ledger::{ClientId, Grant, Grants, LedgerRefusal, MoneyEventKind, SubjectRef},
	scopes::{Compartment, ScopeAccountLabel, ScopeId},
	users::UserId,
};
use piggybank_core::{
	application::{
		scope_invoices::{self as invoices_app, CreateInvoiceRequest, InvoicePorts, PayableView, PayerPorts},
		scope_ledger::{self as ledger_app, ScopeLedgerPorts, TransferError},
	},
	infrastructure::{compartments::PgCompartments, relay::Relay, scope_invoices, scope_ledger::PgScopeLedger},
	ports::{
		ledger::{Ledger, LedgerTransfer},
		scope_invoices::{PaymentError, RecordedInvoice},
		scope_ledger::{EventAccount, MoneyEventRecord},
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
	let ledger = common::seeded_ledger(&pool, "scope invoice tests").await?;
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

fn issuer_ports<'a>(h: &'a Harness, grants: &'a Grants) -> InvoicePorts<'a> {
	InvoicePorts {
		grants,
		invoices: &h.store,
		scopes: &h.scopes,
	}
}

fn payer_ports(h: &Harness) -> PayerPorts<'_> {
	PayerPorts {
		invoices: &h.store,
		scopes: &h.scopes,
		ledger: h.ledger.as_ref(),
		relay: &h.notify,
	}
}

fn ledger_ports<'a>(h: &'a Harness, grants: &'a Grants) -> ScopeLedgerPorts<'a> {
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

/// A scope of this test's own, active, default policy, banking's fee collector open.
async fn own_scope(h: &Harness) -> ScopeId {
	let id = ScopeId::parse(&format!("i_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	sqlx::query("INSERT INTO scopes (id, display_name) VALUES ($1, 'Invoice scope')")
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

/// A client granted `sales:` in a fresh scope, owning `sales:main`.
async fn issuer(h: &Harness) -> (ClientId, ScopeId, Grants) {
	let client = fresh_client();
	let scope = own_scope(h).await;
	let grants = Grants::new(vec![Grant {
		client: client.clone(),
		scope: scope.clone(),
		label_prefix: "sales:".into(),
		compartments: false,
	}])
	.unwrap();
	ledger_app::open_account(&ledger_ports(h, &grants), &client, &scope, &label("sales:main"), &key()).await.unwrap();
	(client, scope, grants)
}

/// A user with `in_main` in their wallet and `in_scope` in their compartment in `scope`,
/// straight on the ledger (custody grows with it, so the cash invariant holds).
async fn payer(h: &Harness, scope: &ScopeId, in_main: &str, in_scope: &str) -> (UserId, String) {
	let user = UserId::new();
	let auth_subject = format!("itest-{}", Uuid::new_v4());
	sqlx::query("INSERT INTO users (id, auth_subject, email) VALUES ($1, $2, $3)")
		.bind(user.raw())
		.bind(&auth_subject)
		.bind(format!("{}@example.com", Uuid::new_v4().simple()))
		.execute(&h.pool)
		.await
		.unwrap();
	for (credit, amount) in [(LedgerAccountKey::UserClaim(user), in_main), (LedgerAccountKey::ScopeClaim(user, scope.clone()), in_scope)] {
		if usdt(amount).is_zero() {
			continue;
		}
		h.ledger
			.post(&LedgerTransfer {
				id: Uuid::new_v4().as_u128(),
				debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
				credit,
				amount: usdt(amount).base_units(),
				code: TransferCode::Deposit,
				reference: 0,
			})
			.await
			.unwrap();
	}
	(user, auth_subject)
}

async fn posted(h: &Harness, key: LedgerAccountKey) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

/// An hour from the database's clock.
async fn in_an_hour(h: &Harness) -> i64 {
	sqlx::query_scalar::<_, i64>("SELECT EXTRACT(EPOCH FROM now())::BIGINT + 3600").fetch_one(&h.pool).await.unwrap()
}

async fn invoice_request(h: &Harness, scope: &ScopeId, amount: &str, payer: Option<&str>) -> CreateInvoiceRequest {
	CreateInvoiceRequest {
		scope: scope.clone(),
		to: label("sales:main"),
		amount: usdt(amount),
		memo: "Monthly plan".into(),
		expires_at: in_an_hour(h).await,
		payer_auth_subject: payer.map(str::to_owned),
		subject_ref: SubjectRef::parse(&format!("order:{}", Uuid::new_v4())).unwrap(),
		idempotency_key: key(),
	}
}

async fn issue(h: &Harness, client: &ClientId, grants: &Grants, scope: &ScopeId, amount: &str, payer: Option<&str>) -> RecordedInvoice {
	invoices_app::create_invoice(&issuer_ports(h, grants), client, invoice_request(h, scope, amount, payer).await)
		.await
		.unwrap()
}

fn invoice_id(recorded: &RecordedInvoice) -> Uuid {
	recorded.invoice.id().raw()
}

async fn pay(h: &Harness, user: UserId, recorded: &RecordedInvoice, from: Compartment, idempotency_key: &str) -> Result<PayableView, PaymentError> {
	invoices_app::pay(&payer_ports(h), user, invoice_id(recorded), from, idempotency_key).await
}

fn refusal<T: std::fmt::Debug>(result: Result<T, TransferError>) -> LedgerRefusal {
	match result {
		Err(TransferError::Refused(refusal)) => refusal,
		other => panic!("expected a refusal, got {other:?}"),
	}
}

fn payment_refusal<T: std::fmt::Debug>(result: Result<T, PaymentError>) -> InvoiceRefusal {
	match result {
		Err(PaymentError::Refused(refusal)) => refusal,
		other => panic!("expected a payment refusal, got {other:?}"),
	}
}

fn payment_domain_err<T: std::fmt::Debug>(result: Result<T, PaymentError>) -> DomainError {
	match result {
		Err(PaymentError::Domain(err)) => err,
		other => panic!("expected a domain error, got {other:?}"),
	}
}

async fn events_of(h: &Harness, client: &ClientId, grants: &Grants, scope: &ScopeId) -> Vec<MoneyEventRecord> {
	ledger_app::pull(&ledger_ports(h, grants), client, Some(scope), 0, 500).await.unwrap().events
}

fn scope_of(raw: &str) -> Compartment {
	Compartment::parse(raw).unwrap()
}

#[tokio::test]
async fn an_invoice_is_issued_once_per_key_into_an_account_the_caller_owns() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = issuer(&h).await;
	let p = issuer_ports(&h, &grants);

	let request = invoice_request(&h, &scope, "25", None).await;
	let issued = invoices_app::create_invoice(&p, &client, request.clone()).await.unwrap();
	assert_eq!(issued.invoice.state(), InvoiceState::Open);
	assert_eq!(
		invoices_app::pay_url(issued.invoice.id()),
		format!("https://evinvest.ltd/wallet/pay?invoice={}", invoice_id(&issued))
	);
	let repeat = invoices_app::create_invoice(&p, &client, request.clone()).await.unwrap();
	assert_eq!(invoice_id(&repeat), invoice_id(&issued), "a repeat is the original invoice");
	let other_terms = CreateInvoiceRequest {
		memo: "Another plan".into(),
		..request.clone()
	};
	assert!(matches!(
		invoices_app::create_invoice(&p, &client, other_terms).await,
		Err(TransferError::Domain(DomainError::Conflict(_)))
	));

	// The scope gate first: `main` and a scope with no grant read alike.
	for elsewhere in [ScopeId::main(), own_scope(&h).await] {
		let asked = CreateInvoiceRequest {
			scope: elsewhere,
			idempotency_key: key(),
			..request.clone()
		};
		assert_eq!(refusal(invoices_app::create_invoice(&p, &client, asked).await), LedgerRefusal::NotGranted);
	}
	let outside_prefix = CreateInvoiceRequest {
		to: label("alloc:x"),
		idempotency_key: key(),
		..request.clone()
	};
	assert_eq!(refusal(invoices_app::create_invoice(&p, &client, outside_prefix).await), LedgerRefusal::NotGranted);
	let never_opened = CreateInvoiceRequest {
		to: label("sales:other"),
		idempotency_key: key(),
		..request.clone()
	};
	assert_eq!(refusal(invoices_app::create_invoice(&p, &client, never_opened).await), LedgerRefusal::NotAccountOwner);
	let with_link = CreateInvoiceRequest {
		memo: "pay at https://evil.example".into(),
		idempotency_key: key(),
		..request.clone()
	};
	assert!(matches!(
		invoices_app::create_invoice(&p, &client, with_link).await,
		Err(TransferError::Domain(DomainError::Validation(_)))
	));

	sqlx::query("UPDATE scopes SET state = 'frozen' WHERE id = $1")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	let frozen = CreateInvoiceRequest {
		idempotency_key: key(),
		..request.clone()
	};
	assert_eq!(refusal(invoices_app::create_invoice(&p, &client, frozen).await), LedgerRefusal::ScopeFrozen);
	assert_eq!(
		invoice_id(&invoices_app::create_invoice(&p, &client, request).await.unwrap()),
		invoice_id(&issued),
		"a repeat answers whatever happened since"
	);

	let events = events_of(&h, &client, &grants, &scope).await;
	let created: Vec<_> = events.iter().filter(|event| event.kind == MoneyEventKind::InvoiceCreated).collect();
	assert_eq!(created.len(), 1, "one invoice, one creation");
	assert_eq!(created[0].ref_id, invoice_id(&issued).to_string());
	assert_eq!(created[0].to, Some(EventAccount::Account(label("sales:main"))));
	assert_eq!(created[0].amount, Some(usdt("25")));
}

#[tokio::test]
async fn paying_from_main_posts_one_transfer_into_the_scope_account_without_the_move_limits() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = issuer(&h).await;
	// Limits on moves in would refuse a move of 25 into this scope; a payment from main never
	// enters the compartment, so they do not apply (D24 (6)).
	sqlx::query("UPDATE scope_policies SET max_per_move = '1000000000000000000', max_balance = '1000000000000000000' WHERE scope = $1")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	let (user, subject) = payer(&h, &scope, "30", "0").await;
	let invoice = issue(&h, &client, &grants, &scope, "25", None).await;

	let k = key();
	let paid = pay(&h, user, &invoice, Compartment::Main, &k).await.unwrap();
	// Paid means posted: until the relay applies it, nobody — payer or issuer — reads it paid;
	// both read it processing.
	assert_eq!(paid.state(), InvoiceState::Processing);
	assert!(paid.own_payment().is_none());
	assert_eq!(paid.recorded.invoice.payment().map(|payment| payment.from.clone()), Some(Compartment::Main));
	let unposted = invoices_app::get_invoice(&issuer_ports(&h, &grants), &client, &scope, invoice_id(&invoice)).await.unwrap();
	assert_eq!(unposted.invoice.state_at(unposted.read_at), InvoiceState::Processing);
	let listed = invoices_app::get_payable(&payer_ports(&h), user, invoice_id(&invoice)).await.unwrap();
	assert_eq!(listed.state(), InvoiceState::Processing);
	assert!(unposted.invoice.posted_payment().is_none());
	// Before the relay applied it, the payment is already spoken for.
	let second = issue(&h, &client, &grants, &scope, "10", None).await;
	assert_eq!(payment_refusal(pay(&h, user, &second, Compartment::Main, &key()).await), InvoiceRefusal::InsufficientFunds);

	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, LedgerAccountKey::UserClaim(user)).await, usdt("5"));
	assert_eq!(posted(&h, LedgerAccountKey::ScopeAccount(scope.clone(), label("sales:main"))).await, usdt("25"));
	assert_eq!(
		posted(&h, LedgerAccountKey::ScopeClaim(user, scope.clone())).await,
		Usdt::ZERO,
		"nothing went through the compartment"
	);

	let repeat = pay(&h, user, &invoice, Compartment::Main, &k).await.unwrap();
	assert_eq!(repeat.state(), InvoiceState::Paid, "a repeat of the key returns the paid invoice");
	assert_eq!(repeat.own_payment().map(|payment| payment.from.clone()), Some(Compartment::Main));
	let (stranger, _) = payer(&h, &scope, "0", "0").await;
	let seen = invoices_app::get_payable(&payer_ports(&h), stranger, invoice_id(&invoice)).await.unwrap();
	assert_eq!(
		(seen.state(), seen.own_payment().is_none()),
		(InvoiceState::Paid, true),
		"who paid, and from where, is the payer's alone"
	);
	assert_eq!(payment_refusal(pay(&h, user, &invoice, Compartment::Main, &key()).await), InvoiceRefusal::NotOpen);
	assert!(matches!(payment_domain_err(pay(&h, user, &second, Compartment::Main, &k).await), DomainError::Conflict(_)));

	let events = events_of(&h, &client, &grants, &scope).await;
	let paid_event = events.iter().find(|event| event.kind == MoneyEventKind::InvoicePaid).expect("the issuer is told");
	assert_eq!(paid_event.auth_subject, subject);
	assert_eq!(paid_event.paid_from, "main");
	assert_eq!(paid_event.initiator_client, "", "the payer, not a client, caused it");
	assert_eq!(
		paid_event.balances.iter().map(|entry| (entry.label.clone(), entry.balance)).collect::<Vec<_>>(),
		[(label("sales:main"), usdt("25"))]
	);
}

#[tokio::test]
async fn paying_from_the_scope_compartment_moves_the_compartment_straight_to_the_scope() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = issuer(&h).await;
	let (user, _) = payer(&h, &scope, "0", "12").await;
	let invoice = issue(&h, &client, &grants, &scope, "12", None).await;

	assert_eq!(payment_refusal(pay(&h, user, &invoice, Compartment::Main, &key()).await), InvoiceRefusal::InsufficientFunds);
	let other_scope = own_scope(&h).await;
	assert!(matches!(
		payment_domain_err(pay(&h, user, &invoice, Compartment::Scope(other_scope), &key()).await),
		DomainError::Validation(_)
	));
	let paid = pay(&h, user, &invoice, scope_of(scope.as_str()), &key()).await.unwrap();
	assert_eq!(paid.recorded.invoice.payment().map(|payment| payment.from.as_str().to_owned()), Some(scope.to_string()));

	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, LedgerAccountKey::ScopeClaim(user, scope.clone())).await, Usdt::ZERO);
	assert_eq!(posted(&h, LedgerAccountKey::ScopeAccount(scope.clone(), label("sales:main"))).await, usdt("12"));
	let events = events_of(&h, &client, &grants, &scope).await;
	let paid_event = events.iter().find(|event| event.kind == MoneyEventKind::InvoicePaid).unwrap();
	assert_eq!(paid_event.paid_from, scope.as_str());
}

#[tokio::test]
async fn the_receiving_scopes_kyc_floor_and_the_payers_freeze_refuse_from_either_source() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = issuer(&h).await;
	sqlx::query("UPDATE scope_policies SET min_kyc_level = 2 WHERE scope = $1")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	let (user, _) = payer(&h, &scope, "10", "10").await;
	let invoice = issue(&h, &client, &grants, &scope, "1", None).await;

	common::set_kyc_level(&h.pool, user, 1).await;
	for from in [Compartment::Main, scope_of(scope.as_str())] {
		assert_eq!(payment_refusal(pay(&h, user, &invoice, from, &key()).await), InvoiceRefusal::KycLevel);
	}
	common::set_kyc_level(&h.pool, user, 2).await;
	sqlx::query("UPDATE users SET frozen = TRUE WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
	for from in [Compartment::Main, scope_of(scope.as_str())] {
		assert_eq!(payment_refusal(pay(&h, user, &invoice, from, &key()).await), InvoiceRefusal::UserFrozen);
	}
	sqlx::query("UPDATE users SET frozen = FALSE WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
	sqlx::query("UPDATE scopes SET state = 'closed' WHERE id = $1")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	assert_eq!(payment_refusal(pay(&h, user, &invoice, Compartment::Main, &key()).await), InvoiceRefusal::ScopeClosed);
	sqlx::query("UPDATE scopes SET state = 'active' WHERE id = $1")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	pay(&h, user, &invoice, Compartment::Main, &key()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn two_payers_racing_for_one_invoice_pay_it_once() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = issuer(&h).await;
	let invoice = issue(&h, &client, &grants, &scope, "5", None).await;
	let (first, _) = payer(&h, &scope, "5", "0").await;
	let (second, _) = payer(&h, &scope, "5", "0").await;

	let (key_a, key_b) = (key(), key());
	let (a, b) = tokio::join!(pay(&h, first, &invoice, Compartment::Main, &key_a), pay(&h, second, &invoice, Compartment::Main, &key_b));
	let outcomes = [a.is_ok(), b.is_ok()];
	assert_eq!(outcomes.iter().filter(|paid| **paid).count(), 1, "exactly one payment: {outcomes:?}");
	let loser = if outcomes[0] { b } else { a };
	assert_eq!(payment_refusal(loser), InvoiceRefusal::NotOpen);

	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, LedgerAccountKey::ScopeAccount(scope.clone(), label("sales:main"))).await, usdt("5"));
	let total = posted(&h, LedgerAccountKey::UserClaim(first))
		.await
		.checked_add(posted(&h, LedgerAccountKey::UserClaim(second)).await)
		.unwrap();
	assert_eq!(total, usdt("5"), "one of them paid, once");
}

#[tokio::test]
async fn another_clients_or_another_payers_invoice_is_not_found() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = issuer(&h).await;
	let (user, subject) = payer(&h, &scope, "10", "0").await;
	let (stranger, _) = payer(&h, &scope, "10", "0").await;
	let restricted = issue(&h, &client, &grants, &scope, "1", Some(&subject)).await;

	// The payer side: restricted to someone else and unknown are one NOT_FOUND.
	let unknown = invoices_app::get_payable(&payer_ports(&h), stranger, Uuid::new_v4()).await.unwrap_err();
	let theirs = invoices_app::get_payable(&payer_ports(&h), stranger, invoice_id(&restricted)).await.unwrap_err();
	assert!(matches!((&unknown, &theirs), (DomainError::NotFound { .. }, DomainError::NotFound { .. })));
	assert!(matches!(
		payment_domain_err(pay(&h, stranger, &restricted, Compartment::Main, &key()).await),
		DomainError::NotFound { .. }
	));
	let view = invoices_app::get_payable(&payer_ports(&h), user, invoice_id(&restricted)).await.unwrap();
	assert_eq!((view.available_main, view.available_scope, view.display_name.as_str()), (usdt("10"), Usdt::ZERO, "Invoice scope"));

	// The issuing side: another client granted in the same scope sees nothing of it, and one
	// granted elsewhere is stopped at the scope gate.
	let neighbour = fresh_client();
	let elsewhere = own_scope(&h).await;
	let neighbours = Grants::new(vec![
		Grant {
			client: neighbour.clone(),
			scope: scope.clone(),
			label_prefix: "other:".into(),
			compartments: false,
		},
		Grant {
			client: neighbour.clone(),
			scope: elsewhere.clone(),
			label_prefix: "other:".into(),
			compartments: false,
		},
	])
	.unwrap();
	let p = issuer_ports(&h, &neighbours);
	assert!(matches!(
		invoices_app::get_invoice(&p, &neighbour, &scope, invoice_id(&restricted)).await,
		Err(TransferError::Domain(DomainError::NotFound { .. }))
	));
	assert!(matches!(
		invoices_app::cancel_invoice(&p, &neighbour, &scope, invoice_id(&restricted)).await,
		Err(TransferError::Domain(DomainError::NotFound { .. }))
	));
	assert!(matches!(
		invoices_app::get_invoice(&p, &neighbour, &elsewhere, invoice_id(&restricted)).await,
		Err(TransferError::Domain(DomainError::NotFound { .. }))
	));
	assert_eq!(
		refusal(invoices_app::get_invoice(&issuer_ports(&h, &grants), &client, &elsewhere, invoice_id(&restricted)).await),
		LedgerRefusal::NotGranted
	);
	assert!(events_of(&h, &neighbour, &neighbours, &scope).await.is_empty(), "another client's invoices never reach it");
}

#[tokio::test]
async fn a_cancelled_invoice_stays_cancelled_and_takes_no_payment() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = issuer(&h).await;
	let (user, _) = payer(&h, &scope, "10", "0").await;
	let invoice = issue(&h, &client, &grants, &scope, "1", None).await;
	let p = issuer_ports(&h, &grants);

	let cancelled = invoices_app::cancel_invoice(&p, &client, &scope, invoice_id(&invoice)).await.unwrap();
	assert_eq!(cancelled.invoice.state(), InvoiceState::Cancelled);
	let again = invoices_app::cancel_invoice(&p, &client, &scope, invoice_id(&invoice)).await.unwrap();
	assert_eq!((again.invoice.state(), again.invoice.version()), (InvoiceState::Cancelled, cancelled.invoice.version()));
	assert_eq!(payment_refusal(pay(&h, user, &invoice, Compartment::Main, &key()).await), InvoiceRefusal::NotOpen);

	let paid = issue(&h, &client, &grants, &scope, "1", None).await;
	pay(&h, user, &paid, Compartment::Main, &key()).await.unwrap();
	// The payment waits for the ledger: a cancel gets its own reason, not "closed".
	assert_eq!(
		refusal(invoices_app::cancel_invoice(&p, &client, &scope, invoice_id(&paid)).await),
		LedgerRefusal::InvoiceProcessing
	);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(
		refusal(invoices_app::cancel_invoice(&p, &client, &scope, invoice_id(&paid)).await),
		LedgerRefusal::InvoiceNotOpen,
		"once posted it is closed for good"
	);

	let kinds: Vec<MoneyEventKind> = events_of(&h, &client, &grants, &scope)
		.await
		.into_iter()
		.filter(|event| event.ref_id == invoice_id(&invoice).to_string())
		.map(|event| event.kind)
		.collect();
	assert_eq!(kinds, [MoneyEventKind::InvoiceCreated, MoneyEventKind::InvoiceCancelled], "one cancellation, however often asked");
}

#[tokio::test]
async fn an_invoice_past_its_expiry_reads_expired_takes_no_payment_and_the_reaper_records_it() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = issuer(&h).await;
	let (user, _) = payer(&h, &scope, "10", "0").await;
	let invoice = issue(&h, &client, &grants, &scope, "1", None).await;
	// The clock cannot be moved; the invoice's expiry can.
	sqlx::query("UPDATE scope_invoices SET expires_at = now() - interval '1 second' WHERE id = $1")
		.bind(invoice_id(&invoice))
		.execute(&h.pool)
		.await
		.unwrap();

	let p = issuer_ports(&h, &grants);
	let read = invoices_app::get_invoice(&p, &client, &scope, invoice_id(&invoice)).await.unwrap();
	assert_eq!((read.invoice.state(), read.invoice.state_at(read.read_at)), (InvoiceState::Open, InvoiceState::Expired));
	assert_eq!(
		invoices_app::get_payable(&payer_ports(&h), user, invoice_id(&invoice)).await.unwrap().state(),
		InvoiceState::Expired
	);
	assert_eq!(payment_refusal(pay(&h, user, &invoice, Compartment::Main, &key()).await), InvoiceRefusal::NotOpen);
	assert_eq!(
		refusal(invoices_app::cancel_invoice(&p, &client, &scope, invoice_id(&invoice)).await),
		LedgerRefusal::InvoiceNotOpen
	);

	assert!(scope_invoices::expire_due_invoices(&h.pool).await.unwrap() >= 1);
	let expired = invoices_app::get_invoice(&p, &client, &scope, invoice_id(&invoice)).await.unwrap();
	assert_eq!((expired.invoice.state(), expired.invoice.version()), (InvoiceState::Expired, 2));
	scope_invoices::expire_due_invoices(&h.pool).await.unwrap();
	let events: Vec<MoneyEventRecord> = events_of(&h, &client, &grants, &scope)
		.await
		.into_iter()
		.filter(|event| event.kind == MoneyEventKind::InvoiceExpired)
		.collect();
	assert_eq!(events.len(), 1, "expired once, however often swept");
	assert_eq!((events[0].aggregate_version, events[0].initiator_client.as_str()), (2, ""));
	assert_eq!(posted(&h, LedgerAccountKey::UserClaim(user)).await, usdt("10"));
}

#[tokio::test]
async fn a_payment_the_ledger_refuses_reopens_the_invoice_and_holds_the_money_until_then() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = issuer(&h).await;
	let (user, _) = payer(&h, &scope, "10", "0").await;
	let invoice = issue(&h, &client, &grants, &scope, "10", None).await;
	let k = key();
	pay(&h, user, &invoice, Compartment::Main, &k).await.unwrap();

	// Not released before the ledger answers: the pending payment holds the whole wallet.
	let other = issue(&h, &client, &grants, &scope, "1", None).await;
	assert_eq!(payment_refusal(pay(&h, user, &other, Compartment::Main, &key()).await), InvoiceRefusal::InsufficientFunds);

	// The ledger disagrees with the control plane: the money left the claim behind its back,
	// so the relay's leg is refused.
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::UserClaim(user),
			credit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			amount: usdt("10").base_units(),
			code: TransferCode::Withdraw,
			reference: 0,
		})
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let reopened = invoices_app::get_invoice(&issuer_ports(&h, &grants), &client, &scope, invoice_id(&invoice)).await.unwrap();
	assert_eq!((reopened.invoice.state(), reopened.invoice.version()), (InvoiceState::Open, 3));
	assert!(reopened.invoice.payment().is_none());
	let reason: Option<String> = sqlx::query_scalar("SELECT pay_failure_reason FROM scope_invoices WHERE id = $1")
		.bind(invoice_id(&invoice))
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(reason.as_deref(), Some("insufficient_funds"));
	assert_eq!(posted(&h, LedgerAccountKey::ScopeAccount(scope.clone(), label("sales:main"))).await, Usdt::ZERO);
	assert!(
		!events_of(&h, &client, &grants, &scope).await.iter().any(|event| event.kind == MoneyEventKind::InvoicePaid),
		"the issuer is never told of a payment that did not happen"
	);

	// Paid again — under the same click's key, which named nothing that happened — it posts.
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: LedgerAccountKey::UserClaim(user),
			amount: usdt("10").base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.unwrap();
	pay(&h, user, &invoice, Compartment::Main, &k).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let paid = invoices_app::get_payable(&payer_ports(&h), user, invoice_id(&invoice)).await.unwrap();
	assert_eq!(paid.state(), InvoiceState::Paid);
	assert_eq!(posted(&h, LedgerAccountKey::ScopeAccount(scope.clone(), label("sales:main"))).await, usdt("10"));
	let paid_events: Vec<MoneyEventRecord> = events_of(&h, &client, &grants, &scope)
		.await
		.into_iter()
		.filter(|event| event.kind == MoneyEventKind::InvoicePaid)
		.collect();
	assert_eq!(paid_events.len(), 1);
	assert_eq!(paid_events[0].aggregate_version, 4, "the second attempt's payment");
}
