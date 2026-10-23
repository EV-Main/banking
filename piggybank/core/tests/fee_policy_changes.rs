//! Integration tests for changing a fund's fee terms (#233) — real Postgres **and**
//! TigerBeetle (no mocks, per the project rules). They run when `DATABASE_URL` is set and a
//! TigerBeetle replica is reachable (`nix run .#db` + `.#tb`), and skip otherwise.
//!
//! What is pinned here is the LIFE of a change rather than the arithmetic of a fee (that is
//! `tests/fee_policy.rs`): the notice period binds exactly while someone holds units; a
//! tightening beyond the house envelope needs an owner to propose it and the owners to carry
//! it; every holder is queued one notice when a change becomes scheduled; a refused or
//! withdrawn quorum closes the change; one change is on its way per product at a time; and a
//! promotion settles the elapsed window at the OLD rate before the new one binds.
//!
//! The owner roster is global (`users.role = 'owner'`), so every test takes
//! [`exclusive`] and starts from a cleared roster; products, holders and changes are all
//! per test.

use std::sync::{
	Arc,
	atomic::{AtomicBool, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use domain::{
	allocations::{Allocation, AllocationAccess, AllocationIcon, AllocationId},
	auth::AuthSubject,
	balance::{LedgerAccountKey, ServiceId},
	consilium::{ConsiliumId, ConsiliumState, VoteDecision},
	error::DomainError,
	fees::{ChangeRequirement, CrystallizationPeriod, FeePolicy, FeePolicyChangeId, FeePolicyChangeState, MAX_EFFECTIVE_FROM_HORIZON_SECS, MIN_NOTICE_SECS, ManagementBasis, Trigger},
	money::{Network, Shares, TxRef, Usdt},
	users::{Email, UserId},
};
use piggybank_core::{
	application::{balance as balance_app, consilium as consilium_app, fees as fee_app, funds as funds_app},
	config::KycGate,
	infrastructure::{
		allocations::PgAllocations,
		consilium::PgConsilia,
		consilium_mailer::ConsiliumMailer,
		custody::StubCustody,
		deposits::PgDeposits,
		fee_policy_changes::PgFeePolicyChanges,
		fees::{PgFeeAssessments, PgFeePolicies, PgPositionAccruals},
		issuance::PgUnitIssuances,
		nav::PgNav,
		outflow::PgOutflowPolicy,
		payments::PgPayments,
		relay::Relay,
		subscriptions::PgSubscriptions,
		users::PgUsers,
		withdrawals::PgWithdrawals,
	},
	ports::{
		AllocationRegistry, ConsiliumRepository, PaymentRepository, UserRepository, WithdrawalRepository,
		consilium::{MAX_CODE_ATTEMPTS, VoteAudit},
		fees::{FeePolicies, FeePolicyChange, FeePolicyChanges, NewFeePolicyChange, PositionAccruals},
		governance_mail::{GovernanceMail, GovernanceMailer, MailDeliveryError},
		ledger::Ledger,
	},
};
use sqlx::{AssertSqlSafe, PgPool};
use tokio::sync::Notify;
use uuid::Uuid;

mod common;

const YEAR: i64 = 365 * 24 * 60 * 60;
const APPROVAL_URL_BASE: &str = "https://example.test/approve";
const CONSENT_URL_BASE: &str = "https://example.test/consent";
const CONFIGURED: [Network; 1] = [Network::Bep20];
/// Slack for the second or two between a test's `now()` and the one the write path stamps.
const CLOCK_SLACK: i64 = 30;

static EXCLUSIVE: std::sync::LazyLock<tokio::sync::Mutex<()>> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
	EXCLUSIVE.lock().await
}

struct Harness {
	pool: PgPool,
	allocations: PgAllocations,
	subs: PgSubscriptions,
	nav: PgNav,
	deposits: PgDeposits,
	policies: PgFeePolicies,
	changes: PgFeePolicyChanges,
	accruals: PgPositionAccruals,
	assessments: PgFeeAssessments,
	issuances: PgUnitIssuances,
	consilia: Arc<dyn ConsiliumRepository>,
	withdrawals: Arc<dyn WithdrawalRepository>,
	payments: Arc<dyn PaymentRepository>,
	users: Arc<dyn UserRepository>,
	outflow: PgOutflowPolicy,
	ledger: Arc<dyn Ledger>,
	relay: Relay,
	notify: Arc<Notify>,
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "fee-policy-change test").await?;
	let notify = Arc::new(Notify::new());
	Some(Harness {
		allocations: PgAllocations::new(pool.clone()),
		subs: PgSubscriptions::new(pool.clone()),
		nav: PgNav::new(pool.clone()),
		deposits: PgDeposits::new(pool.clone()),
		policies: PgFeePolicies::new(pool.clone()),
		changes: PgFeePolicyChanges::new(pool.clone()),
		accruals: PgPositionAccruals::new(pool.clone()),
		assessments: PgFeeAssessments::new(pool.clone()),
		issuances: PgUnitIssuances::new(pool.clone()),
		consilia: Arc::new(PgConsilia::new(pool.clone())),
		withdrawals: Arc::new(PgWithdrawals::new(pool.clone())),
		payments: Arc::new(PgPayments::new(pool.clone())),
		users: Arc::new(PgUsers::new(pool.clone())),
		outflow: PgOutflowPolicy::new(pool.clone()),
		relay: Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone()),
		ledger,
		notify,
		pool,
	})
}

fn policy_ports(h: &Harness) -> fee_app::FeePolicyPorts<'_> {
	fee_app::FeePolicyPorts {
		policies: &h.policies,
		changes: &h.changes,
		allocations: &h.allocations,
		consilia: h.consilia.as_ref(),
		approval_url_base: APPROVAL_URL_BASE,
		governance_mail_wired: true,
	}
}

fn consilium_ports(h: &Harness) -> consilium_app::ConsiliumPorts<'_> {
	consilium_app::ConsiliumPorts {
		consilia: h.consilia.as_ref(),
		withdrawals: h.withdrawals.as_ref(),
		payments: h.payments.as_ref(),
		users: h.users.as_ref(),
		ledger: h.ledger.as_ref(),
		custody: &StubCustody,
		policy: &h.outflow,
		allocations: &h.allocations,
		fee_changes: &h.changes,
		issuances: &h.issuances,
		deposits: &h.deposits,
		addresses: &common::NoAddresses,
		subscriptions: &h.subs,
		nav: &h.nav,
		relay: &h.notify,
		configured: &CONFIGURED,
		kyc: KycGate::LIFTED,
		approval_url_base: APPROVAL_URL_BASE,
		consent_url_base: CONSENT_URL_BASE,
		governance_mail_wired: true,
	}
}

/// The terms as an `AllocationManage` holder reads them: every product, hidden or not.
fn manager(h: &Harness) -> fee_app::PolicyReader<'_> {
	fee_app::PolicyReader {
		allocations: &h.allocations,
		caller: UserId::new(),
		unrestricted: true,
	}
}

/// The terms as one investor reads them: only the products they can see.
fn reader(h: &Harness, caller: UserId) -> fee_app::PolicyReader<'_> {
	fee_app::PolicyReader {
		allocations: &h.allocations,
		caller,
		unrestricted: false,
	}
}

fn fund_ports(h: &Harness) -> funds_app::FundPorts<'_> {
	funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &h.nav,
		relay: &h.notify,
	}
}

fn now() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn policy(management: u32, performance: u32, hurdle: u32, basis: ManagementBasis, period: CrystallizationPeriod) -> FeePolicy {
	FeePolicy::new(management, performance, hurdle, basis, period).unwrap()
}

/// The house terms with the management rate raised past the envelope — the change that
/// needs the owners.
fn dearer() -> FeePolicy {
	policy(300, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual)
}

fn unique_service() -> ServiceId {
	ServiceId::parse(&format!("fpc-{}", Uuid::new_v4())).unwrap()
}

/// A registered, open product with NO policy yet — every test decides its own terms.
async fn open_fund(h: &Harness, service: &ServiceId) {
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "EV Trading", "Systematic crypto", AllocationIcon::default()).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	h.allocations.open(service).await.unwrap();
	h.allocations.set_access(service, AllocationAccess::Invest).await.unwrap();
}

async fn schedule(h: &Harness, requester: UserId, service: &ServiceId, policy: FeePolicy, effective_from: i64, reason: &str) -> Result<FeePolicyChange, DomainError> {
	fee_app::schedule_policy(
		&policy_ports(h),
		requester,
		fee_app::PolicyChangeRequest {
			service: service.clone(),
			policy,
			requested_effective_from_unix: effective_from,
			reason: reason.to_owned(),
		},
		now(),
	)
	.await
}

/// Install terms on a fund with no holders: scheduled and promoted in one breath.
async fn install(h: &Harness, service: &ServiceId, policy: FeePolicy) {
	let change = schedule(h, UserId::new(), service, policy, 0, "").await.unwrap();
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(service).await.unwrap(), Some(policy));
}

/// A provisioned investor with a mirrored identity-plane id, so a notice can be addressed.
async fn investor(h: &Harness) -> UserId {
	let id = unmirrored_investor(h).await;
	mirror(h, id).await;
	id
}

/// A provisioned investor the identity plane has not mirrored yet — a money-plane row with
/// no `concierge_user_id`, as the bridge leaves one until the first platform sign-in lands.
async fn unmirrored_investor(h: &Harness) -> UserId {
	let tag = Uuid::new_v4();
	h.users
		.provision(
			AuthSubject::parse(&format!("fpc-{tag}")).unwrap(),
			Email::parse(&format!("fpc-{}@example.test", tag.simple())).unwrap(),
			true,
		)
		.await
		.unwrap()
		.id()
}

/// Stand in for the bridge mirroring the investor's identity-plane id. Returns that id.
async fn mirror(h: &Harness, user: UserId) -> Uuid {
	let concierge_id = Uuid::new_v4();
	sqlx::query("UPDATE users SET concierge_user_id = $2 WHERE id = $1")
		.bind(user.raw())
		.bind(concierge_id)
		.execute(&h.pool)
		.await
		.unwrap();
	concierge_id
}

async fn fund_user(h: &Harness, user: UserId, amount: &str) {
	let tx_ref = TxRef::parse(&format!("fpc-{}", Uuid::new_v4())).unwrap();
	balance_app::record_deposit(&h.deposits, &h.notify, tx_ref, user, Network::Bep20, usdt(amount)).await.unwrap();
	h.relay.drain().await;
}

/// Subscribe and wait for the projection to land — the projection stamps the accrual
/// clocks, so anything that backdates them must run after it.
async fn subscribe(h: &Harness, user: UserId, service: &ServiceId, amount: &str) {
	funds_app::subscribe(&fund_ports(h), &h.subs, user, service.clone(), usdt(amount), now()).await.unwrap();
	h.relay.drain().await;
	for _ in 0..100 {
		if h.accruals.find(user, service).await.unwrap().is_some_and(|accrual| !accrual.cost_basis.is_zero()) {
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(50)).await;
	}
	panic!("the subscription's position projection never landed");
}

/// A funded holder of `amount` USDT of the product.
async fn holder(h: &Harness, service: &ServiceId, amount: &str) -> UserId {
	let user = investor(h).await;
	fund_user(h, user, amount).await;
	subscribe(h, user, service, amount).await;
	user
}

async fn backdate(h: &Harness, user: UserId, service: &ServiceId, secs: i64) {
	sqlx::query(
		"UPDATE fund_positions SET fees_accrued_at = now() - make_interval(secs => $3), \
		 crystallized_at = now() - make_interval(secs => $3) WHERE user_id = $1 AND service = $2",
	)
	.bind(user.raw())
	.bind(service.as_str())
	.bind(secs as f64)
	.execute(&h.pool)
	.await
	.unwrap();
}

