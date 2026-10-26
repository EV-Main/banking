//! Payment use cases — open, read and withdraw an order (operators); the investor's answer in
//! the wallet host; and the execution that turns an approved order into a withdrawal or one
//! settled transfer.
//!
//! The order is the INTENT plus its one approval plus its outcome; the money moves either
//! through the ordinary withdrawal saga (L1) or through the relay from the two events the
//! order drains (L3). Which approval an order needs is read off its source —
//! [`PaymentTerms::requirement`] — and this module is where each requirement is actually
//! seated: a concierge `bank:payment` approval for banking's treasury (proposed by
//! [`payment_approvals`](super::payment_approvals)), a session seat for the investor whose
//! claim is spent.
//!
//! Execution is driven from three places — the investor's answer, the approval round, and
//! the sweeper — and is safe to reach more than once from each: only an `approved` order executes, the
//! payload hash and the consent pins are re-read first, an L1 withdrawal's id is a pure
//! function of the order so a retry re-creates the same row, and an L3 settlement is
//! recorded only once the relay has actually applied the reservation it posts.

use domain::{
	balance::Party,
	error::DomainError,
	money::{Network, Usdt},
	payments::{CONCIERGE_TTL_SECS, PaymentApproval, PaymentDestination, PaymentEffect, PaymentId, PaymentOrder, PaymentState, PaymentTerms, PaymentTier},
	users::UserId,
	withdrawals::WithdrawalId,
};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{
	application::withdrawals::{self as withdrawal_app, AdmissionGates, WithdrawalPorts, require_outflows_enabled},
	config::KycGate,
	infrastructure::{payments::digest, relay::payment_reserve_id},
	ports::{
		Custody, OutflowPolicy, UserRepository, WithdrawalRepository,
		ledger::Ledger,
		payments::{
			ApprovalSeat, ConsentAudit, ConsentDecision, ConsentOutcome, ExecutionOutcome, PaymentFeed, PaymentFilter, PaymentRepository, PaymentView, ReservationStatus, SeatView,
			SessionSeat, already_open, consent_not_found,
		},
	},
};

/// The salt that makes an L1 order's withdrawal id a pure function of the order.
const WITHDRAWAL_SALT: &[u8] = b"payment:withdrawal";

/// The driven ports the payment write-path borrows: executing an investor's L1 order admits a
/// withdrawal, which needs everything its own use case needs.
pub struct PaymentPorts<'a> {
	pub payments: &'a dyn PaymentRepository,
	pub users: &'a dyn UserRepository,
	pub withdrawals: &'a dyn WithdrawalRepository,
	pub ledger: &'a dyn Ledger,
	pub custody: &'a dyn Custody,
	/// The read-only kill-switch, re-read at execution: an approved order is still money
	/// leaving, and an operator pause must hold it exactly as it holds a queued withdrawal.
	pub policy: &'a dyn OutflowPolicy,
	pub relay: &'a Notify,
	pub configured: &'a [Network],
	pub kyc: KycGate,
}

impl PaymentPorts<'_> {
	fn withdrawal_ports(&self) -> WithdrawalPorts<'_> {
		WithdrawalPorts {
			withdrawals: self.withdrawals,
			ledger: self.ledger,
			custody: self.custody,
			relay: self.relay,
		}
	}

	fn admission_gates(&self) -> AdmissionGates<'_> {
		AdmissionGates {
			policy: self.policy,
			configured: self.configured,
			kyc: self.kyc,
		}
	}
}

/// The withdrawal an L1 order creates: `uuid_v5(payment_id, "payment:withdrawal")`.
///
/// Deterministic on purpose: a retried execution recomputes the same id and finds the
/// withdrawal already there instead of opening a second one.
pub fn withdrawal_id(payment: PaymentId) -> WithdrawalId {
	WithdrawalId::from_raw(Uuid::new_v5(&payment.raw(), WITHDRAWAL_SALT))
}

/// One sweep's outcome, as counts.
#[derive(Clone, Copy, Debug, Default)]
pub struct SweepReport {
	pub expired: usize,
	pub executed: usize,
	pub execution_failures: usize,
	/// Approved orders whose reservation the relay has not applied yet — left for the next
	/// sweep, nothing recorded.
	pub deferred: usize,
}

