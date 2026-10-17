//! Consilium use cases — open, cancel and read (owners); the emailed invitation and vote
//! (no session); and the execution that carries an approved consilium into its effect: a
//! payment's approval, a NAV mark, a scheduled change of fee terms, a holder grant's mint,
//! a seed's deposit and `fund` subscription.
//!
//! The consilium is a **separate aggregate** from whatever it authorizes. It reserves
//! nothing and refunds nothing; on approval it calls the ordinary path the subject already
//! has, so the queue, the relay, the sweepers and reconciliation cover the result with no
//! new machinery — and the money aggregates never learn that governance exists.

use domain::{
	allocations::AllocationAccess,
	balance::{LedgerAccountKey, ServiceId, ValuationId},
	consilium::{Consilium, ConsiliumEffect, ConsiliumId, ConsiliumKind, ConsiliumState, ConsiliumTerms, HolderGrantTerms, SeedCapitalTerms, ValuationOverrideTerms, VoteDecision},
	error::DomainError,
	fees::FeePolicySubject,
	issuance::{IdempotencyKey, UnitHolder},
	money::{Nav, Network, Shares},
	payments::{PaymentState, PaymentSubject},
	users::UserId,
};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{
	application::{
		allocations as allocations_app,
		balance::{self as balance_app, Arrival, SeedPorts},
		credentials::{self, token_digest},
		funds::{self as funds_app, FundPorts},
		issuance as issuance_app, payments as payments_app,
	},
	config::KycGate,
	infrastructure::consilium::digest,
	ports::{
		AllocationRegistry, Custody, Deposits, FeePolicyChanges, OutflowPolicy, PaymentRepository, SubscriptionRepository, UnitIssuanceRepository, UserRepository, WithdrawalRepository,
		consilium::{ConsiliumRepository, ConsiliumView, ExecutionOutcome, InvitationView, SubmitOutcome, VoteAudit, VoterCredential},
		deposit_addresses::DepositAddresses,
		ledger::Ledger,
		nav::NavMarks,
	},
};

/// The salt that makes a valuation override's mark id a pure function of the consilium.
const VALUATION_SALT: &[u8] = b"consilium:valuation-override";

/// The driven ports the consilium write-path borrows. The withdrawal-side handles are here
/// only for the execution step — opening and voting touch no money at all.
pub struct ConsiliumPorts<'a> {
	pub consilia: &'a dyn ConsiliumRepository,
	pub withdrawals: &'a dyn WithdrawalRepository,
	/// The payment orders a `ConsiliumKind::Payment` quorum authorizes. Only the execution
	/// step touches it: opening and voting on a payment consilium know nothing about the
	/// order beyond the subject frozen into the terms.
	pub payments: &'a dyn PaymentRepository,
	/// The user rows an L1 payment's withdrawal is admitted against. Only the payment
	/// execution chained off a carried quorum reaches it.
	pub users: &'a dyn UserRepository,
	pub ledger: &'a dyn Ledger,
	pub custody: &'a dyn Custody,
	/// The read-only kill-switch, for the same chained execution.
	pub policy: &'a dyn OutflowPolicy,
	/// The product registry — the valuation override's open gate, and carried so the payment
	/// ports can be borrowed from here whole.
	pub allocations: &'a dyn AllocationRegistry,
	/// The valuation marks a `ConsiliumKind::ValuationOverride` quorum writes to. Read at
	/// open (is there a price to vote over?) and written at execution.
	pub nav: &'a dyn NavMarks,
	/// The fee-policy changes a `ConsiliumKind::FeePolicy` quorum schedules. Only the
	/// execution step touches it.
	pub fee_changes: &'a dyn FeePolicyChanges,
	/// The issuance records a `ConsiliumKind::HolderGrant` quorum mints through. Only the
	/// execution step touches it.
	pub issuances: &'a dyn UnitIssuanceRepository,
	/// The deposit gate a `ConsiliumKind::SeedCapital` quorum books the arrival through —
	/// read at open (has this reference been booked already?) and written at execution.
	pub deposits: &'a dyn Deposits,
	/// Who owns which deposit address — the seed's evidence that the transfer reached the
	/// treasury from outside, read at open and again at execution.
	pub addresses: &'a dyn DepositAddresses,
	/// The `fund` subscriptions a seed opens. Only the execution step touches it.
	pub subscriptions: &'a dyn SubscriptionRepository,
	pub relay: &'a Notify,
	pub configured: &'a [Network],
	/// The deployment's verification gate, for the same chained execution.
	pub kyc: KycGate,
	/// Base URL the emailed approval link is built on.
	pub approval_url_base: &'a str,
	/// Base URL the emailed CONSENT link is built on — carried so the payment ports can be
	/// borrowed from here whole, though nothing on this path mints a consent.
	pub consent_url_base: &'a str,
	/// Whether a governance mailer is actually wired behind the `consilium_mail` queue.
	///
	/// A runtime fact rather than a `cfg!`, because it is the composition root that decides
	/// whether a mailer exists — and because a test needs to exercise both answers on one
	/// build. See [`require_governance_mail`].
	pub governance_mail_wired: bool,
}