/// Stand in for the mailer having handed every notice of a change to the relay.
async fn deliver_notices(h: &Harness, change: &FeePolicyChange) {
	sqlx::query("UPDATE consilium_mail SET sent_at = now() WHERE fee_policy_change_id = $1 AND sent_at IS NULL")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// Stand in for the mailer having given up on every notice of a change (ten is its ceiling,
/// `tests/consilium_mailer.rs` pins it).
async fn retire_notices(h: &Harness, change: &FeePolicyChange) {
	sqlx::query("UPDATE consilium_mail SET attempts = 10, last_error = 'recipient has no mirrored concierge user id' WHERE fee_policy_change_id = $1 AND sent_at IS NULL")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// Stand in for the mailer having given up on ONE holder's notice, the rest still in the queue.
async fn retire_notice(h: &Harness, change: &FeePolicyChange, user: UserId) {
	sqlx::query("UPDATE consilium_mail SET attempts = 10, last_error = 'recipient has no mirrored concierge user id' WHERE fee_policy_change_id = $1 AND user_id = $2 AND sent_at IS NULL")
		.bind(change.id.raw())
		.bind(user.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// Stand in for the notice period having run: pull a scheduled change's moment into the past.
async fn let_the_notice_run(h: &Harness, change: &FeePolicyChange) {
	sqlx::query("UPDATE fee_policy_changes SET effective_from = now() - interval '1 hour' WHERE id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// The identity plane's relay, stood in for at the mailer's port: DOWN (every send
/// deferred, as an unreachable concierge is) or UP (every send taken and kept, with the
/// recipient it was addressed to), switched by the test.
#[derive(Default)]
struct SwitchedRelay {
	down: AtomicBool,
	seen: std::sync::Mutex<Vec<(Uuid, GovernanceMail)>>,
}

#[async_trait]
impl GovernanceMailer for SwitchedRelay {
	async fn send(&self, recipient: Uuid, _dedupe_key: &str, mail: &GovernanceMail) -> Result<(), MailDeliveryError> {
		if self.down.load(Ordering::SeqCst) {
			return Err(MailDeliveryError::Deferred("governance mail relay: status: Unavailable".into()));
		}
		self.seen.lock().unwrap().push((recipient, mail.clone()));
		Ok(())
	}
}

/// The queue is shared by every test in this binary and drained in batches of 100 by id, so
/// a test that runs the mailer first retires the backlog the others left behind.
async fn quiet_queue(h: &Harness) {
	sqlx::query("UPDATE consilium_mail SET sent_at = now() WHERE sent_at IS NULL AND withdrawn_at IS NULL")
		.execute(&h.pool)
		.await
		.unwrap();
}

/// Every notice row of a change as the queue holds it, by holder: `(sent, withdrawn)`.
async fn notice_states(h: &Harness, change: &FeePolicyChange) -> std::collections::HashMap<UserId, (bool, bool)> {
	let rows: Vec<(Uuid, bool, bool)> =
		sqlx::query_as("SELECT user_id, sent_at IS NOT NULL, withdrawn_at IS NOT NULL FROM consilium_mail WHERE fee_policy_change_id = $1 AND kind = 'fee_policy_notice'")
			.bind(change.id.raw())
			.fetch_all(&h.pool)
			.await
			.unwrap();
	rows.into_iter().map(|(user, sent, withdrawn)| (UserId::from_raw(user), (sent, withdrawn))).collect()
}

/// Stand in for a deferred row's backoff having run out, so the next pass asks the relay again.
async fn let_the_backoff_run(h: &Harness, change: &FeePolicyChange) {
	sqlx::query("UPDATE consilium_mail SET next_attempt_at = NULL WHERE fee_policy_change_id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// Clear the global roster and the cooling-off clock, then seat `n` fresh owners.
async fn owners(h: &Harness, n: usize) -> Vec<UserId> {
	sqlx::query("UPDATE users SET role = 'investor' WHERE role = 'owner'").execute(&h.pool).await.unwrap();
	sqlx::query("DELETE FROM governance_roster_change").execute(&h.pool).await.unwrap();
	let mut roster = Vec::with_capacity(n);
	for _ in 0..n {
		let id = investor(h).await;
		sqlx::query("UPDATE users SET role = 'owner' WHERE id = $1").bind(id.raw()).execute(&h.pool).await.unwrap();
		roster.push(id);
	}
	roster
}

/// The token and code mailed to one seat, read out of the queue as the owner would.
async fn credentials(h: &Harness, consilium: ConsiliumId, voter: UserId) -> (String, String) {
	let payload: String = sqlx::query_scalar("SELECT payload::text FROM consilium_mail WHERE consilium_id = $1 AND user_id = $2 AND kind = 'fee_policy_approval'")
		.bind(consilium.raw())
		.bind(voter.raw())
		.fetch_one(&h.pool)
		.await
		.expect("every eligible seat is mailed an approval");
	let mail: serde_json::Value = serde_json::from_str(&payload).unwrap();
	let url = mail["approval_url"].as_str().unwrap().to_owned();
	(url.rsplit('/').next().unwrap().to_owned(), mail["code"].as_str().unwrap().to_owned())
}

async fn vote(h: &Harness, consilium: ConsiliumId, voter: UserId, decision: VoteDecision) -> bool {
	let (token, code) = credentials(h, consilium, voter).await;
	let audit = VoteAudit {
		client_ip: "203.0.113.7".to_owned(),
		user_agent: "itest".to_owned(),
	};
	let outcome = consilium_app::submit_decision(h.consilia.as_ref(), &token, &code, decision, &audit, now()).await.unwrap();
	if outcome.approved {
		consilium_app::execute(&consilium_ports(h), consilium, now()).await.unwrap();
	}
	outcome.decided
}

async fn change_of(h: &Harness, change: &FeePolicyChange) -> FeePolicyChange {
	h.changes.find(change.id).await.unwrap().expect("the change row exists")
}

async fn consilium_state(h: &Harness, id: ConsiliumId) -> ConsiliumState {
	consilium_app::find(h.consilia.as_ref(), id).await.unwrap().consilium.state()
}

/// Why a change was closed, as the history records it.
async fn closed_reason(h: &Harness, change: &FeePolicyChange) -> String {
	sqlx::query_scalar::<_, Option<String>>("SELECT closed_reason FROM fee_policy_changes WHERE id = $1")
		.bind(change.id.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
		.unwrap_or_default()
}

/// The queued notices for one change, as `(recipient, payload)`.
async fn notices(h: &Harness, change: &FeePolicyChange) -> Vec<(Uuid, serde_json::Value)> {
	let rows: Vec<(Uuid, String, String)> =
		sqlx::query_as("SELECT user_id, dedupe_key, payload::text FROM consilium_mail WHERE fee_policy_change_id = $1 AND kind = 'fee_policy_notice' ORDER BY user_id")
			.bind(change.id.raw())
			.fetch_all(&h.pool)
			.await
			.unwrap();
	rows.into_iter()
		.map(|(user, key, payload)| {
			assert_eq!(key, format!("fee-policy-notice:{}:{user}", change.id), "one notice per holder, keyed by change and holder");
			(user, serde_json::from_str(&payload).unwrap())
		})
		.collect()
}

async fn fee_debt_of(h: &Harness, user: UserId, service: &ServiceId) -> Usdt {
	h.accruals.find(user, service).await.unwrap().expect("a position").debt
}

fn assert_close(actual: Usdt, expected: Usdt, what: &str) {
	let epsilon = usdt("0.05");
	let diff = if actual > expected {
		actual.checked_sub(expected).unwrap()
	} else {
		expected.checked_sub(actual).unwrap()
	};
	assert!(diff <= epsilon, "{what}: expected ~{expected}, got {actual}");
}

#[tokio::test]
async fn a_change_on_a_fund_with_no_holders_binds_at_once_and_notifies_nobody() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;

	// From nothing to the house terms: a tightening, but inside the envelope — an
	// administrator's call, and with nobody holding units there is nobody to give notice to.
	let before = now();
	let change = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!(change.requirement, ChangeRequirement::Admin);
	assert_eq!(change.state, FeePolicyChangeState::Scheduled);
	assert!(change.consilium_id.is_none());
	assert!(
		change.effective_from_unix >= before && change.effective_from_unix <= now() + CLOCK_SLACK,
		"binds at once: {}",
		change.effective_from_unix
	);
	assert!(notices(&h, &change).await.is_empty(), "no holders, no notices");
	// Not live until promoted: the policy port still says the product charges nothing.
	assert_eq!(h.policies.find(&service).await.unwrap(), None);
	assert_eq!(h.changes.pending(&service).await.unwrap().map(|pending| pending.id), Some(change.id));

	assert!(
		fee_app::promote_due(&h.changes, now(), &mut std::collections::HashMap::new()).await.unwrap() >= 1,
		"the sweeper's promotion picks it up"
	);
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	let current = h.policies.current(&service).await.unwrap().expect("a live row");
	assert_eq!(current.version, 1);
	assert_eq!(current.effective_from_unix, change.effective_from_unix);
	let applied = change_of(&h, &change).await;
	assert_eq!(applied.state, FeePolicyChangeState::Active);
	assert!(applied.applied_at_unix.is_some());
	assert!(h.changes.pending(&service).await.unwrap().is_none());
	// A second promotion of the same change is a no-op, not a second version.
	assert!(!h.changes.promote(change.id, now()).await.unwrap());
}

#[tokio::test]
async fn a_change_over_a_held_fund_waits_out_the_notice_and_mails_every_holder() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let first = holder(&h, &service, "1000").await;
	let second = holder(&h, &service, "500").await;

	// A loosening is an administrator's call, but the holders still get their 24 hours.
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	let before = now();
	let change = schedule(&h, UserId::new(), &service, cheaper, 0, "").await.unwrap();
	assert_eq!(change.requirement, ChangeRequirement::Admin);
	assert_eq!(change.state, FeePolicyChangeState::Scheduled);
	assert!(
		change.effective_from_unix >= before + MIN_NOTICE_SECS,
		"never earlier than the notice period: {}",
		change.effective_from_unix
	);
	assert!(change.effective_from_unix <= now() + MIN_NOTICE_SECS + CLOCK_SLACK);

	// One notice per holder, in the same transaction, stating the old and the new terms.
	let queued = notices(&h, &change).await;
	assert_eq!(queued.len(), 2);
	let mut recipients: Vec<Uuid> = queued.iter().map(|(user, _)| *user).collect();
	recipients.sort();
	let mut expected = vec![first.raw(), second.raw()];
	expected.sort();
	assert_eq!(recipients, expected);
	for (_, mail) in &queued {
		assert_eq!(mail["kind"], "fee_policy_notice");
		assert_eq!(mail["fund"], format!("EV Trading ({service})"), "the title, and the slug it is known by");
		assert_eq!(mail["current"]["management_bps"], 200);
		assert_eq!(mail["proposed"]["management_bps"], 100);
		assert_eq!(mail["effective_at"], change.effective_from_unix);
		assert_eq!(mail["link"], format!("/invest/{service}"));
		assert!(!mail["subject_user_id"].as_str().unwrap().is_empty(), "addressed in the identity plane");
	}

	// Not due yet: the old terms stay live and the sweeper leaves the change alone.
	assert!(!h.changes.due(now()).await.unwrap().contains(&change.id));
	assert!(!h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	// Once the moment has come, it is promoted and becomes version 2.
	assert!(h.changes.due(change.effective_from_unix + 1).await.unwrap().contains(&change.id));
	assert!(h.changes.promote(change.id, change.effective_from_unix + 1).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper));
	assert_eq!(h.policies.current(&service).await.unwrap().unwrap().version, 2);
	let history = h.changes.list(&service).await.unwrap();
	assert_eq!(
		history.iter().map(|row| (row.version, row.state)).collect::<Vec<_>>(),
		vec![(2, FeePolicyChangeState::Active), (1, FeePolicyChangeState::Superseded)]
	);

	// A later moment the operator asks for is honoured as it is.
	let later = now() + 3 * MIN_NOTICE_SECS;
	let deferred = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, later, "").await.unwrap();
	assert_eq!(deferred.effective_from_unix, later);
}

#[tokio::test]
async fn a_change_does_not_bind_while_a_holder_notice_has_been_given_up_on() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	// Start below the house terms so a rise back to them tightens without needing the owners.
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	install(&h, &service, cheaper).await;
	holder(&h, &service, "1000").await;
	let change = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!(change.requirement, ChangeRequirement::Admin);
	assert_eq!(notices(&h, &change).await.len(), 1);
	let_the_notice_run(&h, &change).await;

	// The relay has been down since the change was scheduled: the notice sits deferred with
	// not one attempt charged. Nobody has been told, and dearer terms do not bind.
	let deferred = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(matches!(deferred, DomainError::Conflict(_)), "{deferred:?}");
	assert!(
		deferred.to_string().contains("1 holder notice(s) for this change are undelivered, 0 of them given up on"),
		"{deferred}"
	);

	// The relay refused the notice on every attempt until the mailer retired it (ten is the
	// mailer's ceiling, `tests/consilium_mailer.rs` pins it): still a holder never told.
	retire_notices(&h, &change).await;
	let refused = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(matches!(refused, DomainError::Conflict(_)), "{refused:?}");
	assert!(refused.to_string().contains("1 of them given up on"), "{refused}");
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Scheduled, "the change waits; nothing was written");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper), "the old terms stay live");
	assert!(h.changes.due(now()).await.unwrap().contains(&change.id), "still due: the sweeper keeps retrying, and escalating");
	let mut failures = std::collections::HashMap::new();
	fee_app::promote_due(&h.changes, now(), &mut failures).await.unwrap();
	assert_eq!(failures.get(&change.id), Some(&1), "the sweeper counts the refusal towards its error streak");

	// Delivered after all (the relay came back, or the operator reached the holder): the
	// terms bind on the next tick, with nobody's help.
	deliver_notices(&h, &change).await;
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Active);
}

/// One notice row as the mailer left it: `(attempts, deferrals, last_error, sent, subject_user_id)`.
async fn notice_row(h: &Harness, change: &FeePolicyChange, user: UserId) -> (i32, i32, Option<String>, bool, String) {
	sqlx::query_as("SELECT attempts, deferrals, last_error, sent_at IS NOT NULL, payload ->> 'subject_user_id' FROM consilium_mail WHERE fee_policy_change_id = $1 AND user_id = $2")
		.bind(change.id.raw())
		.bind(user.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

const NO_MIRROR: &str = "recipient has no mirrored concierge user id";

/// A holder the identity plane had not mirrored when the change was scheduled (#325): the
/// notice is queued with no addressee, and the worker names one from the mirror at SEND
/// time. Until the mirror lands the row is DEFERRED, not charged — units can be issued days
/// before the holder's first platform sign-in, and a row charged per pass would be given up on
/// five minutes after the scheduling — so a mirror landing after far more passes than the
/// attempt ceiling is still enough for the notice to go out, addressed to it.
#[tokio::test]
async fn a_holder_mirrored_after_the_scheduling_is_still_told() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	install(&h, &service, cheaper).await;
	let late = unmirrored_investor(&h).await;
	fund_user(&h, late, "1000").await;
	subscribe(&h, late, &service, "1000").await;
	quiet_queue(&h).await;

	// A tightening, so that the notice is the one thing the change waits on.
	let change = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	let (_, mail) = notices(&h, &change).await.pop().expect("queued for the unmirrored holder all the same");
	assert_eq!(mail["subject_user_id"], "", "no identity-plane id to name yet");

	// Still unmirrored, pass after pass — more of them than the attempt ceiling allows a
	// refused mail: deferred every time, nothing charged, the reason saying what is missing.
	let relay = Arc::new(SwitchedRelay::default());
	let mailer = ConsiliumMailer::new(h.pool.clone(), relay.clone());
	for pass in 1..=12 {
		let_the_backoff_run(&h, &change).await;
		assert_eq!(mailer.drain().await.unwrap(), 0);
		let (attempts, deferrals, last_error, sent, _) = notice_row(&h, &change, late).await;
		assert_eq!((attempts, deferrals, sent), (0, pass, false), "pass {pass}: deferred, not charged");
		assert_eq!(last_error.as_deref(), Some(NO_MIRROR));
	}
	assert!(relay.seen.lock().unwrap().is_empty(), "nothing was handed over without an address");
	assert_eq!(change_of(&h, &change).await.notices_given_up, 0, "still being tried, so nothing to acknowledge yet");

	// The bridge mirrors the holder (a first platform sign-in): the next pass reaches them,
	// addressed to that id in the identity plane, and the row says so afterwards.
	let concierge_id = mirror(&h, late).await;
	let_the_backoff_run(&h, &change).await;
	assert_eq!(mailer.drain().await.unwrap(), 1);
	let seen = relay.seen.lock().unwrap().clone();
	let [(recipient, GovernanceMail::FeePolicyNotice(notice))] = seen.as_slice() else {
		panic!("one notice, kept as it was handed over: {seen:?}");
	};
	assert_eq!(*recipient, concierge_id);
	assert_eq!(
		notice.subject_user_id,
		concierge_id.to_string(),
		"named as it is addressed — concierge refuses the two disagreeing"
	);
	assert_eq!(notice.proposed.management_bps, 200);
	let (attempts, _, _, sent, subject) = notice_row(&h, &change, late).await;
	assert_eq!((attempts, sent), (0, true), "delivered on the pass after the mirror landed, never charged");
	assert_eq!(subject, concierge_id.to_string(), "the audit row names who it went to");

	// Told, so the dearer terms bind once the period has run — with nobody's acknowledgement.
	assert_eq!(change_of(&h, &change).await.undelivered_notices, 0);
	let_the_notice_run(&h, &change).await;
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
}

/// A holder still unmirrored a whole notice period after the scheduling: the deferral runs
/// out at the ceiling and the notice is given up on with the same reason as before — the row
/// the operator's acknowledgement (0042) is offered over, so a holder the identity plane
/// never mirrors still has the documented way through, and not one minute earlier than the
/// notice period they were owed. An approval mail to an unmirrored owner is charged as
/// before: five minutes, not a day, before somebody is told.
#[tokio::test]
async fn an_unmirrored_holder_is_given_up_on_at_the_ceiling_and_can_then_be_acknowledged() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	install(&h, &service, cheaper).await;
	let never = unmirrored_investor(&h).await;
	fund_user(&h, never, "1000").await;
	subscribe(&h, never, &service, "1000").await;
	quiet_queue(&h).await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();

	let relay = Arc::new(SwitchedRelay::default());
	let mailer = ConsiliumMailer::new(h.pool.clone(), relay.clone());
	assert_eq!(mailer.drain().await.unwrap(), 0);
	assert_eq!(notice_row(&h, &change, never).await.0, 0, "deferred, not charged");
	// The notice period runs out with the holder still unmirrored.
	sqlx::query("UPDATE consilium_mail SET created_at = now() - interval '25 hours' WHERE fee_policy_change_id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	let_the_backoff_run(&h, &change).await;
	assert_eq!(mailer.drain().await.unwrap(), 0);
	let (attempts, _, last_error, sent, _) = notice_row(&h, &change, never).await;
	assert!(attempts >= 10 && !sent, "given up on: attempts={attempts}");
	assert_eq!(last_error.as_deref(), Some(NO_MIRROR));
	assert!(relay.seen.lock().unwrap().is_empty());
	// And never asked about again — a mirror landing now would be too late for the mailer.
	mirror(&h, never).await;
	let_the_backoff_run(&h, &change).await;
	assert_eq!(mailer.drain().await.unwrap(), 0);

	// The dearer terms wait on the operator, exactly as a refused notice did.
	let_the_notice_run(&h, &change).await;
	assert_eq!(change_of(&h, &change).await.notices_given_up, 1);
	let refused = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(refused.to_string().contains("1 of them given up on"), "{refused}");
	let acknowledged = acknowledge(&h, &service, &change, requester).await.unwrap();
	assert_eq!(acknowledged.notices_waiver.map(|waiver| waiver.users), Some(vec![never]));
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
}

/// The relay is down when a change is scheduled and still down when the change is
/// cancelled (#319): the holders' notices are withdrawn with it, so the relay coming back
/// does not tell anybody about terms that will never bind. A notice that had already gone
/// out stands, and neither figures as a notice anybody is still owed.
#[tokio::test]
async fn cancelling_a_change_withdraws_the_notices_the_relay_has_not_taken() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let told = holder(&h, &service, "1000").await;
	let untold = holder(&h, &service, "500").await;
	quiet_queue(&h).await;

	let relay = Arc::new(SwitchedRelay::default());
	let mailer = ConsiliumMailer::new(h.pool.clone(), relay.clone());
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	let change = schedule(&h, UserId::new(), &service, cheaper, 0, "").await.unwrap();
	assert_eq!(notices(&h, &change).await.len(), 2);
	// One holder was reached before the outage; the other's notice sits deferred behind it.
	sqlx::query("UPDATE consilium_mail SET sent_at = now() WHERE fee_policy_change_id = $1 AND user_id = $2")
		.bind(change.id.raw())
		.bind(told.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	relay.down.store(true, Ordering::SeqCst);
	assert_eq!(mailer.drain().await.unwrap(), 0);
	assert_eq!(change_of(&h, &change).await.undelivered_notices, 1, "deferred, and still owed while the change stands");

	let admin = UserId::new();
	let cancelled = fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, change.id, admin, now()).await.unwrap();
	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled);
	assert_eq!(cancelled.undelivered_notices, 0, "a cancelled change waits on nobody");
	let states = notice_states(&h, &change).await;
	assert_eq!(states.get(&told), Some(&(true, false)), "what was delivered stands");
	assert_eq!(states.get(&untold), Some(&(false, true)), "what was not is withdrawn, not delivered");

	// The relay comes back: the withdrawn notice is not handed over — not now, not after its
	// backoff, and it is not among the mails the boot warning would count as pending.
	relay.down.store(false, Ordering::SeqCst);
	let_the_backoff_run(&h, &change).await;
	assert_eq!(mailer.drain().await.unwrap(), 0);
	assert!(relay.seen.lock().unwrap().is_empty(), "nothing about the cancelled change reaches the relay");
	assert_eq!(notice_states(&h, &change).await.get(&untold), Some(&(false, true)));
	assert_eq!(piggybank_core::infrastructure::consilium_mailer::pending_count(&h.pool).await.unwrap(), 0);
	// A repeat of the cancel withdraws nothing more and changes nothing.
	fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, change.id, admin, now()).await.unwrap();
	assert_eq!(notice_states(&h, &change).await.len(), 2);

	// The slot is free: the next change over the same holders queues fresh notices, and
	// those go out — the withdrawal was the cancelled change's alone.
	let next = schedule(&h, UserId::new(), &service, cheaper, 0, "").await.unwrap();
	assert_eq!(mailer.drain().await.unwrap(), 2);
	assert_eq!(change_of(&h, &next).await.undelivered_notices, 0);
	assert!(
		relay
			.seen
			.lock()
			.unwrap()
			.iter()
			.all(|(_, mail)| matches!(mail, GovernanceMail::FeePolicyNotice(notice) if notice.effective_at == next.effective_from_unix))
	);
}

