//! Payouts out of scope accounts and the operations decided by concierge's approvals
//! (`scope.v1.ScopeLedgerService` Payout / RequestPayout / RequestTransfer /
//! GetPendingOperation, and the approval executor) — real Postgres and TigerBeetle, with an
//! in-process concierge ([`FakeConcierge`]) standing in for the approvals seam. Every query of
//! the payout and operation stores runs here: the ceilings and the scope's budget under their
//! lock, the linked chain the relay posts, the executor's open / re-read / execute / report
//! and the operation's one way out of `awaiting_approval`.
//!
//! Each test works in scopes, clients and users of its own.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};

use async_trait::async_trait;
use domain::{
	balance::{LedgerAccountKey, TransferCode},
	error::DomainError,
	money::{Network, Usdt},
	scope_ledger::{ClientId, Grant, Grants, LedgerRefusal, MoneyEventKind, SubjectRef},
	scope_operations::{ApprovalValue, BANK_CATALOG, BANK_CATALOG_VERSION, KindSpec, OperationKind, OperationState, PayoutReason, RequestedLine},
	scopes::{MoveState, ScopeAccountLabel, ScopeId, ScopeState},
	users::UserId,
};
use piggybank_core::{
	application::{
		scope_ledger::{self as ledger_app, ScopeLedgerPorts, TransferError},
		scope_operations::{self as ops, CatalogGate, ExecutorPorts, ExecutorRound, MAX_LIVE_OPERATIONS, OperationPorts, PayoutRequest, RequestPayoutRequest, RequestTransferRequest},
	},
	infrastructure::{compartments::PgCompartments, relay::Relay, scope_ledger::PgScopeLedger, scope_payouts, telemetry},
	ports::{
		ledger::{Ledger, LedgerTransfer},
		scope_ledger::EventAccount,
		scope_operations::{ApprovalLevel, ApprovalOutcome, ApprovalState, ApprovalView, Approvals, ApprovalsError, Proposal, RecordedOperation},
	},
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

// ── concierge, in process ────────────────────────────────────────────────────

/// What concierge holds about one proposal.
#[derive(Clone, Debug)]
struct Held {
	view: ApprovalView,
	proposal: Proposal,
	reported: Option<(ApprovalOutcome, String)>,
}

#[derive(Default)]
struct FakeState {
	approvals: HashMap<String, Held>,
	by_dedupe: HashMap<String, String>,
	/// Every call refused as unavailable.
	down: bool,
	/// Every `Propose` refused for good.
	refuse_propose: bool,
	/// The hash the next `Propose` answers with instead of its own.
	tamper_open: Option<String>,
	/// `GetProposal` answers this state for the id, whatever the pull serves.
	stale_get: HashMap<String, ApprovalState>,
	/// Serve passed decisions on every pull, reported or not.
	serve_reported: bool,
	reports: Vec<(String, ApprovalOutcome, String)>,
	/// The catalog stored for `bank`: its version and its kinds by name; `None` before the
	/// first publication.
	catalog: Option<(u64, Vec<String>)>,
	/// Every publication that changed the stored catalog.
	published_versions: Vec<u64>,
	/// Publications refused as unavailable before one goes through.
	publish_outages: u32,
	/// Calls in order: `publish` and `propose`.
	calls: Vec<&'static str>,
	/// The next this many `Propose` calls answer RESOURCE_EXHAUSTED.
	throttle_next: u32,
	/// Every `PublishScopeCatalog` refused for good.
	refuse_publish: bool,
}

/// concierge's `ApprovalExecutionService` as banking sees it. The payload hash is computed
/// here independently of banking's encoder, from concierge's doc, so a disagreement in the
/// encoding shows as a mismatch.
#[derive(Default)]
struct FakeConcierge {
	state: Mutex<FakeState>,
}

impl FakeConcierge {
	fn with<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
		f(&mut self.state.lock().unwrap())
	}

	fn set(&self, id: &str, state: ApprovalState) {
		self.with(|s| s.approvals.get_mut(id).unwrap().view.state = state);
	}

	fn tamper(&self, id: &str, hash: &str) {
		self.with(|s| s.approvals.get_mut(id).unwrap().view.payload_hash = hash.to_owned());
	}

	fn proposal(&self, id: &str) -> Proposal {
		self.with(|s| s.approvals[id].proposal.clone())
	}

	fn reports(&self, id: &str) -> Vec<(ApprovalOutcome, String)> {
		self.with(|s| {
			s.reports
				.iter()
				.filter(|(approval, ..)| approval == id)
				.map(|(_, outcome, detail)| (*outcome, detail.clone()))
				.collect()
		})
	}
}

/// concierge's `payload_hash` (its doc, "payload_hash").
fn concierge_hash(proposal: &Proposal) -> String {
	let mut fields: Vec<(String, String, String)> = proposal
		.fields
		.iter()
		.map(|field| {
			let (kind, value) = match &field.value {
				ApprovalValue::Text(text) => ("TEXT", text.clone()),
				ApprovalValue::Amount(amount) => ("AMOUNT", format!("{} USDT", amount.to_decimal_string())),
				ApprovalValue::Flag(flag) => ("FLAG", if *flag { "true".into() } else { "false".into() }),
				ApprovalValue::User(subject) => ("USER", subject.clone()),
				ApprovalValue::Count(count) => ("COUNT", count.to_string()),
			};
			(field.key.to_owned(), kind.to_owned(), value)
		})
		.collect();
	fields.sort();
	let mut bytes = format!("ev-approval-v1\n{}\n{}\n{}\n", proposal.kind, proposal.scope, proposal.subject_ref);
	for (key, kind, value) in fields {
		bytes.push_str(&format!("{key}\t{kind}\t{value}\n"));
	}
	hex::encode(Sha256::digest(bytes.as_bytes()))
}

fn flag(proposal: &Proposal, key: &str) -> bool {
	proposal.fields.iter().any(|field| field.key == key && field.value == ApprovalValue::Flag(true))
}

#[async_trait]
impl Approvals for FakeConcierge {
	async fn propose(&self, proposal: &Proposal) -> Result<ApprovalView, ApprovalsError> {
		self.with(|s| {
			if s.down {
				return Err(ApprovalsError::Unavailable("down".into()));
			}
			s.calls.push("propose");
			if s.throttle_next > 0 {
				s.throttle_next -= 1;
				return Err(ApprovalsError::Throttled("200 live proposals".into()));
			}
			// concierge refuses a kind its namespace's catalog does not hold.
			let known = s.catalog.as_ref().is_some_and(|(_, kinds)| kinds.contains(&proposal.kind));
			if s.refuse_propose || !known {
				return Err(ApprovalsError::Refused("unknown kind".into()));
			}
			if let Some(id) = s.by_dedupe.get(&proposal.dedupe_key) {
				return Ok(s.approvals[id].view.clone());
			}
			// What the floor sends to the owners: a transfer between scopes, a distribution, the
			// scope's budget broken.
			let platform = proposal.kind == "bank:scope_transfer" || flag(proposal, "distribution") || flag(proposal, "over_scope_budget");
			let id = Uuid::new_v4().to_string();
			let view = ApprovalView {
				id: id.clone(),
				kind: proposal.kind.clone(),
				scope: proposal.scope.clone(),
				subject_ref: proposal.subject_ref.clone(),
				state: ApprovalState::Open,
				level: Some(if platform { ApprovalLevel::Platform } else { ApprovalLevel::Scope }),
				payload_hash: s.tamper_open.take().unwrap_or_else(|| concierge_hash(proposal)),
			};
			s.by_dedupe.insert(proposal.dedupe_key.clone(), id.clone());
			s.approvals.insert(
				id,
				Held {
					view: view.clone(),
					proposal: proposal.clone(),
					reported: None,
				},
			);
			Ok(view)
		})
	}

