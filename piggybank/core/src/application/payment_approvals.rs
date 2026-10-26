//! Payments decided over concierge's `bank:payment` approvals: the proposal opened by the
//! request that writes the order ([`propose_on_open`]), and the executor that carries out the
//! ones that passed, ends the orders whose approval ended otherwise, tells concierge how each
//! came out, and takes back the proposals of orders that died first.
//!
//! The proposal is opened WITH THE PROOF of who asked (D24(1)): the initiator's own access
//! token and their `auth_subject`. With it concierge keeps the initiator off the owners'
//! roster, so nobody votes on their own payment. The proof is one token for one request and
//! is only in the hands of the request that carries it, so the executor never opens a
//! proposal itself: an order whose request died before it did is closed, not proposed later
//! without the proof.
//!
//! The order is stored whole and is what executes. A passed decision is acted on only after
//! it is re-read by its id and checked against the order — kind, scope, `payment:<id>`, the
//! payload hash the order was proposed under — and the money moves through the same
//! [`payments::execute`] every other approval reaches, which is idempotent: only an
//! `approved` order executes, an L1 withdrawal's id is a function of the order, and an
//! L2/L3 settlement waits for the relay's reservation. So a decision served twice (concierge
//! serves it until its outcome is reported), a second replica, or a crash between the approval
//! and the money each cost nothing.
//!
//! A step concierge does not answer is left for the next round; nothing here fails an order
//! because concierge is away.

use std::sync::Arc;

use domain::{
	error::DomainError,
	payments::{DECISION_MARGIN_SECS, DECISION_WINDOW_SECS, PaymentApproval, PaymentId, PaymentState, PaymentTerms, approval_subject_ref, parse_approval_subject_ref},
	scope_operations::{PAYMENT_APPROVAL_SCOPE, PAYMENT_KIND},
	users::UserId,
};
use tracing::{error, info, warn};

use crate::{
	application::{
		payments::{self, PaymentPorts},
		scope_operations::{CatalogGate, backoff, sha256_hex},
	},
	ports::{
		payments::{ConciergeSeatView, PaymentApprovalStore, PaymentView, SeatView},
		scope_operations::{ApprovalOutcome, ApprovalState, ApprovalView, Approvals, ApprovalsError, Proposal, UserProof},
	},
};

/// Orders handled per step of a round.
const BATCH: u32 = 50;

/// How long an order whose stored record this build cannot use, or for which concierge holds
/// something it should not, is put off. Each such round logs at error: it is a fault to be
/// read, not a state to wait out.
const ANOMALY_RETRY_SECS: u32 = 3600;

/// How long an order may stand without its proposal before the executor closes it. The
/// request that writes an order proposes in the same breath; a minute and a half of the
/// sweep's cadence past that is a request that died.
const UNPROPOSED_AFTER_SECS: u32 = 300;

/// The handles a request needs to open an approval, and the executor to watch it: concierge's
/// approvals seam, the executor's bookkeeping, and the catalog gate they share (one process
/// publishes the catalog once).
pub struct PaymentProposer {
	pub store: Arc<dyn PaymentApprovalStore>,
	pub approvals: Arc<dyn Approvals>,
	pub catalog: Arc<CatalogGate>,
}

/// What the executor works with. `payments` is the full set of handles an execution needs.
pub struct ApprovalRoundPorts<'a> {
	pub payments: &'a PaymentPorts<'a>,
	pub store: &'a dyn PaymentApprovalStore,
	pub approvals: &'a dyn Approvals,
	pub catalog: &'a CatalogGate,
}

/// One round's outcome, as counts — for the tests and the log.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ApprovalRound {
	pub proposed: usize,
	/// Orders the round saw reach `executed`.
	pub executed: usize,
	/// Orders the round saw reach `execution_failed`.
	pub failed: usize,
	/// Orders ended because their approval did not pass, or did not match.
	pub rejected: usize,
	/// Decisions reported VOID: the order had ended otherwise, or the decision was not the
	/// order's.
	pub voided: usize,
	pub reported: usize,
	pub withdrawn: usize,
}