/// A relay that, while it holds the one mail it is handed, has the change CANCELLED from
/// another connection — and hands the mail over only once that cancel is queued on the mail
/// row's lock. The race #319's withdrawal opened: a cancel landing between the worker's read
/// and its write-back.
struct CancellingRelay {
	pool: PgPool,
	service: ServiceId,
	change: FeePolicyChangeId,
	cancel: std::sync::Mutex<Option<tokio::task::JoinHandle<Result<FeePolicyChange, DomainError>>>>,
	seen: AtomicUsize,
}

#[async_trait]
impl GovernanceMailer for CancellingRelay {
	async fn send(&self, _recipient: Uuid, _dedupe_key: &str, _mail: &GovernanceMail) -> Result<(), MailDeliveryError> {
		self.seen.fetch_add(1, Ordering::SeqCst);
		let (pool, service, change) = (self.pool.clone(), self.service.clone(), self.change);
		let cancel = tokio::spawn(async move {
			let changes = PgFeePolicyChanges::new(pool.clone());
			let consilia = PgConsilia::new(pool);
			fee_app::cancel_change(&changes, &consilia, &service, change, UserId::new(), now()).await
		});
		// Not awaited here: the cancel must queue behind the worker's lock on the row, and
		// waiting for it inside the delivery would be the deadlock the lock exists to order.
		for _ in 0..200 {
			let waiting: i64 = sqlx::query_scalar(
				"SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' \
				 AND query LIKE '%consilium_mail SET withdrawn_at%'",
			)
			.fetch_one(&self.pool)
			.await
			.unwrap();
			if waiting >= 1 {
				*self.cancel.lock().unwrap() = Some(cancel);
				return Ok(());
			}
			tokio::time::sleep(std::time::Duration::from_millis(50)).await;
		}
		panic!("the cancel never queued on the mail row's lock within 10s — the worker is not holding it across the send");
	}
}

/// A cancel that lands while the worker is handing a notice over waits for the outcome
/// rather than withdrawing a mail in flight: the row is delivered on the record, the cancel
/// then withdraws nothing (there is nothing left undelivered), and no write trips
/// `consilium_mail_withdrawn_is_unsent`. Without the row lock held across the send the mail
/// would go out AND the write-back would fail, leaving a delivered notice recorded as
/// "nobody told".
#[tokio::test]
async fn a_cancel_racing_a_delivery_waits_for_the_outcome_and_withdraws_nothing_delivered() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let holder_ = holder(&h, &service, "1000").await;
	quiet_queue(&h).await;
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	let change = schedule(&h, UserId::new(), &service, cheaper, 0, "").await.unwrap();

	let relay = Arc::new(CancellingRelay {
		pool: h.pool.clone(),
		service: service.clone(),
		change: change.id,
		cancel: std::sync::Mutex::new(None),
		seen: AtomicUsize::new(0),
	});
	let mailer = ConsiliumMailer::new(h.pool.clone(), relay.clone());
	assert_eq!(mailer.drain().await.unwrap(), 1, "handed over, and the outcome written without tripping the schema");
	assert_eq!(relay.seen.load(Ordering::SeqCst), 1);

	let cancel = relay.cancel.lock().unwrap().take().expect("the cancel was queued during the send");
	let cancelled = cancel.await.unwrap().unwrap();
	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled);
	assert_eq!(notice_states(&h, &change).await.get(&holder_), Some(&(true, false)), "delivered stands; nothing to withdraw");
	// A second pass has nothing left to do for this change.
	assert_eq!(mailer.drain().await.unwrap(), 0);
	assert_eq!(relay.seen.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cheaper_terms_bind_over_an_unreachable_holder_and_a_holder_who_left_holds_nothing() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let unreachable = holder(&h, &service, "1000").await;
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);

	// A loosening over a holder the mailer gave up on: nobody is worse off, and a product
	// whose one holder cannot be reached must still be able to lower its terms.
	let loosening = schedule(&h, UserId::new(), &service, cheaper, 0, "").await.unwrap();
	let_the_notice_run(&h, &loosening).await;
	retire_notices(&h, &loosening).await;
	assert!(h.changes.promote(loosening.id, now()).await.unwrap(), "cheaper terms bind, on the record");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper));

	// A tightening back to the house terms over the same holder, still unreachable: refused.
	let tightening = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	let_the_notice_run(&h, &tightening).await;
	retire_notices(&h, &tightening).await;
	let refused = h.changes.promote(tightening.id, now()).await.unwrap_err();
	assert!(matches!(refused, DomainError::Conflict(_)), "{refused:?}");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper));

	// The holder redeems every unit: a notice that never reached them is no longer about
	// anyone's terms, and the change binds for whoever comes next.
	sqlx::query("UPDATE fund_positions SET units = '0' WHERE user_id = $1 AND service = $2")
		.bind(unreachable.raw())
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(h.changes.promote(tightening.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
}

#[tokio::test]
async fn the_terms_of_a_hidden_product_are_kept_from_an_investor_who_cannot_see_it() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	h.allocations.set_access(&service, AllocationAccess::Hidden).await.unwrap();
	let outsider = investor(&h).await;
	let listed = |views: Vec<(ServiceId, fee_app::PolicyView)>| views.into_iter().any(|(listed, _)| listed == service);

	// A direct read answers as `GetAllocation` does for a product hidden from the caller:
	// as if it were not registered. The catalog of terms simply omits it.
	let Err(view) = fee_app::policy_view(&h.policies, &h.changes, &reader(&h, outsider), &service).await else {
		panic!("the terms of a hidden product were shown to an outsider")
	};
	assert!(matches!(view, DomainError::NotFound { .. }), "{view:?}");
	let history = fee_app::list_changes(&h.changes, &reader(&h, outsider), &service).await.unwrap_err();
	assert!(matches!(history, DomainError::NotFound { .. }), "{history:?}");
	assert!(!listed(fee_app::list_policies(&h.policies, &h.changes, &reader(&h, outsider)).await.unwrap()));

	// A manager reads every product's terms, hidden or not.
	let view = fee_app::policy_view(&h.policies, &h.changes, &manager(&h), &service).await.unwrap();
	assert_eq!(view.current.map(|current| current.policy), Some(FeePolicy::HOUSE));
	assert_eq!(fee_app::list_changes(&h.changes, &manager(&h), &service).await.unwrap().len(), 1);
	assert!(listed(fee_app::list_policies(&h.policies, &h.changes, &manager(&h)).await.unwrap()));

	// Raised to `view` by name, the same investor reads the same terms as anyone listed.
	h.allocations.grant_access(&service, outsider, AllocationAccess::View, outsider).await.unwrap();
	let view = fee_app::policy_view(&h.policies, &h.changes, &reader(&h, outsider), &service).await.unwrap();
	assert_eq!(view.current.map(|current| current.policy), Some(FeePolicy::HOUSE));
	assert_eq!(fee_app::list_changes(&h.changes, &reader(&h, outsider), &service).await.unwrap().len(), 1);
	assert!(listed(fee_app::list_policies(&h.policies, &h.changes, &reader(&h, outsider)).await.unwrap()));
}

