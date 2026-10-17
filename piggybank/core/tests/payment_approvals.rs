//! Integration tests for payments decided over concierge's `bank:payment` approvals and by the
//! investor's own answer in the cabinet — real Postgres and TigerBeetle, no mocks of either
//! (project rule); only concierge, which is another plane's service, is played by a fake that
//! keeps its rules (a catalog gates the kinds, `dedupe_key` makes a proposal idempotent, an
//! outcome is closed once).
//!
//! They run when `DATABASE_URL` is set and TigerBeetle answers, and skip otherwise.
//!
//! What they pin is the money: a decision approves an order once, however many times and from
//! however many places it is served; a decision that is not the stored order's, or about an
//! order that ended otherwise, moves nothing; and an investor's answer inside the cabinet is
//! the old emailed consent's equal — the same pins, the same execution, the same outcome.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};

use async_trait::async_trait;
use domain::{
	balance::{LedgerAccountKey, Party, ServiceId, TransferCode},
	error::DomainError,
	money::{Network, Usdt},
	payments::{CONCIERGE_TTL_SECS, PaymentDestination, PaymentId, PaymentReason, PaymentState, PaymentTerms, TTL_SECS, approval_subject_ref},
	scope_operations::{BANK_CATALOG_VERSION, KindSpec, MONEY_MOVEMENT_TOPIC, PAYMENT_KIND, TopicSpec, approval_payload},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{
		payment_approvals::{self, ApprovalRound, ApprovalRoundPorts},
		payments as payments_app,
		scope_operations::CatalogGate,
	},
	config::{KycGate, PaymentApprovalMode},
	infrastructure::{
		allocations::PgAllocations, consilium::PgConsilia, custody::StubCustody, operations, outflow::PgOutflowPolicy, payments::PgPayments, relay::Relay,
		scope_notifications::ScopeNotificationWorker, users::PgUsers, withdrawals::PgWithdrawals,
	},
	ports::{
		LedgerTransfer, UserRepository,
		ledger::Ledger,
		payments::{ConsentAudit, ConsentDecision, ExecutionOutcome, PaymentRepository},
		scope_notifications::{NoticeError, ScopeNotice, ScopeNotifier},
		scope_operations::{ApprovalLevel, ApprovalOutcome, ApprovalState, ApprovalView, Approvals, ApprovalsError, Proposal},
	},
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

static PAYMENTS: std::sync::LazyLock<tokio::sync::Mutex<()>> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
	PAYMENTS.lock().await
}

fn now() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn audit() -> ConsentAudit {
	ConsentAudit {
		client_ip: "203.0.113.7".to_owned(),
		user_agent: "integration-test".to_owned(),
	}
}

fn sha256_hex(input: &str) -> String {
	hex::encode(Sha256::digest(input.as_bytes()))
}

// ── concierge, as a fake that keeps its rules ───────────────────────────────

#[derive(Clone, Default)]
struct Concierge(Arc<Mutex<ConciergeState>>);

#[derive(Default)]
struct ConciergeState {
	down: bool,
	catalog: Option<(u64, Vec<String>, Vec<String>)>,
	approvals: HashMap<String, Held>,
	by_dedupe: HashMap<String, String>,
	/// What the next opened approval claims its `payload_hash` is, instead of the real one.
	tamper_open: Option<String>,
	/// Serve already-reported decisions again, as concierge does until it takes the outcome.
	serve_reported: bool,
	reports: Vec<(String, ApprovalOutcome)>,
	withdrawn: Vec<String>,
	proposals: usize,
}

struct Held {
	view: ApprovalView,
	proposal: Option<Proposal>,
	reported: Option<ApprovalOutcome>,
}

impl Concierge {
	fn with<R>(&self, f: impl FnOnce(&mut ConciergeState) -> R) -> R {
		f(&mut self.0.lock().unwrap())
	}

	fn set_state(&self, approval_id: &str, state: ApprovalState) {
		self.with(|s| s.approvals.get_mut(approval_id).expect("a known approval").view.state = state);
	}

	fn tamper_hash(&self, approval_id: &str, hash: &str) {
		self.with(|s| s.approvals.get_mut(approval_id).expect("a known approval").view.payload_hash = hash.to_owned());
	}

	fn proposal(&self, approval_id: &str) -> Proposal {
		self.with(|s| s.approvals[approval_id].proposal.clone().expect("a proposal banking opened"))
	}

	/// A second approval concierge holds for the same subject — what a bug or an attacker with
	/// the bridge token would present as authority for an order that already has its own.
	fn inject(&self, kind: &str, subject_ref: &str, payload_hash: &str) -> String {
		let id = Uuid::new_v4().to_string();
		self.with(|s| {
			s.approvals.insert(
				id.clone(),
				Held {
					view: ApprovalView {
						id: id.clone(),
						kind: kind.to_owned(),
						scope: "main".to_owned(),
						subject_ref: subject_ref.to_owned(),
						state: ApprovalState::Passed,
						level: Some(ApprovalLevel::Platform),
						payload_hash: payload_hash.to_owned(),
					},
					proposal: None,
					reported: None,
				},
			);
		});
		id
	}

	fn reports(&self) -> Vec<(String, ApprovalOutcome)> {
		self.with(|s| s.reports.clone())
	}
}

fn concierge_hash(proposal: &Proposal) -> String {
	sha256_hex(&approval_payload(&proposal.kind, &proposal.scope, &proposal.subject_ref, &proposal.fields))
}