/// One pass. Safe beside another replica's and from a test: every step is idempotent.
pub async fn round(ports: &ApprovalRoundPorts<'_>, now: i64) -> Result<ApprovalRound, DomainError> {
	let mut round = ApprovalRound::default();
	// Published here as well as by the request that proposes, so the topic the notices ride
	// is in concierge's catalog from the first round of a process, proposals or none. Pulling
	// and reporting need no catalog: a decision already made is served whatever it says now.
	ports.catalog.ensure(ports.approvals).await;
	close_unproposed(ports, &mut round, now).await?;
	execute_approved(ports, &mut round, now).await?;
	settle_decided(ports, &mut round, now).await?;
	report_ended(ports, &mut round).await?;
	withdraw_dead(ports, &mut round).await?;
	Ok(round)
}

/// Whether the round's step goes on to the next order.
enum Flow {
	Next,
	/// Concierge cannot be reached: the others would fail the same way.
	Stop,
}

async fn set_aside(ports: &ApprovalRoundPorts<'_>, id: PaymentId, step: &str, err: &DomainError) {
	error!(payment_id = %id, step, "payment approvals: an order failed its step, set aside for an hour: {err}");
	if let Err(err) = ports.store.postpone(id, ANOMALY_RETRY_SECS, &err.to_string()).await {
		error!(payment_id = %id, "payment approvals: could not set the order aside: {err}");
	}
}

fn concierge_seat(view: &PaymentView) -> Option<&ConciergeSeatView> {
	match &view.seat {
		SeatView::Concierge(seat) => Some(seat),
		SeatView::Session(_) => None,
	}
}

/// The payload banking proposes an order under, re-rendered from the stored order and the
/// destination as it was shown — and checked against the hash stored at open, so a row whose
/// content and hash disagree is never proposed and never executed.
fn proposed_payload(view: &PaymentView) -> Result<(Proposal, String), DomainError> {
	let seat = concierge_seat(view).ok_or_else(|| DomainError::Conflict(format!("payment {} is not decided by a concierge approval", view.order.id())))?;
	let order = &view.order;
	let fields = order.terms().approval_fields(&seat.destination)?;
	let hash = sha256_hex(&order.approval_payload(&seat.destination)?);
	if hash != seat.payload_hash {
		return Err(DomainError::Repository(format!("payment {} no longer renders to the payload it was stored under", order.id())));
	}
	let subject_ref = approval_subject_ref(order.id());
	Ok((
		Proposal {
			kind: PAYMENT_KIND.to_owned(),
			scope: PAYMENT_APPROVAL_SCOPE.to_owned(),
			dedupe_key: subject_ref.clone(),
			subject_ref,
			reason: format!("Banking payment order: {}", order.terms().reason()),
			fields,
		},
		hash,
	))
}

/// Open a payment as the RPC does: a fund-owned order needs the proof of who asked — their
/// token, minted for this request — and is proposed with it right after it is stored; every
/// other order opens as [`payments::open`] does and needs neither.
pub async fn open_with_proof(ports: &ApprovalRoundPorts<'_>, initiator: UserId, terms: PaymentTerms, proof_token: Option<&str>, now: i64) -> Result<PaymentView, DomainError> {
	if terms.requirement() != PaymentApproval::OwnerConsilium {
		return payments::open(ports.payments, initiator, terms, now).await;
	}
	// Before the order is written: nothing to undo when the proof is missing.
	let Some(token) = proof_token.filter(|token| !token.is_empty()) else {
		return Err(DomainError::Precondition(
			"a payment out of fund money is proposed to the owners in the name of the person who opens it: sign in again and retry".into(),
		));
	};
	let user = ports.payments.users.find_by_id(initiator).await?.ok_or_else(|| DomainError::NotFound {
		entity: "user",
		id: initiator.to_string(),
	})?;
	let proof = UserProof {
		requested_by: user.auth_subject().as_str().to_owned(),
		token: token.to_owned(),
	};
	let view = payments::open(ports.payments, initiator, terms, now).await?;
	propose_on_open(ports, &view, &proof, now).await
}

