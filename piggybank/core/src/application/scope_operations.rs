//! Payouts out of scope accounts and the operations that wait on concierge's approval
//! (`scope.v1.ScopeLedgerService.Payout`, `RequestPayout`, `RequestTransfer`,
//! `GetPendingOperation`), and the executor that opens those approvals and carries out the
//! ones that pass.
//!
//! A payout runs at once only for an automatic reason and inside every ceiling — the client's
//! per operation and per rolling 24 h, and the scope's one daily budget all its clients share
//! (D24(4)); past any of them it is `approval_required`. A requested operation is stored whole
//! and executed ONLY after concierge says its approval passed: the executor re-reads the
//! decision by its id right before moving money and checks it against the stored record —
//! kind, scope, `pending_operation:<id>`, `lines_digest`, `payload_hash` — and the store moves
//! the operation out of `awaiting_approval` once, under its row lock. Nothing a client sends
//! can stand in for an approval: there is no `approval_ref` on the wire.

use std::{
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use domain::{
	balance::LedgerAccountKey,
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, LedgerRefusal, ScopeTransfer, ScopeTransferId, SubjectRef, TransferReason, TransferTerms, canonical},
	scope_operations::{
		AutoPayoutFacts, BANK_CATALOG, BANK_CATALOG_VERSION, OperationBody, OperationId, OperationKind, OperationState, OperationTerms, PayoutLine, PayoutReason, PayoutTerms,
		PendingOperation, RequestedLine, ScopePayout, ScopePayoutId, canonical_lines, check_lines,
	},
	scopes::{ScopeAccountLabel, ScopeId, ScopeState},
};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
	application::scope_ledger::{ScopeLedgerPorts, check_key, granted_scope, same_terms},
	ports::{
		ledger::{Ledger, LockedReads},
		scope_ledger::{Recorded, RequestKey, StoredRequest, TransferError},
		scope_operations::{
			AdmittedOperation, ApprovalOutcome, ApprovalState, ApprovalView, Approvals, ApprovalsError, Execution, ExecutionFacts, OperationAdmission, OperationEnd, OperationExecution,
			OperationStore, OutflowFacts, PayoutAdmission, PayoutStore, Proposal, RPC_PAYOUT, RPC_REQUEST_PAYOUT, RPC_REQUEST_TRANSFER, RecordedOperation, RecordedPayout,
		},
	},
};

/// The scope ledger's ports plus what payouts and operations add.
pub struct OperationPorts<'a> {
	pub base: &'a ScopeLedgerPorts<'a>,
	pub payouts: &'a dyn PayoutStore,
	pub operations: &'a dyn OperationStore,
	/// Nudged after an operation is stored, so its approval is opened at once.
	pub executor: &'a Notify,
}

fn sha256_hex(input: &str) -> String {
	hex::encode(Sha256::digest(input.as_bytes()))
}

/// Every name a request's lines give, in their order.
fn subjects(lines: &[RequestedLine]) -> Vec<String> {
	lines.iter().map(|line| line.auth_subject.clone()).collect()
}

/// The lines with their users resolved, in the order asked. A name banking has no user for is
/// NOT_FOUND: a payout is a credit, so learning that a subject has no wallet here tells the
/// scope's own service nothing it cannot already see in its holders' list.
fn resolve(lines: &[RequestedLine], facts: &OutflowFacts) -> Result<Vec<PayoutLine>, TransferError> {
	lines
		.iter()
		.map(|line| {
			let known = facts.users.iter().find(|user| user.auth_subject == line.auth_subject).ok_or_else(|| DomainError::NotFound {
				entity: "user",
				id: line.auth_subject.clone(),
			})?;
			Ok(PayoutLine {
				user: known.user.id,
				auth_subject: line.auth_subject.clone(),
				amount: line.amount,
			})
		})
		.collect()
}

fn known_scope<'f>(facts: &'f OutflowFacts, scope: &ScopeId) -> Result<&'f crate::ports::compartments::ScopeRecord, TransferError> {
	Ok(facts.scope.as_ref().ok_or_else(|| DomainError::NotFound {
		entity: "scope",
		id: scope.to_string(),
	})?)
}

/// A frozen scope takes no new debit; a closed one may still pay out what it holds.
fn not_frozen(state: ScopeState) -> Result<(), TransferError> {
	match state {
		ScopeState::Frozen => Err(TransferError::Refused(LedgerRefusal::ScopeFrozen)),
		ScopeState::Active | ScopeState::Closed => Ok(()),
	}
}

