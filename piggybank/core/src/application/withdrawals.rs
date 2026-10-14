//! Withdrawal use cases — request + cancel (user), dispatch/settle/fail (operator),
//! list (user).
//!
//! `request_withdrawal` is a command with a **two-part Read-First**: it gates on the
//! user's money-out standing — not frozen (a concierge SUSPENDED or a banking
//! `DisableUser`) and KYC-verified — read through the same [`OutflowPolicy`] the dispatch
//! re-check uses, confirms the **available** unified claim
//! (posted − already-reserved) covers the gross (user solvency; the TB non-negative
//! flag is the backstop), then checks the **chosen rail's liquidity** — the min of the
//! TB rail accounting balance and the custody adapter's real on-chain treasury view —
//! and dispatches immediately when it covers the net, otherwise the withdrawal is
//! accepted and left `Queued` for the [`Dispatcher`](crate::infrastructure::dispatcher)
//! worker (or the admin `dispatch_withdrawal`) to send once the rail is topped up.
//! Admission is therefore NOT the last word on policy: `dispatch_withdrawal` re-evaluates
//! the kill-switch, the freeze and the tier-1 floor at the moment the money leaves, so a
//! pause, an AML hold or a revoked verification landing on an already-queued withdrawal
//! still stops it — whichever path (sweep or admin RPC) reaches it first. The tier floor
//! specifically is the deployment's [`KycGate`] (enforced by default), and the SAME value
//! must reach both points: a gate lifted only at admission would accept withdrawals that
//! dispatch then parks indefinitely.
//! `settle`/`fail` are the operator/watcher-driven
//! completions (admin-gated at the boundary), standing in for a chain watcher + custody
//! confirmation callback; `cancel` (user) refunds a still-queued withdrawal. The
//! cardinal rule — fail (void) only when the broadcast certainly did not land — is why
//! `fail` is only legal once `Processing`, while a `Queued` one is always safe to cancel.

use domain::{
	balance::LedgerAccountKey,
	error::DomainError,
	money::{Network, TxRef, Usdt, WalletAddress},
	users::UserId,
	withdrawals::{Withdrawal, WithdrawalId, WithdrawalPolicy, WithdrawalSource},
};
use tokio::sync::Notify;
use tracing::warn;

use crate::{
	config::KycGate,
	ports::{Custody, OutflowPolicy, WithdrawalRepository, ledger::Ledger, outflow::PayoutStanding},
};

/// What a caller is told when the outflow kill-switch is engaged. One constant for the
/// boundary gate (`services::support::unfrozen_caller`), the admission path and the
/// dispatch re-check, so the three cannot drift into three descriptions of one switch.
pub const OUTFLOWS_PAUSED: &str = "money movements are temporarily paused (read-only mode)";
/// What a caller is told when their account is blocked from moving money — the
/// `frozen OR disabled` fold. Shared for the same reason as [`OUTFLOWS_PAUSED`].
pub const ACCOUNT_FROZEN: &str = "account is frozen";

/// The driven ports the withdrawal write-path borrows: the aggregate's repository, the
/// ledger both Read-First checks read, the custody gateway the rail-liquidity check asks,
/// and the relay nudged once the control-plane commit lands. Exactly the set
/// [`open_withdrawal`] — the shared body of every request path — needs, so each use-case's
/// own parameters stay its *request*: which source, which rail, where, how much. The
/// user-facing entry point's extra gates (the [`OutflowPolicy`] freeze/KYC standing, the
/// configured-rail list, the verification switch) are deliberately NOT here but in
/// [`AdmissionGates`], so a caller that runs its own admission (a payment order, at open)
/// is not handed them twice. A plain borrow-holder: it owns nothing and does nothing.
pub struct WithdrawalPorts<'a> {
	/// The `withdrawals` aggregate's driven port (Postgres control plane).
	pub withdrawals: &'a dyn WithdrawalRepository,
	/// The money gateway (TigerBeetle): the source's claim and the rail's accounting balance.
	pub ledger: &'a dyn Ledger,
	/// The custody gateway — read-only here, for the on-chain treasury view.
	pub custody: &'a dyn Custody,
	/// Nudged after the commit so the outbox relay broadcasts promptly.
	pub relay: &'a Notify,
}