/// Open the approval of a fund-owned order that was just written, with the proof of who
/// asked. Called by the request that wrote it, after the order is stored.
///
/// Anything short of an opened, matching proposal closes the order and answers why: the
/// initiator opens it again with a fresh proof, and the source is free at once. A proposal
/// that is not opened here is never opened later, because the proof is not kept.
pub async fn propose_on_open(ports: &ApprovalRoundPorts<'_>, view: &PaymentView, proof: &UserProof, now: i64) -> Result<PaymentView, DomainError> {
	let id = view.order.id();
	let refuse = |why: String| async move {
		ports.payments.payments.record_rejection(id, now).await?;
		Err::<PaymentView, _>(DomainError::Precondition(format!("the payment was not opened: {why}; open it again")))
	};
	// An order that could pass and then find itself expired is closed before the owners spend
	// a vote on it.
	if view.order.expires_at() - now < DECISION_WINDOW_SECS + DECISION_MARGIN_SECS {
		return refuse("too late to be decided: its window is shorter than a decision takes".into()).await;
	}
	let (proposal, hash) = match proposed_payload(view) {
		Ok(rendered) => rendered,
		Err(err) => {
			error!(payment_id = %id, "payment approvals: the order cannot be rendered into an approval: {err}");
			return refuse("its terms could not be put in front of the owners".into()).await;
		}
	};
	if !ports.catalog.ensure(ports.approvals).await {
		return refuse("concierge does not hold banking's approval catalog yet".into()).await;
	}
	match ports.approvals.propose_for(&proposal, Some(proof)).await {
		Ok(opened) if opened.payload_hash != hash => {
			// The two planes disagree on what is being approved: nothing approved under this
			// proposal could ever be executed.
			error!(payment_id = %id, approval_id = %opened.id, ours = %hash, theirs = %opened.payload_hash, "payment approvals: the opened approval's payload_hash is not the order's");
			if let Err(err) = ports.approvals.withdraw(&opened.id, "the opened approval does not match the payment order").await {
				warn!(payment_id = %id, approval_id = %opened.id, "payment approvals: could not withdraw the mismatched proposal: {err}");
			}
			refuse("concierge and banking disagree about what is being approved".into()).await
		}
		Ok(opened) => {
			ports.store.opened(id, &opened.id, opened.level).await?;
			payments::find(ports.payments.payments, id).await
		}
		Err(ApprovalsError::Refused(why)) => {
			error!(payment_id = %id, "payment approvals: concierge refused to open the approval: {why}");
			refuse(format!("concierge refused it ({why})")).await
		}
		Err(ApprovalsError::Throttled(why) | ApprovalsError::Unavailable(why)) => {
			warn!(payment_id = %id, "payment approvals: could not open the approval: {why}");
			refuse(format!("concierge could not take it now ({why})")).await
		}
	}
}

/// Close the orders whose request died before it proposed them (see the module docs).
async fn close_unproposed(ports: &ApprovalRoundPorts<'_>, round: &mut ApprovalRound, now: i64) -> Result<(), DomainError> {
	for due in ports.store.unproposed(UNPROPOSED_AFTER_SECS, BATCH).await? {
		warn!(payment_id = %due.id, "payment approvals: an order was never proposed; closing it");
		ports.payments.payments.record_rejection(due.id, now).await?;
		round.rejected += 1;
	}
	Ok(())
}

async fn execute_approved(ports: &ApprovalRoundPorts<'_>, round: &mut ApprovalRound, now: i64) -> Result<(), DomainError> {
	let approved = match ports.approvals.pull_approved(BATCH).await {
		Ok(approved) => approved,
		Err(err) => {
			warn!("payment approvals: could not pull approved decisions, will retry: {err}");
			return Ok(());
		}
	};
	for pulled in approved {
		// Another `bank:*` kind is executed by its own flow.
		if pulled.kind != PAYMENT_KIND {
			continue;
		}
		if let Err(err) = carry_out(ports, &pulled, round, now).await {
			error!(approval_id = %pulled.id, subject_ref = %pulled.subject_ref, "payment approvals: executing a passed payment failed, the next pull retries: {err}");
		}
	}
	Ok(())
}

