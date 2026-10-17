//! A scope service's requests for mandates and the grant that stands on one — real Postgres.
//! Every query of the request store runs here: creating under a key, reading unexpired. Also
//! that a grant by request id gives the mandate the request asked for, and that the user's own
//! key keeps the grant idempotent.
//!
//! Each test works in scopes, clients and users of its own.

use domain::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, Grant, Grants, MandateOperation},
	scopes::ScopeId,
	users::UserId,
};
use piggybank_core::{
	application::scope_mandates::{self as mandates_app, CreateRequest, MandatePorts, RequestPorts},
	infrastructure::{compartments::PgCompartments, scope_ledger::PgScopeLedger},
};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

struct Harness {
	pool: PgPool,
	store: PgScopeLedger,
	scopes: PgCompartments,
	notify: Notify,
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	Some(Harness {
		store: PgScopeLedger::new(pool.clone()),
		scopes: PgCompartments::new(pool.clone()),
		notify: Notify::new(),
		pool,
	})
}

fn request_ports<'a>(h: &'a Harness, grants: &'a Grants) -> RequestPorts<'a> {
	RequestPorts {
		grants,
		requests: &h.store,
		scopes: &h.scopes,
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

fn key() -> String {
	Uuid::new_v4().to_string()
}

fn fresh_client() -> ClientId {
	ClientId::parse(&format!("r-{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap()
}

async fn own_scope(h: &Harness) -> ScopeId {
	let id = ScopeId::parse(&format!("r_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	sqlx::query("INSERT INTO scopes (id, display_name) VALUES ($1, 'Request scope')")
		.bind(id.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("INSERT INTO scope_policies (scope) VALUES ($1)").bind(id.as_str()).execute(&h.pool).await.unwrap();
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

async fn user(h: &Harness) -> UserId {
	let user = UserId::new();
	sqlx::query("INSERT INTO users (id, auth_subject, email) VALUES ($1, $2, $3)")
		.bind(user.raw())
		.bind(format!("itest-{}", Uuid::new_v4()))
		.bind(format!("{}@example.com", Uuid::new_v4().simple()))
		.execute(&h.pool)
		.await
		.unwrap();
	user
}

/// A client allowed to draw on compartments in a fresh scope.
async fn asker(h: &Harness) -> (ClientId, ScopeId, Grants) {
	let client = fresh_client();
	let scope = own_scope(h).await;
	let grants = Grants::new(vec![grant(&client, &scope, "alloc:", true)]).unwrap();
	(client, scope, grants)
}

fn create(scope: &ScopeId, per_op: &str, per_24h: &str) -> CreateRequest {
	CreateRequest {
		scope: scope.clone(),
		cap_per_op: usdt(per_op),
		cap_24h: usdt(per_24h),
		cap_outstanding: None,
		cap_book_fill_24h: None,
		operations: vec![MandateOperation::Reserve],
		mandate_expires_at: None,
		idempotency_key: key(),
	}
}

#[tokio::test]
async fn a_request_is_stored_and_a_repeat_of_its_key_is_the_same_request() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = asker(&h).await;
	let p = request_ports(&h, &grants);
	let asked = CreateRequest {
		operations: vec![MandateOperation::Charge, MandateOperation::Reserve],
		..create(&scope, "5", "10")
	};
	let made = mandates_app::create_request(&p, &client, asked.clone()).await.unwrap();
	assert_eq!(made.terms.client, client);
	assert_eq!(made.terms.caps.outstanding, usdt("10"), "an empty outstanding cap is cap_24h");
	assert_eq!(made.terms.operations, [MandateOperation::Reserve, MandateOperation::Charge], "operations come back in wire order");
	assert_eq!(made.terms.mandate_expires_at, None);
	assert_eq!(made.expires_at - made.created_at, mandates_app::REQUEST_TTL_SECONDS);

	let repeat = mandates_app::create_request(&p, &client, asked.clone()).await.unwrap();
	assert_eq!(repeat.id, made.id);
	let reordered = CreateRequest {
		operations: vec![MandateOperation::Reserve, MandateOperation::Charge],
		..asked.clone()
	};
	assert_eq!(
		mandates_app::create_request(&p, &client, reordered).await.unwrap().id,
		made.id,
		"the order of operations is not a term"
	);

	let err = mandates_app::create_request(
		&p,
		&client,
		CreateRequest {
			cap_per_op: usdt("6"),
			..asked.clone()
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(&err, DomainError::Conflict(why) if why == domain::scope_ledger::IDEMPOTENCY_CONFLICT), "{err:?}");

	let other = fresh_client();
	let other_grants = Grants::new(vec![grant(&client, &scope, "alloc:", true), grant(&other, &scope, "beta:", true)]).unwrap();
	let theirs = mandates_app::create_request(&request_ports(&h, &other_grants), &other, asked).await.unwrap();
	assert_ne!(theirs.id, made.id, "the key is per client");
}

#[tokio::test]
async fn a_request_must_be_one_a_grant_could_pass() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = asker(&h).await;
	let p = request_ports(&h, &grants);
	let accounts_only = fresh_client();
	let unknown_scope = ScopeId::parse(&format!("r_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	let grants_more = Grants::new(vec![
		grant(&client, &scope, "alloc:", true),
		grant(&accounts_only, &scope, "panel:", false),
		grant(&client, &unknown_scope, "alloc:", true),
	])
	.unwrap();
	let pm = request_ports(&h, &grants_more);
	let now: i64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::BIGINT").fetch_one(&h.pool).await.unwrap();

	for (ports, who, request, why) in [
		(&pm, &accounts_only, create(&scope, "1", "1"), "no compartments flag"),
		(&pm, &fresh_client(), create(&scope, "1", "1"), "no grant at all"),
		(&p, &client, create(&ScopeId::main(), "1", "1"), "main"),
		(&p, &client, create(&scope, "2", "1"), "cap_24h below cap_per_op"),
		(
			&p,
			&client,
			CreateRequest {
				cap_outstanding: Some(usdt("1")),
				..create(&scope, "2", "2")
			},
			"outstanding below cap_per_op",
		),
		(
			&p,
			&client,
			CreateRequest {
				operations: vec![],
				..create(&scope, "1", "1")
			},
			"no operations",
		),
		(
			&p,
			&client,
			CreateRequest {
				operations: vec![MandateOperation::Reserve, MandateOperation::Reserve],
				..create(&scope, "1", "1")
			},
			"operation twice",
		),
		(
			&p,
			&client,
			CreateRequest {
				cap_book_fill_24h: Some(usdt("1")),
				..create(&scope, "1", "1")
			},
			"book fill cap without BOOK_FILL",
		),
		(
			&p,
			&client,
			CreateRequest {
				operations: vec![MandateOperation::BookFill],
				..create(&scope, "1", "1")
			},
			"BOOK_FILL without its cap",
		),
		(
			&p,
			&client,
			CreateRequest {
				mandate_expires_at: Some(now - 1),
				..create(&scope, "1", "1")
			},
			"a mandate over before it starts",
		),
		(
			&p,
			&client,
			CreateRequest {
				mandate_expires_at: Some(now + 6 * 366 * 24 * 3600),
				..create(&scope, "1", "1")
			},
			"past five years",
		),
		(
			&p,
			&client,
			CreateRequest {
				idempotency_key: String::new(),
				..create(&scope, "1", "1")
			},
			"empty key",
		),
	] {
		let err = mandates_app::create_request(ports, who, request).await.expect_err(why);
		assert!(matches!(err, DomainError::Validation(_)), "{why}: {err:?}");
	}
	let err = mandates_app::create_request(&pm, &client, create(&unknown_scope, "1", "1")).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { entity: "scope", .. }), "{err:?}");

	let booked = CreateRequest {
		operations: vec![MandateOperation::BookFill],
		cap_book_fill_24h: Some(usdt("3")),
		mandate_expires_at: Some(now + 2 * 3600),
		..create(&scope, "1", "5")
	};
	let made = mandates_app::create_request(&p, &client, booked).await.unwrap();
	assert_eq!(made.terms.caps.book_fill_24h, Some(usdt("3")));
	assert_eq!(made.terms.mandate_expires_at, Some(now + 2 * 3600));
}

#[tokio::test]
async fn a_request_that_cannot_be_read_is_one_not_found() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = asker(&h).await;
	let p = request_ports(&h, &grants);
	let made = mandates_app::create_request(&p, &client, create(&scope, "1", "1")).await.unwrap();
	assert_eq!(mandates_app::read_request(&p, made.id).await.unwrap(), made);

	let expired = mandates_app::create_request(&p, &client, create(&scope, "1", "1")).await.unwrap();
	sqlx::query("UPDATE scope_mandate_requests SET expires_at = now() - interval '1 second' WHERE id = $1")
		.bind(expired.id)
		.execute(&h.pool)
		.await
		.unwrap();
	// The client lost its grant after asking: the user is not shown a request nobody honours.
	let revoked_grants = Grants::new(vec![grant(&fresh_client(), &scope, "alloc:", true)]).unwrap();
	for (ports, id, why) in [
		(&p, Uuid::new_v4(), "unknown"),
		(&p, expired.id, "expired"),
		(&request_ports(&h, &revoked_grants), made.id, "its client no longer allowed"),
	] {
		let err = mandates_app::read_request(ports, id).await.expect_err(why);
		assert!(matches!(&err, DomainError::NotFound { entity: "mandate request", .. }), "{why}: {err:?}");
	}
}

#[tokio::test]
async fn a_grant_by_request_gives_the_mandate_the_request_asked_for() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = asker(&h).await;
	let p = request_ports(&h, &grants);
	let asked = CreateRequest {
		cap_outstanding: Some(usdt("7")),
		operations: vec![MandateOperation::Reserve, MandateOperation::BookFill],
		cap_book_fill_24h: Some(usdt("2")),
		mandate_expires_at: Some(sqlx::query_scalar::<_, i64>("SELECT EXTRACT(EPOCH FROM now())::BIGINT + 7200").fetch_one(&h.pool).await.unwrap()),
		..create(&scope, "5", "10")
	};
	let made = mandates_app::create_request(&p, &client, asked).await.unwrap();
	let other = mandates_app::create_request(&p, &client, create(&scope, "1", "2")).await.unwrap();
	let user = user(&h).await;
	let mp = mandate_ports(&h, &grants);

	let user_key = key();
	let granted = mandates_app::grant(&mp, user, mandates_app::grant_of(mandates_app::read_request(&p, made.id).await.unwrap(), user_key.clone()))
		.await
		.unwrap();
	let terms = &granted.mandate.terms;
	assert_eq!((terms.user, &terms.scope, &terms.client), (user, &scope, &client));
	assert_eq!(terms.caps, made.terms.caps);
	assert_eq!(terms.operations, made.terms.operations);
	assert_eq!(terms.expires_at, made.terms.mandate_expires_at);

	// A request is not spent: the user can grant it again, and the key still answers.
	let repeat = mandates_app::grant(&mp, user, mandates_app::grant_of(mandates_app::read_request(&p, made.id).await.unwrap(), user_key.clone()))
		.await
		.unwrap();
	assert_eq!(repeat.mandate.id, granted.mandate.id);

	let err = mandates_app::grant(&mp, user, mandates_app::grant_of(mandates_app::read_request(&p, other.id).await.unwrap(), user_key))
		.await
		.unwrap_err();
	assert!(matches!(&err, DomainError::Conflict(why) if why == domain::scope_ledger::IDEMPOTENCY_CONFLICT), "{err:?}");
}

#[tokio::test]
async fn a_mandate_that_would_end_before_the_user_grants_is_refused_at_the_grant() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = asker(&h).await;
	let p = request_ports(&h, &grants);
	let in_two_hours: i64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::BIGINT + 3600").fetch_one(&h.pool).await.unwrap();
	let made = mandates_app::create_request(
		&p,
		&client,
		CreateRequest {
			mandate_expires_at: Some(in_two_hours),
			..create(&scope, "1", "1")
		},
	)
	.await
	.unwrap();
	sqlx::query("UPDATE scope_mandate_requests SET mandate_expires_at = now() - interval '1 second' WHERE id = $1")
		.bind(made.id)
		.execute(&h.pool)
		.await
		.unwrap();
	let user = user(&h).await;
	let err = mandates_app::grant(
		&mandate_ports(&h, &grants),
		user,
		mandates_app::grant_of(mandates_app::read_request(&p, made.id).await.unwrap(), key()),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "{err:?}");
}

async fn db_now(h: &Harness) -> i64 {
	sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::BIGINT").fetch_one(&h.pool).await.unwrap()
}

async fn expire(h: &Harness, id: Uuid) {
	sqlx::query("UPDATE scope_mandate_requests SET expires_at = now() - interval '1 second' WHERE id = $1")
		.bind(id)
		.execute(&h.pool)
		.await
		.unwrap();
}

#[tokio::test]
async fn a_repeat_after_the_request_expired_extends_the_same_request() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = asker(&h).await;
	let p = request_ports(&h, &grants);
	let asked = create(&scope, "1", "1");
	let made = mandates_app::create_request(&p, &client, asked.clone()).await.unwrap();
	expire(&h, made.id).await;
	assert!(mandates_app::read_request(&p, made.id).await.is_err());

	let again = mandates_app::create_request(&p, &client, asked.clone()).await.unwrap();
	assert_eq!(again.id, made.id);
	assert!(again.expires_at >= db_now(&h).await + mandates_app::REQUEST_TTL_SECONDS - 5, "{again:?}");
	assert_eq!(mandates_app::read_request(&p, made.id).await.unwrap().expires_at, again.expires_at);
	let twice = mandates_app::create_request(&p, &client, asked).await.unwrap();
	assert_eq!(twice.expires_at, again.expires_at, "a live request is not extended again");
}

#[tokio::test]
async fn two_repeats_of_an_expired_request_agree_on_one_term() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = asker(&h).await;
	let p = request_ports(&h, &grants);
	let asked = create(&scope, "1", "1");
	let made = mandates_app::create_request(&p, &client, asked.clone()).await.unwrap();
	expire(&h, made.id).await;
	let (a, b) = tokio::join!(mandates_app::create_request(&p, &client, asked.clone()), mandates_app::create_request(&p, &client, asked.clone()));
	let (a, b) = (a.unwrap(), b.unwrap());
	assert_eq!((a.id, b.id), (made.id, made.id));
	assert_eq!(a.expires_at, b.expires_at);
}

