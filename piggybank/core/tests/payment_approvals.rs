//! Integration tests for payments decided over concierge's `bank:payment` approvals and by the
//! investor's own answer in the wallet host — real Postgres and TigerBeetle, no mocks of either
//! (project rule); only concierge, which is another plane's service, is played by a fake that
//! keeps its rules (a catalog gates the kinds, `dedupe_key` makes a proposal idempotent, an
//! outcome is closed once).
//!
//! They run when `DATABASE_URL` is set and TigerBeetle answers, and skip otherwise.
//!
//! What they pin is the money: a decision approves an order once, however many times and from
//! however many places it is served; a decision that is not the stored order's, or about an
//! order that ended otherwise, moves nothing; and an investor's answer inside the wallet host is
//! pinned to their standing at open and executes once.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};

use async_trait::async_trait;
use domain::{
	balance::{LedgerAccountKey, Party, TransferCode},
	error::DomainError,
	money::{Network, Usdt},
	payments::{CONCIERGE_TTL_SECS, PaymentDestination, PaymentId, PaymentOrder, PaymentReason, PaymentState, PaymentTerms, TTL_SECS, approval_subject_ref},
	scope_operations::{BANK_CATALOG_VERSION, KindSpec, MONEY_MOVEMENT_TOPIC, PAYMENT_KIND, TopicSpec, WALLET_SECURITY_TOPIC, approval_payload},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{
		payment_approvals::{self, ApprovalRound, ApprovalRoundPorts, open_with_proof, propose_on_open},
		payments as payments_app,
		scope_operations::CatalogGate,
	},
	config::KycGate,
	infrastructure::{
		custody::StubCustody, operations, outflow::PgOutflowPolicy, payments::PgPayments, relay::Relay, scope_notifications::ScopeNotificationWorker, users::PgUsers,
		withdrawals::PgWithdrawals,
	},
	ports::{
		LedgerTransfer, UserRepository,
		audit::RequestAudit,
		ledger::Ledger,
		payments::{ApprovalSeat, ConsentDecision, ExecutionOutcome, PaymentRepository, PaymentView, SeatView},
		scope_notifications::{NoticeError, ScopeNotice, ScopeNotifier},
		scope_operations::{ApprovalLevel, ApprovalOutcome, ApprovalState, ApprovalView, Approvals, ApprovalsError, Proposal, UserProof},
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

fn audit() -> RequestAudit {
	RequestAudit::new("https://evinvest.ltd", "203.0.113.7", "integration-test", "", "aal2")
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
	/// The owners concierge would seat on a platform roster, by `auth_subject`.
	owners: Vec<String>,
	/// Proposals opened WITHOUT a proof of who asked — the executor must never make one.
	unproven: usize,
	/// A proof older than concierge accepts (120 s): every proof is refused.
	stale_proofs: bool,
	/// Concierge no longer takes any outcome (a decision past its `execute_by`).
	refuse_reports: bool,
	/// The `execute_by` the next opened approval carries once passed.
	execute_by: i64,
}

struct Held {
	view: ApprovalView,
	proposal: Option<Proposal>,
	reported: Option<ApprovalOutcome>,
	/// Who may vote: the roster frozen at open, the proven initiator taken off it.
	roster: Vec<String>,
	requested_by: Option<String>,
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
						execute_by: 0,
					},
					proposal: None,
					reported: None,
					roster: Vec::new(),
					requested_by: None,
				},
			);
		});
		id
	}

	fn voters(&self, approval_id: &str) -> Vec<String> {
		self.with(|s| s.approvals[approval_id].roster.clone())
	}

	fn requested_by(&self, approval_id: &str) -> Option<String> {
		self.with(|s| s.approvals[approval_id].requested_by.clone())
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
		self.with(|s| s.unproven += 1);
		self.propose_for(proposal, None).await
	}

	async fn propose_for(&self, proposal: &Proposal, proof: Option<&UserProof>) -> Result<ApprovalView, ApprovalsError> {
		self.with(|s| {
			if proof.is_some() && s.stale_proofs {
				return Err(ApprovalsError::Refused("the user token is older than 120 seconds: mint one for this request".into()));
			}
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
				execute_by: s.execute_by,
			};
			// The roster is frozen at open, and a PROVEN initiator is kept off it (D24(1)).
			let roster: Vec<String> = s.owners.iter().filter(|owner| proof.is_none_or(|proof| **owner != proof.requested_by)).cloned().collect();
			s.by_dedupe.insert(proposal.dedupe_key.clone(), id.clone());
			s.approvals.insert(
				id,
				Held {
					view: view.clone(),
					proposal: Some(proposal.clone()),
					reported: None,
					roster,
					requested_by: proof.map(|proof| proof.requested_by.clone()),
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
			if s.refuse_reports {
				return Err(ApprovalsError::Refused("the decision lapsed".into()));
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
	users: PgUsers,
	withdrawals: PgWithdrawals,
	outflow: PgOutflowPolicy,
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
		users: PgUsers::new(pool.clone()),
		withdrawals: PgWithdrawals::new(pool.clone()),
		outflow: PgOutflowPolicy::new(pool.clone()),
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
		users: &a.users,
		withdrawals: &a.withdrawals,
		ledger: a.ledger.as_ref(),
		custody: &StubCustody,
		policy: &a.outflow,
		relay: &a.notify,
		configured: &[Network::Bep20],
		kyc: KycGate::LIFTED,
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

fn treasury_to(recipient: UserId, amount: &str) -> PaymentTerms {
	terms(Party::Treasury, PaymentDestination::Internal(Party::User(recipient)), amount)
}

async fn relayed_kinds(pool: &PgPool, order: PaymentId) -> Vec<String> {
	sqlx::query_scalar("SELECT payload::jsonb ->> 'type' FROM outbox WHERE aggregate = 'payment' AND aggregate_id = $1 ORDER BY seq")
		.bind(order.raw())
		.fetch_all(pool)
		.await
		.expect("read the outbox")
}

/// The proof of who asked, as a request carries it. The fake concierge accepts any token it is
/// not told is stale; what matters on this side is that it is passed and never kept.
const PROOF: &str = "initiator-access-token";

async fn open_with_token(a: &App, initiator: UserId, terms: PaymentTerms, token: Option<&str>) -> Result<PaymentView, DomainError> {
	let payment_ports = ports(a);
	open_with_proof(
		&ApprovalRoundPorts {
			payments: &payment_ports,
			store: &a.payments,
			approvals: &a.concierge,
			catalog: &a.catalog,
		},
		initiator,
		terms,
		token,
		now(),
	)
	.await
}

/// An order out of the treasury, written the way the repository allows: `payments::open` refuses
/// it (D24), and the approval protocol is what these tests are about.
async fn write_treasury_order(a: &App, initiator: UserId, terms: PaymentTerms) -> PaymentView {
	let payload_hash = Sha256::digest(terms.canonical_bytes()).into();
	let mut order = PaymentOrder::open_for(PaymentId::new(), terms, payload_hash, initiator, now(), CONCIERGE_TTL_SECS);
	a.payments.open(&mut order, ApprovalSeat::Concierge).await.expect("write the treasury order");
	payments_app::find(&a.payments, order.id()).await.unwrap()
}

/// [`write_treasury_order`], then proposed to the owners as the request that wrote it would.
async fn open_treasury(a: &App, initiator: UserId, terms: PaymentTerms) -> Result<PaymentView, DomainError> {
	let view = write_treasury_order(a, initiator, terms).await;
	let proof = UserProof {
		requested_by: subject_of(&a.pool, initiator).await,
		token: PROOF.into(),
	};
	let payment_ports = ports(a);
	propose_on_open(
		&ApprovalRoundPorts {
			payments: &payment_ports,
			store: &a.payments,
			approvals: &a.concierge,
			catalog: &a.catalog,
		},
		&view,
		&proof,
		now(),
	)
	.await
}

async fn subject_of(pool: &PgPool, user: UserId) -> String {
	sqlx::query_scalar("SELECT auth_subject FROM users WHERE id = $1").bind(user.raw()).fetch_one(pool).await.unwrap()
}

/// The one order the test wrote.
async fn only_payment(pool: &PgPool) -> PaymentId {
	PaymentId::from_raw(sqlx::query_scalar("SELECT id FROM payments").fetch_one(pool).await.expect("exactly one order"))
}

async fn state_of(a: &App, id: PaymentId) -> PaymentState {
	payments_app::find(&a.payments, id).await.unwrap().order.state()
}

/// The order's concierge seat; a test that asks for it opened a fund payment.
fn concierge_seat(view: &PaymentView) -> &piggybank_core::ports::payments::ConciergeSeatView {
	match &view.seat {
		SeatView::Concierge(seat) => seat,
		SeatView::Session(_) => panic!("payment {} is answered in the wallet host, not over an approval", view.order.id()),
	}
}

async fn approval_id_of(a: &App, id: PaymentId) -> String {
	let view = payments_app::find(&a.payments, id).await.unwrap();
	concierge_seat(&view).approval_id.clone().expect("the proposal was opened")
}

/// Open a treasury payment, fund its source, and propose it. Returns the order, the approval
/// concierge holds for it, and the source's and the destination's balances before anything moved
/// (the claims are shared across the suite, so assertions are deltas).
struct Opened {
	id: PaymentId,
	approval: String,
	recipient: UserId,
	treasury_locked: u128,
	recipient_posted: u128,
}

async fn a_proposed_treasury_payment(a: &App, initiator: UserId, amount: &str) -> Opened {
	let recipient = an_investor(&a.pool).await;
	fund(a, Party::Treasury.claim_key(), "1000").await;
	a.relay.drain().await;
	let treasury_locked = a.ledger.balance(&Party::Treasury.claim_key()).await.unwrap().locked;
	let recipient_posted = a.ledger.balance(&LedgerAccountKey::UserClaim(recipient)).await.unwrap().posted;
	let view = open_treasury(a, initiator, treasury_to(recipient, amount)).await.expect("open the payment");
	let id = view.order.id();
	Opened {
		approval: approval_id_of(a, id).await,
		id,
		recipient,
		treasury_locked,
		recipient_posted,
	}
}

// ── treasury money: a `bank:payment` approval ─────────────────────────────────

/// `open` and `open_with_proof` refuse the treasury as a source (D24), before anything is
/// written or proposed.
#[tokio::test]
async fn the_treasury_cannot_be_spent_through_open_with_proof() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals treasury closed").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let recipient = an_investor(&a.pool).await;

	let refused = open_with_token(&a, initiator, treasury_to(recipient, "5"), Some(PROOF)).await;
	assert!(
		matches!(refused, Err(DomainError::Precondition(ref why)) if why == payments_app::TREASURY_SPENDING_CLOSED),
		"{refused:?}"
	);
	let refused = payments_app::open(&ports(&a), initiator, treasury_to(recipient, "5"), now()).await;
	assert!(
		matches!(refused, Err(DomainError::Precondition(ref why)) if why == payments_app::TREASURY_SPENDING_CLOSED),
		"{refused:?}"
	);
	let orders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments").fetch_one(&a.pool).await.unwrap();
	assert_eq!(orders, 0, "a refused open writes nothing");
	assert!(a.concierge.with(|s| s.approvals.is_empty()), "and proposes nothing");
}

/// The whole life of a treasury payment over a concierge approval — and the money read back from
/// the ledger that owns it. Spending is closed (D24), so an approved order is reserved once and
/// then recorded failed with the reason, giving the reservation back; concierge keeps serving the
/// decision until its outcome is taken, and the order must neither reserve nor fail twice.
#[tokio::test]
async fn a_treasury_payment_is_decided_over_a_concierge_approval_and_closes_failed_once() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals flow").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let recipient = an_investor(&a.pool).await;
	fund(&a, Party::Treasury.claim_key(), "1000").await;
	a.relay.drain().await;
	let treasury_locked_before = a.ledger.balance(&Party::Treasury.claim_key()).await.unwrap().locked;
	let recipient_before = a.ledger.balance(&LedgerAccountKey::UserClaim(recipient)).await.unwrap().posted;

	let view = open_treasury(&a, initiator, treasury_to(recipient, "250")).await.expect("open the payment");
	let id = view.order.id();
	// Seated on the approval, and the window is the approval's, not the 72 hours a consent has.
	assert!(matches!(view.seat, SeatView::Concierge(_)));
	// `created_at` is the database's clock and `expires_at` the caller's, a second apart at most.
	assert!((view.order.expires_at() - view.order.created_at() - CONCIERGE_TTL_SECS).abs() <= 2);
	const { assert!(CONCIERGE_TTL_SECS > TTL_SECS) };
	// The request that wrote the order opened the proposal, publishing the catalog first.
	assert_eq!(round(&a).await.proposed, 0, "the executor opens nothing");
	let (version, kinds, topics) = a.concierge.with(|s| s.catalog.clone()).expect("the catalog was published");
	assert_eq!(version, BANK_CATALOG_VERSION);
	assert!(kinds.iter().any(|kind| kind == PAYMENT_KIND), "{kinds:?}");
	assert_eq!(topics, vec![MONEY_MOVEMENT_TOPIC.to_owned(), WALLET_SECURITY_TOPIC.to_owned()]);
	let approval = approval_id_of(&a, id).await;
	assert_eq!(a.concierge.with(|s| s.unproven), 0, "never proposed without the proof of who asked");
	assert_eq!(a.concierge.requested_by(&approval), Some(subject_of(&a.pool, initiator).await));
	let proposal = a.concierge.proposal(&approval);
	assert_eq!((proposal.kind.as_str(), proposal.scope.as_str()), (PAYMENT_KIND, "main"));
	assert_eq!(proposal.subject_ref, approval_subject_ref(id));
	let field = |key: &str| proposal.fields.iter().find(|f| f.key == key).unwrap_or_else(|| panic!("{key}")).value.encoded();
	assert_eq!(field("tier"), "internal");
	assert_eq!(field("source"), "banking's treasury");
	assert_eq!(field("amount"), "250 USDT");
	assert_eq!(field("reason"), "quarterly rebalance");

	// Open: nothing to carry out.
	assert_eq!(round(&a).await, ApprovalRound::default());
	assert_eq!(state_of(&a, id).await, PaymentState::Pending);
	a.concierge.set_state(&approval, ApprovalState::Passed);

	// Passed: approved and reserved, then carried out in the same round — as a failure.
	let second = round(&a).await;
	assert_eq!((second.executed, second.failed, second.voided, second.rejected), (0, 1, 0, 0));
	let view = payments_app::find(&a.payments, id).await.unwrap();
	assert_eq!(view.order.state(), PaymentState::ExecutionFailed);
	assert_eq!(view.order.failure_reason(), Some(payments_app::TREASURY_SPENDING_CLOSED));
	assert_eq!(relayed_kinds(&a.pool, id).await, vec!["reserved".to_owned(), "released".to_owned()]);
	assert_eq!(a.concierge.reports(), vec![(approval.clone(), ApprovalOutcome::Failed)]);
	a.relay.drain().await;
	assert_eq!(
		a.ledger.balance(&Party::Treasury.claim_key()).await.unwrap().locked,
		treasury_locked_before,
		"the reservation was given back"
	);
	assert_eq!(
		a.ledger.balance(&LedgerAccountKey::UserClaim(recipient)).await.unwrap().posted,
		recipient_before,
		"the destination saw nothing"
	);

	// Concierge keeps serving a decision whose outcome it has not recorded: nothing moves again.
	a.concierge.with(|s| s.serve_reported = true);
	for _ in 0..3 {
		round(&a).await;
	}
	assert_eq!(relayed_kinds(&a.pool, id).await, vec!["reserved".to_owned(), "released".to_owned()]);
	a.relay.drain().await;
	assert_eq!(
		a.ledger.balance(&Party::Treasury.claim_key()).await.unwrap().locked,
		treasury_locked_before,
		"one decision, one reservation"
	);
	assert!(a.concierge.reports().iter().all(|(_, outcome)| *outcome == ApprovalOutcome::Failed));

	// A second executor that read the order pending records the decision after the first one
	// failed it: the same decision, already acted on — not a conflict to report void.
	let again = a.payments.record_concierge_approval(id, &approval, 0, now()).await.expect("the same decision, recorded again");
	assert_eq!(again.order.state(), PaymentState::ExecutionFailed);

	reset(&a.pool).await;
}

/// Two executors reach the same passed decision at once — a second replica, or the sweep
/// beside a retry — and the order is still approved, reserved and closed once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_executors_on_one_decision_reserve_once() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals race").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let open = a_proposed_treasury_payment(&a, initiator, "40").await;
	a.concierge.set_state(&open.approval, ApprovalState::Passed);

	let (one, two) = tokio::join!(round(&a), round(&a));
	assert_eq!(one.voided + two.voided, 0, "{one:?} {two:?}");
	assert_eq!(state_of(&a, open.id).await, PaymentState::ExecutionFailed);
	assert_eq!(
		relayed_kinds(&a.pool, open.id).await,
		vec!["reserved".to_owned(), "released".to_owned()],
		"reserved once, released once"
	);
	a.relay.drain().await;
	let (_one, _two) = tokio::join!(round(&a), round(&a));
	assert_eq!(
		relayed_kinds(&a.pool, open.id).await,
		vec!["reserved".to_owned(), "released".to_owned()],
		"nothing more on a later pair"
	);
	a.relay.drain().await;
	assert_eq!(a.ledger.balance(&LedgerAccountKey::UserClaim(open.recipient)).await.unwrap().posted, open.recipient_posted);

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
	let open = a_proposed_treasury_payment(&a, initiator, "75").await;

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
	assert_eq!(a.ledger.balance(&Party::Treasury.claim_key()).await.unwrap().locked, open.treasury_locked);

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
	let recipient = an_investor(&a.pool).await;
	fund(&a, Party::Treasury.claim_key(), "1000").await;
	a.concierge.with(|s| s.tamper_open = Some("0".repeat(64)));
	let refused = open_treasury(&a, initiator, treasury_to(recipient, "10")).await;
	assert!(matches!(refused, Err(DomainError::Precondition(ref why)) if why.contains("disagree")), "{refused:?}");
	let id = only_payment(&a.pool).await;
	assert_eq!(state_of(&a, id).await, PaymentState::Rejected, "the order ends now, not after the owners voted on it");
	assert_eq!(a.concierge.with(|s| s.withdrawn.len()), 1, "and the proposal is taken back");

	// At pull: the decision carries a hash that is not the order's.
	let open = a_proposed_treasury_payment(&a, initiator, "20").await;
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
	let open = a_proposed_treasury_payment(&a, initiator, "30").await;
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

	// The real one still works: approved, then closed failed because the treasury is closed.
	a.concierge.set_state(&open.approval, ApprovalState::Passed);
	round(&a).await;
	assert_eq!(state_of(&a, open.id).await, PaymentState::ExecutionFailed);
	assert_eq!(relayed_kinds(&a.pool, open.id).await, vec!["reserved".to_owned(), "released".to_owned()]);

	reset(&a.pool).await;
}

/// Concierge being away when the order is opened closes it with the reason and frees the
/// source: the proof of who asked is not kept, so there is no later attempt to wait for. Opened
/// again with concierge back, it goes through.
#[tokio::test]
async fn a_concierge_outage_at_open_closes_the_order_and_a_retry_opens_it() {
	let _guard = exclusive().await;
	let Some(mut a) = app("payment approvals outage").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let recipient = an_investor(&a.pool).await;
	fund(&a, Party::Treasury.claim_key(), "1000").await;

	a.concierge.with(|s| s.down = true);
	let down = open_treasury(&a, initiator, treasury_to(recipient, "10")).await;
	assert!(matches!(down, Err(DomainError::Precondition(ref why)) if why.contains("open it again")), "{down:?}");
	let first = only_payment(&a.pool).await;
	assert_eq!(state_of(&a, first).await, PaymentState::Rejected);
	assert!(a.concierge.with(|s| s.approvals.is_empty()));

	a.concierge.with(|s| s.down = false);
	// The catalog gate backs off after a failed publish; a fresh process publishes at once.
	a.catalog = CatalogGate::new();
	let again = open_treasury(&a, initiator, treasury_to(recipient, "10")).await.expect("the source is free again");
	assert!(concierge_seat(&again).approval_id.is_some());
	assert_eq!(again.order.state(), PaymentState::Pending);

	reset(&a.pool).await;
}

/// The initiator of a payment out of the treasury is kept off the owners' roster, so nobody
/// votes on their own request — and without the proof of who asked the order is not written at
/// all. An order whose request died between the write and the proposal is closed by the
/// executor, never proposed later without the proof.
#[tokio::test]
async fn the_initiator_does_not_vote_on_their_own_payment() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals initiator").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let subject = subject_of(&a.pool, initiator).await;
	let recipient = an_investor(&a.pool).await;
	fund(&a, Party::Treasury.claim_key(), "1000").await;
	a.concierge.with(|s| s.owners = vec!["owner-a".into(), subject.clone(), "owner-b".into(), "owner-c".into()]);

	let view = open_treasury(&a, initiator, treasury_to(recipient, "10")).await.unwrap();
	let approval = concierge_seat(&view).approval_id.clone().unwrap();
	assert_eq!(
		a.concierge.voters(&approval),
		vec!["owner-a".to_owned(), "owner-b".to_owned(), "owner-c".to_owned()],
		"the initiator holds no seat"
	);
	assert_eq!(a.concierge.requested_by(&approval), Some(subject));
	payments_app::cancel(&a.payments, view.order.id(), initiator, now()).await.unwrap();

	// No proof, no order.
	for missing in [None, Some("")] {
		let refused = open_with_token(&a, initiator, treasury_to(recipient, "11"), missing).await;
		assert!(
			matches!(refused, Err(DomainError::Precondition(ref why)) if why.contains("sign in again")),
			"{missing:?}: {refused:?}"
		);
	}
	let orders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments").fetch_one(&a.pool).await.unwrap();
	assert_eq!(orders, 1, "only the first order was written");

	// A proof concierge finds stale (older than 120 s) closes the order with the reason.
	a.concierge.with(|s| s.stale_proofs = true);
	let stale = open_treasury(&a, initiator, treasury_to(recipient, "12")).await;
	assert!(matches!(stale, Err(DomainError::Precondition(ref why)) if why.contains("older than 120")), "{stale:?}");
	a.concierge.with(|s| s.stale_proofs = false);

	// A request that died after writing the order: the executor closes it, and proposes nothing.
	let stranded = write_treasury_order(&a, initiator, treasury_to(recipient, "13")).await.order.id();
	assert_eq!(round(&a).await.rejected, 0, "not before the request could still be in flight");
	sqlx::query("UPDATE payment_concierge_approval SET created_at = now() - interval '10 minutes' WHERE payment_id = $1")
		.bind(stranded.raw())
		.execute(&a.pool)
		.await
		.unwrap();
	assert_eq!(round(&a).await.rejected, 1);
	assert_eq!(state_of(&a, stranded).await, PaymentState::Rejected);
	assert_eq!(a.concierge.with(|s| s.unproven), 0, "the executor never proposes without the proof");

	reset(&a.pool).await;
}

/// An order that could pass and then find itself expired is closed before the owners are
/// asked: a decision needs 96 hours (72 open, 24 of veto) plus a margin.
#[tokio::test]
async fn an_order_with_too_little_window_left_is_not_proposed() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals window").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let recipient = an_investor(&a.pool).await;
	fund(&a, Party::Treasury.claim_key(), "1000").await;
	let view = write_treasury_order(&a, initiator, treasury_to(recipient, "10")).await;
	let late = now() + CONCIERGE_TTL_SECS - 97 * 3600;
	let payment_ports = ports(&a);
	let refused = propose_on_open(
		&ApprovalRoundPorts {
			payments: &payment_ports,
			store: &a.payments,
			approvals: &a.concierge,
			catalog: &a.catalog,
		},
		&view,
		&UserProof {
			requested_by: subject_of(&a.pool, initiator).await,
			token: PROOF.into(),
		},
		late,
	)
	.await;
	assert!(matches!(refused, Err(DomainError::Precondition(ref why)) if why.contains("too late")), "{refused:?}");
	assert_eq!(state_of(&a, view.order.id()).await, PaymentState::Rejected);
	assert!(a.concierge.with(|s| s.approvals.is_empty()), "nothing was proposed");

	reset(&a.pool).await;
}