#[async_trait]
impl Approvals for Concierge {
	async fn propose(&self, proposal: &Proposal) -> Result<ApprovalView, ApprovalsError> {
		self.with(|s| {
			if s.down {
				return Err(ApprovalsError::Unavailable("down".into()));
			}
			// A kind the namespace's catalog in force does not hold is refused for good.
			if !s.catalog.as_ref().is_some_and(|(_, kinds, _)| kinds.contains(&proposal.kind)) {
				return Err(ApprovalsError::Refused("unknown kind".into()));
			}
			if let Some(id) = s.by_dedupe.get(&proposal.dedupe_key) {
				return Ok(s.approvals[id].view.clone());
			}
			s.proposals += 1;
			let id = Uuid::new_v4().to_string();
			let view = ApprovalView {
				id: id.clone(),
				kind: proposal.kind.clone(),
				scope: proposal.scope.clone(),
				subject_ref: proposal.subject_ref.clone(),
				state: ApprovalState::Open,
				// The floor: a `bank:payment` is the owners'.
				level: Some(ApprovalLevel::Platform),
				payload_hash: s.tamper_open.take().unwrap_or_else(|| concierge_hash(proposal)),
			};
			s.by_dedupe.insert(proposal.dedupe_key.clone(), id.clone());
			s.approvals.insert(
				id,
				Held {
					view: view.clone(),
					proposal: Some(proposal.clone()),
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
			s.approvals
				.get(approval_id)
				.map(|held| held.view.clone())
				.ok_or_else(|| ApprovalsError::Refused("not found".into()))
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
				.filter(|held| held.view.state == ApprovalState::Passed && (held.reported.is_none() || serve_reported))
				.take(limit as usize)
				.map(|held| held.view.clone())
				.collect())
		})
	}

	async fn report(&self, approval_id: &str, outcome: ApprovalOutcome, _detail: &str) -> Result<(), ApprovalsError> {
		self.with(|s| {
			if s.down {
				return Err(ApprovalsError::Unavailable("down".into()));
			}
			let held = s.approvals.get_mut(approval_id).ok_or_else(|| ApprovalsError::Refused("not found".into()))?;
			// Repeating the same outcome is a no-op; a different one is refused.
			match held.reported {
				Some(done) if done != outcome => return Err(ApprovalsError::Refused("a different outcome is already recorded".into())),
				_ => held.reported = Some(outcome),
			}
			s.reports.push((approval_id.to_owned(), outcome));
			Ok(())
		})
	}

	async fn withdraw(&self, approval_id: &str, _reason: &str) -> Result<(), ApprovalsError> {
		self.with(|s| {
			if s.down {
				return Err(ApprovalsError::Unavailable("down".into()));
			}
			let held = s.approvals.get_mut(approval_id).ok_or_else(|| ApprovalsError::Refused("not found".into()))?;
			if !matches!(held.view.state, ApprovalState::Open | ApprovalState::Passed) {
				return Err(ApprovalsError::Refused("not open".into()));
			}
			held.view.state = ApprovalState::Cancelled;
			s.withdrawn.push(approval_id.to_owned());
			Ok(())
		})
	}

	async fn publish_catalog(&self, version: u64, kinds: &[KindSpec], topics: &[TopicSpec]) -> Result<(), ApprovalsError> {
		self.with(|s| {
			if s.down {
				return Err(ApprovalsError::Unavailable("down".into()));
			}
			s.catalog = Some((
				version,
				kinds.iter().map(|spec| spec.kind.to_owned()).collect(),
				topics.iter().map(|topic| topic.topic.to_owned()).collect(),
			));
			Ok(())
		})
	}
}

// ── the harness ──────────────────────────────────────────────────────────────

struct App {
	pool: PgPool,
	payments: PgPayments,
	consilia: PgConsilia,
	users: PgUsers,
	withdrawals: PgWithdrawals,
	outflow: PgOutflowPolicy,
	allocations: PgAllocations,
	ledger: Arc<dyn Ledger>,
	relay: Relay,
	notify: Arc<Notify>,
	concierge: Concierge,
	catalog: CatalogGate,
}

async fn app(skipping: &str) -> Option<App> {
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, skipping).await?;
	let notify = Arc::new(Notify::new());
	Some(App {
		payments: PgPayments::new(pool.clone()),
		consilia: PgConsilia::new(pool.clone()),
		users: PgUsers::new(pool.clone()),
		withdrawals: PgWithdrawals::new(pool.clone()),
		outflow: PgOutflowPolicy::new(pool.clone()),
		allocations: PgAllocations::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		ledger,
		notify,
		concierge: Concierge::default(),
		catalog: CatalogGate::new(),
		pool,
	})
}

fn ports(a: &App) -> payments_app::PaymentPorts<'_> {
	payments_app::PaymentPorts {
		payments: &a.payments,
		consilia: &a.consilia,
		users: &a.users,
		withdrawals: &a.withdrawals,
		ledger: a.ledger.as_ref(),
		custody: &StubCustody,
		policy: &a.outflow,
		allocations: &a.allocations,
		relay: &a.notify,
		configured: &[Network::Bep20],
		kyc: KycGate::LIFTED,
		approval_url_base: "https://example.test/approve",
		consent_url_base: "https://example.test/consent",
		// Unwired on purpose: the new path must not need the governance mailer at all.
		governance_mail_wired: false,
		approvals: PaymentApprovalMode::Concierge,
	}
}

async fn round(a: &App) -> ApprovalRound {
	let payment_ports = ports(a);
	payment_approvals::round(
		&ApprovalRoundPorts {
			payments: &payment_ports,
			store: &a.payments,
			approvals: &a.concierge,
			catalog: &a.catalog,
		},
		now(),
	)
	.await
	.expect("a round")
}

async fn reset(pool: &PgPool) {
	for statement in [
		"DELETE FROM scope_notification_outbox",
		"DELETE FROM payments",
		"DELETE FROM outbox WHERE aggregate = 'payment'",
		"DELETE FROM event_log WHERE aggregate = 'payment'",
	] {
		sqlx::query(statement).execute(pool).await.expect("clear the payments plane");
	}
}