impl ConsiliumPorts<'_> {
	/// The same handles, as the fund use cases borrow them — the holder grant's mint.
	fn fund_ports(&self) -> FundPorts<'_> {
		FundPorts {
			allocations: self.allocations,
			ledger: self.ledger,
			nav: self.nav,
			relay: self.relay,
		}
	}

	/// The same handles, as the seed borrows them: the arrival's evidence, the deposit gate
	/// and the dealing ports.
	fn seed_ports(&self) -> SeedPorts<'_> {
		SeedPorts {
			deposits: self.deposits,
			custody: self.custody,
			addresses: self.addresses,
			subscriptions: self.subscriptions,
			funds: self.fund_ports(),
		}
	}

	/// The same handles, as the payment use cases borrow them.
	pub fn payment_ports(&self) -> payments_app::PaymentPorts<'_> {
		payments_app::PaymentPorts {
			payments: self.payments,
			consilia: self.consilia,
			users: self.users,
			withdrawals: self.withdrawals,
			ledger: self.ledger,
			custody: self.custody,
			policy: self.policy,
			allocations: self.allocations,
			relay: self.relay,
			configured: self.configured,
			kyc: self.kyc,
			approval_url_base: self.approval_url_base,
			consent_url_base: self.consent_url_base,
			governance_mail_wired: self.governance_mail_wired,
			// Only `open` reads the mode, and a consilium's own flows never open an order: they
			// execute and close the ones already seated, whichever way those were seated.
			approvals: crate::config::PaymentApprovalModes::ALL_CONSILIUM,
		}
	}
}

/// The mark a valuation-override consilium will record:
/// `uuid_v5(consilium_id, "consilium:valuation-override")`. Deterministic on purpose:
/// `fund_valuations` is append-only, and a retried execution must find the mark it already
/// wrote rather than append a second one.
pub fn valuation_id(consilium: ConsiliumId) -> ValuationId {
	ValuationId::from_raw(Uuid::new_v5(&consilium.raw(), VALUATION_SALT))
}

/// How long an admission to or removal from the owner roster blocks a new payout proposal.
///
/// WHY THIS EXISTS, AND WHAT IT DOES NOT FIX. The quorum rules compose weaker than any of
/// them reads on its own: at `N = 3` a removal carries on ONE peer's vote, so two colluding
/// owners can expel the third, admit puppets by unanimity, and then reach a payout quorum
/// entirely legitimately. No arrangement of thresholds fixes that — a majority of the roster
/// owns the roster, and that is what "majority" means.
///
/// What a cooling-off period buys is VISIBILITY. It makes the seizure and the payout two
/// separate events two days apart instead of one uninterrupted motion, and the delay is
/// itself a signal any remaining honest owner, operator or auditor can act on. The cost is
/// real and accepted: a legitimate roster change also delays a legitimate payout by 48h.
pub const ROSTER_COOLING_OFF_SECS: i64 = 48 * 60 * 60;

/// Refuse to open a proposal of `kind` in the shadow of a roster change.
///
/// Paired with [`ConsiliumRepository::void_open_for_roster_change`], which closes the other
/// half: a change landing while a proposal is already open voids that proposal, so the
/// window cannot be straddled by opening first and changing the roster after.
pub(crate) async fn require_settled_roster(consilia: &dyn ConsiliumRepository, kind: ConsiliumKind, now: i64) -> Result<(), DomainError> {
	let Some(changed_at) = consilia.last_roster_change_at().await? else {
		return Ok(());
	};
	let lifts_at = changed_at.saturating_add(ROSTER_COOLING_OFF_SECS);
	if now >= lifts_at {
		return Ok(());
	}
	Err(cooling_off_refusal(kind, lifts_at - now))
}

/// The refusal [`require_settled_roster`] raises, `remaining_secs` before the window lifts.
///
/// The "cooling-off" fragment and the "lifts in {h}h {m}m" clock are what the cabinet's
/// `consilium-refusal.ts` classifies and parses; only the noun varies between kinds.
fn cooling_off_refusal(kind: ConsiliumKind, remaining_secs: i64) -> DomainError {
	let hours = remaining_secs.div_euclid(3600);
	let minutes = remaining_secs.rem_euclid(3600).div_euclid(60);
	let noun = kind.noun();
	DomainError::Conflict(format!(
		"the owner roster changed less than {}h ago; a {noun} consilium cannot be opened until the cooling-off period lifts in {hours}h {minutes}m",
		ROSTER_COOLING_OFF_SECS / 3600
	))
}