/// A decision past its `execute_by` is not authority any more: the order fails closed with the
/// reason, the reservation is given back, and concierge's refusal to take the outcome is kept
/// on the seat instead of being passed off as "told".
#[tokio::test]
async fn a_decision_past_its_execute_by_fails_the_order_closed() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals execute_by").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	a.concierge.with(|s| s.execute_by = now() - 60);
	let open = a_proposed_treasury_payment(&a, initiator, "25").await;
	a.concierge.set_state(&open.approval, ApprovalState::Passed);
	a.concierge.with(|s| s.refuse_reports = true);

	round(&a).await;
	let view = payments_app::find(&a.payments, open.id).await.unwrap();
	assert_eq!(view.order.state(), PaymentState::ExecutionFailed);
	assert!(
		view.order.failure_reason().is_some_and(|why| why.contains("execution window lapsed")),
		"{:?}",
		view.order.failure_reason()
	);
	assert!(concierge_seat(&view).execute_by.is_some());
	assert_eq!(relayed_kinds(&a.pool, open.id).await, vec!["reserved".to_owned(), "released".to_owned()]);
	let (reported, note): (bool, Option<String>) = sqlx::query_as("SELECT reported_at IS NOT NULL, last_error FROM payment_concierge_approval WHERE payment_id = $1")
		.bind(open.id.raw())
		.fetch_one(&a.pool)
		.await
		.unwrap();
	assert!(reported && note.is_some_and(|why| why.contains("refused the outcome")), "the refusal is kept");
	a.relay.drain().await;
	assert_eq!(a.ledger.balance(&Party::Treasury.claim_key()).await.unwrap().locked, open.treasury_locked);

	reset(&a.pool).await;
}

