//! The read behind the approvers' payout-lines page (banking#529): a stored operation's
//! lines, paged, with the recipients banking has never paid in the scope marked — real
//! Postgres, every query of `infrastructure::payout_review` runs here. No ledger is needed:
//! the read touches only the control-plane rows.

mod common;

use domain::{
	money::Usdt,
	scope_operations::{RequestedLine, lines_digest_input},
};
use piggybank_core::infrastructure::payout_review::{approval_of, page};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

fn scope() -> String {
	format!("r_{}", &Uuid::new_v4().simple().to_string()[..12])
}

async fn user(pool: &PgPool, subject: &str) -> Uuid {
	let id = Uuid::new_v4();
	sqlx::query("INSERT INTO users (id, auth_subject, email) VALUES ($1, $2, $3)")
		.bind(id)
		.bind(subject)
		.bind(format!("{}@example.com", Uuid::new_v4().simple()))
		.execute(pool)
		.await
		.unwrap();
	id
}

/// A stored payout operation with `lines` as `(user, subject, base units)`.
async fn operation(pool: &PgPool, scope: &str, approval: Option<&str>, lines: &[(Uuid, String, u128)]) -> Uuid {
	let id = Uuid::new_v4();
	let total: u128 = lines.iter().map(|l| l.2).sum();
	let requested: Vec<RequestedLine> = lines
		.iter()
		.map(|(_, subject, amount)| RequestedLine {
			auth_subject: subject.clone(),
			amount: Usdt::from_base_units(*amount),
		})
		.collect();
	let digest = hex::encode(Sha256::digest(lines_digest_input(&requested).as_bytes()));
	sqlx::query(
		"INSERT INTO scope_operations (id, client_id, scope, kind, from_label, total, distribution, over_scope_budget, lines_digest, payload_hash, subject_ref, approval_id) \
		 VALUES ($1, 'svc', $2, 'bank:scope_payout', 'main', $3, true, false, $4, $4, $5, $6)",
	)
	.bind(id)
	.bind(scope)
	.bind(total.to_string())
	.bind(digest)
	.bind(format!("pending_operation:{id}"))
	.bind(approval)
	.execute(pool)
	.await
	.unwrap();
	for (n, (user, subject, amount)) in lines.iter().enumerate() {
		sqlx::query("INSERT INTO scope_operation_lines (operation_id, line_no, user_id, auth_subject, amount) VALUES ($1, $2, $3, $4, $5)")
			.bind(id)
			.bind(n as i32)
			.bind(user)
			.bind(subject)
			.bind(amount.to_string())
			.execute(pool)
			.await
			.unwrap();
	}
	id
}

/// An earlier payout of `scope` that paid `user`, for `reason`, in `state`.
async fn earlier_payout(pool: &PgPool, scope: &str, user: Uuid, subject: &str, reason: &str, state: &str) {
	let id = Uuid::new_v4();
	let failure = (state == "failed").then_some("ledger refused");
	sqlx::query(
		"INSERT INTO scope_payouts (id, client_id, scope, from_label, total, recipients, reason, subject_ref, state, failure_reason, operation_id) \
		 VALUES ($1, 'svc', $2, 'main', '5', 1, $3, $4, $5, $6, $7)",
	)
	.bind(id)
	.bind(scope)
	.bind(reason)
	.bind(format!("{reason}:{id}"))
	.bind(state)
	.bind(failure)
	.bind((reason == "approved").then(Uuid::new_v4))
	.execute(pool)
	.await
	.unwrap();
	sqlx::query("INSERT INTO scope_payout_lines (payout_id, line_no, user_id, auth_subject, amount) VALUES ($1, 0, $2, $3, '5')")
		.bind(id)
		.bind(user)
		.bind(subject)
		.execute(pool)
		.await
		.unwrap();
}

fn usdt(raw: &str) -> Usdt {
	Usdt::parse_decimal(raw).unwrap()
}

#[tokio::test]
async fn the_approval_of_a_payout_is_read_only_once_it_is_opened() {
	let Some(pool) = common::pool().await else { return };
	let scope = scope();
	let u = user(&pool, &format!("s-{}", Uuid::new_v4())).await;
	let lines = [(u, "alice".to_owned(), 10u128)];
	let approval = Uuid::new_v4().to_string();
	let opened = operation(&pool, &scope, Some(&approval), &lines).await;
	let unopened = operation(&pool, &scope, None, &lines).await;

	assert_eq!(approval_of(&pool, opened).await.unwrap().as_deref(), Some(approval.as_str()));
	assert_eq!(approval_of(&pool, unopened).await.unwrap(), None, "no approval yet");
	assert_eq!(approval_of(&pool, Uuid::new_v4()).await.unwrap(), None, "unknown id");
}