/// Open a payment order and seat its one approval.
///
/// Refused at OPEN for everything that could never execute, so an approver is never asked
/// to spend 72 hours on an order that would fail the moment it was carried: the source
/// cannot cover the amount right now, the external shape is impossible (an unconfigured
/// rail, a sub-minimum, an unverified investor), or the source is one the withdrawal saga
/// cannot pay out from at all. Solvency is a pre-check, not a guarantee — the claim can
/// still fall between here and execution, which is what `execution_failed` exists for.
pub async fn open(ports: &PaymentPorts<'_>, initiator: UserId, terms: PaymentTerms, now: i64) -> Result<PaymentView, DomainError> {
	if spending_closed(terms.from()) {
		return Err(DomainError::Precondition(TREASURY_SPENDING_CLOSED.into()));
	}
	if terms.requirement() == PaymentApproval::OwnerConsilium {
		// The owners decide over the sentence the order carries; one that would not fit the
		// approval is refused here, before an approval nobody could read is proposed.
		terms.approval_fields(&terms.destination_label())?;
	}
	check_executable(ports, &terms).await?;
	let payload_hash = digest(&terms.canonical_bytes());
	match terms.requirement() {
		PaymentApproval::SubjectConsent(subject) => {
			let seat = session_seat(ports, subject).await?;
			let mut order = PaymentOrder::open(PaymentId::new(), terms, payload_hash, initiator, now);
			ports.payments.open(&mut order, ApprovalSeat::Session(seat)).await?;
			find(ports.payments, order.id()).await
		}
		PaymentApproval::OwnerConsilium => {
			if ports.payments.has_open_against(terms.from()).await? {
				return Err(already_open());
			}
			let mut order = PaymentOrder::open_for(PaymentId::new(), terms, payload_hash, initiator, now, CONCIERGE_TTL_SECS);
			ports.payments.open(&mut order, ApprovalSeat::Concierge).await?;
			find(ports.payments, order.id()).await
		}
	}
}

/// Why an order out of banking's treasury is refused, as the caller reads it.
pub const TREASURY_SPENDING_CLOSED: &str = "treasury spending waits for scope approvals";

/// The treasury pays nothing out until its spending is decided by the scope approvals of
/// concierge (`bank:treasury_surplus`, D24). Money still flows IN — the withdrawal fees it collects.
fn spending_closed(from: &Party) -> bool {
	match from {
		Party::Treasury => true,
		Party::User(_) => false,
	}
}

/// Everything the order's eventual execution will check, checked now.
async fn check_executable(ports: &PaymentPorts<'_>, terms: &PaymentTerms) -> Result<(), DomainError> {
	match (terms.to(), terms.from()) {
		(PaymentDestination::External { network, address }, Party::User(user)) =>
			withdrawal_app::check_user_withdrawal(ports.ledger, &ports.admission_gates(), *user, *network, address.clone(), terms.amount()).await,
		// banking's treasury (D17) has a withdrawal source of its own. `open` refuses the
		// treasury before this runs (D24); the arm stays for when its spending reopens under
		// concierge's scope approvals.
		(PaymentDestination::External { network, address }, Party::Treasury) =>
			withdrawal_app::check_treasury_withdrawal(ports.ledger, &ports.admission_gates(), *network, address.clone(), terms.amount()).await,
		// Read-First on the source's claim: the spendable balance (posted minus what other
		// in-flight spends have already reserved) must cover the amount. TigerBeetle's
		// non-negative flag is the backstop at settlement.
		(PaymentDestination::Internal(_), from) => {
			// An investor's claim leaves on their say-so whichever tier it lands on, and the
			// verification floor applies to the say-so, not to the rail: without this, an
			// unverified account could pay a verified one, who then withdraws.
			if let Party::User(user) = from {
				withdrawal_app::admit_user_account(&ports.admission_gates(), *user).await?;
			}
			// Net of every debit of the source recorded and not yet applied (`Ledger::spendable`).
			if Usdt::from_base_units(ports.ledger.spendable(&terms.source_claim()).await?) < terms.amount() {
				return Err(DomainError::Validation("the source claim's available balance does not cover the payment".into()));
			}
			Ok(())
		}
	}
}