/// An order whose own window ended is not approved by a decision that arrives late, whether or
/// not the sweep reached it: the decision is reported VOID and the order stays as it was.
#[tokio::test]
async fn a_decision_arriving_after_the_orders_window_approves_nothing() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals expired window").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let open = a_proposed_treasury_payment(&a, initiator, "18").await;
	sqlx::query("UPDATE payments SET expires_at = now() - interval '1 second' WHERE id = $1")
		.bind(open.id.raw())
		.execute(&a.pool)
		.await
		.unwrap();
	a.concierge.set_state(&open.approval, ApprovalState::Passed);

	let late = round(&a).await;
	assert_eq!(late.voided, 1, "{late:?}");
	assert_eq!(state_of(&a, open.id).await, PaymentState::Pending, "no state change from a late decision");
	assert!(relayed_kinds(&a.pool, open.id).await.is_empty());

	reset(&a.pool).await;
}

/// The migration may run twice (a restart after a half-applied boot) and changes nothing the
/// second time.
#[tokio::test]
async fn the_migration_is_idempotent() {
	let _guard = exclusive().await;
	let Some(pool) = common::pool().await else { return };
	let mut tx = pool.begin().await.unwrap();
	sqlx::raw_sql(include_str!("../migrations/0066_payment_approvals.sql"))
		.execute(&mut *tx)
		.await
		.expect("the second run is a no-op");
	tx.commit().await.unwrap();
}