/// Refuse to open a consilium that nobody could ever vote on.
///
/// Every seat's approval token reaches its owner by exactly one route: the `consilium_mail`
/// queue, drained by [`ConsiliumMailer`](crate::infrastructure::consilium_mailer) into
/// concierge. On a build where that seam is not compiled in, the queue fills and is never
/// drained — so a consilium opens, mails nothing, and expires 72h later having been
/// unvotable from the first instant.
///
/// WHY REFUSE HERE AND NOT AT BOOT. Refusing at boot would take the whole money plane down —
/// deposits, withdrawals, subscriptions, reconciliation — over a feature that is off by
/// default and orthogonal to all of them; a self-inflicted outage is a worse failure than
/// the one it reports. Refusing here fails exactly the one operation that cannot work, names
/// why, and leaves every other path running. It is safe to be this strict only because no
/// consilium kind has a single-admin bypass (the direct payout RPC is gone from the
/// contract, #245): an operator who needs a payment, a mark, a fee change, a grant or a
/// seed approved has to wire the mailer rather than route around governance.
pub(crate) fn require_governance_mail(wired: bool) -> Result<(), DomainError> {
	if wired {
		return Ok(());
	}
	Err(DomainError::Conflict(
		"governance mail is not configured, so no owner could be sent an approval token; a consilium opened now could never be voted on. Build with the `concierge_governance_mail` feature and configure the concierge mail relay first.".into(),
	))
}

/// Open a consilium over a NAV mark the move guard refuses (banking#232).
///
/// The same two gates every consilium applies — a wired mailer, a settled roster — because
/// a mark moves the price every redemption settles at, which is the same class of decision
/// as paying the fund's money out. Then the two facts execution will need, checked now so
/// nobody spends 72 hours approving a mark that cannot be recorded: the allocation exists
/// (any state — a closed product is still marked so its queued redemptions price), and the
/// fund has units outstanding, so `Nav::from_aum` yields a price the owners are actually
/// voting over. The NAV itself is NOT frozen into the terms: it is re-derived from the live
/// supply at execution, because units can be minted or burned during the vote.
///
/// No move guard is applied here, by definition — this is the path for moves the guard
/// refuses. The initiator's own standing is what the domain checks (`Consilium::open`
/// refuses a non-owner), and the RPC boundary gates on `ValuationPost`: the poster
/// proposes, the owners decide.
pub async fn open_valuation_override(ports: &ConsiliumPorts<'_>, initiator: UserId, terms: ValuationOverrideTerms, now: i64) -> Result<ConsiliumView, DomainError> {
	require_governance_mail(ports.governance_mail_wired)?;
	require_settled_roster(ports.consilia, ConsiliumKind::ValuationOverride, now).await?;
	funds_app::refuse_mark_on_reserved(&terms.service)?;
	allocations_app::get(ports.allocations, &terms.service).await?;
	let units = Shares::from_base_units(ports.ledger.balance(&LedgerAccountKey::SharesOutstanding(terms.service.clone())).await?.posted);
	Nav::from_aum(terms.aum, units)?;
	let owners = ports.consilia.owner_roster().await?;
	let terms = ConsiliumTerms::ValuationOverride(terms);
	let payload_hash = digest(&terms.canonical_bytes());
	let mut consilium = Consilium::open(ConsiliumId::new(), terms, payload_hash, initiator, &owners, now)?;
	// One token and one code per ELIGIBLE seat. The initiator is not among them, which is
	// what makes "the initiator cannot vote" a fact about what exists rather than a check
	// somewhere that could be forgotten.
	let credentials = consilium.eligible().iter().map(|voter| mint_credential(*voter)).collect::<Result<Vec<_>, _>>()?;
	ports.consilia.open(&mut consilium, &credentials, ports.approval_url_base).await?;
	find(ports.consilia, consilium.id()).await
}

/// Open a consilium over seating a new holder of a reserved allocation (#245): `units` of
/// `fee` or `fund` minted to a person.
///
/// The same two gates every kind applies — a wired mailer, a settled roster — then the
/// facts execution will need, checked now so nobody spends 72 hours approving a grant
/// that cannot be minted: the terms name a reserved allocation (the domain constructor),
/// the person exists as a mirrored, ACTIVE user (units for a frozen account are units
/// nobody can redeem, and the mint re-checks it), and the allocation prices — its NAV is
/// derived from what it holds, and a stale product mark under it would refuse the mint.
/// The NAV is NOT frozen into the terms; the units are, and what they are worth is the
/// allocation's price at execution, as a subscription's is at its own moment.
///
/// Who may open: any owner, as for every kind — `Consilium::open` refuses an initiator
/// without a seat. There is no operator permission that reaches this: seating a holder
/// of the owners' money is the owners' call from the first step.
pub async fn open_holder_grant(ports: &ConsiliumPorts<'_>, initiator: UserId, terms: HolderGrantTerms, now: i64) -> Result<ConsiliumView, DomainError> {
	require_governance_mail(ports.governance_mail_wired)?;
	require_settled_roster(ports.consilia, ConsiliumKind::HolderGrant, now).await?;
	require_active_user(ports.users, terms.user).await?;
	allocations_app::get(ports.allocations, &terms.allocation).await?;
	funds_app::dealing_nav(ports.nav, ports.ledger, &terms.allocation, now).await?;
	let owners = ports.consilia.owner_roster().await?;
	let terms = ConsiliumTerms::HolderGrant(terms);
	let payload_hash = digest(&terms.canonical_bytes());
	let mut consilium = Consilium::open(ConsiliumId::new(), terms, payload_hash, initiator, &owners, now)?;
	let credentials = consilium.eligible().iter().map(|voter| mint_credential(*voter)).collect::<Result<Vec<_>, _>>()?;
	ports.consilia.open(&mut consilium, &credentials, ports.approval_url_base).await?;
	find(ports.consilia, consilium.id()).await
}