#[tokio::test]
async fn a_page_carries_every_line_of_the_summary_and_only_its_own_slice() {
	let Some(pool) = common::pool().await else { return };
	let scope = scope();
	let mut lines = Vec::new();
	for n in 0..5u128 {
		let subject = format!("recipient-{n}");
		lines.push((user(&pool, &format!("s-{}", Uuid::new_v4())).await, subject, (n + 1) * 1_000_000_000_000_000_000));
	}
	let approval = Uuid::new_v4().to_string();
	let id = operation(&pool, &scope, Some(&approval), &lines).await;

	let first = page(&pool, id, &approval, 0, 2).await.unwrap().expect("the operation's own approval reads it");
	assert_eq!(first.recipients, 5);
	assert_eq!(first.total, usdt("15"));
	assert_eq!(first.max_line, usdt("5"));
	assert_eq!(first.from_account, format!("{scope}:main"));
	assert_eq!(first.state, "awaiting_approval");
	assert!(first.distribution && !first.over_scope_budget);
	assert_eq!(
		first.lines.iter().map(|l| (l.line_no, l.recipient.as_str())).collect::<Vec<_>>(),
		[(0, "recipient-0"), (1, "recipient-1")]
	);

	let last = page(&pool, id, &approval, 4, 50).await.unwrap().unwrap();
	assert_eq!(last.lines.len(), 1);
	assert_eq!((last.lines[0].recipient.as_str(), last.lines[0].amount), ("recipient-4", usdt("5")));
	assert!(page(&pool, id, &approval, 99, 50).await.unwrap().unwrap().lines.is_empty(), "past the end is an empty page");
}

#[tokio::test]
async fn an_approval_that_is_not_the_operations_own_reads_nothing() {
	let Some(pool) = common::pool().await else { return };
	let scope = scope();
	let u = user(&pool, &format!("s-{}", Uuid::new_v4())).await;
	let approval = Uuid::new_v4().to_string();
	let id = operation(&pool, &scope, Some(&approval), &[(u, "alice".to_owned(), 10)]).await;
	let other = operation(&pool, &scope, Some(&Uuid::new_v4().to_string()), &[(u, "alice".to_owned(), 10)]).await;

	assert!(page(&pool, id, &Uuid::new_v4().to_string(), 0, 50).await.unwrap().is_none(), "another approval id");
	assert!(page(&pool, id, "", 0, 50).await.unwrap().is_none(), "no approval id");
	assert!(page(&pool, other, &approval, 0, 50).await.unwrap().is_none(), "another operation's approval");
	assert!(page(&pool, Uuid::new_v4(), &approval, 0, 50).await.unwrap().is_none(), "unknown operation");
}

#[tokio::test]
async fn a_recipient_has_history_only_through_a_posted_approved_payout_in_this_scope() {
	let Some(pool) = common::pool().await else { return };
	let scope = scope();
	let names = ["approved-posted", "approved-pending", "approved-failed", "automatic-posted", "elsewhere", "fresh"];
	let mut users = Vec::new();
	for _ in names {
		users.push(user(&pool, &format!("s-{}", Uuid::new_v4())).await);
	}
	earlier_payout(&pool, &scope, users[0], names[0], "approved", "posted").await;
	earlier_payout(&pool, &scope, users[1], names[1], "approved", "pending").await;
	earlier_payout(&pool, &scope, users[2], names[2], "approved", "failed").await;
	for (user, reason) in [(users[3], "redemption"), (users[3], "book"), (users[3], "fee_rebate")] {
		earlier_payout(&pool, &scope, user, names[3], reason, "posted").await;
	}
	earlier_payout(&pool, &self::scope(), users[4], names[4], "approved", "posted").await;
	let approval = Uuid::new_v4().to_string();
	let lines: Vec<_> = users.iter().zip(names).enumerate().map(|(n, (u, name))| (*u, name.to_owned(), n as u128 + 1)).collect();
	let id = operation(&pool, &scope, Some(&approval), &lines).await;

	let read = page(&pool, id, &approval, 0, 50).await.unwrap().unwrap();
	let marks: Vec<(&str, bool)> = read.lines.iter().map(|l| (l.recipient.as_str(), l.no_history)).collect();
	assert_eq!(
		marks,
		[
			("approved-posted", false),
			("approved-pending", true),
			("approved-failed", true),
			("automatic-posted", true),
			("elsewhere", true),
			("fresh", true)
		]
	);
	assert_eq!(read.recipients_without_history, 5);
}

#[tokio::test]
async fn lines_that_no_longer_hash_to_the_stored_digest_are_not_shown() {
	let Some(pool) = common::pool().await else { return };
	let scope = scope();
	let u = user(&pool, &format!("s-{}", Uuid::new_v4())).await;
	let approval = Uuid::new_v4().to_string();
	let id = operation(&pool, &scope, Some(&approval), &[(u, "alice".to_owned(), 10)]).await;
	assert!(page(&pool, id, &approval, 0, 50).await.unwrap().is_some(), "intact lines read");

	sqlx::query("UPDATE scope_operation_lines SET amount = '11' WHERE operation_id = $1")
		.bind(id)
		.execute(&pool)
		.await
		.unwrap();
	assert!(page(&pool, id, &approval, 0, 50).await.is_err(), "a changed amount is an error, not a page");

	sqlx::query("UPDATE scope_operation_lines SET amount = '10', auth_subject = 'mallory' WHERE operation_id = $1")
		.bind(id)
		.execute(&pool)
		.await
		.unwrap();
	assert!(page(&pool, id, &approval, 0, 50).await.is_err(), "a changed recipient is an error, not a page");
}