/// The operator's pause holds an approved order without closing it: the decision is recorded,
/// nothing executes and nothing is reported, and the order is carried out once the pause lifts
/// (as a failure: the treasury is closed).
#[tokio::test]
async fn an_operator_pause_holds_a_decided_payment_without_closing_it() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals pause").await else { return };
	reset(&a.pool).await;
	let initiator = an_investor(&a.pool).await;
	let open = a_proposed_treasury_payment(&a, initiator, "15").await;
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
	assert_eq!((done.executed, done.failed), (0, 1), "{done:?}");
	assert_eq!(state_of(&a, open.id).await, PaymentState::ExecutionFailed);
	assert_eq!(a.concierge.reports(), vec![(open.approval, ApprovalOutcome::Failed)]);

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
	let open = a_proposed_treasury_payment(&a, initiator, "12").await;

	let cancelled = payments_app::cancel(&a.payments, open.id, initiator, now()).await.unwrap();
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

/// An investor's money is answered by that investor, signed in, so nobody's proof of who
/// asked is needed for it — and nothing is proposed to concierge.
#[tokio::test]
async fn an_investors_payment_needs_no_proof_of_who_asked() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals investor proof").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let recipient = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let payment_ports = ports(&a);
	let round_ports = ApprovalRoundPorts {
		payments: &payment_ports,
		store: &a.payments,
		approvals: &a.concierge,
		catalog: &a.catalog,
	};
	let own = open_with_proof(&round_ports, investor, investor_payment(investor, recipient, "5"), None, now()).await.unwrap();
	assert!(matches!(own.seat, SeatView::Session(_)));
	assert!(a.concierge.with(|s| s.approvals.is_empty()));

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