#[tokio::test]
async fn a_holder_the_queue_cannot_address_still_gets_the_notice_period() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	// A position with units and no `users` row behind it — nothing the mail queue could
	// reference, but somebody whose terms are about to change all the same.
	sqlx::query("INSERT INTO fund_positions (user_id, service, cost_basis, units, high_water_mark) VALUES ($1, $2, '1000000000', '1000000000000000000000', '1000000000')")
		.bind(Uuid::new_v4())
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();

	let before = now();
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	let change = schedule(&h, UserId::new(), &service, cheaper, 0, "").await.unwrap();
	assert_eq!(change.state, FeePolicyChangeState::Scheduled);
	assert!(
		change.effective_from_unix >= before + MIN_NOTICE_SECS,
		"the floor is decided on every position with units, mailed or not"
	);
	assert!(notices(&h, &change).await.is_empty(), "and nobody the queue cannot address is queued");
	assert!(!h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
}

#[tokio::test]
async fn one_change_is_on_its_way_per_product_until_it_is_cancelled() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	holder(&h, &service, "100").await;

	let first = schedule(
		&h,
		UserId::new(),
		&service,
		policy(150, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual),
		0,
		"",
	)
	.await
	.unwrap();
	let err = schedule(
		&h,
		UserId::new(),
		&service,
		policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual),
		0,
		"",
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("cancel it"), "the refusal must say what to do: {err}");

	let admin = UserId::new();
	let cancelled = fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, first.id, admin, now()).await.unwrap();
	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled);
	// Idempotent, and never promoted.
	assert_eq!(
		fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, first.id, admin, now()).await.unwrap().state,
		FeePolicyChangeState::Cancelled
	);
	let_the_notice_run(&h, &first).await;
	assert!(!h.changes.promote(first.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	// The slot is free again, and a change filed under another product's slug is not this
	// product's to cancel.
	let second = schedule(
		&h,
		UserId::new(),
		&service,
		policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual),
		0,
		"",
	)
	.await
	.unwrap();
	assert!(matches!(
		fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &unique_service(), second.id, admin, now())
			.await
			.unwrap_err(),
		DomainError::NotFound { .. }
	));
	// A live version cannot be cancelled: it is not on its way anywhere.
	let active = h.changes.list(&service).await.unwrap().into_iter().find(|row| row.state == FeePolicyChangeState::Active).unwrap();
	assert!(matches!(
		fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, active.id, admin, now()).await.unwrap_err(),
		DomainError::Conflict(_)
	));
}

#[tokio::test]
async fn a_tightening_beyond_the_envelope_is_proposed_by_an_owner_and_carried_by_the_owners() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let investor = holder(&h, &service, "1000").await;
	let roster = owners(&h, 3).await;

	// An administrator who is not an owner is told exactly why this one is out of reach.
	let err = schedule(&h, UserId::new(), &service, dearer(), 0, "the new mandate costs more to run").await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)), "got {err:?}");
	assert!(err.to_string().contains("only a fund owner"), "{err}");
	// The owners are never asked to approve a change with no stated reason.
	let err = schedule(&h, roster[0], &service, dearer(), 0, "  ").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "got {err:?}");
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "nothing is written before the refusal");

	let opened_at = now();
	let change = schedule(&h, roster[0], &service, dearer(), 0, "the new mandate costs more to run").await.unwrap();
	assert_eq!(change.requirement, ChangeRequirement::OwnerConsilium);
	assert_eq!(change.state, FeePolicyChangeState::AwaitingConsilium);
	assert!(change.scheduled_at_unix.is_none());
	let consilium = change.consilium_id.expect("a change needing the owners names its consilium");
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Open);
	assert!(notices(&h, &change).await.is_empty(), "holders are told when the owners carry it, not before");

	// One approval per eligible seat, naming the fund, both sets of terms and the reason.
	let approvals: Vec<String> = sqlx::query_scalar("SELECT payload::text FROM consilium_mail WHERE consilium_id = $1 AND kind = 'fee_policy_approval'")
		.bind(consilium.raw())
		.fetch_all(&h.pool)
		.await
		.unwrap();
	assert_eq!(approvals.len(), 2, "the initiator holds no seat");
	for payload in &approvals {
		let mail: serde_json::Value = serde_json::from_str(payload).unwrap();
		assert_eq!(mail["fund"], format!("EV Trading ({service})"), "the title, and the slug it is known by");
		assert_eq!(mail["current"]["management_bps"], 200);
		assert_eq!(mail["proposed"]["management_bps"], 300);
		assert_eq!(mail["reason"], "the new mandate costs more to run");
		assert!(!mail["code"].as_str().unwrap().is_empty());
	}
	// The consilium reads back as its own kind, with the live presentation beside the subject.
	let view = consilium_app::find(h.consilia.as_ref(), consilium).await.unwrap();
	let detail = view.fee_policy.expect("a fee-policy consilium carries its presentation");
	assert_eq!(detail.allocation_name, "EV Trading");
	assert_eq!(detail.holder_count, 1);
	assert_eq!(
		h.consilia.list(10).await.unwrap().first().map(|row| row.consilium.id()),
		Some(consilium),
		"the history rehydrates the new kind"
	);

	// Two of the two peers carry it: the change is scheduled from the moment of the vote,
	// the holder gets notice, and the consilium records the change as its effect.
	assert!(!vote(&h, consilium, roster[1], VoteDecision::Approve).await);
	assert!(vote(&h, consilium, roster[2], VoteDecision::Approve).await);
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Executed);
	let scheduled = change_of(&h, &change).await;
	assert_eq!(scheduled.state, FeePolicyChangeState::Scheduled);
	assert!(scheduled.scheduled_at_unix.is_some_and(|at| at >= opened_at));
	assert!(scheduled.effective_from_unix >= opened_at + MIN_NOTICE_SECS, "the notice clock starts at the carrying vote");
	let queued = notices(&h, &scheduled).await;
	assert_eq!(queued.len(), 1);
	assert_eq!(queued[0].0, investor.raw());
	assert_eq!(queued[0].1["proposed"]["management_bps"], 300);
	let view = consilium_app::find(h.consilia.as_ref(), consilium).await.unwrap();
	assert_eq!(view.consilium.executed_fee_policy_change_id(), Some(change.id));
	assert_eq!(view.consilium.executed_withdrawal_id(), None);
	// A retried execution (the sweeper's) is a no-op, not a second scheduling.
	consilium_app::execute(&consilium_ports(&h), consilium, now()).await.unwrap();
	assert_eq!(notices(&h, &scheduled).await.len(), 1);

	// Still the old terms until the notice has run; then version 2, and the consilium's
	// source claim is free for the next change over this product.
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	let_the_notice_run(&h, &scheduled).await;
	deliver_notices(&h, &scheduled).await;
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(dearer()));
	assert_eq!(h.policies.current(&service).await.unwrap().unwrap().version, 2);
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Active);
}

#[tokio::test]
async fn a_refused_or_withdrawn_quorum_closes_the_change() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let roster = owners(&h, 3).await;

	// Refused: with three owners a single peer's refusal ends it, and the change goes with it.
	let service = unique_service();
	open_fund(&h, &service).await;
	let change = schedule(&h, roster[0], &service, dearer(), 0, "refused for the record").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	assert!(vote(&h, consilium, roster[1], VoteDecision::Reject).await);
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Rejected);
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Rejected);
	assert_eq!(h.policies.find(&service).await.unwrap(), None, "a refused change never reaches the live terms");
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "the slot is free for the next proposal");
	// An administrator's cancel after the verdict has nothing to withdraw: the owners'
	// refusal, and the reason it was recorded with, stand.
	let after = fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, change.id, roster[2], now()).await.unwrap();
	assert_eq!(after.state, FeePolicyChangeState::Rejected);
	assert_eq!(closed_reason(&h, &change).await, "the owners' consilium ended rejected");

	// Withdrawn by another owner: the change is cancelled and its consilium goes with it.
	let change = schedule(&h, roster[0], &service, dearer(), 0, "withdrawn for the record").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	// Read as the owner would have, from the invitation delivered before the withdrawal — the
	// queue row's copy is blanked with the withdrawal.
	let (token, code) = credentials(&h, consilium, roster[1]).await;
	let cancelled = fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, change.id, roster[2], now()).await.unwrap();
	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled);
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Cancelled);
	// A vote arriving after the withdrawal lands on a closed consilium and changes nothing.
	let audit = VoteAudit {
		client_ip: "203.0.113.7".to_owned(),
		user_agent: "itest".to_owned(),
	};
	assert!(
		consilium_app::submit_decision(h.consilia.as_ref(), &token, &code, VoteDecision::Approve, &audit, now())
			.await
			.is_err()
	);
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Cancelled);

	// Withdrawn by the owner who opened it, through the consilium itself: the same cascade,
	// as `rejected` — the change did not get its quorum.
	let change = schedule(&h, roster[0], &service, dearer(), 0, "withdrawn by the initiator").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	consilium_app::cancel(h.consilia.as_ref(), consilium, roster[0], now()).await.unwrap();
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Rejected);
}

/// The owners hear how a fee-policy consilium ended, and that a token burned on one of its
/// seats, the way they do for a payout: one outcome mail per member of the audience on the
/// verdict, one burn notice each on the fifth wrong code — both describing the terms (the
/// fund line the approval used, the terms now and proposed) and naming no rail and no
/// payment, since concierge renders exactly one description. The initiator's reason rides
/// the verdict only: a burn notice is an alert about a brute-force attempt, not their request.
#[tokio::test]
async fn the_owners_are_mailed_the_verdict_and_the_burn_of_a_fee_policy_consilium() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let roster = owners(&h, 3).await;
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;

	// Refused by one peer: the verdict reaches the initiator and both seats.
	let change = schedule(&h, roster[0], &service, dearer(), 0, "the new mandate costs more to run").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	assert!(vote(&h, consilium, roster[1], VoteDecision::Reject).await);
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Rejected);
	let outcomes = outcome_mails(&h, consilium, "payout_outcome").await;
	assert_eq!(outcomes.len(), 3, "the initiator and every seat hear the verdict");
	for mail in &outcomes {
		assert_eq!(mail["outcome"], "REJECTED");
		assert_fee_description(mail, &service, "the new mandate costs more to run");
	}

	// A token burned on the next proposal: the whole roster is warned, over the same terms.
	let change = schedule(&h, roster[0], &service, dearer(), 0, "the new mandate costs more to run").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	let (token, _) = credentials(&h, consilium, roster[1]).await;
	let audit = VoteAudit {
		client_ip: "203.0.113.7".to_owned(),
		user_agent: "itest".to_owned(),
	};
	for _ in 0..MAX_CODE_ATTEMPTS {
		consilium_app::submit_decision(h.consilia.as_ref(), &token, "WRONGCODE1", VoteDecision::Approve, &audit, now())
			.await
			.unwrap_err();
	}
	let burns = outcome_mails(&h, consilium, "token_burned").await;
	assert_eq!(burns.len(), 3, "the whole roster hears about a brute-force attempt");
	for mail in &burns {
		assert_eq!(mail["outcome"], "TOKEN_BURNED");
		assert!(mail["detail"].as_str().unwrap().contains(&roster[1].to_string()), "the seat is named: {}", mail["detail"]);
		assert_fee_description(mail, &service, "");
	}
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Open, "one burned token does not disarm the consilium");
}

/// The outcome mails of one kind queued for a consilium, as the worker will read them.
async fn outcome_mails(h: &Harness, consilium: ConsiliumId, kind: &str) -> Vec<serde_json::Value> {
	sqlx::query_scalar::<_, String>("SELECT payload::text FROM consilium_mail WHERE consilium_id = $1 AND kind = $2 ORDER BY user_id")
		.bind(consilium.raw())
		.bind(kind)
		.fetch_all(&h.pool)
		.await
		.unwrap()
		.iter()
		.map(|payload| serde_json::from_str(payload).unwrap())
		.collect()
}

/// What an outcome or burn mail over a fee-policy consilium says — the fee description of
/// the approval mail, and nothing of a payout's or a payment's. `reason` is the initiator's
/// note the mail is expected to carry: theirs on a verdict, none on a burn notice.
fn assert_fee_description(mail: &serde_json::Value, service: &ServiceId, reason: &str) {
	assert_eq!(mail["fund"], format!("EV Trading ({service})"), "the title, and the slug it is known by");
	assert_eq!(mail["current"]["management_bps"], 200, "the house terms in force when it was proposed");
	assert_eq!(mail["proposed"]["management_bps"], 300);
	assert_eq!(mail["proposed"]["basis"], "invested_capital");
	assert_eq!(mail["reason"], reason, "the initiator's note rides the verdict, never the burn notice");
	for empty in ["network", "address", "amount", "tier", "source", "destination"] {
		assert_eq!(mail[empty], "", "a fee outcome names no rail and no payment: {empty}");
	}
}