/// The idempotency key a holder grant mints under: a pure function of the consilium, so a
/// retried execution finds the row it already wrote (`grant_units` is idempotent by
/// `(service, key)`) rather than minting twice. Well inside the key's 64-char bound.
pub fn holder_grant_key(consilium: ConsiliumId) -> IdempotencyKey {
	IdempotencyKey::parse(&format!("holder-grant:{consilium}")).expect("a uuid under a fixed prefix is a valid idempotency key")
}

/// Open a consilium over attributing a treasury arrival to a person as their seed of
/// `fund` (#245): `terms.tx_ref` on `terms.network`, which the chain reports as
/// `terms.amount` USDT, is `terms.depositor`'s.
///
/// The chain is consulted NOW, not only at execution, and with the amount as an assertion
/// rather than a lookup: the owners must be voting over a transfer that exists, that
/// reached the treasury from outside every wallet we control, and that is worth exactly
/// what the terms say — a reference that names some other real transfer is refused here
/// rather than seating the person with whatever that one was worth. Then the facts
/// execution will need, checked so nobody spends 72 hours approving a seed that cannot
/// be booked: the reference has not been booked already (under any name), the depositor is
/// a mirrored, active user, `fund` takes new money, and its price is fresh.
///
/// Who may open: any owner, as for every kind — `Consilium::open` refuses an initiator
/// without a seat. The `CapitalManage` permission the RPC gates on admits an administrator
/// to the door and no further: the money the chain proved is nobody's until the owners say
/// whose, which is the whole reason this is a consilium and not a write.
pub async fn open_seed_capital(ports: &ConsiliumPorts<'_>, initiator: UserId, terms: SeedCapitalTerms, now: i64) -> Result<ConsiliumView, DomainError> {
	require_governance_mail(ports.governance_mail_wired)?;
	require_settled_roster(ports.consilia, ConsiliumKind::SeedCapital, now).await?;
	// The seat is checked FIRST here, ahead of the gates below, and again by the domain
	// at open: the gates answer different things about a reference and a person, and an
	// administrator with no seat must not be able to probe the deposit log or a user's
	// standing through the refusals of a proposal they could never make.
	let owners = ports.consilia.owner_roster().await?;
	if !owners.contains(&initiator) {
		return Err(DomainError::Forbidden("only a fund owner may open a consilium".into()));
	}
	let (arrival, transfer) = balance_app::verify_arrival(ports.custody, ports.addresses, terms.network, &terms.tx_ref, Some(terms.amount)).await?;
	if let Arrival::User(_) = arrival {
		return Err(DomainError::Validation(format!(
			"{} is a user's deposit address, so this transfer is that user's deposit, not seed capital — record it with RecordDeposit",
			transfer.to
		)));
	}
	if ports.deposits.is_recorded(&terms.tx_ref).await? {
		return Err(DomainError::Conflict(format!("{} has already been recorded as a deposit", terms.tx_ref.as_str())));
	}
	require_active_user(ports.users, terms.depositor).await?;
	let fund = ServiceId::fund();
	allocations_app::get(ports.allocations, &fund).await?.ensure_subscribable(AllocationAccess::Invest)?;
	funds_app::dealing_nav(ports.nav, ports.ledger, &fund, now).await?;
	let terms = ConsiliumTerms::SeedCapital(terms);
	let payload_hash = digest(&terms.canonical_bytes());
	let mut consilium = Consilium::open(ConsiliumId::new(), terms, payload_hash, initiator, &owners, now)?;
	let credentials = consilium.eligible().iter().map(|voter| mint_credential(*voter)).collect::<Result<Vec<_>, _>>()?;
	ports.consilia.open(&mut consilium, &credentials, ports.approval_url_base).await?;
	find(ports.consilia, consilium.id()).await
}

/// The person a consilium seats must exist as a mirrored user AND be active: units minted
/// to a frozen account are units nobody can redeem, and a disabled principal is not one the
/// owners should be asked to make a holder of their money.
async fn require_active_user(users: &dyn UserRepository, user: UserId) -> Result<(), DomainError> {
	let Some(row) = users.find_by_id(user).await? else {
		return Err(DomainError::NotFound {
			entity: "user",
			id: user.to_string(),
		});
	};
	if !row.is_active() {
		return Err(DomainError::Precondition(format!("user {user} is not active, so they cannot be seated as a holder")));
	}
	Ok(())
}