/// What a client asks of `Payout`, parsed.
#[derive(Clone, Debug)]
pub struct PayoutRequest {
	pub scope: ScopeId,
	pub from: ScopeAccountLabel,
	pub lines: Vec<RequestedLine>,
	/// One of the automatic reasons; [`PayoutReason::Approved`] is banking's own.
	pub reason: PayoutReason,
	pub subject_ref: SubjectRef,
	pub memo: String,
	pub idempotency_key: String,
}

/// Pay out of an account the caller owns into users' compartments in the scope, at once — for
/// an automatic reason and inside every ceiling. The batch is atomic: one linked ledger chain.
pub async fn payout(ports: &OperationPorts<'_>, client: &ClientId, request: PayoutRequest) -> Result<RecordedPayout, TransferError> {
	check_key(&request.idempotency_key)?;
	granted_scope(ports.base, client, &request.scope).await?;
	if request.reason == PayoutReason::Approved {
		return Err(DomainError::Validation("an approved payout is banking's to execute, not a client's reason".into()).into());
	}
	if request.from.is_reserved() {
		return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
	}
	let total = check_lines(&request.lines)?;
	let terms = canonical(&[
		request.from.as_str(),
		request.reason.as_str(),
		request.subject_ref.as_str(),
		&request.memo,
		&canonical_lines(&request.lines),
	]);
	let key = RequestKey {
		client,
		scope: &request.scope,
		rpc: RPC_PAYOUT,
		key: &request.idempotency_key,
	};
	let admission = PayoutChecks {
		base: ports.base,
		client,
		request: &request,
		total,
	};
	match ports.payouts.record_payout(key, &terms, &request.from, &subjects(&request.lines), &admission).await? {
		Recorded::New(recorded) => {
			ports.base.relay.notify_one();
			Ok(recorded)
		}
		Recorded::Existing(stored) => {
			let id = stored_id(&stored, &terms, &request.idempotency_key, "payout")?;
			Ok(ports
				.payouts
				.payout(client, &request.scope, id)
				.await?
				.ok_or_else(|| DomainError::Repository(format!("key {} names payout {id}, which is gone", request.idempotency_key)))?)
		}
	}
}

/// The id an earlier call under the same key recorded, when it asked for the same thing.
fn stored_id(stored: &StoredRequest, terms: &str, key: &str, what: &str) -> Result<Uuid, TransferError> {
	same_terms(stored, terms)?;
	Ok(Uuid::parse_str(&stored.result_id).map_err(|_| DomainError::Repository(format!("key {key} names {what} {:?}, not a uuid", stored.result_id)))?)
}

struct PayoutChecks<'a> {
	base: &'a ScopeLedgerPorts<'a>,
	client: &'a ClientId,
	request: &'a PayoutRequest,
	total: Usdt,
}

#[async_trait]
impl PayoutAdmission for PayoutChecks<'_> {
	async fn admit(&self, facts: &OutflowFacts, reads: &mut dyn LockedReads) -> Result<ScopePayout, TransferError> {
		let request = self.request;
		let scope = known_scope(facts, &request.scope)?;
		// Checked for a new payout only, so a repeat of a key used before a grant changed
		// still gets its original result.
		if !self.base.grants.compartments(self.client, &request.scope) || !self.base.grants.may_open(self.client, &request.scope, &request.from) {
			return Err(TransferError::Refused(LedgerRefusal::NotGranted));
		}
		if facts.source_owner.as_ref() != Some(self.client) {
			return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
		}
		not_frozen(scope.state)?;
		let lines = resolve(&request.lines, facts)?;
		scope
			.policy
			.admit_auto_payout(AutoPayoutFacts {
				total: self.total,
				paid_24h: facts.paid_24h,
				scope_out_24h: facts.scope_out_24h,
			})
			.map_err(TransferError::Refused)?;
		let source = LedgerAccountKey::ScopeAccount(request.scope.clone(), request.from.clone());
		let spendable = self.base.ledger.spendable_locked(&source, reads).await.map_err(DomainError::from)?;
		if Usdt::from_base_units(spendable) < self.total {
			return Err(TransferError::Refused(LedgerRefusal::InsufficientFunds));
		}
		Ok(ScopePayout::request(
			ScopePayoutId::new(),
			PayoutTerms {
				client: self.client.clone(),
				scope: request.scope.clone(),
				from: request.from.clone(),
				lines,
				reason: request.reason,
				subject_ref: request.subject_ref.clone(),
				memo: request.memo.clone(),
				operation_id: None,
			},
		)?)
	}
}