async fn an_investor(pool: &PgPool) -> UserId {
	let users = PgUsers::new(pool.clone());
	let tag = Uuid::new_v4();
	let subject = domain::auth::AuthSubject::parse(&format!("payment-approvals-{tag}")).unwrap();
	let email = Email::parse(&format!("approvals-{tag}@example.test")).unwrap();
	let id = users.provision(subject, email, true).await.expect("provision an investor").id();
	sqlx::query("UPDATE users SET concierge_user_id = $2 WHERE id = $1")
		.bind(id.raw())
		.bind(Uuid::new_v4())
		.execute(pool)
		.await
		.expect("mirror the concierge id");
	id
}

async fn fund(a: &App, claim: LedgerAccountKey, amount: &str) {
	a.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
			credit: claim,
			amount: usdt(amount).base_units(),
			code: TransferCode::Deposit,
			reference: 0,
		})
		.await
		.expect("fund the claim");
}

fn terms(from: Party, to: PaymentDestination, amount: &str) -> PaymentTerms {
	PaymentTerms::new(from, to, usdt(amount), PaymentReason::new("quarterly rebalance").unwrap()).unwrap()
}

fn fund_to_fee(amount: &str) -> PaymentTerms {
	terms(Party::Service(ServiceId::fund()), PaymentDestination::Internal(Party::Service(ServiceId::fee())), amount)
}

async fn relayed_kinds(pool: &PgPool, order: PaymentId) -> Vec<String> {
	sqlx::query_scalar("SELECT payload::jsonb ->> 'type' FROM outbox WHERE aggregate = 'payment' AND aggregate_id = $1 ORDER BY seq")
		.bind(order.raw())
		.fetch_all(pool)
		.await
		.expect("read the outbox")
}

async fn mail_rows(pool: &PgPool, order: PaymentId) -> i64 {
	sqlx::query_scalar("SELECT COUNT(*) FROM consilium_mail WHERE payment_id = $1")
		.bind(order.raw())
		.fetch_one(pool)
		.await
		.unwrap()
}

async fn state_of(a: &App, id: PaymentId) -> PaymentState {
	payments_app::find(&a.payments, id).await.unwrap().order.state()
}

async fn approval_id_of(a: &App, id: PaymentId) -> String {
	payments_app::find(&a.payments, id)
		.await
		.unwrap()
		.concierge
		.expect("a concierge seat")
		.approval_id
		.expect("the proposal was opened")
}

/// Open a fund payment, fund its source, and run the round that proposes it. Returns the
/// order, the approval concierge holds for it, and the source's and the destination's balances
/// before anything moved (the claims are shared across the suite, so assertions are deltas).
struct Opened {
	id: PaymentId,
	approval: String,
	fund_locked: u128,
	fee_posted: u128,
}

async fn a_proposed_fund_payment(a: &App, initiator: UserId, amount: &str) -> Opened {
	fund(a, LedgerAccountKey::ServiceClaim(ServiceId::fund()), "1000").await;
	a.relay.drain().await;
	let fund_locked = a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fund())).await.unwrap().locked;
	let fee_posted = a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted;
	let view = payments_app::open(&ports(a), initiator, fund_to_fee(amount), now()).await.expect("open the payment");
	let id = view.order.id();
	let proposed = round(a).await;
	assert_eq!(proposed.proposed, 1, "{proposed:?}");
	Opened {
		approval: approval_id_of(a, id).await,
		id,
		fund_locked,
		fee_posted,
	}
}

// ── fund money: a `bank:payment` approval ─────────────────────────────────────

/// The whole life of a fund payment over a concierge approval — and the money read back from
/// the ledger that owns it. The settlement is recorded only once the relay applied the
/// reservation, so the decision is served more than once; concierge keeps serving it until its
/// outcome is taken, and the order must neither reserve nor settle twice.
#[tokio::test]
async fn a_fund_payment_is_decided_over_a_concierge_approval_and_moves_money_once() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals flow").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::ServiceClaim(ServiceId::fund()), "1000").await;
	a.relay.drain().await;
	let fund_locked_before = a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fund())).await.unwrap().locked;
	let fee_before = a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted;

	let view = payments_app::open(&ports(&a), initiator, fund_to_fee("250"), now()).await.expect("open the payment");
	let id = view.order.id();
	// Seated on the approval and on nothing of the old path: no consilium, no governance mail,
	// and the window is the approval's, not the 72 hours the old quorum had.
	assert!(view.concierge.is_some() && view.consilium_id.is_none() && view.consent.is_none() && view.session.is_none());
	// `created_at` is the database's clock and `expires_at` the caller's, a second apart at most.
	assert!((view.order.expires_at() - view.order.created_at() - CONCIERGE_TTL_SECS).abs() <= 2);
	const { assert!(CONCIERGE_TTL_SECS > TTL_SECS) };
	assert_eq!(mail_rows(&a.pool, id).await, 0, "no governance mail is written");
	assert!(a.concierge.with(|s| s.approvals.is_empty()), "nothing is proposed inside open");

	// The round publishes the catalog first, then opens the proposal.
	let first = round(&a).await;
	assert_eq!(first.proposed, 1);
	let (version, kinds, topics) = a.concierge.with(|s| s.catalog.clone()).expect("the catalog was published");
	assert_eq!(version, BANK_CATALOG_VERSION);
	assert!(kinds.iter().any(|kind| kind == PAYMENT_KIND), "{kinds:?}");
	assert_eq!(topics, vec![MONEY_MOVEMENT_TOPIC.to_owned()]);
	let approval = approval_id_of(&a, id).await;
	let proposal = a.concierge.proposal(&approval);
	assert_eq!((proposal.kind.as_str(), proposal.scope.as_str()), (PAYMENT_KIND, "main"));
	assert_eq!(proposal.subject_ref, approval_subject_ref(id));
	let field = |key: &str| proposal.fields.iter().find(|f| f.key == key).unwrap_or_else(|| panic!("{key}")).value.encoded();
	assert_eq!(field("tier"), "service");
	assert_eq!(field("source"), "the fund allocation");
	assert_eq!(field("amount"), "250 USDT");
	assert_eq!(field("reason"), "quarterly rebalance");

	// Open: nothing to carry out.
	assert_eq!(round(&a).await, ApprovalRound::default());
	assert_eq!(state_of(&a, id).await, PaymentState::Pending);
	a.concierge.set_state(&approval, ApprovalState::Passed);

	// Passed: approved and reserved, but not settled until the relay applied the reserve.
	let second = round(&a).await;
	assert_eq!((second.executed, second.voided, second.rejected), (0, 0, 0));
	assert_eq!(state_of(&a, id).await, PaymentState::Approved);
	assert_eq!(relayed_kinds(&a.pool, id).await, vec!["reserved".to_owned()]);
	assert!(a.concierge.reports().is_empty(), "no outcome to report yet");
	a.relay.drain().await;
	assert_eq!(
		a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fund())).await.unwrap().locked - fund_locked_before,
		usdt("250").base_units()
	);

	// Served again, now settleable: executed once and reported once.
	let third = round(&a).await;
	assert_eq!(third.executed, 1);
	assert_eq!(state_of(&a, id).await, PaymentState::Executed);
	assert_eq!(a.concierge.reports(), vec![(approval.clone(), ApprovalOutcome::Executed)]);
	a.relay.drain().await;
	assert_eq!(
		a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted - fee_before,
		usdt("250").base_units()
	);
	assert_eq!(a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fund())).await.unwrap().locked, fund_locked_before);

	// Concierge keeps serving a decision whose outcome it has not recorded: nothing moves again.
	a.concierge.with(|s| s.serve_reported = true);
	for _ in 0..3 {
		round(&a).await;
	}
	assert_eq!(relayed_kinds(&a.pool, id).await, vec!["reserved".to_owned(), "settled".to_owned()]);
	a.relay.drain().await;
	assert_eq!(
		a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted - fee_before,
		usdt("250").base_units(),
		"one decision, one settlement"
	);
	assert!(a.concierge.reports().iter().all(|(_, outcome)| *outcome == ApprovalOutcome::Executed));

	reset(&a.pool).await;
}