/// What came of telling concierge an outcome.
enum Told {
	/// Concierge recorded it.
	Yes,
	/// Concierge will not take it (the decision lapsed, or is closed): nothing is left to
	/// report, but the order did NOT get its outcome recorded there, and that is kept.
	Refused(String),
	/// Concierge could not be asked; the next round does.
	Later,
}

async fn report(ports: &ApprovalRoundPorts<'_>, approval_id: &str, outcome: ApprovalOutcome, detail: &str) -> Told {
	// Unreported, the decision is served again on the next pull and reported then: the
	// order's own state is what keeps it from executing twice.
	match ports.approvals.report(approval_id, outcome, detail).await {
		Ok(()) => Told::Yes,
		Err(ApprovalsError::Refused(why)) => {
			error!(%approval_id, ?outcome, "payment approvals: concierge refused the outcome, so it holds none for this decision: {why}");
			Told::Refused(why)
		}
		Err(err @ (ApprovalsError::Unavailable(_) | ApprovalsError::Throttled(_))) => {
			warn!(%approval_id, ?outcome, "payment approvals: could not report the outcome, the next round retries: {err}");
			Told::Later
		}
	}
}

/// A decision that is not carried out, told to concierge as VOID so it stops being served.
async fn void(ports: &ApprovalRoundPorts<'_>, pulled: &ApprovalView, why: &str) -> Result<(), DomainError> {
	// A void that concierge refuses has nothing to keep: the order it names is not executed.
	let _ = report(ports, &pulled.id, ApprovalOutcome::Void, why).await;
	Ok(())
}

/// Why a passed decision is not the order's, if it is not.
fn mismatch(view: &PaymentView, approval: &ApprovalView) -> Option<&'static str> {
	let order = &view.order;
	let seat = concierge_seat(view)?;
	if approval.kind != PAYMENT_KIND {
		return Some("kind");
	}
	if approval.scope != PAYMENT_APPROVAL_SCOPE {
		return Some("scope");
	}
	if approval.subject_ref != approval_subject_ref(order.id()) {
		return Some("subject_ref");
	}
	// Recomputed from the stored order, so a row whose hash column and content disagree is
	// refused like a decision about something else.
	let recomputed = order.approval_payload(&seat.destination).ok().map(|payload| sha256_hex(&payload));
	if recomputed.as_deref() != Some(seat.payload_hash.as_str()) || approval.payload_hash != seat.payload_hash {
		return Some("payload_hash");
	}
	None
}