#[tokio::test]
async fn promotion_settles_the_elapsed_window_at_the_old_rate_before_the_new_one_binds() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let investor = holder(&h, &service, "1000").await;
	// A year on the house 2%, never swept.
	backdate(&h, investor, &service, YEAR).await;
	assert_eq!(fee_debt_of(&h, investor, &service).await, Usdt::ZERO);

	// Halve the management rate. The change binds after the notice; stand in for the notice
	// having run.
	let halved = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	let change = schedule(&h, UserId::new(), &service, halved, 0, "").await.unwrap();
	let_the_notice_run(&h, &change).await;
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(halved));

	// The year that had already passed was priced at the OLD 2% — 20 USDT on the 1000
	// invested — and carried into the position's debt, with the clock restarted at the
	// moment the new terms bound. Nobody re-prices time that has passed.
	assert_close(fee_debt_of(&h, investor, &service).await, usdt("20"), "a year at the old rate, carried");
	let accrual = h.accruals.find(investor, &service).await.unwrap().unwrap();
	let effective_from = change_of(&h, &change).await.effective_from_unix;
	assert_eq!(accrual.accrued_at_unix, effective_from, "the elapsed clock restarts where the new terms begin");

	// The next assessment collects the carried 20 plus an hour at the NEW 1% — a hair over
	// 20, nowhere near the 30 a year at 1% plus a re-priced year would be, and nowhere near
	// the 40 of two years at 2%.
	let charge = fee_app::assess_position(
		&h.policies,
		&h.accruals,
		&h.assessments,
		h.ledger.as_ref(),
		&h.nav,
		&h.notify,
		investor,
		service.clone(),
		Trigger::Period,
		now(),
	)
	.await
	.unwrap()
	.expect("the carried debt is collectable");
	h.relay.drain().await;
	assert_close(charge.charge().due, usdt("20"), "carried debt plus an hour at the new rate");
	assert_close(charge.charge().debt_opening, usdt("20"), "the carried year is the opening debt");
	assert_eq!(fee_debt_of(&h, investor, &service).await, Usdt::ZERO, "a 1000-unit holding covers the charge whole");
	let taken = Shares::from_base_units(h.ledger.balance(&LedgerAccountKey::FeeShares(service.clone())).await.unwrap().posted);
	assert_eq!(taken, charge.charge().charged_units);
}

/// The two management ceilings, as `(table, constraint)`.
const MANAGEMENT_CEILINGS: [(&str, &str); 2] = [
	("fee_policies", "fee_policies_management_ceiling"),
	("fee_policy_changes", "fee_policy_changes_management_ceiling"),
];

/// A policy written under the OLD schema, above today's ceiling — the very row #233 is
/// about. The ceilings are `NOT VALID`, so an INSERT after the migration is held to them;
/// the only way to stage a legacy row is the way the migration met it: with the constraints
/// off, then re-added exactly as the schema states them — read back from the catalogue, so
/// a ceiling moved in a later migration is re-added as moved rather than as this file
/// remembers it.
async fn plant_legacy_policy(h: &Harness, service: &ServiceId, bps: i32) {
	let mut tx = h.pool.begin().await.unwrap();
	let mut definitions = Vec::new();
	for (table, constraint) in MANAGEMENT_CEILINGS {
		let definition: String = sqlx::query_scalar("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = $1")
			.bind(constraint)
			.fetch_one(&mut *tx)
			.await
			.expect("the ceiling exists before it is lifted");
		assert!(definition.ends_with("NOT VALID"), "{constraint} must stay NOT VALID or a legacy row stops the boot: {definition}");
		sqlx::query(AssertSqlSafe(format!("ALTER TABLE {table} DROP CONSTRAINT {constraint}")))
			.execute(&mut *tx)
			.await
			.unwrap();
		definitions.push((table, constraint, definition));
	}
	sqlx::query("INSERT INTO fee_policies (service, management_bps, performance_bps, hurdle_bps, basis, crystallization, updated_by, version, effective_from) VALUES ($1, $2, 2000, 0, 'invested_capital', 'annual', 'legacy', 1, now() - interval '30 days')")
		.bind(service.as_str())
		.bind(bps)
		.execute(&mut *tx)
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO fee_policy_changes (id, service, version, management_bps, performance_bps, hurdle_bps, basis, crystallization, state, requirement, effective_from, requested_by, requested_at, scheduled_at, applied_at) \
		 VALUES ($1, $2, 1, $3, 2000, 0, 'invested_capital', 'annual', 'active', 'admin', now() - interval '30 days', 'legacy', now() - interval '30 days', now() - interval '30 days', now() - interval '30 days')",
	)
	.bind(Uuid::new_v4())
	.bind(service.as_str())
	.bind(bps)
	.execute(&mut *tx)
	.await
	.unwrap();
	for (table, constraint, definition) in definitions {
		sqlx::query(AssertSqlSafe(format!("ALTER TABLE {table} ADD CONSTRAINT {constraint} {definition}")))
			.execute(&mut *tx)
			.await
			.unwrap();
	}
	tx.commit().await.unwrap();
}

#[tokio::test]
async fn a_legacy_policy_above_the_ceiling_can_still_be_lowered() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	plant_legacy_policy(&h, &service, 10_000).await;
	let investor = holder(&h, &service, "1000").await;
	// Two hours on the legacy rate, so the promotion has a window to settle at it.
	backdate(&h, investor, &service, 2 * 60 * 60).await;
	assert_eq!(
		h.policies.find(&service).await.unwrap().map(|p| p.management_bps()),
		Some(10_000),
		"the legacy row reads back as it is"
	);

	// Lowering to the house terms is a loosening: one administrator, and the holder's notice.
	let change = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!(change.requirement, ChangeRequirement::Admin);
	assert_eq!(notices(&h, &change).await.len(), 1);
	let_the_notice_run(&h, &change).await;
	// The promotion supersedes the legacy row — an UPDATE the ceiling must not refuse.
	// Notices still undelivered do not stand in the way of a lowering.
	assert!(
		h.changes.promote(change.id, now()).await.unwrap(),
		"the over-the-ceiling policy is the one that must always be lowerable"
	);
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	let history = h.changes.list(&service).await.unwrap();
	assert_eq!(
		history.iter().map(|row| (row.version, row.state, row.policy.management_bps())).collect::<Vec<_>>(),
		vec![(2, FeePolicyChangeState::Active, 200), (1, FeePolicyChangeState::Superseded, 10_000)]
	);
	// And the window before the change was priced at the legacy rate, as any other.
	assert!(fee_debt_of(&h, investor, &service).await > Usdt::ZERO);
}

#[tokio::test]
async fn a_requirement_decided_against_stale_terms_is_refused_under_the_lock() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;

	// What a request judged before a promotion landed would carry: an administrator's
	// requirement for a change the live terms make the owners'.
	let stale = NewFeePolicyChange {
		id: FeePolicyChangeId::new(),
		service: service.clone(),
		policy: dearer(),
		requirement: ChangeRequirement::Admin,
		requested_effective_from_unix: 0,
		requested_by: UserId::new().to_string(),
		reason: String::new(),
		now_unix: now(),
	};
	let err = h.changes.schedule(&stale, None).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("re-submit"), "{err}");
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "nothing was recorded");
	// The same terms with the requirement the live terms call for are not stale.
	assert_eq!(
		fee_app::policy_view(&h.policies, &h.changes, &manager(&h), &service).await.unwrap().current.map(|c| c.policy),
		Some(FeePolicy::HOUSE)
	);
}

#[tokio::test]
async fn a_consilium_gated_change_is_withdrawn_only_by_an_owner() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let roster = owners(&h, 3).await;
	let service = unique_service();
	open_fund(&h, &service).await;
	let change = schedule(&h, roster[0], &service, dearer(), 0, "for the withdrawal test").await.unwrap();
	let consilium = change.consilium_id.unwrap();

	// An administrator who is not an owner could not have opened the quorum and cannot close it.
	let err = fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, change.id, UserId::new(), now())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)), "got {err:?}");
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Open);
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::AwaitingConsilium);
	// Another owner may.
	let cancelled = fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, change.id, roster[2], now()).await.unwrap();
	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled);
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Cancelled);
	// And the proposer, even after leaving the roster: the request was theirs.
	let change = schedule(&h, roster[1], &service, dearer(), 0, "for the proposer test").await.unwrap();
	sqlx::query("UPDATE users SET role = 'investor' WHERE id = $1")
		.bind(roster[1].raw())
		.execute(&h.pool)
		.await
		.unwrap();
	assert_eq!(
		fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, change.id, roster[1], now())
			.await
			.unwrap()
			.state,
		FeePolicyChangeState::Cancelled
	);
	// An administrator's own change stays an administrator's to withdraw.
	let admin_change = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!(
		fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, admin_change.id, UserId::new(), now())
			.await
			.unwrap()
			.state,
		FeePolicyChangeState::Cancelled
	);
}

/// Every mail of one kind queued for a consilium as the queue holds it, by recipient:
/// `(sent, withdrawn, code)` — the code blank where the row carries none, or no longer.
async fn consilium_mail_states(h: &Harness, consilium: ConsiliumId, kind: &str) -> std::collections::HashMap<UserId, (bool, bool, String)> {
	let rows: Vec<(Uuid, bool, bool, String)> =
		sqlx::query_as("SELECT user_id, sent_at IS NOT NULL, withdrawn_at IS NOT NULL, COALESCE(payload->>'code', '') FROM consilium_mail WHERE consilium_id = $1 AND kind = $2")
			.bind(consilium.raw())
			.bind(kind)
			.fetch_all(&h.pool)
			.await
			.unwrap();
	rows.into_iter().map(|(user, sent, withdrawn, code)| (UserId::from_raw(user), (sent, withdrawn, code))).collect()
}