/// What a client asks of `RequestPayout`, parsed.
#[derive(Clone, Debug)]
pub struct RequestPayoutRequest {
	pub scope: ScopeId,
	pub from: ScopeAccountLabel,
	pub lines: Vec<RequestedLine>,
	pub distribution: bool,
	pub subject_ref: SubjectRef,
	pub memo: String,
	pub idempotency_key: String,
}

/// What a client asks of `RequestTransfer`, parsed.
#[derive(Clone, Debug)]
pub struct RequestTransferRequest {
	pub scope: ScopeId,
	pub from: ScopeAccountLabel,
	/// `None` = `scope`.
	pub to_scope: Option<ScopeId>,
	pub to: ScopeAccountLabel,
	pub amount: Usdt,
	pub subject_ref: SubjectRef,
	pub memo: String,
	pub idempotency_key: String,
}

/// Store a payout to be decided by an approval. Ownership is what it takes to ASK: a label
/// whose prefix was taken away still may be paid out of, once the approvers say so — that is
/// how the money left there gets out. Users' compartments still need the `compartments` grant.
pub async fn request_payout(ports: &OperationPorts<'_>, client: &ClientId, request: RequestPayoutRequest) -> Result<RecordedOperation, TransferError> {
	check_key(&request.idempotency_key)?;
	granted_scope(ports.base, client, &request.scope).await?;
	if request.from.is_reserved() {
		return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
	}
	check_lines(&request.lines)?;
	let terms = canonical(&[
		request.from.as_str(),
		if request.distribution { "distribution" } else { "" },
		request.subject_ref.as_str(),
		&request.memo,
		&canonical_lines(&request.lines),
	]);
	let key = RequestKey {
		client,
		scope: &request.scope,
		rpc: RPC_REQUEST_PAYOUT,
		key: &request.idempotency_key,
	};
	let admission = PayoutRequestChecks {
		base: ports.base,
		client,
		request: &request,
	};
	let recorded = ports.operations.record_operation(key, &terms, &request.from, None, &subjects(&request.lines), &admission).await?;
	answer_operation(ports, client, &request.scope, recorded, &terms, &request.idempotency_key).await
}

async fn answer_operation(
	ports: &OperationPorts<'_>,
	client: &ClientId,
	scope: &ScopeId,
	recorded: Recorded<RecordedOperation>,
	terms: &str,
	key: &str,
) -> Result<RecordedOperation, TransferError> {
	match recorded {
		Recorded::New(operation) => {
			ports.executor.notify_one();
			Ok(operation)
		}
		Recorded::Existing(stored) => {
			let id = stored_id(&stored, terms, key, "operation")?;
			Ok(ports
				.operations
				.operation(client, scope, id)
				.await?
				.ok_or_else(|| DomainError::Repository(format!("key {key} names operation {id}, which is gone")))?)
		}
	}
}

/// The digests an operation's approval carries, taken over the operation as it will be stored.
fn admitted(operation: PendingOperation) -> Result<AdmittedOperation, TransferError> {
	let lines_digest = operation.lines_digest_input().map(|input| sha256_hex(&input));
	let payload_hash = sha256_hex(&operation.approval_payload(lines_digest.as_deref())?);
	Ok(AdmittedOperation {
		operation,
		lines_digest,
		payload_hash,
	})
}

struct PayoutRequestChecks<'a> {
	base: &'a ScopeLedgerPorts<'a>,
	client: &'a ClientId,
	request: &'a RequestPayoutRequest,
}

#[async_trait]
impl OperationAdmission for PayoutRequestChecks<'_> {
	async fn admit(&self, facts: &OutflowFacts) -> Result<AdmittedOperation, TransferError> {
		let request = self.request;
		let scope = known_scope(facts, &request.scope)?;
		if !self.base.grants.compartments(self.client, &request.scope) {
			return Err(TransferError::Refused(LedgerRefusal::NotGranted));
		}
		if facts.source_owner.as_ref() != Some(self.client) {
			return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
		}
		not_frozen(scope.state)?;
		let lines = resolve(&request.lines, facts)?;
		let total = check_lines(&request.lines)?;
		let over_scope_budget = scope.policy.over_scope_budget(facts.scope_out_24h, total);
		admitted(PendingOperation::request(
			OperationId::new(),
			OperationTerms {
				client: self.client.clone(),
				scope: request.scope.clone(),
				from: request.from.clone(),
				body: OperationBody::Payout {
					lines,
					distribution: request.distribution,
					over_scope_budget,
				},
				subject_ref: request.subject_ref.clone(),
				memo: request.memo.clone(),
			},
		)?)
	}
}