async fn carry_out(ports: &ApprovalRoundPorts<'_>, pulled: &ApprovalView, round: &mut ApprovalRound, now: i64) -> Result<(), DomainError> {
	let Some(id) = parse_approval_subject_ref(&pulled.subject_ref) else {
		error!(approval_id = %pulled.id, subject_ref = %pulled.subject_ref, "payment approvals: a passed payment approval names no payment");
		void(ports, pulled, "no such payment").await?;
		round.voided += 1;
		return Ok(());
	};
	let view = match payments::find(ports.payments.payments, id).await {
		Ok(view) => view,
		Err(DomainError::NotFound { .. }) => {
			error!(approval_id = %pulled.id, payment_id = %id, "payment approvals: a passed approval names a payment banking does not have");
			void(ports, pulled, "no such payment").await?;
			round.voided += 1;
			return Ok(());
		}
		// Concierge serves it again on the next pull; the other decisions go on.
		Err(err) => return Err(err),
	};
	let Some(seat) = concierge_seat(&view) else {
		error!(approval_id = %pulled.id, payment_id = %id, "payment approvals: a passed approval names a payment that no approval decides");
		void(ports, pulled, "this payment is not decided by an approval").await?;
		round.voided += 1;
		return Ok(());
	};
	// Before anything else, whatever state the order is in: a decision that is not the one
	// recorded for it is never reported as its outcome.
	if seat.approval_id.as_deref().is_some_and(|known| known != pulled.id) {
		error!(payment_id = %id, approval_id = %pulled.id, "payment approvals: a second approval passed for one payment; only the first one opened may execute it");
		void(ports, pulled, "another approval decides this payment").await?;
		round.voided += 1;
		return Ok(());
	}
	match view.order.state() {
		PaymentState::Pending | PaymentState::Approved => {}
		// Done before and not recorded by concierge: say it again, never act again.
		PaymentState::Executed | PaymentState::ExecutionFailed => return report_final(ports, &view, &pulled.id, round).await.map(|_| ()),
		PaymentState::Rejected | PaymentState::Expired | PaymentState::Cancelled => {
			info!(payment_id = %id, state = view.order.state().as_str(), "payment approvals: a decision about a payment that already ended; reporting it void");
			void(ports, pulled, "the payment ended before the decision was carried out").await?;
			round.voided += 1;
			return Ok(());
		}
	}
	// A pulled decision can still go VOID before it is acted on (the owners changed): re-read
	// it by its id right before moving money, and act only on PASSED.
	let approval = match ports.approvals.get(&pulled.id).await {
		Ok(approval) => approval,
		Err(err) => {
			warn!(payment_id = %id, approval_id = %pulled.id, "payment approvals: could not re-read the decision, will retry: {err}");
			return Ok(());
		}
	};
	if approval.state != ApprovalState::Passed || approval.id != pulled.id {
		info!(payment_id = %id, approval_id = %pulled.id, state = approval.state.as_str(), "payment approvals: the decision is no longer passed; not executing");
		return Ok(());
	}
	if let Some(field) = mismatch(&view, &approval) {
		error!(payment_id = %id, approval_id = %approval.id, field, "payment approvals: the passed decision does not match the stored order; refusing to execute");
		if view.order.state() == PaymentState::Pending {
			ports.payments.payments.record_rejection(id, now).await?;
			round.rejected += 1;
		}
		void(ports, pulled, "the decision does not match the payment order").await?;
		return Ok(());
	}
	// Approved once and reserved once, whatever number of times the decision is served.
	match ports.payments.payments.record_concierge_approval(id, &approval.id, approval.execute_by, now).await {
		Ok(_) => {}
		Err(DomainError::Conflict(why)) => {
			error!(payment_id = %id, approval_id = %approval.id, "payment approvals: the decision could not be recorded: {why}");
			void(ports, pulled, "the payment can no longer be approved").await?;
			round.voided += 1;
			return Ok(());
		}
		Err(err) => return Err(err),
	};
	ports.payments.relay.notify_one();
	// An `Err` here is a hold (the operator's pause, a freeze) or an infrastructure failure:
	// the order stays `approved` and the sweeper, or the next pull, comes back to it.
	let done = match payments::execute(ports.payments, id, now).await {
		Ok(done) => done,
		Err(err) => {
			warn!(payment_id = %id, "payment approvals: approved but not executed yet: {err}");
			return Ok(());
		}
	};
	ports.payments.relay.notify_one();
	report_final(ports, &done, &approval.id, round).await.map(|_| ())
}

/// Tell concierge how a finished order came out, once. An order still `approved` (waiting for
/// the relay's reservation, or held) has no outcome yet.
async fn report_final(ports: &ApprovalRoundPorts<'_>, view: &PaymentView, approval_id: &str, round: &mut ApprovalRound) -> Result<bool, DomainError> {
	let id = view.order.id();
	let (outcome, detail) = match view.order.state() {
		PaymentState::Executed => (ApprovalOutcome::Executed, String::new()),
		PaymentState::ExecutionFailed => (ApprovalOutcome::Failed, view.order.failure_reason().unwrap_or("not executed").to_owned()),
		PaymentState::Pending | PaymentState::Approved | PaymentState::Rejected | PaymentState::Expired | PaymentState::Cancelled => return Ok(false),
	};
	let told = match report(ports, approval_id, outcome, &detail).await {
		Told::Yes => {
			ports.store.reported(id, None).await?;
			true
		}
		Told::Refused(why) => {
			ports.store.reported(id, Some(&why)).await?;
			false
		}
		Told::Later => return Ok(false),
	};
	match outcome {
		ApprovalOutcome::Executed => round.executed += 1,
		ApprovalOutcome::Failed => round.failed += 1,
		ApprovalOutcome::Void => {}
	}
	Ok(told)
}