/// Stand in for a consilium's deferred rows' backoff having run out.
async fn let_the_consilium_backoff_run(h: &Harness, consilium: ConsiliumId) {
	sqlx::query("UPDATE consilium_mail SET next_attempt_at = NULL WHERE consilium_id = $1")
		.bind(consilium.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// Every seat's `notified` flag — what the worker recorded as delivered, by seat.
async fn seats_notified(h: &Harness, consilium: ConsiliumId) -> std::collections::HashMap<UserId, bool> {
	let rows: Vec<(Uuid, bool)> = sqlx::query_as("SELECT user_id, notified FROM consilium_voter WHERE consilium_id = $1")
		.bind(consilium.raw())
		.fetch_all(&h.pool)
		.await
		.unwrap();
	rows.into_iter().map(|(user, notified)| (UserId::from_raw(user), notified)).collect()
}

/// A consilium closed while nothing had left the queue: both seats' invitations withdrawn
/// with their secrets gone, the three verdict mails (initiator and both seats) untouched.
async fn assert_invitations_withdrawn_and_verdict_kept(h: &Harness, consilium: ConsiliumId) {
	let invitations = consilium_mail_states(h, consilium, "fee_policy_approval").await;
	assert_eq!(invitations.len(), 2, "{invitations:?}");
	assert!(invitations.values().all(|(sent, withdrawn, code)| !sent && *withdrawn && code.is_empty()), "{invitations:?}");
	let outcomes = consilium_mail_states(h, consilium, "payout_outcome").await;
	assert_eq!(outcomes.len(), 3, "{outcomes:?}");
	assert!(outcomes.values().all(|(sent, withdrawn, _)| !sent && !withdrawn), "{outcomes:?}");
}

/// The relay is down when an owner proposes a tightening and still down when the change is
/// withdrawn (#342): the seats' approval invitations are withdrawn with the consilium, so the
/// relay coming back does not ask anybody to vote on a request nobody can act on. An
/// invitation that had already gone out stands, and the verdict mail the withdrawal queues
/// is not caught by it — the audience is still told how it ended. The same for every other
/// way a consilium closes without a verdict: the owner's own withdrawal, the window running
/// out, a roster change.
#[tokio::test]
async fn withdrawing_a_consilium_gated_change_withdraws_the_invitations_the_relay_has_not_taken() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let roster = owners(&h, 3).await;
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	quiet_queue(&h).await;

	let relay = Arc::new(SwitchedRelay::default());
	let mailer = ConsiliumMailer::new(h.pool.clone(), relay.clone());
	let change = schedule(&h, roster[0], &service, dearer(), 0, "withdrawn while the relay was down").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	let (told, untold) = (roster[1], roster[2]);
	assert_eq!(consilium_mail_states(&h, consilium, "fee_policy_approval").await.len(), 2, "one invitation per seat");
	// One seat is reached before the outage — its invitation goes through the worker, so
	// the row is a delivered one as the worker leaves it — while the other's is held back
	// (its backoff not yet run) and then sits deferred behind the outage.
	sqlx::query("UPDATE consilium_mail SET next_attempt_at = now() + interval '1 hour' WHERE consilium_id = $1 AND user_id = $2")
		.bind(consilium.raw())
		.bind(untold.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	assert_eq!(mailer.drain().await.unwrap(), 1);
	assert!(matches!(relay.seen.lock().unwrap().as_slice(), [(_, GovernanceMail::FeePolicyApproval(_))]));
	relay.seen.lock().unwrap().clear();
	relay.down.store(true, Ordering::SeqCst);
	let_the_consilium_backoff_run(&h, consilium).await;
	assert_eq!(mailer.drain().await.unwrap(), 0);
	assert_eq!(
		seats_notified(&h, consilium).await,
		[(told, true), (untold, false)].into(),
		"the worker's own record of who was told"
	);

	let cancelled = fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, change.id, roster[0], now()).await.unwrap();
	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled);
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Cancelled);
	let invitations = consilium_mail_states(&h, consilium, "fee_policy_approval").await;
	assert_eq!(invitations.len(), 2);
	let (sent, withdrawn, code) = &invitations[&told];
	assert!(
		*sent && !withdrawn && code.is_empty(),
		"what was delivered stands, its secrets stripped on delivery: {invitations:?}"
	);
	let (sent, withdrawn, code) = &invitations[&untold];
	assert!(
		!sent && *withdrawn && code.is_empty(),
		"what was not is withdrawn, not delivered, and its secrets are gone: {invitations:?}"
	);
	assert_eq!(
		seats_notified(&h, consilium).await,
		[(told, true), (untold, false)].into(),
		"the withdrawal tells nobody and untells nobody"
	);
	let outcomes = consilium_mail_states(&h, consilium, "payout_outcome").await;
	assert_eq!(outcomes.len(), 3, "the initiator and both seats are still told the verdict");
	assert!(
		outcomes.values().all(|(sent, withdrawn, _)| !sent && !withdrawn),
		"the verdict mails queued by the withdrawal are not caught by it: {outcomes:?}"
	);
	assert_eq!(piggybank_core::infrastructure::consilium_mailer::pending_count(&h.pool).await.unwrap(), 3);

	// The relay comes back: the verdict goes out, the withdrawn invitation does not — not now,
	// not after its backoff.
	relay.down.store(false, Ordering::SeqCst);
	let_the_consilium_backoff_run(&h, consilium).await;
	assert_eq!(mailer.drain().await.unwrap(), 3);
	{
		let seen = relay.seen.lock().unwrap();
		assert_eq!(seen.len(), 3);
		assert!(
			seen.iter()
				.all(|(_, mail)| matches!(mail, GovernanceMail::PayoutOutcome(outcome) if outcome.outcome == "CANCELLED")),
			"only the verdict reaches the relay: {seen:?}"
		);
	}
	assert!(consilium_mail_states(&h, consilium, "fee_policy_approval").await[&untold].1, "still withdrawn");
	assert_eq!(piggybank_core::infrastructure::consilium_mailer::pending_count(&h.pool).await.unwrap(), 0);

	// The owner who opened it withdraws it through the consilium itself: the same withdrawal.
	relay.down.store(true, Ordering::SeqCst);
	let change = schedule(&h, roster[0], &service, dearer(), 0, "withdrawn by the initiator").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	assert_eq!(mailer.drain().await.unwrap(), 0);
	consilium_app::cancel(h.consilia.as_ref(), consilium, roster[0], now()).await.unwrap();
	assert_invitations_withdrawn_and_verdict_kept(&h, consilium).await;

	// The window runs out on the next one.
	let change = schedule(&h, roster[0], &service, dearer(), 0, "expired").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	assert_eq!(mailer.drain().await.unwrap(), 0);
	assert!(h.consilia.expire_due(now() + domain::consilium::TTL_SECS + 1).await.unwrap() >= 1);
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Expired);
	assert_invitations_withdrawn_and_verdict_kept(&h, consilium).await;

	// And the roster moves under the one after.
	let change = schedule(&h, roster[0], &service, dearer(), 0, "voided").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	assert_eq!(mailer.drain().await.unwrap(), 0);
	assert!(h.consilia.void_open_for_roster_change(now(), now()).await.unwrap() >= 1);
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Cancelled);
	assert_invitations_withdrawn_and_verdict_kept(&h, consilium).await;
}

#[tokio::test]
async fn a_held_product_gets_no_change_without_a_mailer_and_no_change_beyond_the_horizon() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	let unwired = fee_app::FeePolicyPorts {
		governance_mail_wired: false,
		..policy_ports(&h)
	};
	let request = |policy: FeePolicy, effective_from: i64| fee_app::PolicyChangeRequest {
		service: service.clone(),
		policy,
		requested_effective_from_unix: effective_from,
		reason: String::new(),
	};
	// Nobody holds units: nobody is owed a notice, and the change goes through unwired.
	let first = fee_app::schedule_policy(&unwired, UserId::new(), request(FeePolicy::HOUSE, 0), now()).await.unwrap();
	assert!(h.changes.promote(first.id, now()).await.unwrap());
	holder(&h, &service, "100").await;
	// A holder: the notice is the protection, and a relay that never runs is not notice.
	let err = fee_app::schedule_policy(
		&unwired,
		UserId::new(),
		request(policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual), 0),
		now(),
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("notice"), "{err}");
	assert!(h.changes.pending(&service).await.unwrap().is_none());
	// Wired, the same request is fine — but not a year and a day out.
	let err = schedule(
		&h,
		UserId::new(),
		&service,
		policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual),
		now() + MAX_EFFECTIVE_FROM_HORIZON_SECS + 60,
		"",
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "got {err:?}");
	assert!(
		schedule(
			&h,
			UserId::new(),
			&service,
			policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual),
			now() + MAX_EFFECTIVE_FROM_HORIZON_SECS - 60,
			""
		)
		.await
		.is_ok()
	);
}

/// How many backends of this database are queued on the product lock right now — the one
/// observable fact that says a transaction over the terms is WAITING rather than running.
async fn backends_waiting_on_the_product_lock(pool: &PgPool) -> i64 {
	sqlx::query_scalar(
		"SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' \
		 AND query LIKE '%allocations%FOR UPDATE%'",
	)
	.fetch_one(pool)
	.await
	.unwrap()
}

/// Poll until `n` backends are queued on the product lock, or give up loudly.
async fn wait_for_lock_waiters(pool: &PgPool, n: i64) {
	for _ in 0..200 {
		if backends_waiting_on_the_product_lock(pool).await >= n {
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(50)).await;
	}
	panic!("expected {n} transaction(s) to queue on the product lock; none did within 10s");
}

/// banking#250: the schedule-vs-promote race with two REAL transactions in a forced order.
///
/// An operator's request is judged against the terms it read; a promotion may land between
/// that read and the request's own transaction. The product lock is what turns that into a
/// refusal rather than a change recorded against terms that no longer hold. Here a third
/// transaction holds the product lock, the promotion queues behind it FIRST and the
/// scheduling SECOND, and the lock is released — so the scheduling is guaranteed to run
/// after the promotion has committed, on the terms the promotion wrote.
#[tokio::test]
async fn a_scheduling_queued_behind_a_promotion_is_judged_on_the_promoted_terms() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;

	// The change about to be promoted adds a hurdle — a loosening, an administrator's call,
	// and with no holders it may bind at once.
	let hurdled = policy(200, 2_000, 800, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	let promoting = schedule(&h, UserId::new(), &service, hurdled, 0, "").await.unwrap();
	assert_eq!(promoting.state, FeePolicyChangeState::Scheduled);

	// The request judged BEFORE that promotion: back to the house terms, which against the
	// live (hurdle-less) terms changes nothing and is an administrator's call. Against the
	// promoted terms it LOWERS a hurdle — the owners' call.
	let judged_stale = NewFeePolicyChange {
		id: FeePolicyChangeId::new(),
		service: service.clone(),
		policy: FeePolicy::HOUSE,
		requirement: ChangeRequirement::Admin,
		requested_effective_from_unix: 0,
		requested_by: UserId::new().to_string(),
		reason: String::new(),
		now_unix: now(),
	};
	assert_eq!(domain::fees::requirement_for(Some(&FeePolicy::HOUSE), &judged_stale.policy), ChangeRequirement::Admin);
	assert_eq!(domain::fees::requirement_for(Some(&hurdled), &judged_stale.policy), ChangeRequirement::OwnerConsilium);

	// A third transaction holds the product lock, so the two under test can be queued in a
	// known order behind it.
	let mut gate = h.pool.begin().await.unwrap();
	sqlx::query("SELECT service FROM allocations WHERE service = $1 FOR UPDATE")
		.bind(service.as_str())
		.fetch_one(&mut *gate)
		.await
		.unwrap();

	let promotion = {
		let changes = PgFeePolicyChanges::new(h.pool.clone());
		let id = promoting.id;
		tokio::spawn(async move { changes.promote(id, now()).await })
	};
	wait_for_lock_waiters(&h.pool, 1).await;
	let stale_id = judged_stale.id;
	let scheduling = {
		let changes = PgFeePolicyChanges::new(h.pool.clone());
		tokio::spawn(async move { changes.schedule(&judged_stale, None).await })
	};
	wait_for_lock_waiters(&h.pool, 2).await;
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE), "nothing has moved while the gate is held");

	gate.commit().await.unwrap();

	assert!(promotion.await.unwrap().unwrap(), "the promotion, first in the queue, lands");
	let err = scheduling.await.unwrap().unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("re-submit"), "the refusal names the stale judgement, not the pending slot: {err}");

	assert_eq!(h.policies.find(&service).await.unwrap(), Some(hurdled), "the promoted terms are the live terms");
	assert!(h.changes.find(stale_id).await.unwrap().is_none(), "the stale request left no row behind");
	assert!(h.changes.pending(&service).await.unwrap().is_none());
	assert_eq!(
		h.changes.list(&service).await.unwrap().iter().map(|row| (row.version, row.state)).collect::<Vec<_>>(),
		vec![(2, FeePolicyChangeState::Active), (1, FeePolicyChangeState::Superseded)]
	);
}

#[tokio::test]
async fn the_notice_clock_starts_when_the_owners_carry_the_change_not_when_it_was_proposed() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let investor = holder(&h, &service, "1000").await;
	let roster = owners(&h, 3).await;

	let change = schedule(&h, roster[0], &service, dearer(), 0, "the new mandate costs more to run").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	// The proposal is made to look three days old, provisional moment included. A clock
	// started at the proposal would already have run out.
	sqlx::query("UPDATE fee_policy_changes SET requested_at = now() - interval '3 days', effective_from = now() - interval '3 days' WHERE id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(change_of(&h, &change).await.effective_from_unix < now() - 2 * MIN_NOTICE_SECS);

	let carried_at = now();
	assert!(!vote(&h, consilium, roster[1], VoteDecision::Approve).await);
	assert!(vote(&h, consilium, roster[2], VoteDecision::Approve).await);
	assert_eq!(consilium_state(&h, consilium).await, ConsiliumState::Executed);

	let scheduled = change_of(&h, &change).await;
	assert_eq!(scheduled.state, FeePolicyChangeState::Scheduled);
	assert!(scheduled.scheduled_at_unix.is_some_and(|at| at >= carried_at), "the notice clock is stamped at the carrying vote");
	assert!(
		scheduled.effective_from_unix >= carried_at + MIN_NOTICE_SECS,
		"the holder gets a full day from the vote, not from the proposal: {}",
		scheduled.effective_from_unix
	);
	assert!(scheduled.effective_from_unix <= now() + MIN_NOTICE_SECS + CLOCK_SLACK);
	let queued = notices(&h, &scheduled).await;
	assert_eq!(queued.len(), 1);
	assert_eq!(queued[0].0, investor.raw());
	assert_eq!(queued[0].1["effective_at"], scheduled.effective_from_unix, "the holder is told the real moment");
	// Not due: the day has not passed.
	assert!(!h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
}

#[tokio::test]
async fn a_change_edited_underneath_its_quorum_cannot_spend_the_owners_signature() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let roster = owners(&h, 3).await;

	// The row itself is edited (a rate the ceiling still allows, so the schema lets it
	// through): the owners signed 300 bps, the row now says 400.
	let service = unique_service();
	open_fund(&h, &service).await;
	let change = schedule(&h, roster[0], &service, dearer(), 0, "signed at three hundred").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	sqlx::query("UPDATE fee_policy_changes SET management_bps = 400 WHERE id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(!vote(&h, consilium, roster[1], VoteDecision::Approve).await);
	assert!(vote(&h, consilium, roster[2], VoteDecision::Approve).await);
	let view = consilium_app::find(h.consilia.as_ref(), consilium).await.unwrap();
	assert_eq!(view.consilium.state(), ConsiliumState::ExecutionFailed);
	assert!(
		view.consilium.failure_reason().unwrap_or_default().contains("no longer matches"),
		"reason: {:?}",
		view.consilium.failure_reason()
	);
	// The failed execution closes the change as any verdict short of approval does: never
	// scheduled, and the product's pending slot is free for an honest proposal.
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Rejected);
	assert!(h.changes.pending(&service).await.unwrap().is_none());
	assert!(notices(&h, &change).await.is_empty(), "no holder is told of terms the owners did not approve");
	assert_eq!(h.policies.find(&service).await.unwrap(), None);

	// The SIGNED subject is edited instead — the tamper the payload hash exists for.
	let service = unique_service();
	open_fund(&h, &service).await;
	let change = schedule(&h, roster[0], &service, dearer(), 0, "signed at three hundred").await.unwrap();
	let consilium = change.consilium_id.unwrap();
	sqlx::query("UPDATE consilium SET terms = jsonb_set(terms, '{to,management_bps}', '350') WHERE id = $1")
		.bind(consilium.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(!vote(&h, consilium, roster[1], VoteDecision::Approve).await);
	assert!(vote(&h, consilium, roster[2], VoteDecision::Approve).await);
	let view = consilium_app::find(h.consilia.as_ref(), consilium).await.unwrap();
	assert_eq!(view.consilium.state(), ConsiliumState::ExecutionFailed);
	assert!(
		view.consilium.failure_reason().unwrap_or_default().contains("payload hash"),
		"reason: {:?}",
		view.consilium.failure_reason()
	);
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Rejected);
	assert!(notices(&h, &change).await.is_empty());
	assert_eq!(h.policies.find(&service).await.unwrap(), None);
}