/// Mint one seat's credentials. The plaintexts are returned to the caller (they have to
/// reach the owner's mailbox); only their digests are ever stored.
pub(crate) fn mint_credential(user_id: UserId) -> Result<VoterCredential, DomainError> {
	let secret = credentials::mint()?;
	Ok(VoterCredential {
		user_id,
		token: secret.token,
		code: secret.code,
		token_hash: secret.token_hash,
		code_hash: secret.code_hash,
	})
}

pub async fn find(consilia: &dyn ConsiliumRepository, id: ConsiliumId) -> Result<ConsiliumView, DomainError> {
	consilia.find(id).await?.ok_or_else(|| DomainError::NotFound {
		entity: "consilium",
		id: id.to_string(),
	})
}

pub async fn list(consilia: &dyn ConsiliumRepository, limit: i64) -> Result<Vec<ConsiliumView>, DomainError> {
	consilia.list(limit).await
}

/// Withdraw a consilium the caller opened. Votes are not carried anywhere: reopening starts
/// a fresh request with a fresh hash, which is what keeps an approval bound to the terms it
/// was given.
pub async fn cancel(consilia: &dyn ConsiliumRepository, id: ConsiliumId, by: UserId, now: i64) -> Result<ConsiliumView, DomainError> {
	consilia.cancel(id, by, now).await
}

/// The redacted invitation behind an emailed token. Side-effect free.
pub async fn invitation(consilia: &dyn ConsiliumRepository, token: &str, now: i64) -> Result<InvitationView, DomainError> {
	consilia.invitation(&token_digest(token), now).await
}

/// Cast a vote. The repository does the whole of it — attempt, comparison, tally and
/// transition — in one transaction under the consilium's row lock.
pub async fn submit_decision(consilia: &dyn ConsiliumRepository, token: &str, code: &str, decision: VoteDecision, audit: &VoteAudit, now: i64) -> Result<SubmitOutcome, DomainError> {
	consilia.submit(&token_digest(token), code, decision, audit, now).await
}

/// Turn an approved consilium into its effect.
///
/// Six things make this safe to call more than once, from more than one place (the vote that
/// carried it, and the sweeper picking up an approval whose execution never ran):
///
/// 1. only an `approved` consilium is executable — an expired, rejected, cancelled or
///    already-failed one is refused, so a late vote can never reach the money;
/// 2. the payload hash is re-verified against the stored terms, so the effect that lands is
///    the one the owners were shown;
/// 3. the tally is re-checked against the LIVE owner roster, so an approval that a seat
///    change has since invalidated cannot be spent;
/// 4. an approval that has been sitting too long past its window is refused rather than
///    executed on a stale authorization;
/// 5. every effect's id is derived from the consilium (or its subject), so a retried
///    execution finds the row rather than writing a second one — and a refusal from the
///    effect's path is re-read against that id before being believed, so the loser of a
///    two-caller race records the effect that actually exists instead of a phantom failure;
/// 6. a genuine refusal is recorded as `execution_failed` with its reason — terminal,
///    visible to the owners, and retried by nothing.
pub async fn execute(ports: &ConsiliumPorts<'_>, id: ConsiliumId, now: i64) -> Result<ConsiliumView, DomainError> {
	let view = find(ports.consilia, id).await?;
	let consilium = &view.consilium;
	if consilium.state() == ConsiliumState::Executed {
		return Ok(view);
	}
	if consilium.state() != ConsiliumState::Approved {
		return Err(DomainError::Conflict(format!("consilium is {}, not executable", consilium.state().as_str())));
	}
	// An approval is a signature over `payload_hash`. Re-taking the digest over what is
	// about to be paid is what makes an edited row unable to spend that signature.
	if digest(&consilium.terms().canonical_bytes()) != consilium.payload_hash() {
		let reason = "the stored terms no longer match the payload hash the owners approved".to_owned();
		return ports.consilia.record_execution(id, ExecutionOutcome::Failed(reason), now).await;
	}
	// THE QUORUM IS RE-CHECKED AGAINST THE LIVE ROSTER, NOT THE STORED VERDICT.
	//
	// `state = approved` is a fact about the tally at the moment the carrying vote landed.
	// Execution can happen much later — after a crash, after a transient failure the sweeper
	// only warned about, or simply in the next sweep window — and an owner can lose their
	// seat in between. `find()` recomputes `approvals()` through the live `still_an_owner`
	// join while `threshold()` stays frozen at the roster size the consilium opened against,
	// so this comparison is exactly "does a quorum of CURRENT owners still stand behind
	// this?". Without it the central invariant would hold at tally time and fail at the one
	// moment that actually spends money.
	if consilium.approvals() < consilium.threshold() {
		let reason = format!(
			"only {} of the {} required approvals are still held by current owners — the roster changed after the vote carried",
			consilium.approvals(),
			consilium.threshold()
		);
		return ports.consilia.record_execution(id, ExecutionOutcome::Failed(reason), now).await;
	}
	// AN APPROVAL GOES STALE TOO. `expire_due` only ever touches `open` consilia, so an
	// approved one has no deadline of its own: a payout approved just before a long outage
	// would otherwise execute whenever the process came back, on an authorization the owners
	// gave against a world that no longer exists. The grace is what keeps an ordinary sweep
	// gap or a brief restart from voiding a legitimately fresh approval.
	// A roster change during the request's life voids it. The sweeper does the voiding and
	// tells the owners, but it runs on a 60s cadence — so this refuses immediately rather
	// than leaving a window in which an approval given before the change could still be
	// spent after it.
	if let Some(changed_at) = ports.consilia.last_roster_change_at().await?
		&& changed_at >= consilium.created_at()
	{
		let reason = "the owner roster changed while this request was open, so the approval was voided".to_owned();
		return ports.consilia.record_execution(id, ExecutionOutcome::Failed(reason), now).await;
	}
	if now > consilium.expires_at().saturating_add(EXECUTION_GRACE_SECS) {
		let reason = "the approval went stale: execution did not run within the grace period after the voting window closed".to_owned();
		return ports.consilia.record_execution(id, ExecutionOutcome::Failed(reason), now).await;
	}
	// THE EFFECT DISPATCHER. Everything above this line is kind-agnostic policy; everything
	// below is what one kind actually produces. The match has no `_` arm, so a second kind
	// cannot be added without deciding here how it executes — which is the one question the
	// aggregate deliberately refuses to answer (it records an id, not a mechanism).
	let outcome = match consilium.terms().clone() {
		ConsiliumTerms::Payment(subject) => execute_payment(ports, consilium, subject, now).await?,
		ConsiliumTerms::ValuationOverride(terms) => execute_valuation_override(ports, consilium, terms).await?,
		ConsiliumTerms::FeePolicy(subject) => execute_fee_policy(ports, id, &subject, now).await?,
		ConsiliumTerms::HolderGrant(terms) => execute_holder_grant(ports, id, terms, now).await?,
		ConsiliumTerms::SeedCapital(terms) => execute_seed_capital(ports, terms, now).await?,
	};
	ports.consilia.record_execution(id, outcome, now).await
}