/// Two executors reach the same passed decision at once — a second replica, or the sweep
/// beside a retry — and the order is still approved, reserved and settled once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_executors_on_one_decision_reserve_once() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals race").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let open = a_proposed_fund_payment(&a, initiator, "40").await;
	a.concierge.set_state(&open.approval, ApprovalState::Passed);

	let (one, two) = tokio::join!(round(&a), round(&a));
	assert_eq!(one.voided + two.voided, 0, "{one:?} {two:?}");
	assert_eq!(state_of(&a, open.id).await, PaymentState::Approved);
	assert_eq!(relayed_kinds(&a.pool, open.id).await, vec!["reserved".to_owned()], "reserved once");
	a.relay.drain().await;
	let (_one, _two) = tokio::join!(round(&a), round(&a));
	assert_eq!(state_of(&a, open.id).await, PaymentState::Executed);
	assert_eq!(relayed_kinds(&a.pool, open.id).await, vec!["reserved".to_owned(), "settled".to_owned()], "settled once");
	a.relay.drain().await;
	assert_eq!(
		a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted - open.fee_posted,
		usdt("40").base_units()
	);

	reset(&a.pool).await;
}

/// An approval that ends without passing — vetoed, rejected, expired, cancelled, void — ends
/// the order, reserves nothing, and a stale PASSED served for it afterwards is told VOID.
#[tokio::test]
async fn an_approval_that_does_not_pass_ends_the_order_and_reserves_nothing() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals refusal").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let open = a_proposed_fund_payment(&a, initiator, "75").await;

	a.concierge.set_state(&open.approval, ApprovalState::Vetoed);
	let ended = round(&a).await;
	assert_eq!(ended.rejected, 1);
	assert_eq!(state_of(&a, open.id).await, PaymentState::Rejected);
	assert!(relayed_kinds(&a.pool, open.id).await.is_empty(), "a refused payment reserved nothing");

	// A stale decision for the dead order must not reach the money.
	a.concierge.set_state(&open.approval, ApprovalState::Passed);
	let stale = round(&a).await;
	assert_eq!(stale.voided, 1);
	assert_eq!(state_of(&a, open.id).await, PaymentState::Rejected);
	assert!(relayed_kinds(&a.pool, open.id).await.is_empty());
	assert_eq!(a.concierge.reports(), vec![(open.approval.clone(), ApprovalOutcome::Void)]);
	a.relay.drain().await;
	assert_eq!(a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fund())).await.unwrap().locked, open.fund_locked);

	reset(&a.pool).await;
}

/// A decision that is not the stored order's is refused: when concierge opened it under
/// another hash, and when the hash on the decision pulled later is not the one the order was
/// proposed under.
#[tokio::test]
async fn a_decision_that_is_not_the_stored_orders_moves_nothing() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals mismatch").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;

	// At open: the two planes disagree about what is being approved.
	fund(&a, LedgerAccountKey::ServiceClaim(ServiceId::fund()), "1000").await;
	a.concierge.with(|s| s.tamper_open = Some("0".repeat(64)));
	let id = payments_app::open(&ports(&a), initiator, fund_to_fee("10"), now()).await.unwrap().order.id();
	let ended = round(&a).await;
	assert_eq!((ended.proposed, ended.rejected), (0, 1));
	assert_eq!(state_of(&a, id).await, PaymentState::Rejected, "the order ends now, not after the owners voted on it");
	assert_eq!(a.concierge.with(|s| s.withdrawn.len()), 1, "and the proposal is taken back");

	// At pull: the decision carries a hash that is not the order's.
	let open = a_proposed_fund_payment(&a, initiator, "20").await;
	a.concierge.tamper_hash(&open.approval, &"f".repeat(64));
	a.concierge.set_state(&open.approval, ApprovalState::Passed);
	let refused = round(&a).await;
	assert_eq!((refused.rejected, refused.executed), (1, 0), "{refused:?}");
	assert_eq!(state_of(&a, open.id).await, PaymentState::Rejected);
	assert!(relayed_kinds(&a.pool, open.id).await.is_empty());
	assert_eq!(a.concierge.reports(), vec![(open.approval.clone(), ApprovalOutcome::Void)]);

	reset(&a.pool).await;
}