fn investor_payment(investor: UserId, recipient: UserId, amount: &str) -> PaymentTerms {
	terms(Party::User(investor), PaymentDestination::Internal(Party::User(recipient)), amount)
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
async fn an_investors_payment_is_answered_in_the_wallet_host_and_settles_once() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals session flow").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let stranger = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let recipient = an_investor(&a.pool).await;
	let recipient_before = a.ledger.balance(&LedgerAccountKey::UserClaim(recipient)).await.unwrap().posted;

	let view = payments_app::open(&ports(&a), stranger, investor_payment(investor, recipient, "30"), now())
		.await
		.expect("open the order");
	let id = view.order.id();
	assert!(matches!(view.seat, SeatView::Session(_)));
	assert!((view.order.expires_at() - view.order.created_at() - TTL_SECS).abs() <= 2, "a consent has 72 hours");
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
		format!("/wallet/payment/{}", id.raw()),
		"a path relative to the wallet host root, never a link carrying a secret"
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
	assert_eq!(emitted[0].1.link_scope, "", "no order has a product end, so it is answered in the platform wallet host");

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
		a.ledger.balance(&LedgerAccountKey::UserClaim(recipient)).await.unwrap().posted - recipient_before,
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
	let recipient = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = payments_app::open(&ports(&a), investor, investor_payment(investor, recipient, "30"), now())
		.await
		.unwrap()
		.order
		.id();

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

/// The operator's reason reaches the investor's notice only where an approval would carry it:
/// one with a link, or too long, is left for the confirmation page, which shows it as text.
#[tokio::test]
async fn an_operators_reason_with_a_link_stays_out_of_the_notice() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals notice reason").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let recipient = an_investor(&a.pool).await;
	let body_of = |reason: String| {
		let a = &a;
		async move {
			let terms = PaymentTerms::new(
				Party::User(investor),
				PaymentDestination::Internal(Party::User(recipient)),
				usdt("5"),
				PaymentReason::new(reason).unwrap(),
			)
			.unwrap();
			let id = payments_app::open(&ports(a), investor, terms, now()).await.unwrap().order.id();
			let body: String = sqlx::query_scalar("SELECT body FROM scope_notification_outbox WHERE payment_id = $1")
				.bind(id.raw())
				.fetch_one(&a.pool)
				.await
				.unwrap();
			payments_app::cancel(&a.payments, id, investor, now()).await.unwrap();
			body
		}
	};
	assert!(body_of("monthly rent".into()).await.contains("Reason: monthly rent."));
	for bad in ["pay at https://evil.example/now".to_owned(), "www.evil.example now".to_owned(), "x".repeat(300)] {
		let body = body_of(bad.clone()).await;
		assert!(body.contains("The reason is on the confirmation page."), "{bad}: {body}");
		assert!(!body.contains("evil") && !body.contains("xxxx"), "{body}");
	}

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
	let recipient = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = payments_app::open(&ports(&a), investor, investor_payment(investor, recipient, "30"), now())
		.await
		.unwrap()
		.order
		.id();

	let late = now() + TTL_SECS + 1;
	let refused = payments_app::answer_consent(&ports(&a), investor, id, ConsentDecision::Approve, &audit(), late).await;
	assert!(matches!(refused, Err(DomainError::NotFound { .. })), "{refused:?}");
	assert!(relayed_kinds(&a.pool, id).await.is_empty());
	let swept = payments_app::sweep(&ports(&a), late).await.unwrap();
	assert_eq!(swept.expired, 1);
	assert_eq!(state_of(&a, id).await, PaymentState::Expired);

	reset(&a.pool).await;
}