/// Store a transfer to be decided by an approval — between two accounts of the scope, or into
/// another scope. Both ends must be the caller's own; banking's labels never are.
pub async fn request_transfer(ports: &OperationPorts<'_>, client: &ClientId, request: RequestTransferRequest) -> Result<RecordedOperation, TransferError> {
	check_key(&request.idempotency_key)?;
	granted_scope(ports.base, client, &request.scope).await?;
	let to_scope = request.to_scope.clone().unwrap_or_else(|| request.scope.clone());
	// By the label and the scope, before anything is read: `main` holds only banking's own.
	if request.from.is_reserved() || request.to.is_reserved() || to_scope.is_main() {
		return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
	}
	if request.amount.is_zero() {
		return Err(DomainError::Validation("amount must be positive".into()).into());
	}
	if to_scope == request.scope && request.to == request.from {
		return Err(DomainError::Validation("a transfer needs two different accounts".into()).into());
	}
	let amount = request.amount.base_units().to_string();
	let terms = canonical(&[
		request.from.as_str(),
		to_scope.as_str(),
		request.to.as_str(),
		&amount,
		request.subject_ref.as_str(),
		&request.memo,
	]);
	let key = RequestKey {
		client,
		scope: &request.scope,
		rpc: RPC_REQUEST_TRANSFER,
		key: &request.idempotency_key,
	};
	let admission = TransferRequestChecks {
		client,
		request: &request,
		to_scope: &to_scope,
	};
	let recorded = ports
		.operations
		.record_operation(key, &terms, &request.from, Some((&to_scope, &request.to)), &[], &admission)
		.await?;
	answer_operation(ports, client, &request.scope, recorded, &terms, &request.idempotency_key).await
}

struct TransferRequestChecks<'a> {
	client: &'a ClientId,
	request: &'a RequestTransferRequest,
	to_scope: &'a ScopeId,
}

#[async_trait]
impl OperationAdmission for TransferRequestChecks<'_> {
	async fn admit(&self, facts: &OutflowFacts) -> Result<AdmittedOperation, TransferError> {
		let request = self.request;
		let scope = known_scope(facts, &request.scope)?;
		// A destination that does not exist reads like someone else's, as a source does.
		if facts.source_owner.as_ref() != Some(self.client) || facts.destination_owner.as_ref() != Some(self.client) {
			return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
		}
		not_frozen(scope.state)?;
		admitted(PendingOperation::request(
			OperationId::new(),
			OperationTerms {
				client: self.client.clone(),
				scope: request.scope.clone(),
				from: request.from.clone(),
				body: OperationBody::Transfer {
					to_scope: self.to_scope.clone(),
					to: request.to.clone(),
					amount: request.amount,
				},
				subject_ref: request.subject_ref.clone(),
				memo: request.memo.clone(),
			},
		)?)
	}
}

/// One operation the caller requested in `scope`; anyone else's is the same NOT_FOUND as none.
pub async fn get_pending_operation(ports: &OperationPorts<'_>, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<RecordedOperation, TransferError> {
	granted_scope(ports.base, client, scope).await?;
	Ok(ports.operations.operation(client, scope, id).await?.ok_or_else(|| DomainError::NotFound {
		entity: "operation",
		id: id.to_string(),
	})?)
}

/// Operations opened, checked or executed per step of one executor round.
const BATCH: u32 = 50;
/// How long an operation whose approval is still open waits before it is asked about again.
const CHECK_EVERY_SECS: u32 = 60;
/// How long after an anomaly an operation is looked at again — long enough not to flood the
/// log, short enough that a fix shows within the hour.
const ANOMALY_RETRY_SECS: u32 = 3600;

/// The wait after `attempts` failures to reach concierge: doubling from 30 s, at most 15 min.
fn backoff(attempts: u32) -> u32 {
	30u32.saturating_mul(1u32 << attempts.min(5)).min(900)
}

/// Whether banking's `bank:*` catalog stands published in concierge for this process. A
/// proposal of a kind concierge does not know is refused for good, so nothing is proposed
/// until the catalog went through; publishing again on every start is free — the same
/// version with the same kinds is a no-op there.
#[derive(Default)]
pub struct CatalogGate {
	state: Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
	settled: bool,
	failures: u32,
	retry_at: Option<Instant>,
}

impl CatalogGate {
	pub fn new() -> Self {
		Self::default()
	}