/// One order, one approval: a second decision naming the same payment — even a valid-looking
/// one — never executes it, and neither does a decision naming a payment banking does not have.
#[tokio::test]
async fn only_the_approval_opened_for_an_order_may_execute_it() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals other approval").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let open = a_proposed_fund_payment(&a, initiator, "30").await;
	let own_hash = a.concierge.with(|s| s.approvals[&open.approval].view.payload_hash.clone());

	let rogue = a.concierge.inject(PAYMENT_KIND, &approval_subject_ref(open.id), &own_hash);
	let ghost = a.concierge.inject(PAYMENT_KIND, &approval_subject_ref(PaymentId::new()), &own_hash);
	let other_kind = a.concierge.inject("bank:scope_payout", &format!("pending_operation:{}", Uuid::new_v4()), &own_hash);
	let refused = round(&a).await;
	assert_eq!(refused.voided, 2, "the second approval and the ghost: {refused:?}");
	assert_eq!(state_of(&a, open.id).await, PaymentState::Pending);
	assert!(relayed_kinds(&a.pool, open.id).await.is_empty());
	let reports = a.concierge.reports();
	assert!(reports.contains(&(rogue, ApprovalOutcome::Void)) && reports.contains(&(ghost, ApprovalOutcome::Void)));
	// Not ours to report: another flow executes a pending operation.
	assert!(!reports.iter().any(|(id, _)| *id == other_kind));

	// The real one still works.
	a.concierge.set_state(&open.approval, ApprovalState::Passed);
	round(&a).await;
	assert_eq!(state_of(&a, open.id).await, PaymentState::Approved);

	reset(&a.pool).await;
}

/// Concierge being away costs time and nothing else: the order waits unproposed, is neither
/// failed nor lost, and is proposed once concierge is back.
#[tokio::test]
async fn a_concierge_outage_delays_a_payment_and_never_fails_it() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals outage").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let id = payments_app::open(&ports(&a), initiator, fund_to_fee("10"), now()).await.unwrap().order.id();

	a.concierge.with(|s| s.down = true);
	let down = round(&a).await;
	assert_eq!(down, ApprovalRound::default());
	assert_eq!(state_of(&a, id).await, PaymentState::Pending);
	assert!(a.concierge.with(|s| s.approvals.is_empty()));

	a.concierge.with(|s| s.down = false);
	// The catalog gate backs off after a failed publish; a fresh process publishes at once.
	let restarted = CatalogGate::new();
	let payment_ports = ports(&a);
	let up = payment_approvals::round(
		&ApprovalRoundPorts {
			payments: &payment_ports,
			store: &a.payments,
			approvals: &a.concierge,
			catalog: &restarted,
		},
		now(),
	)
	.await
	.unwrap();
	assert_eq!(up.proposed, 1);
	assert_eq!(state_of(&a, id).await, PaymentState::Pending);

	reset(&a.pool).await;
}

/// The operator's pause holds an approved order without closing it: the decision is recorded,
/// nothing executes and nothing is reported, and the order finishes once the pause lifts.
#[tokio::test]
async fn an_operator_pause_holds_a_decided_payment_without_closing_it() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals pause").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let open = a_proposed_fund_payment(&a, initiator, "15").await;
	a.concierge.set_state(&open.approval, ApprovalState::Passed);

	operations::set_read_only(&a.pool, true).await.unwrap();
	round(&a).await;
	let held = round(&a).await;
	operations::set_read_only(&a.pool, false).await.unwrap();
	assert_eq!(held.executed, 0, "{held:?}");
	assert_eq!(state_of(&a, open.id).await, PaymentState::Approved, "still waiting, not failed");
	assert!(a.concierge.reports().is_empty());

	a.relay.drain().await;
	let done = round(&a).await;
	assert_eq!(done.executed, 1, "{done:?}");
	assert_eq!(state_of(&a, open.id).await, PaymentState::Executed);
	assert_eq!(a.concierge.reports(), vec![(open.approval, ApprovalOutcome::Executed)]);

	a.relay.drain().await;
	reset(&a.pool).await;
}

/// An order withdrawn by its initiator takes its proposal back, once; a decision served for it
/// afterwards is told VOID and moves nothing.
#[tokio::test]
async fn a_withdrawn_payment_takes_its_proposal_back() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals withdraw").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let open = a_proposed_fund_payment(&a, initiator, "12").await;

	let cancelled = payments_app::cancel(&ports(&a), open.id, initiator, now()).await.unwrap();
	assert_eq!(cancelled.order.state(), PaymentState::Cancelled);
	let first = round(&a).await;
	assert_eq!(first.withdrawn, 1);
	assert_eq!(a.concierge.with(|s| s.withdrawn.clone()), vec![open.approval.clone()]);
	assert_eq!(round(&a).await.withdrawn, 0, "taken back once");

	a.concierge.set_state(&open.approval, ApprovalState::Passed);
	let stale = round(&a).await;
	assert_eq!(stale.voided, 1);
	assert_eq!(state_of(&a, open.id).await, PaymentState::Cancelled);
	assert!(relayed_kinds(&a.pool, open.id).await.is_empty());

	reset(&a.pool).await;
}