	async fn get(&self, approval_id: &str) -> Result<ApprovalView, ApprovalsError> {
		self.with(|s| {
			if s.down {
				return Err(ApprovalsError::Unavailable("down".into()));
			}
			let mut view = s
				.approvals
				.get(approval_id)
				.map(|held| held.view.clone())
				.ok_or_else(|| ApprovalsError::Refused("not found".into()))?;
			if let Some(state) = s.stale_get.get(approval_id) {
				view.state = *state;
			}
			Ok(view)
		})
	}

	async fn pull_approved(&self, limit: u32) -> Result<Vec<ApprovalView>, ApprovalsError> {
		self.with(|s| {
			if s.down {
				return Err(ApprovalsError::Unavailable("down".into()));
			}
			let serve_reported = s.serve_reported;
			Ok(s.approvals
				.values()
				.filter(|held| held.view.state == ApprovalState::Passed || (serve_reported && held.reported.is_some()))
				.take(limit as usize)
				.map(|held| ApprovalView {
					state: ApprovalState::Passed,
					..held.view.clone()
				})
				.collect())
		})
	}

	async fn publish_catalog(&self, version: u64, kinds: &[KindSpec]) -> Result<(), ApprovalsError> {
		self.with(|s| {
			s.calls.push("publish");
			if s.refuse_publish {
				return Err(ApprovalsError::Refused("version does not supersede the stored one".into()));
			}
			if s.down || s.publish_outages > 0 {
				s.publish_outages = s.publish_outages.saturating_sub(1);
				return Err(ApprovalsError::Unavailable("down".into()));
			}
			let names: Vec<String> = kinds.iter().map(|spec| spec.kind.as_str().to_owned()).collect();
			match &s.catalog {
				// concierge's rule: the same version with the same content is a no-op, with
				// other content or older is refused.
				Some((stored, kinds)) if *stored == version && *kinds == names => Ok(()),
				Some((stored, _)) if *stored >= version => Err(ApprovalsError::Refused(format!("version {version} does not supersede the stored {stored}"))),
				_ => {
					s.catalog = Some((version, names));
					s.published_versions.push(version);
					Ok(())
				}
			}
		})
	}

	async fn report(&self, approval_id: &str, outcome: ApprovalOutcome, detail: &str) -> Result<(), ApprovalsError> {
		self.with(|s| {
			if s.down {
				return Err(ApprovalsError::Unavailable("down".into()));
			}
			s.reports.push((approval_id.to_owned(), outcome, detail.to_owned()));
			let held = s.approvals.get_mut(approval_id).ok_or_else(|| ApprovalsError::Refused("not found".into()))?;
			held.reported = Some((outcome, detail.to_owned()));
			held.view.state = match outcome {
				ApprovalOutcome::Executed => ApprovalState::Executed,
				ApprovalOutcome::Failed => ApprovalState::ExecutionFailed,
				ApprovalOutcome::Void => ApprovalState::Void,
			};
			Ok(())
		})
	}
}

// ── harness ──────────────────────────────────────────────────────────────────

struct Harness {
	pool: PgPool,
	ledger: Arc<dyn Ledger>,
	store: PgScopeLedger,
	scopes: PgCompartments,
	relay: Relay,
	notify: Arc<Notify>,
	executor: Notify,
	concierge: FakeConcierge,
	catalog: CatalogGate,
	_serial: tokio::sync::MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "scope payout tests").await?;
	let notify = Arc::new(Notify::new());
	Some(Harness {
		store: PgScopeLedger::new(pool.clone()),
		scopes: PgCompartments::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(piggybank_core::infrastructure::custody::StubCustody), notify.clone()),
		pool,
		ledger,
		notify,
		executor: Notify::new(),
		concierge: FakeConcierge::default(),
		catalog: CatalogGate::new(),
		_serial: serial,
	})
}

fn base<'a>(h: &'a Harness, grants: &'a Grants) -> ScopeLedgerPorts<'a> {
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

fn ops_ports<'a>(h: &'a Harness, base: &'a ScopeLedgerPorts<'a>) -> OperationPorts<'a> {
	OperationPorts {
		base,
		payouts: &h.store,
		operations: &h.store,
		executor: &h.executor,
	}
}

fn executor<'a>(h: &'a Harness, grants: &'a Grants) -> ExecutorPorts<'a> {
	ExecutorPorts {
		grants,
		operations: &h.store,
		approvals: &h.concierge,
		catalog: &h.catalog,
		ledger: h.ledger.as_ref(),
		relay: &h.notify,
	}
}