/// Book the seed an approved consilium authorizes — the depositor's deposit and their
/// `fund` subscription — and say how it went.
///
/// Through the SAME writer the direct RPC used to call (`balance::seed_fund_capital`),
/// with the terms' amount as the chain assertion, so a transfer that no longer matches
/// what the owners signed fails rather than books. That writer is idempotent by the chain
/// reference: the deposit gate admits it once and the subscription's id is derived from
/// it, so the two callers that reach this (the carrying vote and the sweeper) cannot mint
/// twice, and a repeat repairs a first attempt that died between the two commits. The
/// effect is the subscription under that derived id, and a refusal is re-read against it
/// before being believed — the loser of a two-caller race records the subscription that
/// exists rather than a phantom failure. A genuine refusal (the transfer vanished, `fund`
/// closed, its price stale, the reference booked under another name meanwhile) is the
/// consilium's failure to record; an infrastructure error is retried by the sweeper.
async fn execute_seed_capital(ports: &ConsiliumPorts<'_>, terms: SeedCapitalTerms, now: i64) -> Result<ExecutionOutcome, DomainError> {
	let subscription = balance_app::seed_subscription_id(&terms.tx_ref);
	if ports.subscriptions.find_by_id(subscription).await?.is_some() {
		return Ok(ExecutionOutcome::Executed(ConsiliumEffect::Subscription(subscription)));
	}
	match balance_app::seed_fund_capital(&ports.seed_ports(), terms.depositor, terms.tx_ref, terms.network, Some(terms.amount), now).await {
		Ok(_) => Ok(ExecutionOutcome::Executed(ConsiliumEffect::Subscription(subscription))),
		Err(err) => match ports.subscriptions.find_by_id(subscription).await? {
			Some(_) => Ok(ExecutionOutcome::Executed(ConsiliumEffect::Subscription(subscription))),
			None => match err {
				DomainError::Repository(_) => Err(err),
				refused => Ok(ExecutionOutcome::Failed(failure_reason(&refused))),
			},
		},
	}
}

/// Mint the units an approved holder grant authorizes, and say how it went.
///
/// Through the SAME writer the data migration uses (`issuance::grant_units`) — the one
/// door into a reserved allocation's supply, which the operator's `IssueUnits` never
/// reaches. The key is derived from the consilium, so a retried execution (the carrying
/// vote and the sweeper both arrive here) is answered with the row the first one wrote;
/// the writer's own race handling covers two callers inserting at once. A refusal from it
/// — the cap no longer admits the units, the allocation's price went stale, the person is
/// gone — is the consilium's failure to record; an infrastructure error is retried by the
/// sweeper. NAV is the allocation's LIVE price now, not the price at open.
async fn execute_holder_grant(ports: &ConsiliumPorts<'_>, id: ConsiliumId, terms: HolderGrantTerms, now: i64) -> Result<ExecutionOutcome, DomainError> {
	let request = issuance_app::IssueUnitsRequest {
		service: terms.allocation,
		holder: UnitHolder::User(terms.user),
		units: terms.units,
		cost_basis: None,
		idempotency_key: holder_grant_key(id),
	};
	match issuance_app::grant_units(&ports.fund_ports(), ports.issuances, ports.users, request, now).await {
		Ok(record) => Ok(ExecutionOutcome::Executed(ConsiliumEffect::Issuance(record.issuance.id()))),
		Err(err @ DomainError::Repository(_)) => Err(err),
		Err(err) => Ok(ExecutionOutcome::Failed(failure_reason(&err))),
	}
}