/// The constraint a refused write names, or a panic naming what was accepted instead.
fn refused_by(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>, what: &str) -> String {
	match result {
		Err(sqlx::Error::Database(err)) => {
			assert_eq!(err.code().as_deref(), Some("23514"), "{what}: expected a CHECK violation, got {err}");
			err.constraint().unwrap_or_default().to_owned()
		}
		Err(other) => panic!("{what}: expected a CHECK violation, got {other}"),
		Ok(_) => panic!("{what}: the schema accepted it"),
	}
}

#[tokio::test]
async fn the_schema_holds_every_new_row_to_the_ceilings_and_only_closed_history_above_them() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;

	// The live row: neither ceiling can be crossed by any write path, SQL included.
	let live = |management: i32, performance: i32| {
		sqlx::query(
			"INSERT INTO fee_policies (service, management_bps, performance_bps, hurdle_bps, basis, crystallization, updated_by) VALUES ($1, $2, $3, 0, 'invested_capital', 'annual', 'itest')",
		)
		.bind(service.as_str())
		.bind(management)
		.bind(performance)
	};
	assert_eq!(
		refused_by(live(501, 2_000).execute(&h.pool).await, "management 501 on the live row"),
		"fee_policies_management_ceiling"
	);
	assert_eq!(
		refused_by(live(200, 5_001).execute(&h.pool).await, "performance 5001 on the live row"),
		"fee_policies_performance_ceiling"
	);
	live(500, 5_000).execute(&h.pool).await.expect("the ceilings themselves are legal terms");

	// The history: a row that can still bind is held to the ceilings; a closed one is not.
	let history = |state: &'static str, management: i32, performance: i32, hurdle: i32| {
		sqlx::query(
			"INSERT INTO fee_policy_changes (id, service, version, management_bps, performance_bps, hurdle_bps, basis, crystallization, state, requirement, \
			   effective_from, requested_by, scheduled_at, closed_reason) \
			 SELECT gen_random_uuid(), $1, COALESCE(MAX(version), 0) + 1, $3, $4, $5, 'invested_capital', 'annual', $2, 'admin', now(), 'itest', \
			   CASE WHEN $2 = 'scheduled' THEN now() END, CASE WHEN $2 = 'cancelled' THEN 'itest' END \
			 FROM fee_policy_changes WHERE service = $1",
		)
		.bind(service.as_str())
		.bind(state)
		.bind(management)
		.bind(performance)
		.bind(hurdle)
	};
	assert_eq!(
		refused_by(history("scheduled", 501, 2_000, 0).execute(&h.pool).await, "management 501 scheduled"),
		"fee_policy_changes_management_ceiling"
	);
	assert_eq!(
		refused_by(history("scheduled", 200, 5_001, 0).execute(&h.pool).await, "performance 5001 scheduled"),
		"fee_policy_changes_performance_ceiling"
	);
	assert!(
		refused_by(history("scheduled", 200, 2_000, 10_001).execute(&h.pool).await, "hurdle 10001").contains("hurdle_bps"),
		"a hurdle above 100% is refused by the column check"
	);
	history("cancelled", 10_000, 10_000, 10_000)
		.execute(&h.pool)
		.await
		.expect("closed history is not held to today's ceilings");
	history("scheduled", 500, 5_000, 10_000).execute(&h.pool).await.expect("the ceilings themselves are legal terms");

	// A LEGACY row above the ceiling (planted the way the migration met it) stays readable and
	// takes exactly one kind of UPDATE — the one that closes it. Any edit that leaves it
	// active re-evaluates the NOT VALID ceiling and is refused: the promotion path never
	// touches an active row except to supersede it, and this pins that nothing else may.
	let legacy_service = unique_service();
	open_fund(&h, &legacy_service).await;
	plant_legacy_policy(&h, &legacy_service, 10_000).await;
	assert_eq!(h.policies.find(&legacy_service).await.unwrap().map(|p| p.management_bps()), Some(10_000));
	let touched = sqlx::query("UPDATE fee_policy_changes SET reason = 'touched' WHERE service = $1 AND state = 'active'")
		.bind(legacy_service.as_str())
		.execute(&h.pool)
		.await;
	assert_eq!(refused_by(touched, "editing an active legacy row"), "fee_policy_changes_management_ceiling");
	sqlx::query("UPDATE fee_policy_changes SET state = 'superseded' WHERE service = $1 AND state = 'active'")
		.bind(legacy_service.as_str())
		.execute(&h.pool)
		.await
		.expect("closing a legacy row is the one UPDATE the ceiling lets through");
}

#[tokio::test]
async fn the_horizon_is_inclusive_at_366_days_and_refuses_the_next_second() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	// The moment is passed in explicitly, so the boundary is exact and not a race with the
	// wall clock.
	let at = now();
	let request = |effective_from: i64| fee_app::PolicyChangeRequest {
		service: service.clone(),
		policy: FeePolicy::HOUSE,
		requested_effective_from_unix: effective_from,
		reason: String::new(),
	};

	let err = fee_app::schedule_policy(&policy_ports(&h), UserId::new(), request(at + MAX_EFFECTIVE_FROM_HORIZON_SECS + 1), at)
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "got {err:?}");
	assert!(err.to_string().contains("366 days"), "{err}");
	assert!(h.changes.pending(&service).await.unwrap().is_none());

	let change = fee_app::schedule_policy(&policy_ports(&h), UserId::new(), request(at + MAX_EFFECTIVE_FROM_HORIZON_SECS), at)
		.await
		.expect("exactly 366 days ahead is the last legal moment");
	assert_eq!(change.effective_from_unix, at + MAX_EFFECTIVE_FROM_HORIZON_SECS);
}

#[tokio::test]
async fn a_change_awaiting_the_owners_holds_the_products_single_pending_slot() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let roster = owners(&h, 3).await;
	let service = unique_service();
	open_fund(&h, &service).await;
	let awaiting = schedule(&h, roster[0], &service, dearer(), 0, "holds the slot").await.unwrap();
	assert_eq!(awaiting.state, FeePolicyChangeState::AwaitingConsilium);

	// An administrator's change that needs no quorum still finds the slot taken: two changes
	// on their way would leave the holders told of terms that never arrive.
	let err = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("already pending"), "{err}");
	assert_eq!(h.changes.pending(&service).await.unwrap().map(|pending| pending.id), Some(awaiting.id));
	assert_eq!(h.changes.list(&service).await.unwrap().len(), 1, "the refused request left no row");
}

/// The statement every transaction over a product's terms opens with, exactly as
/// `lock_product` sends it and `pg_stat_activity` echoes it back.
const PRODUCT_LOCK_STATEMENT: &str = "SELECT service FROM allocations WHERE service = $1 FOR UPDATE";

/// A promotion in flight, as the lock sees it: a transaction holding the product's row
/// with nothing committed yet. The caller decides what it changes, then commits it.
async fn promotion_in_flight(h: &Harness, service: &ServiceId) -> sqlx::Transaction<'static, sqlx::Postgres> {
	let mut tx = h.pool.begin().await.unwrap();
	sqlx::query_scalar::<_, String>(PRODUCT_LOCK_STATEMENT).bind(service.as_str()).fetch_one(&mut *tx).await.unwrap();
	tx
}

/// What the application would hand to `schedule` after judging `next` against the terms
/// it read BEFORE any lock — the same decision `schedule_policy` takes, taken here in the
/// open so the test can hold it stale.
async fn judged_request(h: &Harness, service: &ServiceId, next: FeePolicy) -> NewFeePolicyChange {
	let current = h.policies.find(service).await.unwrap();
	NewFeePolicyChange {
		id: FeePolicyChangeId::new(),
		service: service.clone(),
		policy: next,
		requirement: domain::fees::requirement_for(current.as_ref(), &next),
		requested_effective_from_unix: 0,
		requested_by: UserId::new().to_string(),
		reason: String::new(),
		now_unix: now(),
	}
}

/// `schedule` on its own task, so the test can watch it from outside: it must not be able
/// to finish while another transaction holds the product.
fn spawn_schedule(h: &Harness, change: NewFeePolicyChange) -> tokio::task::JoinHandle<Result<FeePolicyChange, DomainError>> {
	let changes = PgFeePolicyChanges::new(h.pool.clone());
	tokio::spawn(async move { changes.schedule(&change, None).await })
}

/// Block until a backend of THIS database is queued on the product lock, or fail. The
/// proof that a racing `schedule` waits rather than proceeds is the waiter itself in
/// `pg_stat_activity` — not a guess at how long the race takes. Scoped to the current
/// database: a sibling suite on the same server takes the same lock in its own.
async fn wait_for_the_lock_waiter(pool: &PgPool) {
	for _ in 0..500 {
		let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query = $1)")
			.bind(PRODUCT_LOCK_STATEMENT)
			.fetch_one(pool)
			.await
			.unwrap();
		if waiting {
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
	}
	panic!("no transaction queued on the product lock within 10s");
}

/// The blocked `schedule` once the promotion has committed — bounded, so a lock that is
/// never released fails the test instead of hanging the suite.
async fn released(handle: tokio::task::JoinHandle<Result<FeePolicyChange, DomainError>>) -> Result<FeePolicyChange, DomainError> {
	tokio::time::timeout(std::time::Duration::from_secs(10), handle)
		.await
		.expect("schedule is released once the promotion commits")
		.expect("schedule did not panic")
}

#[tokio::test]
async fn a_schedule_racing_a_promotion_waits_on_the_lock_and_is_refused_once_the_terms_moved() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;

	// Against the house terms, halving the management rate is a plain loosening: one
	// administrator, no quorum.
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	let request = judged_request(&h, &service, cheaper).await;
	assert_eq!(request.requirement, ChangeRequirement::Admin);

	// A promotion takes the product before the request reaches the lock ...
	let mut promotion = promotion_in_flight(&h, &service).await;
	let schedule = spawn_schedule(&h, request);
	// ... and the request queues behind it rather than reading the terms of a minute ago.
	wait_for_the_lock_waiter(&h.pool).await;
	assert!(!schedule.is_finished(), "schedule finished while another transaction held the product");

	// The promotion grants the holders a 10% hurdle — terms the queued request never saw.
	// Against THEM the same loosening also takes the hurdle away, which is the owners' call.
	sqlx::query("UPDATE fee_policies SET hurdle_bps = 1000 WHERE service = $1")
		.bind(service.as_str())
		.execute(&mut *promotion)
		.await
		.unwrap();
	promotion.commit().await.unwrap();

	let err = released(schedule).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("re-submit"), "{err}");
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "the stale request left no row");
	assert_eq!(h.changes.list(&service).await.unwrap().len(), 1, "only the installed terms are on record");
	let with_hurdle = policy(200, 2_000, 1000, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	assert_eq!(
		fee_app::policy_view(&h.policies, &h.changes, &manager(&h), &service).await.unwrap().current.map(|c| c.policy),
		Some(with_hurdle),
		"the terms the promotion committed are the live ones"
	);
	// Re-submitted against the live terms, the same request is the owners' to decide.
	assert_eq!(judged_request(&h, &service, cheaper).await.requirement, ChangeRequirement::OwnerConsilium);
}

#[tokio::test]
async fn a_schedule_racing_a_promotion_waits_on_the_lock_and_proceeds_once_the_terms_stayed() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	let request = judged_request(&h, &service, cheaper).await;
	let id = request.id;

	let promotion = promotion_in_flight(&h, &service).await;
	let schedule = spawn_schedule(&h, request);
	wait_for_the_lock_waiter(&h.pool).await;
	assert!(!schedule.is_finished(), "schedule finished while another transaction held the product");

	// The lock is a queue, not a refusal: a transaction that changed nothing lets the
	// request through exactly as it was judged.
	promotion.commit().await.unwrap();
	let scheduled = released(schedule).await.expect("the terms the request was judged against are still the live ones");
	assert_eq!(scheduled.id, id);
	assert_eq!(scheduled.state, FeePolicyChangeState::Scheduled);
	assert_eq!(scheduled.requirement, ChangeRequirement::Admin);
	assert_eq!(scheduled.policy, cheaper);
	assert_eq!(h.changes.pending(&service).await.unwrap().map(|pending| pending.id), Some(id));
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE), "nothing binds before the promotion");
}

/// The acknowledgement as the operator gives it: through the use case, so the
/// requester-or-owner rule is exercised on every call.
async fn acknowledge(h: &Harness, service: &ServiceId, change: &FeePolicyChange, by: UserId) -> Result<FeePolicyChange, DomainError> {
	fee_app::acknowledge_undelivered_notices(&h.changes, h.consilia.as_ref(), service, change.id, by, now()).await
}