async fn round(h: &Harness, grants: &Grants) -> ExecutorRound {
	ops::executor_round(&executor(h, grants)).await.unwrap()
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

fn subject() -> SubjectRef {
	SubjectRef::parse(&format!("redemption:{}", Uuid::new_v4())).unwrap()
}

fn fresh_client() -> ClientId {
	ClientId::parse(&format!("c-{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap()
}

/// A scope of this test's own, active, banking's fee collector open, with the automatic
/// ceilings and the daily budget given (USDT).
async fn own_scope(h: &Harness, per_op: &str, per_day: &str, budget: &str) -> ScopeId {
	let id = ScopeId::parse(&format!("p_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	sqlx::query("INSERT INTO scopes (id, display_name) VALUES ($1, 'Payout scope')")
		.bind(id.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("INSERT INTO scope_policies (scope, auto_payout_per_op, auto_payout_24h, scope_budget_24h) VALUES ($1, $2, $3, $4)")
		.bind(id.as_str())
		.bind(usdt(per_op).base_units().to_string())
		.bind(usdt(per_day).base_units().to_string())
		.bind(usdt(budget).base_units().to_string())
		.execute(&h.pool)
		.await
		.unwrap();
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

/// `client` opens `account` under `grants` and it is funded with `amount` straight on the
/// ledger (custody grows with it, so the cash invariant holds); `"0"` only opens it.
async fn funded(h: &Harness, grants: &Grants, client: &ClientId, scope: &ScopeId, account: &str, amount: &str) {
	ledger_app::open_account(&base(h, grants), client, scope, &label(account), &key()).await.unwrap();
	if usdt(amount).is_zero() {
		return;
	}
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

/// A user banking knows, with an empty wallet. Returns the id and the `auth_subject`.
async fn user(h: &Harness) -> (UserId, String) {
	let id = UserId::new();
	let auth_subject = format!("itest-{}", Uuid::new_v4());
	sqlx::query("INSERT INTO users (id, auth_subject, email) VALUES ($1, $2, $3)")
		.bind(id.raw())
		.bind(&auth_subject)
		.bind(format!("{}@example.com", Uuid::new_v4().simple()))
		.execute(&h.pool)
		.await
		.unwrap();
	(id, auth_subject)
}

async fn compartment(h: &Harness, user: UserId, scope: &ScopeId) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(&LedgerAccountKey::ScopeClaim(user, scope.clone())).await.unwrap().posted)
}

async fn account(h: &Harness, scope: &ScopeId, raw: &str) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(&LedgerAccountKey::ScopeAccount(scope.clone(), label(raw))).await.unwrap().posted)
}

fn line(subject: &str, amount: &str) -> RequestedLine {
	RequestedLine {
		auth_subject: subject.to_owned(),
		amount: usdt(amount),
	}
}

fn payout_request(scope: &ScopeId, from: &str, lines: Vec<RequestedLine>, idempotency_key: &str) -> PayoutRequest {
	PayoutRequest {
		scope: scope.clone(),
		from: label(from),
		lines,
		reason: PayoutReason::Redemption,
		subject_ref: SubjectRef::parse("redemption:4f0c2d1e-0000-4000-8000-0123456789ab").unwrap(),
		memo: String::new(),
		idempotency_key: idempotency_key.to_owned(),
	}
}

fn request_payout(scope: &ScopeId, from: &str, lines: Vec<RequestedLine>, idempotency_key: &str) -> RequestPayoutRequest {
	RequestPayoutRequest {
		scope: scope.clone(),
		from: label(from),
		lines,
		distribution: false,
		subject_ref: SubjectRef::parse("dividend:4f0c2d1e-0000-4000-8000-0123456789ab").unwrap(),
		memo: "Quarterly dividend".into(),
		idempotency_key: idempotency_key.to_owned(),
	}
}

fn request_transfer(scope: &ScopeId, from: &str, to_scope: Option<&ScopeId>, to: &str, amount: &str) -> RequestTransferRequest {
	RequestTransferRequest {
		scope: scope.clone(),
		from: label(from),
		to_scope: to_scope.cloned(),
		to: label(to),
		amount: usdt(amount),
		subject_ref: subject(),
		memo: String::new(),
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

async fn payout_rows(h: &Harness, scope: &ScopeId) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM scope_payouts WHERE scope = $1")
		.bind(scope.as_str())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

async fn reload(h: &Harness, operation: &RecordedOperation) -> RecordedOperation {
	use piggybank_core::ports::scope_operations::OperationStore;
	h.store.operation_by_id(operation.operation.id().raw()).await.unwrap().unwrap()
}

fn approval_id(operation: &RecordedOperation) -> String {
	operation.operation.progress().approval_id.clone().expect("the approval was opened")
}

/// A client granted `alloc:` with the compartments flag in a fresh scope.
fn payer(scope: &ScopeId) -> (ClientId, Grants) {
	let client = fresh_client();
	let grants = Grants::new(vec![grant(&client, scope, "alloc:", true)]).unwrap();
	(client, grants)
}

// ── automatic payouts ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_payout_inside_the_ceilings_posts_at_once_as_one_chain() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "800").await;
	let (alice_id, alice) = user(&h).await;
	let (bob_id, bob) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);

	let k = key();
	let paid = ops::payout(&p, &client, payout_request(&scope, "alloc:x", vec![line(&alice, "300"), line(&bob, "200")], &k))
		.await
		.unwrap();
	assert_eq!((paid.state, paid.payout.total()), (MoveState::Pending, usdt("500")));
	// Before the relay applied it, the money is already spoken for.
	assert_eq!(
		refusal(ops::payout(&p, &client, payout_request(&scope, "alloc:x", vec![line(&alice, "300.000000000000000001")], &key())).await),
		LedgerRefusal::InsufficientFunds
	);
	// The same lines in another order are the same request; other lines under the key are not.
	let repeat = ops::payout(&p, &client, payout_request(&scope, "alloc:x", vec![line(&bob, "200"), line(&alice, "300")], &k))
		.await
		.unwrap();
	assert_eq!(repeat.payout.id(), paid.payout.id(), "a repeat is the original payout");
	assert!(matches!(
		domain_err(ops::payout(&p, &client, payout_request(&scope, "alloc:x", vec![line(&alice, "300")], &k)).await),
		DomainError::Conflict(_)
	));

	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(compartment(&h, alice_id, &scope).await, usdt("300"));
	assert_eq!(compartment(&h, bob_id, &scope).await, usdt("200"));
	assert_eq!(account(&h, &scope, "alloc:x").await, usdt("300"));
	let state: String = sqlx::query_scalar("SELECT state FROM scope_payouts WHERE id = $1")
		.bind(paid.payout.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(state, "posted");

	// One PAID_OUT per line, both to the payer, each with its own account's balance after.
	let pulled = ledger_app::pull(&b, &client, Some(&scope), 0, 100).await.unwrap();
	let paid_out: Vec<_> = pulled.events.iter().filter(|event| event.kind == MoneyEventKind::PaidOut).collect();
	assert_eq!(paid_out.len(), 2);
	for event in &paid_out {
		assert_eq!(event.ref_id, paid.payout.id().to_string());
		assert_eq!(event.from, Some(EventAccount::Account(label("alloc:x"))));
		assert_eq!(event.balances.len(), 1);
		assert_eq!(event.balances[0].balance, usdt("300"));
	}
	let mut to: Vec<_> = paid_out.iter().map(|event| (event.to.clone(), event.amount)).collect();
	to.sort_by_key(|(_, amount)| *amount);
	assert_eq!(to, vec![(Some(EventAccount::User(bob)), Some(usdt("200"))), (Some(EventAccount::User(alice)), Some(usdt("300")))]);
}

#[tokio::test]
async fn a_payout_past_a_ceiling_or_the_scope_budget_is_approval_required() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "100", "250", "400").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "1000").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);
	let pay = |amount: &str| payout_request(&scope, "alloc:x", vec![line(&alice, amount)], &key());

	assert_eq!(refusal(ops::payout(&p, &client, pay("100.000000000000000001")).await), LedgerRefusal::ApprovalRequired);
	ops::payout(&p, &client, pay("100")).await.unwrap();
	ops::payout(&p, &client, pay("100")).await.unwrap();
	assert_eq!(
		refusal(ops::payout(&p, &client, pay("50.000000000000000001")).await),
		LedgerRefusal::ApprovalRequired,
		"the client's 24 h"
	);
	ops::payout(&p, &client, pay("50")).await.unwrap();

	// The budget is the scope's, whatever client draws on it: 250 out already, 150 left.
	let other = fresh_client();
	let both = Grants::new(vec![grant(&client, &scope, "alloc:", true), grant(&other, &scope, "panel:", true)]).unwrap();
	funded(&h, &both, &other, &scope, "panel:cash", "1000").await;
	let b2 = base(&h, &both);
	let p2 = ops_ports(&h, &b2);
	let theirs = |amount: &str| payout_request(&scope, "panel:cash", vec![line(&alice, amount)], &key());
	ops::payout(&p2, &other, theirs("100")).await.unwrap();
	assert_eq!(
		refusal(ops::payout(&p2, &other, theirs("50.000000000000000001")).await),
		LedgerRefusal::ApprovalRequired,
		"the scope's budget"
	);
	ops::payout(&p2, &other, theirs("50")).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

/// banking#525: one recipient cannot take a whole operation that fits every other ceiling.
/// An automatic payout with a line past 1000 USDT needs the owners; a requested one is
/// escalated by the `per_line` rule on the `max_line` field every approval carries.
#[tokio::test]
async fn a_line_past_the_per_line_ceiling_needs_the_owners() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "5000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "5000").await;
	let (_, alice) = user(&h).await;
	let (_, bob) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);

	assert_eq!(
		refusal(
			ops::payout(
				&p,
				&client,
				payout_request(&scope, "alloc:x", vec![line(&alice, "1000.000000000000000001"), line(&bob, "1")], &key())
			)
			.await
		),
		LedgerRefusal::ApprovalRequired,
		"the total is inside every ceiling, the line is not"
	);
	ops::payout(&p, &client, payout_request(&scope, "alloc:x", vec![line(&alice, "1000"), line(&bob, "1000")], &key()))
		.await
		.expect("the ceiling is inclusive, and a total of 2000 is under the per-operation one here");

	let asked = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1500"), line(&bob, "1")], &key()))
		.await
		.unwrap();
	round(&h, &grants).await;
	let asked = reload(&h, &asked).await;
	let proposal = h.concierge.proposal(&approval_id(&asked));
	assert!(
		proposal.fields.iter().any(|f| f.key == "max_line" && f.value == ApprovalValue::Amount(usdt("1500"))),
		"the approvers and the escalation rule see the largest line"
	);
	let spec = BANK_CATALOG.iter().find(|k| k.kind == OperationKind::ScopePayout).unwrap();
	assert!(
		spec.escalations
			.iter()
			.any(|e| e.parameter == "per_line" && e.field_key == "max_line" && e.default_threshold == "1000"),
		"the published catalog escalates a line past 1000 to the owners"
	);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

/// Ten payouts at once under one client's 24 h ceiling of three: each holds the scope's
/// lock while it counts, so exactly three get through, whoever wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_payouts_never_break_the_24h_ceiling() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "100", "300", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "5000").await;
	let (_, alice) = user(&h).await;

	let mut tasks = tokio::task::JoinSet::new();
	for _ in 0..10 {
		let (pool, ledger, grants, client, scope, alice) = (h.pool.clone(), h.ledger.clone(), grants.clone(), client.clone(), scope.clone(), alice.clone());
		tasks.spawn(async move {
			let store = PgScopeLedger::new(pool.clone());
			let scopes = PgCompartments::new(pool);
			let (relay, executor) = (Notify::new(), Notify::new());
			let b = ScopeLedgerPorts {
				grants: &grants,
				store: &store,
				holds: &store,
				events: &store,
				scopes: &scopes,
				ledger: ledger.as_ref(),
				relay: &relay,
			};
			let p = OperationPorts {
				base: &b,
				payouts: &store,
				operations: &store,
				executor: &executor,
			};
			ops::payout(&p, &client, payout_request(&scope, "alloc:x", vec![line(&alice, "100")], &key())).await.map(|_| ())
		});
	}
	let mut admitted = 0;
	while let Some(joined) = tasks.join_next().await {
		match joined.unwrap() {
			Ok(()) => admitted += 1,
			Err(TransferError::Refused(LedgerRefusal::ApprovalRequired)) => {}
			Err(other) => panic!("unexpected: {other:?}"),
		}
	}
	assert_eq!(admitted, 3);
	assert_eq!(payout_rows(&h, &scope).await, 3);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(account(&h, &scope, "alloc:x").await, usdt("4700"));
}