/// Record the mark an approved valuation override authorizes, and say how it went.
///
/// The mark is written through the SAME writer as a direct post (`funds::record_valuation`)
/// with the move guard simply not consulted — that is the whole of what the quorum bought.
/// `posted_by` is the initiator's id in the spelling `claims.sub` has on the direct path,
/// so the redeem cooldown binds the proposer exactly as it binds a direct poster. NAV is
/// derived from the LIVE supply now, not the supply at open.
///
/// The id is derived from the consilium, so a retried execution finds the mark already
/// there; and a refusal from the writer is re-read against that id before being believed.
/// Two callers reach this by construction — the carrying vote and the sweeper — and both
/// can see "no mark" before one of them writes it; recording the loser's error as `Failed`
/// would be a lie that sticks, mailed to every owner, while the mark exists.
async fn execute_valuation_override(ports: &ConsiliumPorts<'_>, consilium: &Consilium, terms: ValuationOverrideTerms) -> Result<ExecutionOutcome, DomainError> {
	let mark = valuation_id(consilium.id());
	if ports.nav.find(mark).await?.is_some() {
		return Ok(ExecutionOutcome::Executed(ConsiliumEffect::Valuation(mark)));
	}
	let posted_by = consilium.initiator().to_string();
	Ok(match funds_app::record_valuation(ports.nav, ports.ledger, mark, terms.service, terms.aum, &posted_by).await {
		Ok(_) => ExecutionOutcome::Executed(ConsiliumEffect::Valuation(mark)),
		Err(err) => match ports.nav.find(mark).await? {
			Some(_) => ExecutionOutcome::Executed(ConsiliumEffect::Valuation(mark)),
			None => ExecutionOutcome::Failed(failure_reason(&err)),
		},
	})
}

/// Carry an approved change of fee terms: schedule it, fix the moment it binds, and tell the
/// holders. No money moves; promoting the change into the live terms once its moment comes
/// is the fee sweeper's job.
///
/// `schedule_approved` is idempotent on a change this consilium already scheduled, which is
/// what makes a retried execution safe, and re-checks the row against the signed subject so
/// a change edited underneath its quorum cannot spend the quorum's signature. A conflict
/// from it (the change was cancelled while the owners voted, or no longer matches) is the
/// consilium's failure to record; an infrastructure error is retried by the sweeper.
pub async fn execute_fee_policy(ports: &ConsiliumPorts<'_>, id: ConsiliumId, subject: &FeePolicySubject, now: i64) -> Result<ExecutionOutcome, DomainError> {
	match ports.fee_changes.schedule_approved(subject, id, now).await {
		Ok(_) => Ok(ExecutionOutcome::Executed(ConsiliumEffect::FeePolicy(subject.change_id))),
		Err(err @ DomainError::Repository(_)) => Err(err),
		Err(err) => Ok(ExecutionOutcome::Failed(failure_reason(&err))),
	}
}

