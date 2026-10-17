//! The read behind the approvers' payout-lines page (banking#529): a stored operation's
//! lines, paged, with the recipients banking has never paid in the scope marked — real
//! Postgres, every query of `infrastructure::payout_review` runs here. No ledger is needed:
//! the read touches only the control-plane rows.

mod common;

use domain::money::Usdt;
use piggybank_core::infrastructure::payout_review::{approval_of, page};
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
	sqlx::query(
		"INSERT INTO scope_operations (id, client_id, scope, kind, from_label, total, distribution, over_scope_budget, lines_digest, payload_hash, subject_ref, approval_id) \
		 VALUES ($1, 'svc', $2, 'bank:scope_payout', 'main', $3, true, false, $4, $4, $5, $6)",
	)
	.bind(id)
	.bind(scope)
	.bind(total.to_string())
	.bind("a".repeat(64))
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

/// An earlier payout of `scope` that paid `user`, in `state`.
async fn earlier_payout(pool: &PgPool, scope: &str, user: Uuid, subject: &str, state: &str) {
	let id = Uuid::new_v4();
	let failure = (state == "failed").then_some("ledger refused");
	sqlx::query(
		"INSERT INTO scope_payouts (id, client_id, scope, from_label, total, recipients, reason, subject_ref, state, failure_reason) \
		 VALUES ($1, 'svc', $2, 'main', '5', 1, 'redemption', $3, $4, $5)",
	)
	.bind(id)
	.bind(scope)
	.bind(format!("redemption:{id}"))
	.bind(state)
	.bind(failure)
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
async fn a_recipient_is_without_history_unless_banking_paid_them_in_this_scope() {
	let Some(pool) = common::pool().await else { return };
	let scope = scope();
	let (paid, failed_only, elsewhere, fresh) = (
		user(&pool, &format!("s-{}", Uuid::new_v4())).await,
		user(&pool, &format!("s-{}", Uuid::new_v4())).await,
		user(&pool, &format!("s-{}", Uuid::new_v4())).await,
		user(&pool, &format!("s-{}", Uuid::new_v4())).await,
	);
	earlier_payout(&pool, &scope, paid, "paid", "posted").await;
	earlier_payout(&pool, &scope, failed_only, "failed-only", "failed").await;
	earlier_payout(&pool, &self::scope(), elsewhere, "elsewhere", "posted").await;
	let approval = Uuid::new_v4().to_string();
	let lines = [
		(paid, "paid".to_owned(), 1u128),
		(failed_only, "failed-only".to_owned(), 2),
		(elsewhere, "elsewhere".to_owned(), 3),
		(fresh, "fresh".to_owned(), 4),
	];
	let id = operation(&pool, &scope, Some(&approval), &lines).await;

	let read = page(&pool, id, &approval, 0, 50).await.unwrap().unwrap();
	let marks: Vec<(&str, bool)> = read.lines.iter().map(|l| (l.recipient.as_str(), l.no_history)).collect();
	assert_eq!(marks, [("paid", false), ("failed-only", true), ("elsewhere", true), ("fresh", true)]);
	assert_eq!(read.recipients_without_history, 3);
}