#[tokio::test]
async fn a_payout_is_refused_outside_the_callers_scope_and_accounts() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "10").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);
	let pay = |scope: &ScopeId, from: &str| payout_request(scope, from, vec![line(&alice, "1")], &key());

	let elsewhere = own_scope(&h, "1000", "5000", "5000").await;
	for scope in [ScopeId::main(), elsewhere] {
		assert_eq!(refusal(ops::payout(&p, &client, pay(&scope, "alloc:x")).await), LedgerRefusal::NotGranted);
	}
	let unknown = ScopeId::parse(&format!("u_{}", &Uuid::new_v4().simple().to_string()[..12])).unwrap();
	let granted_unknown = Grants::new(vec![grant(&client, &unknown, "alloc:", true)]).unwrap();
	let bu = base(&h, &granted_unknown);
	assert!(matches!(
		domain_err(ops::payout(&ops_ports(&h, &bu), &client, pay(&unknown, "alloc:x")).await),
		DomainError::NotFound { .. }
	));

	// Someone else's account sits under someone else's prefix; an unopened label under the
	// caller's own is not the caller's either; banking's labels never are.
	let other = fresh_client();
	let both = Grants::new(vec![grant(&client, &scope, "alloc:", true), grant(&other, &scope, "panel:", true)]).unwrap();
	funded(&h, &both, &other, &scope, "panel:cash", "10").await;
	let bb = base(&h, &both);
	let pb = ops_ports(&h, &bb);
	assert_eq!(refusal(ops::payout(&pb, &client, pay(&scope, "panel:cash")).await), LedgerRefusal::NotGranted);
	assert_eq!(refusal(ops::payout(&pb, &client, pay(&scope, "alloc:never")).await), LedgerRefusal::NotAccountOwner);
	assert_eq!(refusal(ops::payout(&pb, &client, pay(&scope, "fee")).await), LedgerRefusal::NotAccountOwner);

	let accounts_only = Grants::new(vec![grant(&client, &scope, "alloc:", false)]).unwrap();
	let ba = base(&h, &accounts_only);
	assert_eq!(
		refusal(ops::payout(&ops_ports(&h, &ba), &client, pay(&scope, "alloc:x")).await),
		LedgerRefusal::NotGranted,
		"users' compartments need the compartments flag"
	);
	let stranger = format!("itest-{}", Uuid::new_v4());
	assert!(matches!(
		domain_err(ops::payout(&p, &client, payout_request(&scope, "alloc:x", vec![line(&stranger, "1")], &key())).await),
		DomainError::NotFound { entity: "user", .. }
	));
	let auto = PayoutRequest {
		reason: PayoutReason::Approved,
		..pay(&scope, "alloc:x")
	};
	assert!(
		matches!(domain_err(ops::payout(&p, &client, auto).await), DomainError::Validation(_)),
		"approved is banking's reason"
	);

	sqlx::query("UPDATE scopes SET state = $2 WHERE id = $1")
		.bind(scope.as_str())
		.bind(ScopeState::Frozen.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	assert_eq!(refusal(ops::payout(&p, &client, pay(&scope, "alloc:x")).await), LedgerRefusal::ScopeFrozen);
	assert_eq!(payout_rows(&h, &scope).await, 0, "no refusal writes anything");
}

// ── operations decided by an approval ────────────────────────────────────────