/// The rollback switch still routes a new order to the old path: asked for the consilium it
/// wants the governance mailer the new path does not need, and the default seats the same
/// order on a concierge approval with the mailer unwired.
#[tokio::test]
async fn the_consilium_mode_still_routes_a_fund_payment_to_the_old_path() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals rollback").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let rollback = payments_app::PaymentPorts {
		approvals: PaymentApprovalMode::Consilium,
		..ports(&a)
	};
	let refused = payments_app::open(&rollback, initiator, fund_to_fee("5"), now()).await;
	assert!(
		matches!(refused, Err(DomainError::Conflict(ref why)) if why.contains("governance mail")),
		"the old path needs its mailer: {refused:?}"
	);
	let seated = payments_app::open(&ports(&a), initiator, fund_to_fee("5"), now())
		.await
		.expect("the default path needs no governance mailer");
	assert!(seated.concierge.is_some() && seated.consilium_id.is_none());
	assert_eq!(round(&a).await.proposed, 1);

	reset(&a.pool).await;
}

// ── an investor's money: their own answer, signed in ──────────────────────────

/// Records what the worker emits, and answers as the test says.
#[derive(Clone, Default)]
struct Inbox(Arc<Mutex<InboxState>>);

#[derive(Default)]
struct InboxState {
	emitted: Vec<(Uuid, ScopeNotice)>,
	next: Option<NoticeErrorKind>,
}

#[derive(Clone, Copy)]
enum NoticeErrorKind {
	UnknownTopic,
	Refused,
}

#[async_trait]
impl ScopeNotifier for Inbox {
	async fn emit(&self, concierge_user_id: Uuid, notice: &ScopeNotice) -> Result<(), NoticeError> {
		let mut state = self.0.lock().unwrap();
		match state.next.take() {
			Some(NoticeErrorKind::UnknownTopic) => return Err(NoticeError::Deferred("InvalidArgument: unknown topic".into())),
			Some(NoticeErrorKind::Refused) => return Err(NoticeError::Failed("PermissionDenied: no".into())),
			None => {}
		}
		// Idempotent by `dedupe_key`, as concierge is.
		if !state.emitted.iter().any(|(user, sent)| *user == concierge_user_id && sent.dedupe_key == notice.dedupe_key) {
			state.emitted.push((concierge_user_id, notice.clone()));
		}
		Ok(())
	}
}

fn investor_payment(investor: UserId, amount: &str) -> PaymentTerms {
	terms(Party::User(investor), PaymentDestination::Internal(Party::Service(ServiceId::fee())), amount)
}

async fn notices(pool: &PgPool, payment: PaymentId) -> Vec<(Uuid, String, String, String, String, bool)> {
	sqlx::query_as("SELECT user_id, kind, topic, link, dedupe_key, withdrawn_at IS NOT NULL FROM scope_notification_outbox WHERE payment_id = $1 ORDER BY kind, user_id")
		.bind(payment.raw())
		.fetch_all(pool)
		.await
		.unwrap()
}

/// The investor's side: the order is seated on their own session, the invitation is a notice
/// in their inbox (never a mail with a token), and their approval executes it once — read back
/// from the ledger.
#[tokio::test]
async fn an_investors_payment_is_answered_in_the_cabinet_and_settles_once() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals session flow").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let stranger = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let fee_before = a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted;

	let view = payments_app::open(&ports(&a), stranger, investor_payment(investor, "30"), now()).await.expect("open the order");
	let id = view.order.id();
	assert!(view.session.is_some() && view.consent.is_none() && view.concierge.is_none() && view.consilium_id.is_none());
	assert!((view.order.expires_at() - view.order.created_at() - TTL_SECS).abs() <= 2, "the consent window is the old one");
	assert_eq!(mail_rows(&a.pool, id).await, 0, "no token, no code, no governance mail");
	let queued = notices(&a.pool, id).await;
	assert_eq!(queued.len(), 1);
	let concierge_id: Uuid = sqlx::query_scalar("SELECT concierge_user_id FROM users WHERE id = $1")
		.bind(investor.raw())
		.fetch_one(&a.pool)
		.await
		.unwrap();
	assert_eq!(queued[0].0, investor.raw());
	assert_eq!((queued[0].1.as_str(), queued[0].2.as_str()), ("payment_consent", MONEY_MOVEMENT_TOPIC));
	assert_eq!(
		queued[0].3,
		format!("/cabinet/wallet/action/payment/{}", id.raw()),
		"a path in the cabinet, never a link carrying a secret"
	);

	// Only the subject sees it; a stranger finds nothing and cannot answer.
	assert_eq!(payments_app::awaiting_confirmation(&a.payments, investor).await.unwrap().len(), 1);
	assert!(payments_app::awaiting_confirmation(&a.payments, stranger).await.unwrap().is_empty());
	assert!(payments_app::find_for_subject(&a.payments, id, stranger).await.is_err());
	let refused = payments_app::answer_consent(&ports(&a), stranger, id, ConsentDecision::Approve, &audit(), now()).await;
	assert!(matches!(refused, Err(DomainError::NotFound { .. })), "{refused:?}");
	assert_eq!(state_of(&a, id).await, PaymentState::Pending);

	// The notice reaches the inbox, addressed through the mirror, and not twice.
	let inbox = Inbox::default();
	let worker = ScopeNotificationWorker::new(a.pool.clone(), Arc::new(inbox.clone()));
	assert_eq!(worker.drain().await.unwrap(), 1);
	assert_eq!(worker.drain().await.unwrap(), 0);
	let emitted = inbox.0.lock().unwrap().emitted.clone();
	assert_eq!(emitted.len(), 1);
	assert_eq!(emitted[0].0, concierge_id);
	assert!(emitted[0].1.body.contains("30 USDT") && emitted[0].1.body.contains("quarterly rebalance"));

	// The answer: approved, reserved, and executed once the relay has applied the reserve.
	let outcome = payments_app::answer_consent(&ports(&a), investor, id, ConsentDecision::Approve, &audit(), now())
		.await
		.expect("the subject answers");
	assert!(outcome.decided && outcome.approved);
	assert_eq!(state_of(&a, id).await, PaymentState::Approved, "no settlement over a reserve that has not landed");
	assert_eq!(relayed_kinds(&a.pool, id).await, vec!["reserved".to_owned()]);
	a.relay.drain().await;
	assert_eq!(a.ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap().locked, usdt("30").base_units());
	let report = payments_app::sweep(&ports(&a), now()).await.unwrap();
	assert_eq!((report.executed, report.deferred, report.execution_failures), (1, 0, 0));
	a.relay.drain().await;
	let settled = a.ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();
	assert_eq!((settled.posted, settled.locked), (usdt("70").base_units(), 0));
	assert_eq!(
		a.ledger.balance(&LedgerAccountKey::ServiceClaim(ServiceId::fee())).await.unwrap().posted - fee_before,
		usdt("30").base_units()
	);

	// Answered twice the same way is the same answer; the other way is refused; nothing moves again.
	let again = payments_app::answer_consent(&ports(&a), investor, id, ConsentDecision::Approve, &audit(), now()).await.unwrap();
	assert!(!again.decided && !again.approved);
	let flipped = payments_app::answer_consent(&ports(&a), investor, id, ConsentDecision::Reject, &audit(), now()).await;
	assert!(matches!(flipped, Err(DomainError::Conflict(_))), "{flipped:?}");
	assert_eq!(payments_app::execute(&ports(&a), id, now()).await.unwrap().order.state(), PaymentState::Executed);
	assert_eq!(relayed_kinds(&a.pool, id).await, vec!["reserved".to_owned(), "settled".to_owned()]);
	assert!(payments_app::awaiting_confirmation(&a.payments, investor).await.unwrap().is_empty());

	reset(&a.pool).await;
}