	/// Publish the catalog unless this process already did; `true` once proposing may go on.
	/// A transient failure is retried with a doubling backoff and keeps proposals waiting;
	/// a refusal (an older version than concierge stores — a newer banking published — or a
	/// catalog concierge's floor rejects) is logged and settles the gate: proposals then meet
	/// whatever catalog concierge holds.
	async fn ensure(&self, approvals: &dyn Approvals) -> bool {
		{
			let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			if state.settled {
				return true;
			}
			if state.retry_at.is_some_and(|at| Instant::now() < at) {
				return false;
			}
		}
		let outcome = approvals.publish_catalog(BANK_CATALOG_VERSION, BANK_CATALOG).await;
		let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		match outcome {
			Ok(()) => {
				info!(version = BANK_CATALOG_VERSION, "scope operations: bank:* approval catalog published");
				state.settled = true;
			}
			Err(ApprovalsError::Refused(why)) => {
				error!(
					version = BANK_CATALOG_VERSION,
					"scope operations: concierge refused the bank:* approval catalog; proposals meet the catalog it holds: {why}"
				);
				state.settled = true;
			}
			Err(ApprovalsError::Unavailable(why)) => {
				let wait = backoff(state.failures);
				state.failures = state.failures.saturating_add(1);
				state.retry_at = Some(Instant::now() + Duration::from_secs(u64::from(wait)));
				warn!(retry_in_secs = wait, "scope operations: could not publish the bank:* approval catalog, proposals wait: {why}");
			}
		}
		state.settled
	}
}

/// The driven ports the executor runs on.
pub struct ExecutorPorts<'a> {
	pub operations: &'a dyn OperationStore,
	pub approvals: &'a dyn Approvals,
	pub catalog: &'a CatalogGate,
	pub ledger: &'a dyn Ledger,
	/// Nudged after an execution commits so the relay posts it at once.
	pub relay: &'a Notify,
}

/// What one executor round did — for the tests and the log.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExecutorRound {
	pub proposed: usize,
	pub executed: usize,
	pub failed: usize,
	pub rejected: usize,
	/// Approvals reported VOID: their decision did not match the stored operation, or the
	/// operation had already ended.
	pub voided: usize,
}

/// One pass: open the approvals not opened yet, carry out the decisions that passed, and end
/// the operations whose approval ended otherwise. A step concierge does not answer is left for
/// the next pass; nothing here retries a terminal operation.
pub async fn executor_round(ports: &ExecutorPorts<'_>) -> Result<ExecutorRound, DomainError> {
	let mut round = ExecutorRound::default();
	if ports.catalog.ensure(ports.approvals).await {
		propose_due(ports, &mut round).await?;
	}
	execute_approved(ports, &mut round).await?;
	settle_decided(ports, &mut round).await?;
	Ok(round)
}

fn proposal(recorded: &RecordedOperation) -> Result<Proposal, DomainError> {
	let operation = &recorded.operation;
	let fields = operation.approval_fields(recorded.lines_digest.as_deref())?;
	let terms = operation.terms();
	let reason = if terms.memo.is_empty() {
		match operation.kind() {
			OperationKind::ScopePayout => format!("Payout requested by service {}", terms.client),
			OperationKind::AccountTransfer | OperationKind::ScopeTransfer => format!("Transfer requested by service {}", terms.client),
		}
	} else {
		terms.memo.clone()
	};
	Ok(Proposal {
		kind: operation.kind().as_str().to_owned(),
		scope: terms.scope.as_str().to_owned(),
		subject_ref: operation.approval_subject_ref(),
		reason,
		fields,
		dedupe_key: operation.approval_subject_ref(),
	})
}

async fn propose_due(ports: &ExecutorPorts<'_>, round: &mut ExecutorRound) -> Result<(), DomainError> {
	for recorded in ports.operations.due_to_propose(BATCH).await? {
		let id = recorded.operation.id().raw();
		let proposal = proposal(&recorded)?;
		match ports.approvals.propose(&proposal).await {
			Ok(view) if view.payload_hash != recorded.payload_hash => {
				// The two planes disagree on what is being approved: nothing approved under
				// this proposal could ever be executed, so the client is told now rather than
				// after the owners spent a vote on it.
				error!(operation_id = %id, approval_id = %view.id, ours = %recorded.payload_hash, theirs = %view.payload_hash, "scope operations: the opened approval's payload_hash is not the stored operation's");
				if ports.operations.close(id, OperationEnd::Rejected, "approval_mismatch").await? {
					round.rejected += 1;
				}
			}
			Ok(view) => {
				ports.operations.opened(id, &view.id, view.level).await?;
				round.proposed += 1;
			}
			Err(ApprovalsError::Refused(why)) => {
				warn!(operation_id = %id, "scope operations: concierge refused to open the approval: {why}");
				if ports.operations.close(id, OperationEnd::Rejected, "proposal_refused").await? {
					round.rejected += 1;
				}
			}
			Err(ApprovalsError::Unavailable(why)) => {
				warn!(operation_id = %id, "scope operations: could not open the approval, will retry: {why}");
				ports.operations.postpone(id, backoff(recorded.attempts)).await?;
				// Concierge is down for all of them; the rest wait for the next round.
				break;
			}
		}
	}
	Ok(())
}