/// The pins hold for the session seat: sessions revoked after the order
/// opened void it at the answer — the order fails closed and both audiences are told, through
/// the inbox — and a revocation after the answer but before the money moves fails the
/// execution and gives the reservation back.
#[tokio::test]
async fn a_revocation_voids_the_investors_answer_before_and_after_it_is_given() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals revocation").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let recipient = an_investor(&a.pool).await;
	let operator = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;

	// Before the answer.
	let before = payments_app::open(&ports(&a), operator, investor_payment(investor, recipient, "20"), now())
		.await
		.unwrap()
		.order
		.id();
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
	let after = payments_app::open(&ports(&a), investor, investor_payment(investor, recipient, "20"), now())
		.await
		.unwrap()
		.order
		.id();
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
	let recipient = an_investor(&a.pool).await;
	let operator = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = payments_app::open(&ports(&a), operator, investor_payment(investor, recipient, "20"), now())
		.await
		.unwrap()
		.order
		.id();

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
	assert_eq!(
		link_scopes(&a.pool, id).await,
		vec![("payment_outcome".to_owned(), String::new())],
		"the outcome opens where the consent did"
	);

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
	let recipient = an_investor(&a.pool).await;
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;
	let id = payments_app::open(&ports(&a), investor, investor_payment(investor, recipient, "5"), now())
		.await
		.unwrap()
		.order
		.id();
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
	let recipient = an_investor(&a.pool).await;
	sqlx::query("UPDATE users SET concierge_user_id = NULL WHERE id = $1")
		.bind(investor.raw())
		.execute(&a.pool)
		.await
		.unwrap();
	fund(&a, LedgerAccountKey::UserClaim(investor), "100").await;

	let refused = payments_app::open(&ports(&a), investor, investor_payment(investor, recipient, "5"), now()).await;
	assert!(matches!(refused, Err(DomainError::Conflict(_))), "{refused:?}");
	let orders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments").fetch_one(&a.pool).await.unwrap();
	assert_eq!(orders, 0, "an order nobody can answer must not exist");

	reset(&a.pool).await;
}