/// The investor's seat in the wallet host, pinned to their standing as the projection holds it now.
///
/// The subject must be active: a disabled account is not a principal that may authorize its
/// money leaving. The pins are the folded revoke floor of both planes — `GREATEST(
/// concierge_token_version, token_version)`, which only `IssuanceTarget` folds — and the
/// digest of the mailbox, so a revoke on either plane voids the seat. There is no
/// mailbox-verified check: the person answering is signed in, which is the proof the mailbox
/// was.
async fn session_seat(ports: &PaymentPorts<'_>, subject: UserId) -> Result<SessionSeat, DomainError> {
	let user = ports.users.find_by_id(subject).await?.ok_or_else(|| DomainError::NotFound {
		entity: "user",
		id: subject.to_string(),
	})?;
	if !user.is_active() {
		return Err(DomainError::Precondition(
			"the investor's account is not active, so their confirmation cannot be asked for".into(),
		));
	}
	let issuance = ports.users.resolve_issuance_by_banking_id(subject).await?.ok_or_else(|| DomainError::NotFound {
		entity: "user",
		id: subject.to_string(),
	})?;
	Ok(SessionSeat {
		subject,
		token_version_at_open: issuance.token_version,
		email_hash_at_open: digest(issuance.email.as_bytes()),
	})
}

pub async fn find(payments: &dyn PaymentRepository, id: PaymentId) -> Result<PaymentView, DomainError> {
	payments.find(id).await?.ok_or_else(|| DomainError::NotFound {
		entity: "payment",
		id: id.to_string(),
	})
}

pub async fn list(feed: &dyn PaymentFeed, filter: &PaymentFilter, limit: i64) -> Result<Vec<PaymentView>, DomainError> {
	feed.list(filter, limit).await
}

/// Withdraw an order the caller opened. A live concierge proposal is taken back by the
/// approval round.
pub async fn cancel(payments: &dyn PaymentRepository, id: PaymentId, by: UserId, now: i64) -> Result<PaymentView, DomainError> {
	payments.cancel(id, by, now).await
}

/// The orders waiting for the investor's own answer in the wallet host.
pub async fn awaiting_confirmation(payments: &dyn PaymentRepository, subject: UserId) -> Result<Vec<PaymentView>, DomainError> {
	payments.awaiting_session_consent(subject).await
}

/// One order as the investor it spends may see it. Anyone else's, and one that is not
/// answered in the wallet host, are the same NOT_FOUND as one that does not exist.
pub async fn find_for_subject(payments: &dyn PaymentRepository, id: PaymentId, subject: UserId) -> Result<PaymentView, DomainError> {
	payments.find_for_session_subject(id, subject).await?.ok_or_else(consent_not_found)
}

/// The investor's own answer, given signed in. An approval is executed inline; a failure there
/// is the order's to record, and must not undo the answer that was validly given.
pub async fn answer_consent(ports: &PaymentPorts<'_>, subject: UserId, id: PaymentId, decision: ConsentDecision, audit: &ConsentAudit, now: i64) -> Result<ConsentOutcome, DomainError> {
	let outcome = ports.payments.answer_session_consent(id, subject, decision, audit, now).await?;
	if outcome.approved
		&& let Err(err) = execute(ports, outcome.payment.order.id(), now).await
	{
		tracing::error!(payment_id = %outcome.payment.order.id(), "payments: confirmed but the order could not be executed: {err}");
	}
	Ok(outcome)
}