async fn execute_approved(ports: &ExecutorPorts<'_>, round: &mut ExecutorRound) -> Result<(), DomainError> {
	let approved = match ports.approvals.pull_approved(BATCH).await {
		Ok(approved) => approved,
		Err(err) => {
			warn!("scope operations: could not pull approved decisions, will retry: {err}");
			return Ok(());
		}
	};
	for view in approved {
		// Another `bank:*` kind is executed by its own flow.
		if OperationKind::parse(&view.kind).is_err() {
			continue;
		}
		let Some(id) = operation_ref(&view.subject_ref) else {
			error!(approval_id = %view.id, subject_ref = %view.subject_ref, "scope operations: a passed operation approval names no operation");
			continue;
		};
		let Some(recorded) = ports.operations.operation_by_id(id).await? else {
			error!(approval_id = %view.id, operation_id = %id, "scope operations: a passed approval names an operation banking does not have");
			report(ports, &view.id, ApprovalOutcome::Void, "no such operation").await;
			round.voided += 1;
			continue;
		};
		carry_out(ports, &view, recorded, round).await?;
	}
	Ok(())
}

/// The operation id in `pending_operation:<lowercase uuid>`.
fn operation_ref(subject_ref: &str) -> Option<Uuid> {
	let raw = subject_ref.strip_prefix("pending_operation:")?;
	Uuid::parse_str(raw).ok().filter(|id| id.to_string() == raw)
}

async fn report(ports: &ExecutorPorts<'_>, approval_id: &str, outcome: ApprovalOutcome, detail: &str) {
	// Unreported, the decision is served again on the next pull and reported then: the
	// operation's own state is what keeps it from executing twice.
	if let Err(err) = ports.approvals.report(approval_id, outcome, detail).await {
		warn!(%approval_id, ?outcome, "scope operations: could not report the outcome, the next pull retries: {err}");
	}
}

/// Why a passed decision is not the stored operation's, if it is not.
fn mismatch(recorded: &RecordedOperation, approval: &ApprovalView) -> Option<&'static str> {
	let operation = &recorded.operation;
	if approval.kind != operation.kind().as_str() {
		return Some("kind");
	}
	if approval.scope != operation.terms().scope.as_str() {
		return Some("scope");
	}
	if approval.subject_ref != operation.approval_subject_ref() {
		return Some("subject_ref");
	}
	if operation.lines_digest_input().map(|input| sha256_hex(&input)) != recorded.lines_digest {
		return Some("lines_digest");
	}
	// Recomputed from the stored record, so a row whose hash column and content disagree is
	// refused like a decision about something else.
	let payload = operation.approval_payload(recorded.lines_digest.as_deref()).ok().map(|payload| sha256_hex(&payload));
	if payload.as_deref() != Some(recorded.payload_hash.as_str()) || approval.payload_hash != recorded.payload_hash {
		return Some("payload_hash");
	}
	None
}