/// A mandate notice names its scope, and the outbox keeps it until delivery, so the person
/// reads it in that scope's cabinet.
#[tokio::test]
async fn a_queued_mandate_notice_reaches_the_inbox_with_its_scope() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals mandate notice").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let notice = piggybank_core::ports::security_notices::mandate_changed(investor, &Uuid::new_v4().to_string(), true, "invest-core", "invest", usdt("10"), usdt("50"));
	assert_eq!(notice.link_scope, "invest");
	let mut conn = a.pool.acquire().await.unwrap();
	piggybank_core::infrastructure::scope_notifications::enqueue(&mut conn, &notice, None).await.unwrap();
	drop(conn);

	let inbox = Inbox::default();
	let worker = ScopeNotificationWorker::new(a.pool.clone(), Arc::new(inbox.clone()));
	assert_eq!(worker.drain().await.unwrap(), 1);
	let emitted = inbox.0.lock().unwrap().emitted.clone();
	assert_eq!(emitted.len(), 1);
	assert_eq!((emitted[0].1.link.as_str(), emitted[0].1.link_scope.as_str()), ("/wallet", "invest"));
}

/// A scope name concierge would refuse never enters the outbox: it would fail the emit attempt
/// after attempt, long after the fact that queued it.
#[tokio::test]
async fn the_outbox_refuses_a_scope_concierge_would_refuse() {
	let _guard = exclusive().await;
	let Some(a) = app("payment approvals malformed link scope").await else { return };
	reset(&a.pool).await;
	let investor = an_investor(&a.pool).await;
	let mut notice = piggybank_core::ports::security_notices::mandate_changed(investor, &Uuid::new_v4().to_string(), true, "invest-core", "invest", usdt("10"), usdt("50"));
	notice.link_scope = "In-Vest".to_owned();
	let mut conn = a.pool.acquire().await.unwrap();
	let refused = piggybank_core::infrastructure::scope_notifications::enqueue(&mut conn, &notice, None).await;
	assert!(refused.is_err(), "{refused:?}");
	let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM scope_notification_outbox").fetch_one(&a.pool).await.unwrap();
	assert_eq!(queued, 0);
}

async fn link_scopes(pool: &PgPool, payment: PaymentId) -> Vec<(String, String)> {
	sqlx::query_as("SELECT kind, link_scope FROM scope_notification_outbox WHERE payment_id = $1 AND withdrawn_at IS NULL ORDER BY kind")
		.bind(payment.raw())
		.fetch_all(pool)
		.await
		.unwrap()
}