/// Turn an approved order into its effect.
///
/// Safe to call more than once, from more than one place:
///
/// 1. only an `approved` order is executable; `executed` returns as it is and every other
///    state is refused, so a late answer can never reach the money;
/// 2. the read-only pause is re-read and, while it holds, the attempt is an `Err` and NOT
///    a recorded failure: the order stays `approved` for the sweeper to pick up once the
///    operator lifts the pause, rather than being closed for good by a control the
///    operator meant as a hold — and a freeze on an L1 order's subject, read at the
///    withdrawal's admission, holds the order the same way (see [`create_withdrawal`]);
/// 3. the payload hash is re-taken over the stored terms, so what executes is what was
///    approved;
/// 4. a session seat whose pins have moved fails the order closed BEFORE anything is
///    created — and `record_execution` re-checks under the lock, voiding a still-queued
///    withdrawal if a revocation landed in between;
/// 5. an L3 settlement is recorded only once the relay has applied the reservation the
///    approval raised; until then nothing is recorded and the sweeper comes back, and a
///    reservation the ledger refused (parked) fails the order rather than waiting forever;
/// 6. an L1 withdrawal's id is derived from the order, and a refusal from the withdrawal
///    path is re-read against that id before being believed, so the loser of a two-caller
///    race records the withdrawal that exists rather than a phantom failure.
pub async fn execute(ports: &PaymentPorts<'_>, id: PaymentId, now: i64) -> Result<PaymentView, DomainError> {
	let view = find(ports.payments, id).await?;
	let order = &view.order;
	if order.state() == PaymentState::Executed {
		return Ok(view);
	}
	if order.state() != PaymentState::Approved {
		return Err(DomainError::Conflict(format!("payment is {}, not executable", order.state().as_str())));
	}
	require_outflows_enabled(ports.policy).await?;
	if digest(&order.terms().canonical_bytes()) != order.payload_hash() {
		let reason = "the stored terms no longer match the payload hash that was approved".to_owned();
		return ports.payments.record_execution(id, ExecutionOutcome::Failed(reason), now).await;
	}
	// A decision concierge has voided unexecuted is not authority any more, however long this
	// plane held it (the pause, a held relay): the order fails closed with the reason.
	if let SeatView::Concierge(seat) = &view.seat
		&& let Some(by) = seat.execute_by
		&& now > by
	{
		let reason = "the approval's execution window lapsed before the payment was carried out".to_owned();
		return ports.payments.record_execution(id, ExecutionOutcome::Failed(reason), now).await;
	}
	// An order approved before the treasury closed must not pay out either; the failure is
	// terminal, like any other.
	if spending_closed(order.terms().from()) {
		return ports.payments.record_execution(id, ExecutionOutcome::Failed(TREASURY_SPENDING_CLOSED.into()), now).await;
	}
	if let Some(cause) = view.consent_invalidated() {
		let withdrawal = (order.tier() == PaymentTier::External).then(|| withdrawal_id(id));
		return ports.payments.record_execution(id, ExecutionOutcome::ConsentVoid { cause, withdrawal }, now).await;
	}
	let outcome = match order.tier() {
		PaymentTier::Internal => match settle_on_the_ledger(ports, id).await? {
			Some(outcome) => outcome,
			None => return Ok(view),
		},
		PaymentTier::External => create_withdrawal(ports, order).await?,
	};
	ports.payments.record_execution(id, outcome, now).await
}

/// The L3 effect: the settlement is recorded once — and only once — the reservation the
/// approval raised has actually landed. `None` means "not yet": nothing is recorded and the
/// next sweep asks again. The ledger is asked as well as the relay's own record, so a
/// `saga_steps` row can never stand in for a transfer TigerBeetle does not hold.
async fn settle_on_the_ledger(ports: &PaymentPorts<'_>, id: PaymentId) -> Result<Option<ExecutionOutcome>, DomainError> {
	let reserve = payment_reserve_id(id.raw());
	Ok(match ports.payments.reservation_status(id, reserve).await? {
		ReservationStatus::Applied if ports.ledger.transfer_exists(reserve).await? => Some(ExecutionOutcome::Executed(PaymentEffect::Transfer)),
		ReservationStatus::Applied | ReservationStatus::Pending => None,
		ReservationStatus::Parked => Some(ExecutionOutcome::Failed(
			"the reservation against the source claim was refused by the ledger and never applied".to_owned(),
		)),
	})
}