async fn carry_out(ports: &ExecutorPorts<'_>, pulled: &ApprovalView, recorded: RecordedOperation, round: &mut ExecutorRound) -> Result<(), DomainError> {
	let operation = &recorded.operation;
	let id = operation.id().raw();
	match operation.state() {
		OperationState::AwaitingApproval => {}
		// Reported before and not recorded by concierge (or this is the first report): say it
		// again, never act again.
		OperationState::Executed => {
			report(ports, &pulled.id, ApprovalOutcome::Executed, "").await;
			return Ok(());
		}
		OperationState::Failed => {
			report(ports, &pulled.id, ApprovalOutcome::Failed, failure(operation)).await;
			return Ok(());
		}
		OperationState::Rejected => {
			report(ports, &pulled.id, ApprovalOutcome::Void, failure(operation)).await;
			round.voided += 1;
			return Ok(());
		}
	}
	if operation.progress().approval_id.as_deref().is_some_and(|known| known != pulled.id) {
		error!(operation_id = %id, approval_id = %pulled.id, "scope operations: a second approval passed for one operation; only the first one opened may execute it");
		report(ports, &pulled.id, ApprovalOutcome::Void, "another approval decides this operation").await;
		round.voided += 1;
		return Ok(());
	}
	// A pulled decision can still go VOID before it is acted on (the owners changed): re-read
	// it by its id right before moving money, and act only on PASSED.
	let approval = match ports.approvals.get(&pulled.id).await {
		Ok(approval) => approval,
		Err(err) => {
			warn!(operation_id = %id, approval_id = %pulled.id, "scope operations: could not re-read the decision, will retry: {err}");
			return Ok(());
		}
	};
	if approval.state != ApprovalState::Passed || approval.id != pulled.id {
		info!(operation_id = %id, approval_id = %pulled.id, state = approval.state.as_str(), "scope operations: the decision is no longer passed; not executing");
		return Ok(());
	}
	if let Some(field) = mismatch(&recorded, &approval) {
		error!(operation_id = %id, approval_id = %approval.id, field, "scope operations: the passed decision does not match the stored operation; refusing to execute");
		if ports.operations.close(id, OperationEnd::Failed, "approval_mismatch").await? {
			round.failed += 1;
		}
		report(ports, &approval.id, ApprovalOutcome::Void, "the decision does not match the requested operation").await;
		return Ok(());
	}
	if operation.progress().approval_id.is_none() {
		ports.operations.opened(id, &approval.id, approval.level).await?;
	}
	let done = match ports.operations.execute(id, &Carry { ledger: ports.ledger }).await {
		Ok(done) => done,
		Err(TransferError::Domain(err)) => return Err(err),
		Err(TransferError::Internal(why)) => {
			error!(operation_id = %id, "scope operations: executing an approved operation hit banking's own invariant: {why}");
			return Ok(());
		}
		Err(TransferError::Refused(refusal)) => {
			// `Carry` answers refusals as a failed execution; one reaching here is a store bug.
			error!(operation_id = %id, reason = refusal.reason(), "scope operations: the store refused an execution outright");
			return Ok(());
		}
	};
	ports.relay.notify_one();
	match done.operation.state() {
		OperationState::Executed => {
			round.executed += 1;
			report(ports, &approval.id, ApprovalOutcome::Executed, "").await;
		}
		OperationState::Failed => {
			round.failed += 1;
			report(ports, &approval.id, ApprovalOutcome::Failed, failure(&done.operation)).await;
		}
		OperationState::Rejected => report(ports, &approval.id, ApprovalOutcome::Void, failure(&done.operation)).await,
		OperationState::AwaitingApproval => error!(operation_id = %id, "scope operations: an execution left the operation awaiting"),
	}
	Ok(())
}

fn failure(operation: &PendingOperation) -> &str {
	operation.progress().failure_reason.as_deref().unwrap_or("not executed")
}

async fn settle_decided(ports: &ExecutorPorts<'_>, round: &mut ExecutorRound) -> Result<(), DomainError> {
	for recorded in ports.operations.due_to_check(BATCH).await? {
		let id = recorded.operation.id().raw();
		let Some(approval_id) = recorded.operation.progress().approval_id.clone() else { continue };
		let approval = match ports.approvals.get(&approval_id).await {
			Ok(approval) => approval,
			Err(ApprovalsError::Unavailable(why)) => {
				warn!(operation_id = %id, %approval_id, "scope operations: could not ask where the approval stands, will retry: {why}");
				ports.operations.postpone(id, backoff(recorded.attempts)).await?;
				break;
			}
			Err(ApprovalsError::Refused(why)) => {
				error!(operation_id = %id, %approval_id, "scope operations: concierge refused to say where banking's own approval stands: {why}");
				ports.operations.postpone(id, ANOMALY_RETRY_SECS).await?;
				continue;
			}
		};
		match approval.state {
			ApprovalState::Open | ApprovalState::Passed => ports.operations.postpone(id, CHECK_EVERY_SECS).await?,
			ApprovalState::Rejected | ApprovalState::Vetoed | ApprovalState::Expired | ApprovalState::Cancelled | ApprovalState::Void => {
				if ports.operations.close(id, OperationEnd::Rejected, &format!("approval_{}", approval.state.as_str())).await? {
					round.rejected += 1;
				}
			}
			// Only banking reports these, and it records its own execution first; an outcome
			// on an operation still awaiting means the two planes diverged.
			ApprovalState::Executed | ApprovalState::ExecutionFailed => {
				error!(operation_id = %id, %approval_id, state = approval.state.as_str(), "scope operations: concierge holds an outcome for an operation banking did not execute");
				ports.operations.postpone(id, ANOMALY_RETRY_SECS).await?;
			}
		}
	}
	Ok(())
}

/// Turns a locked, still-awaiting operation into its payout or transfer — or into a failure,
/// recorded for good, when the world moved since it was asked.
struct Carry<'a> {
	ledger: &'a dyn Ledger,
}