/// The waiver columns as the row carries them — what the history screen reads.
async fn waiver_row(h: &Harness, change: &FeePolicyChange) -> (Option<String>, Option<i64>, Option<Vec<Uuid>>) {
	sqlx::query_as("SELECT notices_waived_by, EXTRACT(EPOCH FROM notices_waived_at)::bigint, notices_waived_users FROM fee_policy_changes WHERE id = $1")
		.bind(change.id.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

#[tokio::test]
async fn an_acknowledged_notice_lets_a_tightening_bind_and_the_history_says_who_and_for_whom() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	install(&h, &service, cheaper).await;
	let unreachable = holder(&h, &service, "1000").await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!((change.undelivered_notices, change.notices_given_up), (1, 0), "one holder, nothing delivered yet");
	assert_eq!(change.notices_waiver, None);
	let_the_notice_run(&h, &change).await;
	retire_notices(&h, &change).await;
	let retired = change_of(&h, &change).await;
	assert_eq!((retired.undelivered_notices, retired.notices_given_up), (1, 1), "given up on, and still undelivered");

	// Without the acknowledgement: the refusal #263 pinned, unchanged.
	let refused = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(matches!(refused, DomainError::Conflict(_)), "{refused:?}");
	assert!(refused.to_string().contains("1 of them given up on"), "{refused}");

	let before = now();
	let acknowledged = acknowledge(&h, &service, &change, requester).await.unwrap();
	let waiver = acknowledged.notices_waiver.clone().expect("the acknowledgement is on the change");
	assert_eq!(waiver.by, requester.to_string());
	assert!(waiver.at_unix >= before && waiver.at_unix <= now() + CLOCK_SLACK, "{}", waiver.at_unix);
	assert_eq!(waiver.users, vec![unreachable], "exactly the holder whose notice was undelivered");
	assert_eq!(acknowledged.state, FeePolicyChangeState::Scheduled, "an acknowledgement is not a promotion");
	assert_eq!(
		(acknowledged.undelivered_notices, acknowledged.notices_given_up),
		(1, 1),
		"the figures stay honest: the holder is still untold"
	);
	let (by, at, users) = waiver_row(&h, &change).await;
	assert_eq!(by.as_deref(), Some(requester.to_string().as_str()));
	assert_eq!(at, Some(waiver.at_unix));
	assert_eq!(users, Some(vec![unreachable.raw()]));

	// The terms bind over the acknowledged holder, and the record survives the promotion.
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	let active = change_of(&h, &change).await;
	assert_eq!(active.state, FeePolicyChangeState::Active);
	assert_eq!(active.notices_waiver, Some(waiver));
	assert_eq!((active.undelivered_notices, active.notices_given_up), (0, 0), "an active change waits on nobody");
}

#[tokio::test]
async fn an_acknowledgement_covers_the_holders_of_its_moment_and_no_one_else() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	install(&h, &service, cheaper).await;
	let untold = holder(&h, &service, "1000").await;
	let away = holder(&h, &service, "500").await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!(change.undelivered_notices, 2);
	let_the_notice_run(&h, &change).await;
	retire_notices(&h, &change).await;

	// `away` has redeemed everything at the moment of the acknowledgement: they hold nothing
	// to be told about, so the acknowledgement names `untold` alone.
	sqlx::query("UPDATE fund_positions SET units = '0' WHERE user_id = $1 AND service = $2")
		.bind(away.raw())
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	let acknowledged = acknowledge(&h, &service, &change, requester).await.unwrap();
	assert_eq!(acknowledged.notices_waiver.as_ref().map(|waiver| waiver.users.clone()), Some(vec![untold]));

	// They buy back in before the promotion: untold, and NOT on the operator's record — the
	// acknowledgement does not stretch to cover them, and the change waits again.
	sqlx::query("UPDATE fund_positions SET units = '500' WHERE user_id = $1 AND service = $2")
		.bind(away.raw())
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	let refused = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(matches!(refused, DomainError::Conflict(_)), "{refused:?}");
	assert!(refused.to_string().contains("1 of them to holders the acknowledgement by"), "{refused}");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper));

	// Their notice reaches them after all: the change binds — over `untold` on the record,
	// over `away` because they were told.
	sqlx::query("UPDATE consilium_mail SET sent_at = now() WHERE fee_policy_change_id = $1 AND user_id = $2")
		.bind(change.id.raw())
		.bind(away.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
}

#[tokio::test]
async fn there_is_nothing_to_acknowledge_on_a_change_that_is_not_scheduled_or_whose_notices_arrived() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let roster = owners(&h, 3).await;
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	holder(&h, &service, "1000").await;

	// Awaiting the owners: no notice has been queued yet, so there is nothing to waive.
	let awaiting = schedule(&h, roster[0], &service, dearer(), 0, "for the acknowledgement test").await.unwrap();
	assert_eq!(awaiting.state, FeePolicyChangeState::AwaitingConsilium);
	let err = acknowledge(&h, &service, &awaiting, roster[0]).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	assert!(err.to_string().contains("awaiting_consilium"), "{err}");
	assert_eq!(waiver_row(&h, &awaiting).await, (None, None, None));
	fee_app::cancel_change(&h.changes, h.consilia.as_ref(), &service, awaiting.id, roster[0], now()).await.unwrap();

	// A loosening, its notice given up on: it binds over the untold holder by itself, so
	// there is no protection to waive — an acknowledgement would only put "notices waived
	// by X" in the history of a change that never waited on anyone.
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	let loosening = schedule(&h, UserId::new(), &service, cheaper, 0, "").await.unwrap();
	retire_notices(&h, &loosening).await;
	assert_eq!(change_of(&h, &loosening).await.notices_given_up, 1);
	let err = acknowledge(&h, &service, &loosening, roster[1]).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	assert!(err.to_string().contains("only get cheaper"), "{err}");
	assert_eq!(waiver_row(&h, &loosening).await, (None, None, None));
	let_the_notice_run(&h, &loosening).await;
	assert!(h.changes.promote(loosening.id, now()).await.unwrap(), "a loosening binds over the untold holder regardless");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper));

	// A tightening, every notice delivered: the figure the operator acted on is no longer
	// true, and the change binds by itself — a refusal, not a silent no-op.
	let change = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!(change.state, FeePolicyChangeState::Scheduled, "within the envelope: no owners needed");
	deliver_notices(&h, &change).await;
	let err = acknowledge(&h, &service, &change, roster[1]).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	assert!(err.to_string().contains("nothing to acknowledge"), "{err}");
	assert_eq!(waiver_row(&h, &change).await, (None, None, None));

	// Promoted: nothing waits on anybody.
	let_the_notice_run(&h, &change).await;
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	let err = acknowledge(&h, &service, &change, roster[1]).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	assert!(err.to_string().contains("active"), "{err}");

	// A change of another product is not found under this one, as `cancel` answers.
	let other = unique_service();
	open_fund(&h, &other).await;
	let elsewhere = schedule(&h, UserId::new(), &other, cheaper, 0, "").await.unwrap();
	let err = acknowledge(&h, &service, &elsewhere, roster[1]).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { .. }), "{err:?}");
}

#[tokio::test]
async fn an_acknowledgement_waits_for_the_mailer_to_give_up_and_names_only_those_it_gave_up_on() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	install(&h, &service, cheaper).await;
	let stuck = holder(&h, &service, "1000").await;
	let queued = holder(&h, &service, "500").await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!((change.undelivered_notices, change.notices_given_up), (2, 0));

	// Both notices are still in the mailer's queue — nobody has failed to be reached yet,
	// so there is nobody to take responsibility for. The requester's "I take
	// responsibility" a minute after scheduling must not sweep up every holder.
	let err = acknowledge(&h, &service, &change, requester).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	assert!(err.to_string().contains("none has been given up on yet"), "{err}");
	assert_eq!(waiver_row(&h, &change).await, (None, None, None));

	// The mailer gives up on `stuck` alone: the acknowledgement names them and nobody else.
	retire_notice(&h, &change, stuck).await;
	let acknowledged = acknowledge(&h, &service, &change, requester).await.unwrap();
	let waiver = acknowledged.notices_waiver.clone().expect("acknowledged");
	assert_eq!(waiver.users, vec![stuck], "only the holder the mailer gave up on");
	assert_eq!((acknowledged.undelivered_notices, acknowledged.notices_given_up), (2, 1));
	assert_eq!(waiver_row(&h, &change).await, (Some(requester.to_string()), Some(waiver.at_unix), Some(vec![stuck.raw()])));

	// `queued` is still being tried: the change waits on them, acknowledgement or not.
	let_the_notice_run(&h, &change).await;
	let refused = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(matches!(refused, DomainError::Conflict(_)), "{refused:?}");
	assert!(refused.to_string().contains("1 of them to holders the acknowledgement by"), "{refused}");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper));

	// Their notice arrives: the terms bind — over `stuck` on the record, over `queued`
	// because they were told.
	sqlx::query("UPDATE consilium_mail SET sent_at = now() WHERE fee_policy_change_id = $1 AND user_id = $2")
		.bind(change.id.raw())
		.bind(queued.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	assert_eq!(change_of(&h, &change).await.notices_waiver, Some(waiver));
}

#[tokio::test]
async fn a_later_acknowledgement_adds_the_holders_given_up_on_since_and_stands_as_the_latest() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let roster = owners(&h, 1).await;
	let service = unique_service();
	open_fund(&h, &service).await;
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	install(&h, &service, cheaper).await;
	let first_lost = holder(&h, &service, "1000").await;
	let second_lost = holder(&h, &service, "500").await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!((change.undelivered_notices, change.notices_given_up, change.notices_unacknowledged), (2, 0, 0));

	// The mailer gives up on the first holder; the requester takes responsibility for them.
	retire_notice(&h, &change, first_lost).await;
	assert_eq!(change_of(&h, &change).await.notices_unacknowledged, 1, "one holder to offer the acknowledgement over");
	let first = acknowledge(&h, &service, &change, requester).await.unwrap();
	let waiver = first.notices_waiver.clone().expect("acknowledged");
	assert_eq!(waiver.users, vec![first_lost]);
	assert_eq!(
		(first.undelivered_notices, first.notices_given_up, first.notices_unacknowledged),
		(2, 1, 0),
		"covered: nothing left to offer"
	);

	// Then on the second, AFTER the acknowledgement: the record does not stretch to cover
	// them, so the change waits — and the console has somebody to offer again. Without a
	// second acknowledgement this holder would block the tightening forever.
	retire_notice(&h, &change, second_lost).await;
	let_the_notice_run(&h, &change).await;
	let refused = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(matches!(refused, DomainError::Conflict(_)), "{refused:?}");
	assert!(refused.to_string().contains("1 of them to holders the acknowledgement by"), "{refused}");
	let waiting = change_of(&h, &change).await;
	assert_eq!((waiting.undelivered_notices, waiting.notices_given_up, waiting.notices_unacknowledged), (2, 2, 1));
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper));

	// An owner's later acknowledgement adds them. The record is now the owner's, over the
	// whole list: whoever extends it takes responsibility for everyone on it.
	tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
	let second = acknowledge(&h, &service, &change, roster[0]).await.unwrap();
	let extended = second.notices_waiver.clone().expect("still acknowledged");
	assert_eq!(extended.users, vec![first_lost, second_lost], "the earlier record first, the addition after");
	assert_eq!(extended.by, roster[0].to_string());
	assert!(extended.at_unix > waiver.at_unix, "{} <= {}", extended.at_unix, waiver.at_unix);
	assert_eq!(second.notices_unacknowledged, 0);
	assert_eq!(
		waiver_row(&h, &change).await,
		(Some(roster[0].to_string()), Some(extended.at_unix), Some(vec![first_lost.raw(), second_lost.raw()]))
	);

	// A repeat with nobody new to add leaves the latest record as it is.
	tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
	let repeated = acknowledge(&h, &service, &change, requester).await.unwrap();
	assert_eq!(repeated.notices_waiver, Some(extended.clone()));

	// The terms bind over both, and the extended record survives the promotion.
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	let active = change_of(&h, &change).await;
	assert_eq!(active.notices_waiver, Some(extended));
	assert_eq!((active.notices_given_up, active.notices_unacknowledged), (0, 0), "an active change waits on nobody");
}

#[tokio::test]
async fn an_acknowledgement_is_the_requesters_or_an_owners_and_a_repeat_adding_nobody_leaves_it() {
	let _lock = exclusive().await;
	let Some(h) = harness().await else { return };
	let roster = owners(&h, 2).await;
	let service = unique_service();
	open_fund(&h, &service).await;
	let cheaper = policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	install(&h, &service, cheaper).await;
	let unreachable = holder(&h, &service, "1000").await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	retire_notices(&h, &change).await;

	// Another administrator — `AllocationManage` alone — may not waive the holders' notice
	// on a change somebody else asked for.
	let err = acknowledge(&h, &service, &change, UserId::new()).await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)), "{err:?}");
	assert_eq!(waiver_row(&h, &change).await, (None, None, None));

	// An owner may, on anyone's change.
	let first = acknowledge(&h, &service, &change, roster[0]).await.unwrap();
	let waiver = first.notices_waiver.clone().expect("acknowledged");
	assert_eq!(waiver.by, roster[0].to_string());
	assert_eq!(waiver.users, vec![unreachable]);

	// Repeated — by the requester, by the other owner — with nobody given up on since, the
	// record stands untouched: it says who took responsibility, and a retry that adds no
	// holder must not rewrite that.
	tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
	for again in [requester, roster[1]] {
		let repeated = acknowledge(&h, &service, &change, again).await.unwrap();
		assert_eq!(repeated.notices_waiver, Some(waiver.clone()), "idempotent for {again}");
	}
	assert_eq!(waiver_row(&h, &change).await, (Some(roster[0].to_string()), Some(waiver.at_unix), Some(vec![unreachable.raw()])));

	// The schema holds the record whole and non-empty, whichever path writes it.
	let partial = sqlx::query("UPDATE fee_policy_changes SET notices_waived_users = NULL WHERE id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await;
	assert!(refused_by(partial, "a partial waiver").contains("fee_policy_change_notice_waiver_is_whole"));
	let empty = sqlx::query("UPDATE fee_policy_changes SET notices_waived_users = '{}' WHERE id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await;
	assert!(refused_by(empty, "a waiver covering nobody").contains("fee_policy_change_notice_waiver_names_someone"));
}