#[tokio::test]
async fn a_repeat_after_expiry_with_other_terms_is_still_a_conflict() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = asker(&h).await;
	let p = request_ports(&h, &grants);
	let asked = create(&scope, "1", "1");
	let made = mandates_app::create_request(&p, &client, asked.clone()).await.unwrap();
	expire(&h, made.id).await;
	let err = mandates_app::create_request(
		&p,
		&client,
		CreateRequest {
			cap_per_op: usdt("2"),
			cap_24h: usdt("2"),
			..asked
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(&err, DomainError::Conflict(why) if why == domain::scope_ledger::IDEMPOTENCY_CONFLICT), "{err:?}");
	assert!(mandates_app::read_request(&p, made.id).await.is_err(), "a conflict does not revive the request");
}

#[tokio::test]
async fn a_repeat_cannot_revive_a_request_whose_mandate_end_has_passed() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = asker(&h).await;
	let p = request_ports(&h, &grants);
	let made = mandates_app::create_request(
		&p,
		&client,
		CreateRequest {
			mandate_expires_at: Some(db_now(&h).await + 2 * 3600),
			..create(&scope, "1", "1")
		},
	)
	.await
	.unwrap();
	// Let the mandate end it proposed go by: the stored terms, and so the repeat that matches
	// them, name an end that is now behind the database clock.
	let gone = db_now(&h).await - 10;
	let asked = CreateRequest {
		scope: scope.clone(),
		cap_per_op: usdt("1"),
		cap_24h: usdt("1"),
		cap_outstanding: None,
		cap_book_fill_24h: None,
		operations: vec![MandateOperation::Reserve],
		mandate_expires_at: Some(gone),
		idempotency_key: String::new(),
	};
	let mut terms = made.terms.clone();
	terms.mandate_expires_at = Some(gone);
	sqlx::query("UPDATE scope_mandate_requests SET expires_at = now() - interval '1 second', mandate_expires_at = to_timestamp($2), terms = $3 WHERE id = $1")
		.bind(made.id)
		.bind(gone as f64)
		.bind(terms.canonical())
		.execute(&h.pool)
		.await
		.unwrap();
	let key: String = sqlx::query_scalar("SELECT idempotency_key FROM scope_mandate_requests WHERE id = $1")
		.bind(made.id)
		.fetch_one(&h.pool)
		.await
		.unwrap();
	let err = mandates_app::create_request(&p, &client, CreateRequest { idempotency_key: key, ..asked }).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "{err:?}");
	assert!(mandates_app::read_request(&p, made.id).await.is_err(), "the request stays dead");
}

#[tokio::test]
async fn a_request_does_not_outlive_its_mandate_and_a_near_mandate_end_is_refused() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = asker(&h).await;
	let p = request_ports(&h, &grants);
	let end = db_now(&h).await + 2 * 3600;
	let asked = CreateRequest {
		mandate_expires_at: Some(end),
		..create(&scope, "1", "1")
	};
	let made = mandates_app::create_request(&p, &client, asked.clone()).await.unwrap();
	assert_eq!(made.expires_at, end, "the request ends with the mandate it proposes, not 24 h out");

	expire(&h, made.id).await;
	let again = mandates_app::create_request(&p, &client, asked).await.unwrap();
	assert_eq!(again.expires_at, end, "an extension is cut the same way");

	let err = mandates_app::create_request(
		&p,
		&client,
		CreateRequest {
			mandate_expires_at: Some(db_now(&h).await + 1800),
			..create(&scope, "1", "1")
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(&err, DomainError::Validation(why) if why.contains("an hour")), "{err:?}");
}