#[async_trait]
impl OperationExecution for Carry<'_> {
	async fn execute(&self, operation: &PendingOperation, facts: &ExecutionFacts, reads: &mut dyn LockedReads) -> Result<Execution, TransferError> {
		let terms = operation.terms();
		let Some(scope) = facts.scope.as_ref() else {
			return Err(TransferError::Internal(format!(
				"operation {} names scope {}, which banking does not know",
				operation.id(),
				terms.scope
			)));
		};
		if scope.state == ScopeState::Frozen {
			return Ok(Execution::Fail(LedgerRefusal::ScopeFrozen.reason().to_owned()));
		}
		let destination_owned = match &terms.body {
			OperationBody::Payout { .. } => true,
			OperationBody::Transfer { .. } => facts.destination_owner.as_ref() == Some(&terms.client),
		};
		if facts.source_owner.as_ref() != Some(&terms.client) || !destination_owned {
			return Ok(Execution::Fail(LedgerRefusal::NotAccountOwner.reason().to_owned()));
		}
		let source = LedgerAccountKey::ScopeAccount(terms.scope.clone(), terms.from.clone());
		let spendable = self.ledger.spendable_locked(&source, reads).await.map_err(DomainError::from)?;
		if Usdt::from_base_units(spendable) < operation.total() {
			return Ok(Execution::Fail(LedgerRefusal::InsufficientFunds.reason().to_owned()));
		}
		let operation_id = Some(operation.id().raw());
		Ok(match &terms.body {
			OperationBody::Payout { lines, .. } => Execution::Payout(ScopePayout::request(
				ScopePayoutId::new(),
				PayoutTerms {
					client: terms.client.clone(),
					scope: terms.scope.clone(),
					from: terms.from.clone(),
					lines: lines.clone(),
					reason: PayoutReason::Approved,
					subject_ref: terms.subject_ref.clone(),
					memo: terms.memo.clone(),
					operation_id,
				},
			)?),
			OperationBody::Transfer { to_scope, to, amount } => Execution::Transfer(ScopeTransfer::request(
				ScopeTransferId::new(),
				TransferTerms {
					client: terms.client.clone(),
					scope: terms.scope.clone(),
					from: terms.from.clone(),
					to_scope: to_scope.clone(),
					to: to.clone(),
					amount: *amount,
					reason: TransferReason::Approved,
					subject_ref: terms.subject_ref.clone(),
					memo: terms.memo.clone(),
					operation_id,
				},
			)?),
		})
	}
}

/// The executor as a background loop: a round every [`Self::INTERVAL`], or at once when a new
/// operation is stored. Safe beside another replica's: an operation leaves `awaiting_approval`
/// once under its row lock, and opening an approval is idempotent by its `dedupe_key`.
pub struct ApprovalExecutor {
	pub operations: Arc<dyn OperationStore>,
	pub approvals: Arc<dyn Approvals>,
	pub ledger: Arc<dyn Ledger>,
	pub relay: Arc<Notify>,
	pub wake: Arc<Notify>,
	pub catalog: CatalogGate,
}

impl ApprovalExecutor {
	/// Concierge serves a passed decision for 71 hours; a round every 30 s is far inside that.
	pub const INTERVAL: Duration = Duration::from_secs(30);

	pub async fn run(self, shutdown: CancellationToken) {
		info!("scope operations: approval executor every {:?}", Self::INTERVAL);
		loop {
			let ports = ExecutorPorts {
				operations: self.operations.as_ref(),
				approvals: self.approvals.as_ref(),
				catalog: &self.catalog,
				ledger: self.ledger.as_ref(),
				relay: &self.relay,
			};
			match executor_round(&ports).await {
				Ok(round) if round != ExecutorRound::default() => info!(?round, "scope operations: executor round"),
				Ok(_) => {}
				Err(err) => warn!("scope operations: executor round failed, will retry: {err}"),
			}
			tokio::select! {
				() = shutdown.cancelled() => return,
				() = tokio::time::sleep(Self::INTERVAL) => {},
				() = self.wake.notified() => {},
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_backoff_doubles_to_a_ceiling() {
		assert_eq!(backoff(0), 30);
		assert_eq!(backoff(1), 60);
		assert_eq!(backoff(4), 480);
		assert_eq!(backoff(5), 900);
		assert_eq!(backoff(u32::MAX), 900);
	}

	#[test]
	fn an_operation_ref_is_banking_own_lowercase_form() {
		let id = Uuid::new_v4();
		assert_eq!(operation_ref(&format!("pending_operation:{id}")), Some(id));
		assert_eq!(operation_ref(&format!("pending_operation:{}", id.to_string().to_uppercase())), None);
		assert_eq!(operation_ref(&format!("payment:{id}")), None);
	}
}