/// Orders that finished by another road (the sweeper executed an approved one, a retry
/// landed after the pull): concierge is told here, once.
async fn report_ended(ports: &ApprovalRoundPorts<'_>, round: &mut ApprovalRound) -> Result<(), DomainError> {
	for id in ports.store.due_to_report(BATCH).await? {
		let view = payments::find(ports.payments.payments, id).await?;
		let Some(approval_id) = concierge_seat(&view).and_then(|seat| seat.approval_id.clone()) else {
			continue;
		};
		if report_final(ports, &view, &approval_id, round).await? {
			round.reported += 1;
		}
	}
	Ok(())
}

async fn settle_decided(ports: &ApprovalRoundPorts<'_>, round: &mut ApprovalRound, now: i64) -> Result<(), DomainError> {
	for due in ports.store.due_to_check(BATCH).await? {
		match settle_one(ports, due.id, due.attempts, round, now).await {
			Ok(Flow::Next) => {}
			Ok(Flow::Stop) => break,
			Err(err) => set_aside(ports, due.id, "check", &err).await,
		}
	}
	Ok(())
}

async fn settle_one(ports: &ApprovalRoundPorts<'_>, id: PaymentId, attempts: u32, round: &mut ApprovalRound, now: i64) -> Result<Flow, DomainError> {
	let view = payments::find(ports.payments.payments, id).await?;
	let Some(approval_id) = concierge_seat(&view).and_then(|seat| seat.approval_id.clone()) else {
		return Ok(Flow::Next);
	};
	let approval = match ports.approvals.get(&approval_id).await {
		Ok(approval) => approval,
		Err(ApprovalsError::Unavailable(why) | ApprovalsError::Throttled(why)) => {
			warn!(payment_id = %id, %approval_id, "payment approvals: could not ask where the approval stands, will retry: {why}");
			ports.store.postpone(id, backoff(attempts), &why).await?;
			return Ok(Flow::Stop);
		}
		Err(ApprovalsError::Refused(why)) => {
			error!(payment_id = %id, %approval_id, "payment approvals: concierge refused to say where banking's own approval stands: {why}");
			ports.store.postpone(id, ANOMALY_RETRY_SECS, &why).await?;
			return Ok(Flow::Next);
		}
	};
	match approval.state {
		// Passed waits out its veto delay; the pull serves it when it is due.
		ApprovalState::Open | ApprovalState::Passed => {}
		ApprovalState::Rejected | ApprovalState::Vetoed | ApprovalState::Expired | ApprovalState::Cancelled | ApprovalState::Void => {
			info!(payment_id = %id, %approval_id, state = approval.state.as_str(), "payment approvals: the approval ended without passing; closing the order");
			ports.payments.payments.record_rejection(id, now).await?;
			round.rejected += 1;
		}
		// Only banking reports these, and it records its own execution first; an outcome on an
		// order still pending means the two planes diverged.
		ApprovalState::Executed | ApprovalState::ExecutionFailed => {
			error!(payment_id = %id, %approval_id, state = approval.state.as_str(), "payment approvals: concierge holds an outcome for a payment banking did not execute");
			ports.store.postpone(id, ANOMALY_RETRY_SECS, "concierge holds an outcome banking did not report").await?;
		}
	}
	Ok(Flow::Next)
}

async fn withdraw_dead(ports: &ApprovalRoundPorts<'_>, round: &mut ApprovalRound) -> Result<(), DomainError> {
	for (id, approval_id) in ports.store.due_to_withdraw(BATCH).await? {
		match ports.approvals.withdraw(&approval_id, "the payment order was withdrawn or expired").await {
			// Refused: already closed on concierge's side (decided, or past the window where a
			// proposal can be withdrawn) — nothing is left to take back.
			Ok(()) | Err(ApprovalsError::Refused(_)) => {
				ports.store.withdrawn(id).await?;
				round.withdrawn += 1;
			}
			Err(err) => {
				warn!(payment_id = %id, %approval_id, "payment approvals: could not withdraw the proposal of a dead payment, will retry: {err}");
				break;
			}
		}
	}
	Ok(())
}