/// The gates the user-facing entry point runs before the shared body — exactly the set
/// [`WithdrawalPorts`] deliberately leaves out. They travel together because they are
/// answered together, at admission, about one caller: is this rail run at all, is the
/// account frozen, and does the verification floor apply to it ([`KycGate`], enforced
/// unless the deployment lifted it).
///
/// Bundled rather than passed loose for the same reason
/// [`DepositAddressPorts`](crate::application::wallet::DepositAddressPorts) is a bundle:
/// it keeps the use case's own parameters its *request* — whose withdrawal, which rail,
/// where, how much.
pub struct AdmissionGates<'a> {
	/// Where the freeze flag and the mirrored KYC tier are read from — the SAME port
	/// [`dispatch_withdrawal`] re-reads at payout, so admission and dispatch cannot disagree
	/// on what "frozen" means.
	pub policy: &'a dyn OutflowPolicy,
	/// The rails with a running on-chain watcher; a withdrawal on any other is refused.
	pub configured: &'a [Network],
	/// The deployment's verification gate. The SAME value must reach
	/// [`dispatch_withdrawal`]: a gate lifted only here admits withdrawals that the payout
	/// gate then leaves queued indefinitely.
	pub kyc: KycGate,
}

/// The calling user withdraws `amount` (gross) of free balance to `address`. The fee
/// is the per-network policy fee; the net (`amount − fee`) is what leaves on-chain.
///
/// `id` is supplied by the caller so a payment order can derive it from itself (`uuid_v5(payment_id, "payment:withdrawal")`) so a retried
/// execution re-creates the same row instead of a second withdrawal. The self-service wallet
/// passes a fresh [`WithdrawalId::new`].
pub async fn request_withdrawal(
	ports: &WithdrawalPorts<'_>,
	gates: &AdmissionGates<'_>,
	id: WithdrawalId,
	user: UserId,
	network: Network,
	address: WalletAddress,
	amount: Usdt,
) -> Result<Withdrawal, DomainError> {
	admit_user_withdrawal(gates, user, network).await?;
	let source = WithdrawalSource::User(user);
	open_withdrawal(ports, id, source, network, address, amount, true).await
}

/// [`request_withdrawal`], except that the withdrawal is ALWAYS left `Queued` for the
/// dispatcher, however liquid the rail is right now.
///
/// This is the shape a payment order takes. Its consent was given up to 72 hours before
/// the order executes, and the execution re-reads the consent pins under the order's lock
/// only AFTER the withdrawal exists — so a revocation landing in that window must find a
/// withdrawal it can still void. A withdrawal that dispatched on creation is past `Queued`
/// and can never be voided again (the broadcast may have landed). Deferring the dispatch
/// keeps the void possible and, as a bonus, puts every payment withdrawal through
/// [`require_dispatchable`]: the pause, the freeze and the verification floor are all
/// re-read at the moment the money actually leaves, not only when the order was opened.
pub async fn queue_withdrawal(
	ports: &WithdrawalPorts<'_>,
	gates: &AdmissionGates<'_>,
	id: WithdrawalId,
	user: UserId,
	network: Network,
	address: WalletAddress,
	amount: Usdt,
) -> Result<Withdrawal, DomainError> {
	admit_user_withdrawal(gates, user, network).await?;
	let source = WithdrawalSource::User(user);
	open_withdrawal(ports, id, source, network, address, amount, false).await
}

/// The user-facing admission gates, on their own: the rail is run, the account is not
/// frozen, and the verification floor admits it. Shared by [`request_withdrawal`] and the payment
/// order's open-time pre-check, so an L1 payment out of an investor's claim is refused at
/// open for exactly the reasons its execution would refuse it 72 hours later.
async fn admit_user_withdrawal(gates: &AdmissionGates<'_>, user: UserId, network: Network) -> Result<(), DomainError> {
	require_configured(gates.configured, network)?;
	require_unfrozen_rail(gates.policy, network).await?;
	admit_user_account(gates, user).await
}