#[tokio::test]
async fn a_requested_payout_executes_once_and_only_after_its_approval_passes() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "3000").await;
	let (alice_id, alice) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);

	let k = key();
	let asked = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1500")], &k)).await.unwrap();
	assert_eq!(asked.operation.state(), OperationState::AwaitingApproval);
	assert_eq!(asked.operation.kind(), OperationKind::ScopePayout);
	assert!(asked.operation.progress().approval_id.is_none(), "opened by the executor, not on the request path");
	let repeat = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1500")], &k)).await.unwrap();
	assert_eq!(repeat.operation.id(), asked.operation.id());

	assert!(round(&h, &grants).await.proposed >= 1);
	let asked = reload(&h, &asked).await;
	let approval = approval_id(&asked);
	let proposal = h.concierge.proposal(&approval);
	assert_eq!(
		proposal.subject_ref,
		format!("pending_operation:{}", asked.operation.id()),
		"banking's own ref, never the caller's"
	);
	assert_eq!(proposal.dedupe_key, proposal.subject_ref);
	assert!(proposal.fields.iter().any(|field| field.key == "recipient" && field.value == ApprovalValue::User(alice.clone())));
	assert_eq!(asked.payload_hash, concierge_hash(&proposal), "banking's payload_hash is concierge's");
	assert_eq!(asked.approval_level, Some(ApprovalLevel::Scope));

	// Open: nothing moves however often the executor runs.
	round(&h, &grants).await;
	assert_eq!(payout_rows(&h, &scope).await, 0);

	h.concierge.set(&approval, ApprovalState::Passed);
	round(&h, &grants).await;
	let done = reload(&h, &asked).await;
	assert_eq!(done.operation.state(), OperationState::Executed);
	let result = done.operation.progress().result_id.clone().unwrap();
	assert_eq!(h.concierge.reports(&approval), vec![(ApprovalOutcome::Executed, String::new())]);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(compartment(&h, alice_id, &scope).await, usdt("1500"));
	assert_eq!(account(&h, &scope, "alloc:x").await, usdt("1500"));

	// Served again (the report did not reach concierge, say): reported again, never re-run.
	h.concierge.with(|s| s.serve_reported = true);
	round(&h, &grants).await;
	assert_eq!(payout_rows(&h, &scope).await, 1, "one approval, one execution");
	assert_eq!(h.concierge.reports(&approval).len(), 2);

	let pulled = ledger_app::pull(&b, &client, Some(&scope), 0, 100).await.unwrap();
	let requested = pulled.events.iter().find(|event| event.kind == MoneyEventKind::OperationRequested).expect("OPERATION_REQUESTED");
	assert_eq!(requested.ref_id, asked.operation.id().to_string());
	assert_eq!(requested.subject_ref, "dividend:4f0c2d1e-0000-4000-8000-0123456789ab");
	let paid = pulled.events.iter().find(|event| event.kind == MoneyEventKind::PaidOut).expect("PAID_OUT");
	assert_eq!(
		(paid.ref_id.as_str(), paid.subject_ref.as_str()),
		(result.as_str(), "dividend:4f0c2d1e-0000-4000-8000-0123456789ab")
	);

	let seen = ops::get_pending_operation(&p, &client, &scope, asked.operation.id().raw()).await.unwrap();
	assert_eq!(seen.operation.state(), OperationState::Executed);
	let stranger = fresh_client();
	let theirs = Grants::new(vec![grant(&stranger, &scope, "other:", true)]).unwrap();
	let bs = base(&h, &theirs);
	assert!(
		matches!(
			domain_err(ops::get_pending_operation(&ops_ports(&h, &bs), &stranger, &scope, asked.operation.id().raw()).await),
			DomainError::NotFound { .. }
		),
		"another client's operation is the same NOT_FOUND as none"
	);
}

#[tokio::test]
async fn a_rejected_vetoed_or_expired_approval_never_executes() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "9000").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);

	let mut asked = Vec::new();
	for _ in 0..3 {
		asked.push(
			ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1200")], &key()))
				.await
				.unwrap(),
		);
	}
	assert!(round(&h, &grants).await.proposed >= 3);
	let ends = [ApprovalState::Rejected, ApprovalState::Vetoed, ApprovalState::Expired];
	for (operation, end) in asked.iter().zip(ends) {
		h.concierge.set(&approval_id(&reload(&h, operation).await), end);
	}
	// Due for a check only after the minute an open approval waits; bring it forward.
	sqlx::query("UPDATE scope_operations SET next_attempt_at = now() WHERE scope = $1")
		.bind(scope.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	// The executor serves every operation in the database, other tests' included: counts are
	// at least this test's, and its own operations are checked one by one.
	let settled = round(&h, &grants).await;
	assert!(settled.rejected >= 3, "{settled:?}");
	for (operation, end) in asked.iter().zip(ends) {
		let ended = reload(&h, operation).await;
		assert_eq!(ended.operation.state(), OperationState::Rejected);
		assert_eq!(ended.operation.progress().failure_reason.as_deref(), Some(format!("approval_{}", end.as_str()).as_str()));
	}
	assert_eq!(payout_rows(&h, &scope).await, 0);
	let pulled = ledger_app::pull(&b, &client, Some(&scope), 0, 100).await.unwrap();
	assert_eq!(pulled.events.iter().filter(|event| event.kind == MoneyEventKind::OperationRejected).count(), 3);
}

#[tokio::test]
async fn a_decision_that_is_not_the_stored_operations_is_refused() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "9000").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);

	// A passed decision whose digest is not the stored operation's: nothing moves, the
	// operation fails for good and the decision is reported VOID.
	let swapped = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1200")], &key()))
		.await
		.unwrap();
	round(&h, &grants).await;
	let approval = approval_id(&reload(&h, &swapped).await);
	h.concierge.tamper(&approval, &"0".repeat(64));
	h.concierge.set(&approval, ApprovalState::Passed);
	round(&h, &grants).await;
	let ended = reload(&h, &swapped).await;
	assert_eq!(ended.operation.state(), OperationState::Failed);
	assert_eq!(ended.operation.progress().failure_reason.as_deref(), Some("approval_mismatch"));
	assert_eq!(h.concierge.reports(&approval)[0].0, ApprovalOutcome::Void);

	// An approval opened over another payload is refused before anyone votes.
	h.concierge.with(|s| s.tamper_open = Some("f".repeat(64)));
	let misopened = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1300")], &key()))
		.await
		.unwrap();
	round(&h, &grants).await;
	assert_eq!(reload(&h, &misopened).await.operation.progress().failure_reason.as_deref(), Some("approval_mismatch"));

	// Pulled as passed, but VOID by the time banking re-reads it: not executed.
	let raced = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1400")], &key()))
		.await
		.unwrap();
	round(&h, &grants).await;
	let raced_approval = approval_id(&reload(&h, &raced).await);
	h.concierge.set(&raced_approval, ApprovalState::Passed);
	h.concierge.with(|s| s.stale_get.insert(raced_approval.clone(), ApprovalState::Void));
	round(&h, &grants).await;
	assert_eq!(reload(&h, &raced).await.operation.state(), OperationState::AwaitingApproval);
	assert_eq!(payout_rows(&h, &scope).await, 0);
	// Its next check reads it VOID and ends it.
	sqlx::query("UPDATE scope_operations SET next_attempt_at = now() WHERE id = $1")
		.bind(raced.operation.id().raw())
		.execute(&h.pool)
		.await
		.unwrap();
	round(&h, &grants).await;
	let raced = reload(&h, &raced).await;
	assert_eq!(raced.operation.state(), OperationState::Rejected);
	assert_eq!(raced.operation.progress().failure_reason.as_deref(), Some("approval_void"));

	// Concierge refusing to open one ends it; concierge being away only puts it off.
	h.concierge.with(|s| s.refuse_propose = true);
	let refused = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1")], &key()))
		.await
		.unwrap();
	round(&h, &grants).await;
	assert_eq!(reload(&h, &refused).await.operation.progress().failure_reason.as_deref(), Some("proposal_refused"));
	h.concierge.with(|s| {
		s.refuse_propose = false;
		s.down = true;
	});
	let deferred = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "2")], &key()))
		.await
		.unwrap();
	round(&h, &grants).await;
	let later = reload(&h, &deferred).await;
	assert_eq!((later.operation.state(), later.attempts), (OperationState::AwaitingApproval, 1));
}