/// Carry an approved payment order past its one requirement.
///
/// The effect is the ORDER, not its money: `record_approval` moves the order to `approved`
/// and — for an L2/L3 order — raises the reservation that stops N concurrent payments
/// overdrawing one claim. Turning an approved order into a withdrawal or a posted transfer
/// is [`crate::application::payments::execute`]'s job, driven by the same sweeper that
/// drives this one, because those two steps fail for different reasons and must be retried
/// independently.
///
/// No deterministic id to re-read here, for once: the order already exists and carries its
/// own. `record_approval` is idempotent on an order that is already `approved`, which is
/// what makes a retried consilium execution safe.
///
/// THE ORDER IS RE-READ AND RE-HASHED before the approval is recorded. `execute` has already
/// proved the consilium's stored terms hash to what the owners signed; this proves the ORDER
/// ROW those terms name still hashes to the same thing, so an order edited underneath its
/// quorum cannot spend the quorum's signature. And the approval is recorded against THIS
/// consilium's id, checked under the order's lock against the `payment_approval` link, so a
/// quorum over one order can never carry another.
///
/// Public so its outcome can be asserted on its own.
pub async fn execute_payment(ports: &ConsiliumPorts<'_>, consilium: &Consilium, subject: PaymentSubject, now: i64) -> Result<ExecutionOutcome, DomainError> {
	let Some(order) = ports.payments.find(subject.payment_id).await? else {
		return Ok(ExecutionOutcome::Failed(format!("payment {} no longer exists", subject.payment_id)));
	};
	if digest(&order.order.subject().canonical_bytes()) != consilium.payload_hash() {
		return Ok(ExecutionOutcome::Failed("the payment order no longer matches the terms the owners approved".to_owned()));
	}
	let outcome = match ports.payments.record_approval(subject.payment_id, consilium.id(), now).await {
		Ok(_) => Ok(ExecutionOutcome::Executed(ConsiliumEffect::Payment(subject.payment_id))),
		// A REFUSAL IS NOT PROOF THE APPROVAL DID NOT LAND. Two callers reach this — the vote
		// that carried the quorum, and the sweeper — so one can lose the row lock race and see
		// a conflict over an order the other has already approved. Re-read before believing it,
		// exactly as `execute_valuation_override` re-reads the mark id.
		//
		// THE RE-READ ASKS FOR THE POSITIVE FACT. "No longer pending" is not it: an order the
		// initiator withdrew, the sweeper expired or a burned seat rejected is also not
		// pending, and reporting any of those as `Executed` would file a consilium as having
		// authorized money that never moved. Only an order that IS approved (or has already
		// gone past approval) proves the other caller's approval landed.
		Err(err) => match ports.payments.find(subject.payment_id).await? {
			Some(view) if matches!(view.order.state(), PaymentState::Approved | PaymentState::Executed | PaymentState::ExecutionFailed) =>
				Ok(ExecutionOutcome::Executed(ConsiliumEffect::Payment(subject.payment_id))),
			_ => Ok(ExecutionOutcome::Failed(failure_reason(&err))),
		},
	};
	// THE MONEY, CHAINED INLINE. The consilium's effect is the approval, recorded above; the
	// settlement or the withdrawal is the order's own step, and it is attempted here so a
	// carried quorum moves money promptly rather than on the next sweep. Its failure is the
	// ORDER's to record (`execution_failed`, or a deferral the sweeper retries) and must not
	// turn the consilium's outcome into a failure over an approval that did land.
	if let Ok(ExecutionOutcome::Executed(_)) = &outcome
		&& let Err(err) = payments_app::execute(&ports.payment_ports(), subject.payment_id, now).await
	{
		tracing::error!(payment_id = %subject.payment_id, "payments: the quorum carried the order but its execution did not complete: {err}");
	}
	outcome
}

/// How long after the voting window closes an approval may still be spent.
///
/// Sized for the gap an ordinary incident leaves: the sweep runs every minute, so a day
/// covers a restart, a deploy, or a stretch of degraded execution without voiding an
/// approval the owners would still stand behind — while refusing one that has been sitting
/// through a multi-day outage.
const EXECUTION_GRACE_SECS: i64 = 24 * 60 * 60;

/// What the owners are told when a payout could not be created.
///
/// A validation refusal is the fund's own vocabulary — "payout exceeds the fund's available
/// revenue", "amount is below the rail minimum" — and is exactly what an owner needs in
/// order to act. Anything else is an infrastructure detail: a raw Postgres or TigerBeetle
/// string in a governance email tells the owners nothing they can use and leaks schema
/// internals to every seat on the roster. Those become one fixed sentence, and the detail
/// goes to the log where an operator will look for it.
fn failure_reason(err: &DomainError) -> String {
	match err {
		DomainError::Validation(_) | DomainError::Conflict(_) | DomainError::Forbidden(_) | DomainError::Precondition(_) | DomainError::NotFound { .. } => err.to_string(),
		DomainError::Repository(detail) => {
			tracing::error!(detail = %detail, "consilium: payout creation failed on an infrastructure error");
			"the payout could not be created because of an internal error; an operator has been alerted".to_owned()
		}
	}
}

/// Close every consilium whose window has run out. Driven by the periodic sweeper.
pub async fn sweep_expired(consilia: &dyn ConsiliumRepository, now: i64) -> Result<usize, DomainError> {
	consilia.expire_due(now).await
}

#[cfg(test)]
mod tests {
	use super::*;

	fn message(err: DomainError) -> String {
		match err {
			DomainError::Conflict(message) => message,
			other => panic!("the cooling-off refusal is a conflict, got {other:?}"),
		}
	}

	/// One wording is pinned byte for byte — the fragments the cabinet's classifier reads —
	/// and the other kinds differ from it in the noun alone.
	#[test]
	fn cooling_off_refusal_names_the_kind_and_keeps_the_classified_fragments() {
		let payment = message(cooling_off_refusal(ConsiliumKind::Payment, 12 * 3600 + 30 * 60));
		assert_eq!(
			payment,
			"the owner roster changed less than 48h ago; a payment consilium cannot be opened until the cooling-off period lifts in 12h 30m"
		);

		for kind in [ConsiliumKind::ValuationOverride, ConsiliumKind::FeePolicy, ConsiliumKind::HolderGrant, ConsiliumKind::SeedCapital] {
			let got = message(cooling_off_refusal(kind, 12 * 3600 + 30 * 60));
			assert_eq!(got, payment.replace("a payment consilium", &format!("a {} consilium", kind.noun())), "{kind:?}");
			assert!(got.contains("cooling-off") && got.contains("lifts in 12h 30m"), "{kind:?}: {got}");
		}
	}
}