/// A decline ends the order and reserves nothing; the invitation still queued is taken back.
#[tokio::test]
async fn an_investor_may_decline_and_nothing_moves() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals decline").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = payments_app::open(&ports(&a), investor, investor_payment(investor, "30"), now()).await.unwrap().order.id();

	let outcome = payments_app::answer_consent(&ports(&a), investor, id, ConsentDecision::Reject, &audit(), now()).await.unwrap();
	assert!(outcome.decided && !outcome.approved);
	assert_eq!(state_of(&a, id).await, PaymentState::Rejected);
	assert!(relayed_kinds(&a.pool, id).await.is_empty());
	assert!(notices(&a.pool, id).await.iter().all(|notice| notice.5), "the unsent invitation was withdrawn with the answer");
	let worker = ScopeNotificationWorker::new(a.pool.clone(), Arc::new(Inbox::default()));
	assert_eq!(worker.drain().await.unwrap(), 0, "a withdrawn notice is never handed over");
	let late = payments_app::answer_consent(&ports(&a), investor, id, ConsentDecision::Approve, &audit(), now()).await;
	assert!(matches!(late, Err(DomainError::Conflict(_))), "an answered order cannot be answered the other way: {late:?}");

	reset(&a.pool).await;
}

/// An order whose window ran out cannot be approved by an answer that arrives late, whether or
/// not the sweep reached it first.
#[tokio::test]
async fn an_expired_order_cannot_be_approved_by_a_late_answer() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals late answer").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = payments_app::open(&ports(&a), investor, investor_payment(investor, "30"), now()).await.unwrap().order.id();

	let late = now() + TTL_SECS + 1;
	let refused = payments_app::answer_consent(&ports(&a), investor, id, ConsentDecision::Approve, &audit(), late).await;
	assert!(matches!(refused, Err(DomainError::NotFound { .. })), "{refused:?}");
	assert!(relayed_kinds(&a.pool, id).await.is_empty());
	let swept = payments_app::sweep(&ports(&a), late).await.unwrap();
	assert_eq!(swept.expired, 1);
	assert_eq!(state_of(&a, id).await, PaymentState::Expired);

	reset(&a.pool).await;
}