#[tokio::test]
async fn an_approved_transfer_crosses_scopes_and_one_the_source_cannot_cover_fails() {
	let Some(h) = harness().await else { return };
	let invest = own_scope(&h, "1000", "5000", "5000").await;
	let sa = own_scope(&h, "1000", "5000", "5000").await;
	let client = fresh_client();
	let grants = Grants::new(vec![grant(&client, &invest, "alloc:", false), grant(&client, &sa, "alloc:", false)]).unwrap();
	funded(&h, &grants, &client, &invest, "alloc:x", "2000").await;
	funded(&h, &grants, &client, &sa, "alloc:x", "0").await;
	funded(&h, &grants, &client, &invest, "alloc:y", "0").await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);

	let across = ops::request_transfer(&p, &client, request_transfer(&invest, "alloc:x", Some(&sa), "alloc:x", "1500"))
		.await
		.unwrap();
	assert_eq!(across.operation.kind(), OperationKind::ScopeTransfer);
	let inside = ops::request_transfer(&p, &client, request_transfer(&invest, "alloc:x", None, "alloc:y", "1000")).await.unwrap();
	assert_eq!(inside.operation.kind(), OperationKind::AccountTransfer);
	round(&h, &grants).await;
	let across_approval = approval_id(&reload(&h, &across).await);
	assert_eq!(reload(&h, &across).await.approval_level, Some(ApprovalLevel::Platform));
	h.concierge.set(&across_approval, ApprovalState::Passed);
	round(&h, &grants).await;
	assert_eq!(reload(&h, &across).await.operation.state(), OperationState::Executed);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!((account(&h, &invest, "alloc:x").await, account(&h, &sa, "alloc:x").await), (usdt("500"), usdt("1500")));

	// One event per side, each in its own scope, naming the other.
	let pulled = ledger_app::pull(&b, &client, None, 0, 100).await.unwrap();
	let sides: Vec<_> = pulled.events.iter().filter(|event| event.kind == MoneyEventKind::Transferred).collect();
	assert_eq!(sides.len(), 2);
	let source = sides.iter().find(|event| event.scope == invest).unwrap();
	let target = sides.iter().find(|event| event.scope == sa).unwrap();
	assert_eq!((source.counterparty_scope.as_str(), target.counterparty_scope.as_str()), (sa.as_str(), invest.as_str()));
	assert_eq!(source.balances.iter().map(|entry| entry.balance).collect::<Vec<_>>(), vec![usdt("500")]);
	assert_eq!(target.balances.iter().map(|entry| entry.balance).collect::<Vec<_>>(), vec![usdt("1500")]);

	// 500 left: the 1000 inside the scope fails, for good, and says why.
	let inside_approval = approval_id(&reload(&h, &inside).await);
	h.concierge.set(&inside_approval, ApprovalState::Passed);
	round(&h, &grants).await;
	let failed = reload(&h, &inside).await;
	assert_eq!(failed.operation.progress().failure_reason.as_deref(), Some("insufficient_funds"));
	assert_eq!(h.concierge.reports(&inside_approval), vec![(ApprovalOutcome::Failed, "insufficient_funds".to_owned())]);
	let pulled = ledger_app::pull(&b, &client, Some(&invest), 0, 100).await.unwrap();
	assert!(
		pulled
			.events
			.iter()
			.any(|event| event.kind == MoneyEventKind::OperationFailed && event.failure_reason == "insufficient_funds")
	);
}

#[tokio::test]
async fn a_request_names_only_the_callers_own_accounts_and_answers_its_key() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let client = fresh_client();
	let other = fresh_client();
	let grants = Grants::new(vec![grant(&client, &scope, "alloc:", false), grant(&other, &scope, "panel:", false)]).unwrap();
	funded(&h, &grants, &client, &scope, "alloc:x", "10").await;
	funded(&h, &grants, &other, &scope, "panel:cash", "10").await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);
	let (_, alice) = user(&h).await;

	assert_eq!(
		refusal(ops::request_transfer(&p, &client, request_transfer(&scope, "alloc:x", None, "fee", "1")).await),
		LedgerRefusal::NotAccountOwner
	);
	assert_eq!(
		refusal(ops::request_transfer(&p, &client, request_transfer(&scope, "alloc:x", None, "panel:cash", "1")).await),
		LedgerRefusal::NotAccountOwner
	);
	assert_eq!(
		refusal(ops::request_transfer(&p, &client, request_transfer(&scope, "panel:cash", None, "alloc:x", "1")).await),
		LedgerRefusal::NotAccountOwner
	);
	assert_eq!(
		refusal(ops::request_transfer(&p, &client, request_transfer(&scope, "alloc:x", Some(&ScopeId::main()), "treasury", "1")).await),
		LedgerRefusal::NotAccountOwner
	);
	assert_eq!(
		refusal(ops::request_transfer(&p, &client, request_transfer(&ScopeId::main(), "alloc:x", None, "alloc:y", "1")).await),
		LedgerRefusal::NotGranted
	);
	assert_eq!(
		refusal(ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1")], &key())).await),
		LedgerRefusal::NotGranted,
		"a payout to users needs the compartments flag even to be asked for"
	);
	funded(&h, &grants, &client, &scope, "alloc:y", "0").await;
	let mut asked = request_transfer(&scope, "alloc:x", None, "alloc:y", "5");
	let first = ops::request_transfer(&p, &client, asked.clone()).await.unwrap();
	assert_eq!(ops::request_transfer(&p, &client, asked.clone()).await.unwrap().operation.id(), first.operation.id());
	asked.amount = usdt("6");
	assert!(matches!(domain_err(ops::request_transfer(&p, &client, asked).await), DomainError::Conflict(_)));
}