/// The L1 effect: the withdrawal the order pays out through, under the order's derived id.
///
/// ALWAYS QUEUED, never dispatched on creation. The consent pins are re-read under the
/// order's lock only after this withdrawal exists, and the refusal there can void a
/// withdrawal only while it is still `Queued`. Leaving it for the dispatcher is what keeps
/// that void possible — and it puts the withdrawal through `require_dispatchable`, so the
/// pause, the freeze and the verification floor are read at the moment the money leaves.
async fn create_withdrawal(ports: &PaymentPorts<'_>, order: &PaymentOrder) -> Result<ExecutionOutcome, DomainError> {
	let PaymentDestination::External { network, address } = order.terms().to() else {
		return Ok(ExecutionOutcome::Failed("an internal payment has no withdrawal to create".to_owned()));
	};
	let withdrawal = withdrawal_id(order.id());
	if ports.withdrawals.find_by_id(withdrawal).await?.is_some() {
		return Ok(ExecutionOutcome::Executed(PaymentEffect::Withdrawal(withdrawal)));
	}
	let requested = match order.terms().from() {
		Party::User(user) =>
			withdrawal_app::queue_withdrawal(
				&ports.withdrawal_ports(),
				&ports.admission_gates(),
				withdrawal,
				*user,
				*network,
				address.clone(),
				order.terms().amount(),
			)
			.await,
		// banking's treasury (D17): the owners approved the order, so the withdrawal opens with
		// no account gate of its own — only the rail's.
		Party::Treasury =>
			withdrawal_app::queue_treasury_withdrawal(
				&ports.withdrawal_ports(),
				&ports.admission_gates(),
				withdrawal,
				order.id(),
				*network,
				address.clone(),
				order.terms().amount(),
			)
			.await,
	};
	Ok(match requested {
		Ok(created) => ExecutionOutcome::Executed(PaymentEffect::Withdrawal(created.id())),
		// A REFUSAL IS NOT PROOF THE WITHDRAWAL DOES NOT EXIST. Two callers reach this — the
		// answer that carried it and the sweeper — and the loser on the primary key must
		// record the withdrawal that exists, not a phantom failure that mails a refusal and
		// leaves `awaiting_execution` never returning the order again.
		Err(err) => match ports.withdrawals.find_by_id(withdrawal).await? {
			Some(_) => ExecutionOutcome::Executed(PaymentEffect::Withdrawal(withdrawal)),
			// A HOLD IS NOT A FAILURE EITHER. `Precondition` out of `queue_withdrawal` has one
			// source today — `admit_user_account`'s freeze gate (the pause is not read there;
			// `open_withdrawal` answers `Validation` or `Repository`) — and the freeze is a
			// concierge SUSPENDED, which a REINSTATED lifts. Raised, as `execute` raises the
			// pause, so the order stays `approved` for the next sweep instead of being closed
			// for good over a control meant as a hold. `disabled` folds into the same
			// `blocked` flag and has no reversal in the domain, yet it is held here too, not
			// failed: the fold cannot tell the two apart, and the dispatcher keeps a disabled
			// owner's queued withdrawal queued rather than voiding it, so this matches.
			None if matches!(err, DomainError::Precondition(_)) => return Err(err),
			None => ExecutionOutcome::Failed(failure_reason(&err)),
		},
	})
}

/// What the initiator and the approver are told when the effect could not be created — the
/// fund's own vocabulary verbatim, an infrastructure detail replaced by one fixed sentence
/// and logged where an operator will look for it.
fn failure_reason(err: &DomainError) -> String {
	match err {
		DomainError::Validation(_) | DomainError::Conflict(_) | DomainError::Forbidden(_) | DomainError::Precondition(_) | DomainError::NotFound { .. } => err.to_string(),
		DomainError::Repository(detail) => {
			tracing::error!(detail = %detail, "payments: the withdrawal could not be created on an infrastructure error");
			"the withdrawal could not be created because of an internal error; an operator has been alerted".to_owned()
		}
	}
}

/// Close every order whose window ran out, then finish every approved one whose effect
/// does not exist yet. Driven by the periodic sweeper; both halves are idempotent.
pub async fn sweep(ports: &PaymentPorts<'_>, now: i64) -> Result<SweepReport, DomainError> {
	let mut report = SweepReport {
		expired: ports.payments.expire_due(now).await?,
		..SweepReport::default()
	};
	for id in ports.payments.awaiting_execution().await? {
		// Per-order failures warn and continue: one stuck order must not stop the rest.
		match execute(ports, id, now).await {
			Ok(view) => match view.order.state() {
				PaymentState::Executed => report.executed += 1,
				PaymentState::Approved => report.deferred += 1,
				other => {
					report.execution_failures += 1;
					tracing::error!(payment_id = %id, state = other.as_str(), reason = view.order.failure_reason().unwrap_or_default(), "payments: approved order could not be executed");
				}
			},
			Err(err) => tracing::warn!(payment_id = %id, "payments: execution attempt failed (will retry): {err}"),
		}
	}
	Ok(report)
}
