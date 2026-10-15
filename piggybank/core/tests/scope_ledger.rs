//! The scope ledger's use cases (`scope.v1.ScopeLedgerService`, part 1) — real Postgres and
//! TigerBeetle. Every query of the scope ledger store runs here at least once: opening an
//! account and answering its key, listing a client's accounts, recording a transfer under its
//! locks, the relay posting it, the reaper replaying a lost one, the pending sum a spend of a
//! scope account counts, and reconciliation's owner check.
//!
//! Each test works in scopes of its own, created under fresh ids and granted to fresh client
//! ids, so tests sharing this binary's database never see each other's accounts, ceilings or
//! freezes.

use std::sync::Arc;

use domain::{
	balance::{LedgerAccountKey, TransferCode},
	error::DomainError,
	money::{Network, Usdt},
	scope_ledger::{ClientId, Grant, Grants, LedgerRefusal, SubjectRef, TransferReason, TransferTerms, canonical},
	scopes::{MoveState, ScopeAccountLabel, ScopeId, ScopeState},
};
use piggybank_core::{
	application::scope_ledger::{self as ledger_app, ScopeLedgerPorts, TransferError, TransferRequest},
	infrastructure::{
		allocations::PgAllocations, compartments::PgCompartments, reaper::Reaper, reconciliation::Reconciliation, redemptions::PgRedemptions, relay::Relay, scope_ledger::PgScopeLedger,
		withdrawals::PgWithdrawals,
	},
	ports::{
		ledger::{Ledger, LedgerTransfer},
		scope_ledger::RecordedTransfer,
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
	let ledger = common::seeded_ledger(&pool, "scope ledger tests").await?;
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
	SubjectRef::parse(&format!("fee_settlement:{}", Uuid::new_v4())).unwrap()
}

fn grant(client: &ClientId, scope: &ScopeId, prefix: &str) -> Grant {
	Grant {
		client: client.clone(),
		scope: scope.clone(),
		label_prefix: prefix.to_owned(),
		compartments: false,
	}
}

/// A scope of this test's own, active, with the default policy and banking's fee collector —
/// what the scope stream leaves behind when it registers a scope.
async fn own_scope(h: &Harness) -> ScopeId {
	let id = ScopeId::parse(&format!("t_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	sqlx::query("INSERT INTO scopes (id, display_name) VALUES ($1, 'Test scope')")
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

async fn set_state(h: &Harness, scope: &ScopeId, state: ScopeState) {
	sqlx::query("UPDATE scopes SET state = $2 WHERE id = $1")
		.bind(scope.as_str())
		.bind(state.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// Money onto a scope account straight on the ledger, the custody side growing with it, so
/// the cash invariant still holds.
async fn fund(h: &Harness, scope: &ScopeId, account: &str, amount: &str) {
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: LedgerAccountKey::ScopeAccount(scope.clone(), label(account)),
			amount: usdt(amount).base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.unwrap();
}

async fn posted(h: &Harness, scope: &ScopeId, account: &str) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(&LedgerAccountKey::ScopeAccount(scope.clone(), label(account))).await.unwrap().posted)
}

/// A client granted `prefix` in a fresh scope, owning `account` there.
async fn client_with_account(h: &Harness, prefix: &str, account: &str) -> (ClientId, ScopeId, Grants) {
	let client = fresh_client();
	let scope = own_scope(h).await;
	let grants = Grants::new(vec![grant(&client, &scope, prefix)]).unwrap();
	ledger_app::open_account(&ports(h, &grants), &client, &scope, &label(account), &key()).await.unwrap();
	(client, scope, grants)
}

fn sweep(scope: &ScopeId, from: &str, amount: &str, idempotency_key: &str) -> TransferRequest {
	TransferRequest {
		scope: scope.clone(),
		from: label(from),
		to: ScopeAccountLabel::fee(),
		amount: usdt(amount),
		reason: TransferReason::FeeSweep,
		subject_ref: subject(),
		memo: String::new(),
		idempotency_key: idempotency_key.to_owned(),
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

async fn transfer_state(h: &Harness, recorded: &RecordedTransfer) -> (String, Option<String>) {
	sqlx::query_as("SELECT state, failure_reason FROM scope_transfers WHERE id = $1")
		.bind(recorded.transfer.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

#[tokio::test]
async fn an_account_opens_under_the_callers_prefix_and_a_repeat_is_the_same_account() {
	let Some(h) = harness().await else { return };
	let client = fresh_client();
	let scope = own_scope(&h).await;
	let grants = Grants::new(vec![grant(&client, &scope, "alloc:")]).unwrap();
	let p = ports(&h, &grants);
	let k = key();

	let opened = ledger_app::open_account(&p, &client, &scope, &label("alloc:quy_nhon"), &k).await.unwrap();
	assert_eq!(opened.account.owner, client);
	assert_eq!(opened.account.label, label("alloc:quy_nhon"));
	assert_eq!(opened.balance, Usdt::ZERO);

	let repeat = ledger_app::open_account(&p, &client, &scope, &label("alloc:quy_nhon"), &k).await.unwrap();
	assert_eq!(repeat.account, opened.account, "the same key answers with the same account");
	let reopened = ledger_app::open_account(&p, &client, &scope, &label("alloc:quy_nhon"), &key()).await.unwrap();
	assert_eq!(reopened.account, opened.account, "the owner opening its label again gets the existing account");

	let other = domain_err(ledger_app::open_account(&p, &client, &scope, &label("alloc:other"), &k).await);
	assert!(matches!(other, DomainError::Conflict(_)), "a key reused for another label is a conflict: {other}");
}

#[tokio::test]
async fn banking_labels_and_labels_outside_the_grant_never_open() {
	let Some(h) = harness().await else { return };
	let client = fresh_client();
	let (one, two) = (own_scope(&h).await, own_scope(&h).await);
	// Prefixes that cover banking's labels, so only the label rule can refuse them.
	let grants = Grants::new(vec![grant(&client, &one, "f"), grant(&client, &two, "t")]).unwrap();
	let p = ports(&h, &grants);
	assert_eq!(refusal(ledger_app::open_account(&p, &client, &one, &label("fee"), &key()).await), LedgerRefusal::NotGranted);
	assert_eq!(refusal(ledger_app::open_account(&p, &client, &two, &label("treasury"), &key()).await), LedgerRefusal::NotGranted);
	assert_eq!(
		refusal(ledger_app::open_account(&p, &client, &one, &label("alloc:x"), &key()).await),
		LedgerRefusal::NotGranted,
		"outside the prefix"
	);
	ledger_app::open_account(&p, &client, &one, &label("fund"), &key()).await.expect("inside the prefix");
	let owner: String = sqlx::query_scalar("SELECT owner_client FROM scope_accounts WHERE scope = $1 AND label = 'fee'")
		.bind(one.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(owner, "banking", "the fee collector is untouched");
	let treasury: i64 = sqlx::query_scalar("SELECT count(*) FROM scope_accounts WHERE scope = $1 AND label = 'treasury'")
		.bind(two.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(treasury, 0);
}

#[tokio::test]
async fn a_label_another_client_owns_is_already_taken() {
	let Some(h) = harness().await else { return };
	let (first, second) = (fresh_client(), fresh_client());
	let scope = own_scope(&h).await;
	// One configuration keeps prefixes disjoint; a label opened under an earlier one stays
	// its owner's when a later configuration grants the prefix to someone else.
	let before = Grants::new(vec![grant(&first, &scope, "a:")]).unwrap();
	ledger_app::open_account(&ports(&h, &before), &first, &scope, &label("a:one"), &key()).await.unwrap();
	let after = Grants::new(vec![grant(&second, &scope, "a:")]).unwrap();
	let taken = domain_err(ledger_app::open_account(&ports(&h, &after), &second, &scope, &label("a:one"), &key()).await);
	assert!(matches!(taken, DomainError::Conflict(_)), "another client's label is ALREADY_EXISTS: {taken}");
	assert!(matches!(
		domain_err(ledger_app::get_balances(&ports(&h, &after), &second, &scope, &[label("a:one")]).await),
		DomainError::NotFound { .. }
	));
	// Inside the second client's prefix now, and still not its account to debit.
	fund(&h, &scope, "a:one", "1").await;
	assert_eq!(
		refusal(ledger_app::transfer(&ports(&h, &after), &second, sweep(&scope, "a:one", "1", &key())).await),
		LedgerRefusal::NotAccountOwner
	);
}

#[tokio::test]
async fn main_and_ungranted_scopes_are_refused_alike_and_an_unknown_granted_scope_is_not_found() {
	let Some(h) = harness().await else { return };
	let client = fresh_client();
	let scope = own_scope(&h).await;
	let unknown = ScopeId::parse(&format!("u_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	let grants = Grants::new(vec![grant(&client, &scope, "x:"), grant(&client, &unknown, "x:")]).unwrap();
	let p = ports(&h, &grants);
	assert_eq!(refusal(ledger_app::get_balances(&p, &client, &ScopeId::main(), &[]).await), LedgerRefusal::NotGranted);
	assert_eq!(
		refusal(ledger_app::get_balances(&p, &fresh_client(), &scope, &[]).await),
		LedgerRefusal::NotGranted,
		"no grant there"
	);
	let ungranted_unknown = ScopeId::parse("zz_never_registered").unwrap();
	assert_eq!(
		refusal(ledger_app::get_balances(&p, &client, &ungranted_unknown, &[]).await),
		LedgerRefusal::NotGranted,
		"a scope outside the grants is not told apart from a known one"
	);
	assert!(matches!(domain_err(ledger_app::get_balances(&p, &client, &unknown, &[]).await), DomainError::NotFound { .. }));
	assert!(ledger_app::get_balances(&p, &client, &scope, &[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn balances_are_the_callers_own_accounts_only() {
	let Some(h) = harness().await else { return };
	let (client, scope, _) = client_with_account(&h, "alloc:", "alloc:one").await;
	let neighbour = fresh_client();
	let grants = Grants::new(vec![grant(&client, &scope, "alloc:"), grant(&neighbour, &scope, "panel:")]).unwrap();
	let p = ports(&h, &grants);
	ledger_app::open_account(&p, &client, &scope, &label("alloc:two"), &key()).await.unwrap();
	ledger_app::open_account(&p, &neighbour, &scope, &label("panel:main"), &key()).await.unwrap();
	fund(&h, &scope, "alloc:one", "12.5").await;
	fund(&h, &scope, "fee", "1").await;

	let all = ledger_app::get_balances(&p, &client, &scope, &[]).await.unwrap();
	let seen: Vec<(String, String)> = all.iter().map(|v| (v.account.label.to_string(), v.balance.to_decimal_string())).collect();
	assert_eq!(seen, [("alloc:one".to_owned(), "12.5".to_owned()), ("alloc:two".to_owned(), "0".to_owned())]);

	let asked = ledger_app::get_balances(&p, &client, &scope, &[label("alloc:two"), label("alloc:one")]).await.unwrap();
	assert_eq!(
		asked.iter().map(|v| v.account.label.to_string()).collect::<Vec<_>>(),
		["alloc:two", "alloc:one"],
		"in the order asked"
	);

	for not_mine in ["panel:main", "fee", "alloc:none"] {
		let err = domain_err(ledger_app::get_balances(&p, &client, &scope, &[label(not_mine)]).await);
		assert!(matches!(err, DomainError::NotFound { .. }), "{not_mine}: {err}");
	}
}

#[tokio::test]
async fn a_fee_sweep_records_at_once_and_the_relay_moves_the_money() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = client_with_account(&h, "alloc:", "alloc:one").await;
	fund(&h, &scope, "alloc:one", "100").await;
	let p = ports(&h, &grants);
	let recorded = ledger_app::transfer(&p, &client, sweep(&scope, "alloc:one", "40", &key())).await.unwrap();
	assert_eq!(recorded.state, MoveState::Pending);
	assert_eq!(recorded.to_scope, scope);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(transfer_state(&h, &recorded).await, ("posted".to_owned(), None));
	assert_eq!(posted(&h, &scope, "alloc:one").await, usdt("60"));
	assert_eq!(posted(&h, &scope, "fee").await, usdt("40"));
}

#[tokio::test]
async fn a_sweep_debits_only_the_callers_own_account_and_only_into_fee() {
	let Some(h) = harness().await else { return };
	let (client, scope, _) = client_with_account(&h, "alloc:", "alloc:one").await;
	let neighbour = fresh_client();
	let grants = Grants::new(vec![grant(&client, &scope, "alloc:"), grant(&neighbour, &scope, "panel:")]).unwrap();
	let p = ports(&h, &grants);
	ledger_app::open_account(&p, &neighbour, &scope, &label("panel:main"), &key()).await.unwrap();
	ledger_app::open_account(&p, &client, &scope, &label("alloc:two"), &key()).await.unwrap();
	fund(&h, &scope, "alloc:one", "10").await;
	fund(&h, &scope, "panel:main", "10").await;
	fund(&h, &scope, "fee", "10").await;

	assert_eq!(
		refusal(ledger_app::transfer(&p, &client, sweep(&scope, "panel:main", "1", &key())).await),
		LedgerRefusal::NotGranted,
		"another client's account, under a prefix the caller is not granted"
	);
	for (from, why) in [("fee", "banking's collector"), ("alloc:none", "no such account")] {
		assert_eq!(
			refusal(ledger_app::transfer(&p, &client, sweep(&scope, from, "1", &key())).await),
			LedgerRefusal::NotAccountOwner,
			"{why}"
		);
	}
	let mut sideways = sweep(&scope, "alloc:one", "1", &key());
	sideways.to = label("alloc:two");
	assert_eq!(
		refusal(ledger_app::transfer(&p, &client, sideways).await),
		LedgerRefusal::ApprovalRequired,
		"only fee is automatic"
	);
	let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM scope_transfers WHERE scope = $1")
		.bind(scope.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(recorded, 0, "a refusal writes nothing");
}

#[tokio::test]
async fn the_automatic_ceilings_hold_per_operation_and_per_client_over_24_hours() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = client_with_account(&h, "alloc:", "alloc:one").await;
	let p = ports(&h, &grants);
	ledger_app::open_account(&p, &client, &scope, &label("alloc:two"), &key()).await.unwrap();
	fund(&h, &scope, "alloc:one", "4000").await;
	fund(&h, &scope, "alloc:two", "4000").await;

	assert_eq!(
		refusal(ledger_app::transfer(&p, &client, sweep(&scope, "alloc:one", "1000.000000000000000001", &key())).await),
		LedgerRefusal::ApprovalRequired
	);
	for (from, amount) in [("alloc:one", "1000"), ("alloc:one", "1000"), ("alloc:two", "1000"), ("alloc:two", "1000"), ("alloc:one", "999")] {
		ledger_app::transfer(&p, &client, sweep(&scope, from, amount, &key())).await.unwrap();
	}
	// 4999 moved, still pending: the ceiling counts it before the relay does anything.
	assert_eq!(
		refusal(ledger_app::transfer(&p, &client, sweep(&scope, "alloc:two", "1.000000000000000001", &key())).await),
		LedgerRefusal::ApprovalRequired,
		"one ceiling across both accounts"
	);
	ledger_app::transfer(&p, &client, sweep(&scope, "alloc:two", "1", &key())).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, &scope, "fee").await, usdt("5000"));

	// A transfer older than 24 h is out of the window.
	sqlx::query("UPDATE scope_transfers SET created_at = now() - interval '25 hours' WHERE scope = $1 AND from_label = 'alloc:one'")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	ledger_app::transfer(&p, &client, sweep(&scope, "alloc:two", "500", &key())).await.unwrap();
}

#[tokio::test]
async fn a_sweep_cannot_spend_what_an_unposted_one_already_promised() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = client_with_account(&h, "alloc:", "alloc:one").await;
	fund(&h, &scope, "alloc:one", "10").await;
	let p = ports(&h, &grants);
	ledger_app::transfer(&p, &client, sweep(&scope, "alloc:one", "7", &key())).await.unwrap();
	assert_eq!(
		refusal(ledger_app::transfer(&p, &client, sweep(&scope, "alloc:one", "4", &key())).await),
		LedgerRefusal::InsufficientFunds,
		"the first sweep is still pending; the ledger alone would say 10"
	);
	ledger_app::transfer(&p, &client, sweep(&scope, "alloc:one", "3", &key())).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(posted(&h, &scope, "alloc:one").await, Usdt::ZERO);
}

#[tokio::test]
async fn a_frozen_scope_takes_no_sweep_but_a_repeat_still_answers_with_the_original() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = client_with_account(&h, "alloc:", "alloc:one").await;
	fund(&h, &scope, "alloc:one", "10").await;
	let p = ports(&h, &grants);
	let k = key();
	let request = sweep(&scope, "alloc:one", "2", &k);
	let original = ledger_app::transfer(&p, &client, request.clone()).await.unwrap();
	set_state(&h, &scope, ScopeState::Frozen).await;

	let repeat = ledger_app::transfer(&p, &client, request.clone()).await.unwrap();
	assert_eq!(repeat.transfer.id(), original.transfer.id(), "the original result, even though the scope froze since");
	assert_eq!(
		refusal(ledger_app::transfer(&p, &client, sweep(&scope, "alloc:one", "2", &key())).await),
		LedgerRefusal::ScopeFrozen
	);
	let other_terms = TransferRequest { memo: "another".into(), ..request };
	assert!(matches!(domain_err(ledger_app::transfer(&p, &client, other_terms).await), DomainError::Conflict(_)));

	set_state(&h, &scope, ScopeState::Closed).await;
	ledger_app::transfer(&p, &client, sweep(&scope, "alloc:one", "2", &key()))
		.await
		.expect("a closed scope's accounts can still be swept");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn one_key_under_two_rpcs_is_two_keys() {
	let Some(h) = harness().await else { return };
	let client = fresh_client();
	let scope = own_scope(&h).await;
	let grants = Grants::new(vec![grant(&client, &scope, "alloc:")]).unwrap();
	let p = ports(&h, &grants);
	let k = key();
	ledger_app::open_account(&p, &client, &scope, &label("alloc:one"), &k).await.unwrap();
	fund(&h, &scope, "alloc:one", "1").await;
	ledger_app::transfer(&p, &client, sweep(&scope, "alloc:one", "1", &k))
		.await
		.expect("the open_account key does not bind a transfer");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

/// Until a backend is waiting on a lock the WINNER's transaction holds — so the winner is
/// committed only once the call is genuinely queued behind its uncommitted rows.
async fn wait_blocked_by(pool: &PgPool, winner_pid: i32) {
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
	loop {
		let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))")
			.bind(winner_pid)
			.fetch_one(pool)
			.await
			.unwrap();
		if waiting > 0 {
			return;
		}
		assert!(std::time::Instant::now() < deadline, "the call never waited on the winner's rows");
		tokio::task::yield_now().await;
	}
}

/// The unique index behind the key lock: a key another transaction holds uncommitted makes
/// this call wait, and once that one commits the call answers with what it recorded (23505 →
/// re-read) instead of an internal error.
#[tokio::test]
async fn a_key_won_by_a_concurrent_open_is_answered_with_its_result() {
	let Some(h) = harness().await else { return };
	let client = fresh_client();
	let scope = own_scope(&h).await;
	let grants = Grants::new(vec![grant(&client, &scope, "alloc:")]).unwrap();
	let k = key();
	let mut winner = h.pool.begin().await.unwrap();
	let winner_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *winner).await.unwrap();
	sqlx::query("INSERT INTO scope_accounts (scope, label, owner_client) VALUES ($1, 'alloc:raced', $2)")
		.bind(scope.as_str())
		.bind(client.as_str())
		.execute(&mut *winner)
		.await
		.unwrap();
	sqlx::query("INSERT INTO scope_ledger_requests (client_id, scope, rpc, idempotency_key, terms, result_id) VALUES ($1, $2, 'open_account', $3, $4, 'alloc:raced')")
		.bind(client.as_str())
		.bind(scope.as_str())
		.bind(&k)
		.bind(canonical(&["alloc:raced"]))
		.execute(&mut *winner)
		.await
		.unwrap();
	let p = ports(&h, &grants);
	let raced_label = label("alloc:raced");
	let call = ledger_app::open_account(&p, &client, &scope, &raced_label, &k);
	let release = async {
		wait_blocked_by(&h.pool, winner_pid).await;
		winner.commit().await.unwrap();
	};
	let (answered, ()) = tokio::join!(call, release);
	let answered = answered.expect("the winner's result, not an error");
	assert_eq!(answered.account.label, raced_label);
	assert_eq!(answered.account.owner, client);
}

/// The same race on `Transfer`: the call passes every check and records its own transfer,
/// then meets the winner's key on the unique index; it rolls its transfer back and answers
/// with the winner's.
#[tokio::test]
async fn a_key_won_by_a_concurrent_transfer_is_answered_with_its_result() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = client_with_account(&h, "alloc:", "alloc:one").await;
	fund(&h, &scope, "alloc:one", "10").await;
	let k = key();
	let request = sweep(&scope, "alloc:one", "2", &k);
	let terms = TransferTerms {
		client: client.clone(),
		scope: scope.clone(),
		from: request.from.clone(),
		to: request.to.clone(),
		amount: request.amount,
		reason: request.reason,
		subject_ref: request.subject_ref.clone(),
		memo: request.memo.clone(),
	};
	let won = Uuid::new_v4();
	let mut winner = h.pool.begin().await.unwrap();
	let winner_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *winner).await.unwrap();
	sqlx::query(
		"INSERT INTO scope_transfers (id, client_id, scope, from_label, to_scope, to_label, amount, reason, subject_ref, memo, state) \
		 VALUES ($1, $2, $3, 'alloc:one', $3, 'fee', $4, 'fee_sweep', $5, '', 'posted')",
	)
	.bind(won)
	.bind(client.as_str())
	.bind(scope.as_str())
	.bind(usdt("2").base_units().to_string())
	.bind(terms.subject_ref.as_str())
	.execute(&mut *winner)
	.await
	.unwrap();
	sqlx::query("INSERT INTO scope_ledger_requests (client_id, scope, rpc, idempotency_key, terms, result_id) VALUES ($1, $2, 'transfer', $3, $4, $5)")
		.bind(client.as_str())
		.bind(scope.as_str())
		.bind(&k)
		.bind(terms.canonical())
		.bind(won.to_string())
		.execute(&mut *winner)
		.await
		.unwrap();
	let p = ports(&h, &grants);
	let call = ledger_app::transfer(&p, &client, request);
	let release = async {
		wait_blocked_by(&h.pool, winner_pid).await;
		winner.commit().await.unwrap();
	};
	let (answered, ()) = tokio::join!(call, release);
	let answered = answered.expect("the winner's result, not an error");
	assert_eq!(answered.transfer.id().raw(), won, "the winner's transfer");
	let mine: i64 = sqlx::query_scalar("SELECT count(*) FROM scope_transfers WHERE scope = $1")
		.bind(scope.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(mine, 1, "the loser's own transfer rolled back with its key");
	let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox o JOIN scope_transfers t ON t.id = o.aggregate_id WHERE t.scope = $1")
		.bind(scope.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(queued, 0, "and nothing reached the outbox");
}

#[tokio::test]
async fn a_prefix_taken_away_takes_the_debit_with_it() {
	let Some(h) = harness().await else { return };
	let (client, scope, before) = client_with_account(&h, "alloc:", "alloc:one").await;
	fund(&h, &scope, "alloc:one", "10").await;
	let earlier = sweep(&scope, "alloc:one", "2", &key());
	let original = ledger_app::transfer(&ports(&h, &before), &client, earlier.clone()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// The configuration now grants the client another prefix in the scope, not `alloc:`.
	let regranted = Grants::new(vec![grant(&client, &scope, "panel:")]).unwrap();
	let p = ports(&h, &regranted);
	assert_eq!(
		refusal(ledger_app::transfer(&p, &client, sweep(&scope, "alloc:one", "1", &key())).await),
		LedgerRefusal::NotGranted
	);
	assert_eq!(posted(&h, &scope, "alloc:one").await, usdt("8"), "the money stays where it is");
	let repeat = ledger_app::transfer(&p, &client, earlier).await.expect("a repeat gets its original result");
	assert_eq!(repeat.transfer.id(), original.transfer.id());
	let read = ledger_app::get_balances(&p, &client, &scope, &[label("alloc:one")]).await.expect("still readable");
	assert_eq!(read[0].balance, usdt("8"));
}

#[tokio::test]
async fn an_idempotency_key_is_1_to_128_characters_without_control_characters() {
	let Some(h) = harness().await else { return };
	let client = fresh_client();
	let scope = own_scope(&h).await;
	let grants = Grants::new(vec![grant(&client, &scope, "alloc:")]).unwrap();
	let p = ports(&h, &grants);
	let (p, client, scope) = (&p, &client, &scope);
	let open = |k: String| async move { ledger_app::open_account(p, client, scope, &label("alloc:keyed"), &k).await };
	assert!(open("é".repeat(128)).await.is_ok(), "128 characters, 256 bytes");
	for (bad, why) in [(String::new(), "empty"), ("k".repeat(129), "129 characters"), ("a\nb".to_owned(), "a control character")] {
		assert!(matches!(domain_err(open(bad).await), DomainError::Validation(_)), "{why}");
	}
}

#[tokio::test]
async fn a_transfer_an_older_relay_dropped_is_replayed_once() {
	let Some(h) = harness().await else { return };
	let (client, scope, grants) = client_with_account(&h, "alloc:", "alloc:one").await;
	fund(&h, &scope, "alloc:one", "10").await;
	let recorded = ledger_app::transfer(&ports(&h, &grants), &client, sweep(&scope, "alloc:one", "3", &key())).await.unwrap();
	let id = recorded.transfer.id().raw();
	// What an older relay that does not know the kind leaves behind: dispatched, not posted.
	sqlx::query("UPDATE outbox SET dispatched_at = now() WHERE aggregate_id = $1")
		.bind(id)
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("UPDATE scope_transfers SET updated_at = now() - interval '1 hour' WHERE id = $1")
		.bind(id)
		.execute(&h.pool)
		.await
		.unwrap();
	let reaper = Reaper::new(
		h.pool.clone(),
		Arc::new(PgWithdrawals::new(h.pool.clone())),
		Arc::new(PgRedemptions::new(h.pool.clone())),
		h.notify.clone(),
	)
	.with_move_redrive_after(std::time::Duration::from_secs(60));
	assert!(reaper.sweep().await.unwrap().redriven_scope_transfers >= 1);
	assert_eq!(reaper.sweep().await.unwrap().redriven_scope_transfers, 0, "a replay waits for its own event");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(transfer_state(&h, &recorded).await.0, "posted");
	assert_eq!(posted(&h, &scope, "fee").await, usdt("3"));
	// Replaying a posted transfer again cannot post it twice: the ledger id is the transfer's.
	sqlx::query("UPDATE scope_transfers SET state = 'pending', updated_at = now() - interval '1 hour' WHERE id = $1")
		.bind(id)
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(reaper.sweep().await.unwrap().redriven_scope_transfers >= 1);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(transfer_state(&h, &recorded).await.0, "posted");
	assert_eq!(posted(&h, &scope, "fee").await, usdt("3"));
}

#[tokio::test]
async fn reconciliation_alerts_on_value_in_a_scope_account_nobody_owns() {
	let Some(h) = harness().await else { return };
	let reconciliation = Reconciliation::new(h.pool.clone(), h.ledger.clone(), Arc::new(PgAllocations::new(h.pool.clone())));
	let (_, scope, _) = client_with_account(&h, "alloc:", "alloc:owned").await;
	fund(&h, &scope, "alloc:owned", "5").await;
	fund(&h, &scope, "alloc:orphan", "5").await;
	let report = reconciliation.scan().await.unwrap();
	let orphan = format!("scope_account:{scope}:alloc:orphan");
	assert!(report.unowned_scope_accounts.contains(&orphan), "{:?}", report.unowned_scope_accounts);
	assert!(!report.unowned_scope_accounts.contains(&format!("scope_account:{scope}:alloc:owned")));
	assert!(!report.clean());
	assert_eq!(report.custody, report.claims, "the scope accounts are in the cash invariant either way");

	// Owned now, and the finding is gone.
	sqlx::query("INSERT INTO scope_accounts (scope, label, owner_client) VALUES ($1, 'alloc:orphan', 'someone')")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(!reconciliation.scan().await.unwrap().unowned_scope_accounts.contains(&orphan));
}