#[tokio::test]
async fn the_scope_budget_sends_a_requested_payout_to_the_owners() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "1000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "5000").await;
	let (_, alice) = user(&h).await;
	let (_, bob) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);

	ops::payout(&p, &client, payout_request(&scope, "alloc:x", vec![line(&alice, "800")], &key())).await.unwrap();
	let within = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&bob, "200")], &key()))
		.await
		.unwrap();
	let flagged = |operation: &RecordedOperation| match &operation.operation.terms().body {
		domain::scope_operations::OperationBody::Payout { over_scope_budget, .. } => *over_scope_budget,
		domain::scope_operations::OperationBody::Transfer { .. } => unreachable!(),
	};
	assert!(!flagged(&within), "800 + 200 is the budget, not past it");
	// The live request already draws on the budget: the next one breaks it.
	let past = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&bob, "1")], &key())).await.unwrap();
	assert!(flagged(&past));
	// And so does an automatic payout now.
	assert_eq!(
		refusal(ops::payout(&p, &client, payout_request(&scope, "alloc:x", vec![line(&alice, "1")], &key())).await),
		LedgerRefusal::ApprovalRequired
	);
	round(&h, &grants).await;
	let past = reload(&h, &past).await;
	assert_eq!(past.approval_level, Some(ApprovalLevel::Platform), "the flag escalates it");
	assert!(flag(&h.concierge.proposal(&approval_id(&past)), "over_scope_budget"));
	// Once decided by the owners, it no longer draws on the scope's budget; the scope-level
	// one still does.
	assert_eq!(reload(&h, &within).await.approval_level, Some(ApprovalLevel::Scope));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn reconciliation_counts_scope_money_stuck_in_flight() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "10").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let before = scope_payouts::stale_postings(&h.pool, std::time::Duration::ZERO).await.unwrap();
	ops::payout(&ops_ports(&h, &b), &client, payout_request(&scope, "alloc:x", vec![line(&alice, "1")], &key()))
		.await
		.unwrap();
	assert_eq!(scope_payouts::stale_postings(&h.pool, std::time::Duration::ZERO).await.unwrap(), before + 1);
	telemetry::note_stale_scope_postings(before + 1);
	assert_eq!(telemetry::stale_scope_postings(), u64::try_from(before + 1).unwrap());
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(scope_payouts::stale_postings(&h.pool, std::time::Duration::ZERO).await.unwrap(), before);
}

#[tokio::test]
async fn the_catalog_is_published_before_the_first_proposal_and_once_per_version() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "3000").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);
	let asked = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1500")], &key()))
		.await
		.unwrap();

	// concierge away at start: nothing is proposed, nothing is lost, the service runs on.
	h.concierge.with(|s| s.publish_outages = 1);
	round(&h, &grants).await;
	assert_eq!(h.concierge.with(|s| s.calls.clone()), vec!["publish"], "no proposal before the catalog");
	assert_eq!(reload(&h, &asked).await.operation.state(), OperationState::AwaitingApproval);

	// The next try waits out its backoff; once it publishes, the proposal follows.
	let quick = CatalogGate::new();
	let ports = ExecutorPorts {
		catalog: &quick,
		..executor(&h, &grants)
	};
	ops::executor_round(&ports).await.unwrap();
	let calls = h.concierge.with(|s| s.calls.clone());
	assert_eq!(&calls[1..3], ["publish", "propose"], "{calls:?}");
	assert!(reload(&h, &asked).await.operation.progress().approval_id.is_some());
	let stored = h.concierge.with(|s| s.catalog.clone()).unwrap();
	assert_eq!(stored.0, BANK_CATALOG_VERSION);
	assert_eq!(stored.1, BANK_CATALOG.iter().map(|spec| spec.kind.as_str().to_owned()).collect::<Vec<_>>());

	// A restart publishes again, and concierge keeps the one version.
	let restarted = CatalogGate::new();
	ops::executor_round(&ExecutorPorts {
		catalog: &restarted,
		..executor(&h, &grants)
	})
	.await
	.unwrap();
	ops::executor_round(&ExecutorPorts {
		catalog: &restarted,
		..executor(&h, &grants)
	})
	.await
	.unwrap();
	assert_eq!(h.concierge.with(|s| s.published_versions.clone()), vec![BANK_CATALOG_VERSION]);
	assert_eq!(
		h.concierge.with(|s| s.calls.iter().filter(|call| **call == "publish").count()),
		3,
		"once per start, not per round"
	);
}

async fn freeze_user(h: &Harness, user: UserId) {
	sqlx::query("UPDATE users SET frozen = true WHERE id = $1").bind(user.raw()).execute(&h.pool).await.unwrap();
}

async fn set_scope_state(h: &Harness, scope: &ScopeId, state: ScopeState) {
	sqlx::query("UPDATE scopes SET state = $2 WHERE id = $1")
		.bind(scope.as_str())
		.bind(state.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// D24(7): a frozen user takes no money into a compartment — not by an automatic payout, not
/// by a request, and not by an approved payout whose user was frozen after it was asked.
#[tokio::test]
async fn no_payout_credits_a_frozen_user() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "5000").await;
	let (frozen_id, frozen) = user(&h).await;
	let (later_id, later) = user(&h).await;
	freeze_user(&h, frozen_id).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);

	assert_eq!(
		refusal(ops::payout(&p, &client, payout_request(&scope, "alloc:x", vec![line(&frozen, "1")], &key())).await),
		LedgerRefusal::UserFrozen
	);
	assert_eq!(
		refusal(ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&frozen, "1200")], &key())).await),
		LedgerRefusal::UserFrozen
	);

	let asked = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&later, "1200")], &key()))
		.await
		.unwrap();
	round(&h, &grants).await;
	let approval = approval_id(&reload(&h, &asked).await);
	freeze_user(&h, later_id).await;
	h.concierge.set(&approval, ApprovalState::Passed);
	round(&h, &grants).await;
	let ended = reload(&h, &asked).await;
	assert_eq!(ended.operation.state(), OperationState::Failed);
	assert_eq!(ended.operation.progress().failure_reason.as_deref(), Some("user_frozen"));
	assert_eq!(h.concierge.reports(&approval), vec![(ApprovalOutcome::Failed, "user_frozen".to_owned())]);
	assert_eq!(payout_rows(&h, &scope).await, 0);
}

/// The approval decided the money; the client's reach into users' compartments is banking's
/// configuration as it stands when the payout executes.
#[tokio::test]
async fn an_approved_payout_fails_when_the_compartments_grant_is_gone() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "5000").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let asked = ops::request_payout(&ops_ports(&h, &b), &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1200")], &key()))
		.await
		.unwrap();
	round(&h, &grants).await;
	let approval = approval_id(&reload(&h, &asked).await);
	h.concierge.set(&approval, ApprovalState::Passed);
	let accounts_only = Grants::new(vec![grant(&client, &scope, "alloc:", false)]).unwrap();
	round(&h, &accounts_only).await;
	let ended = reload(&h, &asked).await;
	assert_eq!(ended.operation.progress().failure_reason.as_deref(), Some("not_granted"));
	assert_eq!(payout_rows(&h, &scope).await, 0);
}