/// The pins of the emailed consent hold for the session one: sessions revoked after the order
/// opened void it at the answer — the order fails closed and both audiences are told, through
/// the inbox — and a revocation after the answer but before the money moves fails the
/// execution and gives the reservation back.
#[tokio::test]
async fn a_revocation_voids_the_investors_answer_before_and_after_it_is_given() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals revocation").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let operator = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;

	// Before the answer.
	let before = payments_app::open(&ports(&a), operator, investor_payment(investor, "20"), now()).await.unwrap().order.id();
	a.users.revoke_tokens(investor).await.unwrap();
	let refused = payments_app::answer_consent(&ports(&a), investor, before, ConsentDecision::Approve, &audit(), now()).await;
	assert!(matches!(refused, Err(DomainError::Conflict(ref why)) if why.contains("revoked")), "{refused:?}");
	assert_eq!(state_of(&a, before).await, PaymentState::Rejected, "a voided consent fails the payment closed");
	assert!(relayed_kinds(&a.pool, before).await.is_empty(), "nothing was reserved");
	let told: Vec<(Uuid, String)> = notices(&a.pool, before).await.into_iter().filter(|n| n.1 == "payment_outcome").map(|n| (n.0, n.4)).collect();
	let mut told_users: Vec<Uuid> = told.iter().map(|(user, _)| *user).collect();
	told_users.sort();
	let mut expected = vec![investor.raw(), operator.raw()];
	expected.sort();
	assert_eq!(told_users, expected, "the subject and the operator who opened it");
	assert!(told.iter().all(|(_, key)| key.starts_with(&format!("payment:{before}:outcome:"))));

	// After the answer, before the money.
	let reserved_before = a.ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();
	let after = payments_app::open(&ports(&a), investor, investor_payment(investor, "20"), now()).await.unwrap().order.id();
	// Through the port, so nothing executes inline and the window is real.
	a.payments.answer_session_consent(after, investor, ConsentDecision::Approve, &audit(), now()).await.unwrap();
	a.relay.drain().await;
	a.users.revoke_tokens(investor).await.unwrap();
	let view = payments_app::execute(&ports(&a), after, now()).await.expect("the refusal is recorded on the order, not raised");
	assert_eq!(view.order.state(), PaymentState::ExecutionFailed);
	assert!(view.order.failure_reason().is_some_and(|why| why.contains("revoked")));
	assert_eq!(relayed_kinds(&a.pool, after).await, vec!["reserved".to_owned(), "released".to_owned()]);
	a.relay.drain().await;
	let reserved_after = a.ledger.balance(&LedgerAccountKey::UserClaim(investor)).await.unwrap();
	assert_eq!(
		(reserved_after.posted, reserved_after.locked),
		(reserved_before.posted, reserved_before.locked),
		"the reservation was given back"
	);
	// A second caller carrying the same void tells nobody again.
	let cause = piggybank_core::ports::payments::ConsentInvalidation::SessionsRevoked { at_open: 0, now: 1 };
	a.payments
		.record_execution(after, ExecutionOutcome::ConsentVoid { cause, withdrawal: None }, now())
		.await
		.expect("idempotent");
	let outcome_notices = notices(&a.pool, after).await.into_iter().filter(|n| n.1 == "payment_outcome").count();
	assert_eq!(outcome_notices, 1, "the investor, who is also the operator here, is told once");

	reset(&a.pool).await;
}

/// A changed mailbox voids the answer too — and the subject, whose address is the thing that
/// changed, is not told; only the operator is.
#[tokio::test]
async fn a_changed_mailbox_voids_the_answer_and_only_the_operator_is_told() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals mailbox").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let operator = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = payments_app::open(&ports(&a), operator, investor_payment(investor, "20"), now()).await.unwrap().order.id();

	sqlx::query("UPDATE users SET email = $2 WHERE id = $1")
		.bind(investor.raw())
		.bind(format!("moved-{}@example.test", Uuid::new_v4()))
		.execute(&a.pool)
		.await
		.unwrap();
	let refused = payments_app::answer_consent(&ports(&a), investor, id, ConsentDecision::Approve, &audit(), now()).await;
	assert!(matches!(refused, Err(DomainError::Conflict(ref why)) if why.contains("mailbox")), "{refused:?}");
	assert_eq!(state_of(&a, id).await, PaymentState::Rejected);
	let told: Vec<Uuid> = notices(&a.pool, id).await.into_iter().filter(|n| n.1 == "payment_outcome").map(|n| n.0).collect();
	assert_eq!(told, vec![operator.raw()]);

	reset(&a.pool).await;
}

/// The notice worker: a topic concierge does not hold yet waits without charging an attempt; a
/// refusal of the notice itself is charged; a delivered one is not sent again.
#[tokio::test]
async fn the_notice_worker_waits_for_the_catalog_and_charges_only_real_refusals() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals notices").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = payments_app::open(&ports(&a), investor, investor_payment(investor, "5"), now()).await.unwrap().order.id();
	let state = |pool: PgPool| async move {
		sqlx::query_as::<_, (i32, i32, bool)>("SELECT attempts, deferrals, sent_at IS NOT NULL FROM scope_notification_outbox WHERE payment_id = $1")
			.bind(id.raw())
			.fetch_one(&pool)
			.await
			.unwrap()
	};

	let inbox = Inbox::default();
	let worker = ScopeNotificationWorker::new(a.pool.clone(), Arc::new(inbox.clone()));
	inbox.0.lock().unwrap().next = Some(NoticeErrorKind::UnknownTopic);
	assert_eq!(worker.drain().await.unwrap(), 0);
	assert_eq!(state(a.pool.clone()).await, (0, 1, false), "waiting for the catalog costs no attempt");

	// Backoff: the next pass is not due yet; make it due.
	assert_eq!(worker.drain().await.unwrap(), 0);
	sqlx::query("UPDATE scope_notification_outbox SET next_attempt_at = now() WHERE payment_id = $1")
		.bind(id.raw())
		.execute(&a.pool)
		.await
		.unwrap();
	inbox.0.lock().unwrap().next = Some(NoticeErrorKind::Refused);
	assert_eq!(worker.drain().await.unwrap(), 0);
	assert_eq!(state(a.pool.clone()).await, (1, 1, false), "a refusal of the notice is charged");

	assert_eq!(worker.drain().await.unwrap(), 1);
	assert_eq!(state(a.pool.clone()).await, (1, 1, true));
	assert_eq!(worker.drain().await.unwrap(), 0);
	assert_eq!(inbox.0.lock().unwrap().emitted.len(), 1);

	reset(&a.pool).await;
}

/// An investor concierge has never mirrored cannot be told, so the order is refused at open
/// rather than left unanswerable for a week.
#[tokio::test]
async fn an_investor_with_no_mirrored_identity_cannot_be_asked() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals unmirrored").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	sqlx::query("UPDATE users SET concierge_user_id = NULL WHERE id = $1")
		.bind(investor.raw())
		.execute(&a.pool)
		.await
		.unwrap();
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;

	let refused = payments_app::open(&ports(&a), investor, investor_payment(investor, "5"), now()).await;
	assert!(matches!(refused, Err(DomainError::Conflict(_))), "{refused:?}");
	let orders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments").fetch_one(&a.pool).await.unwrap();
	assert_eq!(orders, 0, "an order nobody can answer must not exist");

	reset(&a.pool).await;
}