/// The account half of the admission gates — the freeze and the verification floor, with
/// no rail in the question. On its own so a payment order can run it over an INTERNAL
/// destination too: a hop from one investor's claim to another's is that investor moving
/// money on their say-so exactly as a withdrawal is, and an unverified account must not be
/// able to route around the floor by paying a verified one who then withdraws.
pub async fn admit_user_account(gates: &AdmissionGates<'_>, user: UserId) -> Result<(), DomainError> {
	// Freeze gate — `blocked` is the fold of a concierge SUSPENDED (`frozen`) and a
	// banking-side `DisableUser` (`status = 'disabled'`), the same one `dispatch_withdrawal`
	// re-reads at payout. Read through the policy port rather than the user aggregate on
	// purpose: the aggregate never carried the mirrored `frozen` flag, so a check on it
	// let a suspended account queue withdrawals the dispatcher then had to catch.
	let standing = policy_standing(gates, user).await?;
	if standing.blocked {
		return Err(DomainError::Precondition(ACCOUNT_FROZEN.into()));
	}
	// Verification gate — an unverified account (tier 0 is a registration and a confirmed
	// email, nothing more) may not move money off the platform. The tier is the identity
	// plane's, mirrored onto the local row by the lifecycle bridge; this is the money
	// plane enforcing it. The same `gate` must reach `dispatch_withdrawal` too — a lifted gate that admits a withdrawal the
	// dispatch gate then parks forever is worse than no switch at all.
	if !gates.kyc.admits(standing.kyc_level) {
		return Err(DomainError::Forbidden("identity verification required to withdraw".into()));
	}
	Ok(())
}

/// The caller's money-out standing, with a missing row reported as the user not existing —
/// at admission nothing has been reserved yet, so "no such user" is the honest answer,
/// unlike the fail-closed refusal [`require_dispatchable`] gives the same `None`.
async fn policy_standing(gates: &AdmissionGates<'_>, user: UserId) -> Result<PayoutStanding, DomainError> {
	gates.policy.standing(user).await?.ok_or_else(|| DomainError::NotFound {
		entity: "user",
		id: user.to_string(),
	})
}

/// Would this user withdrawal be accepted *right now*, without recording anything? Run by
/// a payment order at OPEN so an
/// impossible L1 payment is refused before its subject spends 72 hours consenting to it.
pub async fn check_user_withdrawal(ledger: &dyn Ledger, gates: &AdmissionGates<'_>, user: UserId, network: Network, address: WalletAddress, amount: Usdt) -> Result<(), DomainError> {
	admit_user_withdrawal(gates, user, network).await?;
	let source = WithdrawalSource::User(user);
	Withdrawal::request(WithdrawalId::new(), source, network, address, amount, WithdrawalPolicy::fee_for(source, network))?;
	require_solvent(ledger, source, amount).await
}

/// An L1 payment out of banking's treasury (D17), executing an order the owners' consilium
/// approved: always queued, like [`queue_withdrawal`], so the dispatch re-reads the pause
/// and the rail freeze when the money actually leaves. There is no account to freeze or
/// verify — the consilium is the say-so — and no fee: it would be paid to the treasury out
/// of the treasury.
pub async fn queue_treasury_withdrawal(
	ports: &WithdrawalPorts<'_>,
	gates: &AdmissionGates<'_>,
	id: WithdrawalId,
	network: Network,
	address: WalletAddress,
	amount: Usdt,
) -> Result<Withdrawal, DomainError> {
	require_configured(gates.configured, network)?;
	require_unfrozen_rail(gates.policy, network).await?;
	open_withdrawal(ports, id, WithdrawalSource::Treasury, network, address, amount, false).await
}

/// Would a treasury payout be accepted *right now*, without recording anything? The payment
/// order's open-time pre-check, as [`check_user_withdrawal`] is for an investor's.
pub async fn check_treasury_withdrawal(ledger: &dyn Ledger, gates: &AdmissionGates<'_>, network: Network, address: WalletAddress, amount: Usdt) -> Result<(), DomainError> {
	require_configured(gates.configured, network)?;
	require_unfrozen_rail(gates.policy, network).await?;
	let source = WithdrawalSource::Treasury;
	Withdrawal::request(WithdrawalId::new(), source, network, address, amount, WithdrawalPolicy::fee_for(source, network))?;
	require_solvent(ledger, source, amount).await
}

/// Rail gate — the withdrawable view no longer offers an unconfigured rail, but a direct
/// API caller could otherwise queue a withdrawal that only a manual operator settle (the
/// stub custody fallthrough) could ever ship. Pre-existing withdrawals on a since-
/// de-configured rail stay listable/cancellable.
fn require_configured(configured: &[Network], network: Network) -> Result<(), DomainError> {
	if configured.contains(&network) {
		Ok(())
	} else {
		Err(DomainError::Validation(format!("{network} withdrawals are not available")))
	}
}

async fn require_unfrozen_rail(policy: &dyn OutflowPolicy, network: Network) -> Result<(), DomainError> {
	if policy.frozen_rails().await?.contains(&network) {
		return Err(DomainError::Precondition(format!("{network} is frozen by an operator — withdrawals on it are paused")));
	}
	Ok(())
}