#[tokio::test]
async fn requests_are_bounded_per_client_and_scope_and_need_cover_now() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "100").await;
	funded(&h, &grants, &client, &scope, "alloc:y", "0").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);

	assert_eq!(
		refusal(ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "100.000000000000000001")], &key())).await),
		LedgerRefusal::InsufficientFunds
	);
	assert_eq!(
		refusal(ops::request_transfer(&p, &client, request_transfer(&scope, "alloc:x", None, "alloc:y", "101")).await),
		LedgerRefusal::InsufficientFunds
	);
	for _ in 0..MAX_LIVE_OPERATIONS {
		ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1")], &key()))
			.await
			.unwrap();
	}
	assert!(matches!(
		ops::request_transfer(&p, &client, request_transfer(&scope, "alloc:x", None, "alloc:y", "1")).await,
		Err(TransferError::Exhausted(_))
	));
	// Another client of the scope has room of its own.
	let other = fresh_client();
	let both = Grants::new(vec![grant(&client, &scope, "alloc:", true), grant(&other, &scope, "panel:", true)]).unwrap();
	funded(&h, &both, &other, &scope, "panel:cash", "10").await;
	let bb = base(&h, &both);
	ops::request_payout(&ops_ports(&h, &bb), &other, request_payout(&scope, "panel:cash", vec![line(&alice, "1")], &key()))
		.await
		.unwrap();
}

#[tokio::test]
async fn the_executor_proposes_around_a_throttled_one_and_ends_one_never_opened() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "9000").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);
	let first = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1100")], &key()))
		.await
		.unwrap();
	let second = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1200")], &key()))
		.await
		.unwrap();
	// Oldest first: the first is throttled, the second is still proposed in the same round.
	sqlx::query("UPDATE scope_operations SET next_attempt_at = now() - interval '1 minute' WHERE id = $1")
		.bind(first.operation.id().raw())
		.execute(&h.pool)
		.await
		.unwrap();
	h.concierge.with(|s| s.throttle_next = 1);
	round(&h, &grants).await;
	let first = reload(&h, &first).await;
	assert_eq!((first.operation.progress().approval_id.is_none(), first.attempts), (true, 1));
	assert!(reload(&h, &second).await.operation.progress().approval_id.is_some());
	let proposal = h.concierge.proposal(&approval_id(&reload(&h, &second).await));
	assert!(
		proposal
			.fields
			.iter()
			.any(|field| field.key == "service" && field.value == ApprovalValue::Text(client.as_str().to_owned()))
	);
	assert_eq!(proposal.reason, format!("Service {client}: Quarterly dividend"));

	// Never opened for 72 hours: ended, and no longer drawing on the scope's budget.
	sqlx::query("UPDATE scope_operations SET created_at = now() - interval '73 hours' WHERE id = $1")
		.bind(first.operation.id().raw())
		.execute(&h.pool)
		.await
		.unwrap();
	round(&h, &grants).await;
	let ended = reload(&h, &first).await;
	assert_eq!(ended.operation.state(), OperationState::Rejected);
	assert_eq!(ended.operation.progress().failure_reason.as_deref(), Some("proposal_not_opened"));
}

#[tokio::test]
async fn a_refused_catalog_holds_every_proposal_back() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "3000").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let asked = ops::request_payout(&ops_ports(&h, &b), &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1500")], &key()))
		.await
		.unwrap();
	h.concierge.with(|s| s.refuse_publish = true);
	round(&h, &grants).await;
	round(&h, &grants).await;
	assert_eq!(
		h.concierge.with(|s| s.calls.clone()),
		vec!["publish"],
		"refused once, retried only after an hour, nothing proposed"
	);
	assert_eq!(h.catalog.refusals(), 1);
	assert!(reload(&h, &asked).await.operation.progress().approval_id.is_none());
}

#[tokio::test]
async fn a_transfer_into_a_frozen_or_closed_scope_is_refused_and_fails_at_execution() {
	let Some(h) = harness().await else { return };
	let invest = own_scope(&h, "1000", "5000", "5000").await;
	let sa = own_scope(&h, "1000", "5000", "5000").await;
	let client = fresh_client();
	let grants = Grants::new(vec![grant(&client, &invest, "alloc:", false), grant(&client, &sa, "alloc:", false)]).unwrap();
	funded(&h, &grants, &client, &invest, "alloc:x", "5000").await;
	funded(&h, &grants, &client, &sa, "alloc:x", "0").await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);

	set_scope_state(&h, &sa, ScopeState::Closed).await;
	assert_eq!(
		refusal(ops::request_transfer(&p, &client, request_transfer(&invest, "alloc:x", Some(&sa), "alloc:x", "1")).await),
		LedgerRefusal::ScopeClosed
	);
	set_scope_state(&h, &sa, ScopeState::Frozen).await;
	assert_eq!(
		refusal(ops::request_transfer(&p, &client, request_transfer(&invest, "alloc:x", Some(&sa), "alloc:x", "1")).await),
		LedgerRefusal::ScopeFrozen
	);
	set_scope_state(&h, &sa, ScopeState::Active).await;
	let asked = ops::request_transfer(&p, &client, request_transfer(&invest, "alloc:x", Some(&sa), "alloc:x", "1500"))
		.await
		.unwrap();
	round(&h, &grants).await;
	let approval = approval_id(&reload(&h, &asked).await);
	set_scope_state(&h, &sa, ScopeState::Frozen).await;
	h.concierge.set(&approval, ApprovalState::Passed);
	round(&h, &grants).await;
	assert_eq!(reload(&h, &asked).await.operation.progress().failure_reason.as_deref(), Some("scope_frozen"));
	assert_eq!(account(&h, &invest, "alloc:x").await, usdt("5000"));
}

/// An operation this build cannot read is set aside; the round goes on with the others.
#[tokio::test]
async fn one_unreadable_operation_does_not_stop_the_round() {
	let Some(h) = harness().await else { return };
	let scope = own_scope(&h, "1000", "5000", "5000").await;
	let (client, grants) = payer(&scope);
	funded(&h, &grants, &client, &scope, "alloc:x", "9000").await;
	let (_, alice) = user(&h).await;
	let b = base(&h, &grants);
	let p = ops_ports(&h, &b);
	let poisoned = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1100")], &key()))
		.await
		.unwrap();
	let healthy = ops::request_payout(&p, &client, request_payout(&scope, "alloc:x", vec![line(&alice, "1200")], &key()))
		.await
		.unwrap();
	sqlx::query("DELETE FROM scope_operation_lines WHERE operation_id = $1")
		.bind(poisoned.operation.id().raw())
		.execute(&h.pool)
		.await
		.unwrap();
	round(&h, &grants).await;
	assert!(reload_raw_approval(&h, &healthy).await.is_some(), "the healthy one was proposed");
	assert!(reload_raw_approval(&h, &poisoned).await.is_none());
	let set_aside: bool = sqlx::query_scalar("SELECT next_attempt_at > now() + interval '50 minutes' FROM scope_operations WHERE id = $1")
		.bind(poisoned.operation.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert!(set_aside);
}

/// The approval id straight from the row — the poisoned operation no longer restores.
async fn reload_raw_approval(h: &Harness, operation: &RecordedOperation) -> Option<String> {
	sqlx::query_scalar("SELECT approval_id FROM scope_operations WHERE id = $1")
		.bind(operation.operation.id().raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}