/// Read-First on the source's claim: the spendable balance (posted minus what other
/// in-flight withdrawals have already reserved) must cover the gross. TigerBeetle's
/// non-negative flag is the hard backstop.
async fn require_solvent(ledger: &dyn Ledger, source: WithdrawalSource, amount: Usdt) -> Result<(), DomainError> {
	let claim = ledger.balance(&source.claim_key()).await?;
	if Usdt::from_base_units(claim.available()) < amount {
		return Err(DomainError::Validation("insufficient available balance to withdraw".into()));
	}
	Ok(())
}

/// The shared body of every request path: validate the shape, Read-First the **source's**
/// solvency, then record — dispatching straight away when `dispatch_immediately` is set
/// and the rail can already cover it, otherwise leaving the row `Queued` for the
/// dispatcher. A caller that passes `false` is deliberately NOT asked about liquidity: it
/// wants the withdrawal to exist in a state that can still be voided, whatever the rail
/// holds.
async fn open_withdrawal(
	ports: &WithdrawalPorts<'_>,
	id: WithdrawalId,
	source: WithdrawalSource,
	network: Network,
	address: WalletAddress,
	amount: Usdt,
	dispatch_immediately: bool,
) -> Result<Withdrawal, DomainError> {
	let fee = WithdrawalPolicy::fee_for(source, network);
	// Validate the request shape (minimum, fee coverage, no on-chain dust, address net).
	let mut withdrawal = Withdrawal::request(id, source, network, address, amount, fee)?;
	// Read-First #1 — the source can actually cover the gross.
	require_solvent(ports.ledger, source, amount).await?;
	if dispatch_immediately && rail_covers(ports, network, withdrawal.net_amount()).await? {
		withdrawal.dispatch()?;
	}
	ports.withdrawals.open(&mut withdrawal).await?;
	ports.relay.notify_one();
	Ok(withdrawal)
}

/// Read-First #2 — rail liquidity: dispatchable liquidity is `min(TB rail, on-chain
/// treasury)`. The TB `wallet:<net>` balance alone over-counts — it includes confirmed
/// deposits still sitting on users' derived addresses, which the treasury hot wallet cannot
/// spend. `true` means the effective liquidity covers the net and the withdrawal may go to
/// custody now; `false` means accept-and-queue, for the dispatcher to send once the rail is
/// topped up. A treasury read failure also degrades to queued — acceptance and the clearing
/// reserve NEVER depend on rail liquidity, so a flaky node must not refuse a user.
async fn rail_covers(ports: &WithdrawalPorts<'_>, network: Network, net: Usdt) -> Result<bool, DomainError> {
	let rail_liquidity = Usdt::from_base_units(ports.ledger.balance(&LedgerAccountKey::CryptoWallet(network)).await?.posted);
	Ok(match ports.custody.treasury_liquidity(network).await {
		Ok(Some(onchain)) => rail_liquidity.min(onchain) >= net,
		// No chain view (stub / unwired rail) — the TB accounting balance is all there is.
		Ok(None) => rail_liquidity >= net,
		Err(err) => {
			warn!(%network, "treasury liquidity read failed — accepting the withdrawal queued: {err}");
			false
		}
	})
}

/// The global outflow pause — the read-only kill-switch — as a gate.
///
/// Public so the dispatcher can short-circuit a whole sweep on it. That is not a second
/// copy of the rule: [`dispatch_withdrawal`] calls this same function per withdrawal and
/// is the only thing standing between a queued row and the chain. The sweep's early exit
/// exists so a backlog of N under an operator pause costs one read and one log line every
/// interval instead of N of each.
pub async fn require_outflows_enabled(policy: &dyn OutflowPolicy) -> Result<(), DomainError> {
	if policy.outflows_paused().await? {
		return Err(DomainError::Precondition(OUTFLOWS_PAUSED.into()));
	}
	Ok(())
}

/// The outflow policy gate, re-evaluated at the moment of dispatch rather than trusted
/// from admission.
///
/// The three policies that guard a payout — the kill-switch, the cross-plane freeze and
/// the tier-1 floor — were all one-shot admission checks, and a queued withdrawal can sit
/// in the backlog for hours: an operator pause, an AML freeze or a revoked verification
/// that lands after acceptance must still stop the money. Every arm fails **closed**,
/// because by this point the gross is already reserved and the next step is a broadcast:
/// a missing owner row, an unreadable flag and a corrupt tier all refuse.
async fn require_dispatchable(policy: &dyn OutflowPolicy, gate: KycGate, withdrawal: &Withdrawal) -> Result<(), DomainError> {
	require_outflows_enabled(policy).await?;
	require_unfrozen_rail(policy, withdrawal.network()).await?;
	// A treasury payout has no owner to freeze or verify: the owners' consilium approved
	// it. The pause and the rail freeze above still hold it.
	let Some(owner) = withdrawal.user() else {
		return Ok(());
	};
	let standing = policy
		.standing(owner)
		.await?
		.ok_or_else(|| DomainError::Precondition("the withdrawal's owner has no control-plane row — dispatch refused".into()))?;
	if standing.blocked {
		return Err(DomainError::Precondition(ACCOUNT_FROZEN.into()));
	}
	if !gate.admits(standing.kyc_level) {
		return Err(DomainError::Forbidden("identity verification required to withdraw".into()));
	}
	Ok(())
}

/// Dispatch a queued withdrawal to custody (the dispatcher worker / admin): the chosen
/// rail now has liquidity, so the relay broadcasts. Refused — left queued, still
/// user-cancellable — when the outflow policy no longer permits the payout (see
/// [`require_dispatchable`]) or when the rail treasury provably lacks the net on-chain (a
/// dispatch would only park at the custody backstop). `None`/`Err` from the chain view
/// reads dispatch as before: the operator RPC is backed by human judgment, and stub rails
/// stay operator-settled. Idempotent.
///
/// This is where the policy lives *because* this is the single funnel every payout passes
/// through — the sweep and the admin RPC both call it, so neither can inherit a weaker
/// rule than the other.
pub async fn dispatch_withdrawal(
	withdrawals: &dyn WithdrawalRepository,
	custody: &dyn Custody,
	policy: &dyn OutflowPolicy,
	gate: KycGate,
	relay: &Notify,
	id: WithdrawalId,
) -> Result<Withdrawal, DomainError> {
	let existing = withdrawals.find_by_id(id).await?.ok_or_else(|| DomainError::NotFound {
		entity: "withdrawal",
		id: id.to_string(),
	})?;
	require_dispatchable(policy, gate, &existing).await?;
	if let Ok(Some(onchain)) = custody.treasury_liquidity(existing.network()).await
		&& onchain < existing.net_amount()
	{
		return Err(DomainError::Validation("rail treasury underfunded on-chain — withdrawal left queued".into()));
	}
	let withdrawal = withdrawals.dispatch(id).await?;
	relay.notify_one();
	Ok(withdrawal)
}

/// Cancel a still-queued withdrawal (the calling user): voids the reservation,
/// refunding in full. Ownership is checked here; the aggregate refuses to cancel once
/// the withdrawal is processing (a broadcast may have landed).
pub async fn cancel_withdrawal(withdrawals: &dyn WithdrawalRepository, relay: &Notify, id: WithdrawalId, user: UserId) -> Result<Withdrawal, DomainError> {
	let existing = withdrawals.find_by_id(id).await?.ok_or_else(|| DomainError::NotFound {
		entity: "withdrawal",
		id: id.to_string(),
	})?;
	if existing.user() != Some(user) {
		return Err(DomainError::Forbidden("not your withdrawal".into()));
	}
	let withdrawal = withdrawals.cancel(id).await?;
	relay.notify_one();
	Ok(withdrawal)
}

/// Settle a confirmed withdrawal (operator/watcher): records the chain `tx_ref` and
/// posts the reservation, moving the net out of custody. Idempotent.
pub async fn settle_withdrawal(withdrawals: &dyn WithdrawalRepository, relay: &Notify, id: WithdrawalId, tx_ref: TxRef) -> Result<Withdrawal, DomainError> {
	let withdrawal = withdrawals.settle(id, tx_ref).await?;
	relay.notify_one();
	Ok(withdrawal)
}

/// Fail a processing withdrawal (operator/watcher): voids the reservation, refunding
/// the user. Only safe when the broadcast certainly did not reach the chain.
pub async fn fail_withdrawal(withdrawals: &dyn WithdrawalRepository, relay: &Notify, id: WithdrawalId) -> Result<Withdrawal, DomainError> {
	let withdrawal = withdrawals.fail(id).await?;
	relay.notify_one();
	Ok(withdrawal)
}

/// The calling user's withdrawals (projection), newest first.
pub async fn list_withdrawals(withdrawals: &dyn WithdrawalRepository, user: UserId) -> Result<Vec<Withdrawal>, DomainError> {
	withdrawals.list_by_user(user).await
}
