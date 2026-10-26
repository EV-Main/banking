//! Compartment use cases — list the caller's compartments, read a scope's policy, and move
//! the caller's own money between `main` and one scope (D9).
//!
//! `move_funds` answers the idempotency key first (a repeat returns the original move,
//! whatever happened since). A move IN then has to pass the scope's state, the account's
//! freeze and the policy; a move OUT is never limited — the money does not leave the
//! platform, and a frozen user or a paused platform still lets it back into `main`. Every
//! check — the scope's state, the freeze, the policy, what the source may spend — runs under
//! the store's key and claim locks, read on the connection that holds them, against figures
//! net of everything recorded against the source and not yet on the ledger (the source's
//! [`Ledger::spendable_locked`], the same answer a withdrawal or a payment gets), so
//! the loser of two racing spends is refused at once instead of failing in the relay. The
//! relay posts the move and stamps it `posted`; a ledger refusal stamps it `failed`, for good.

use async_trait::async_trait;
use domain::{
	error::DomainError,
	money::Usdt,
	scopes::{Compartment, CompartmentMove, CompartmentMoveId, MIN_MOVE, MoveInFacts, MoveRefusal, ScopeId, ScopeState},
	users::UserId,
};
use tokio::sync::Notify;

pub use crate::ports::compartments::MoveError;
use crate::ports::{
	OutflowPolicy,
	compartments::{CompartmentStore, InFlight, MoveAdmission, MoveOutcome, RecordedMove, ScopeRecord},
	ledger::{Ledger, LedgerBalance, LockedReads},
};

/// The display name of the root compartment; concierge announces every other scope's.
pub const MAIN_DISPLAY_NAME: &str = "Main";

/// Longest idempotency key accepted, in bytes (`wallet.proto`, `ledger.proto`).
const MAX_IDEMPOTENCY_KEY: usize = 128;

/// The driven ports the compartment use cases borrow.
pub struct CompartmentPorts<'a> {
	pub store: &'a dyn CompartmentStore,
	/// Every balance is TigerBeetle's.
	pub ledger: &'a dyn Ledger,
	/// The caller's freeze and mirrored KYC level — the same read the withdrawal gates use.
	pub policy: &'a dyn OutflowPolicy,
	/// Nudged after a move commits so the relay posts it at once.
	pub relay: &'a Notify,
}

/// One compartment of the caller. `total = available + held`.
#[derive(Clone, Debug)]
pub struct CompartmentView {
	pub compartment: Compartment,
	pub display_name: String,
	pub available: Usdt,
	/// Reserved against the compartment and not yet settled.
	pub held: Usdt,
	pub total: Usdt,
	/// The scope with its policy; `None` for `main`.
	pub scope: Option<ScopeRecord>,
}

/// A scope's policy, and whether the caller meets its KYC floor right now.
#[derive(Clone, Debug)]
pub struct PolicyView {
	pub scope: ScopeRecord,
	pub caller_eligible: bool,
}

/// `main` first, then every scope banking knows with the caller's balance there (zero if
/// never entered). A closed scope is listed only while the caller still holds money in it
/// — the one reason left to see it is to move that money out.
pub async fn list_compartments(ports: &CompartmentPorts<'_>, user: UserId) -> Result<(Vec<CompartmentView>, u32), DomainError> {
	let (kyc_level, read) = tokio::try_join!(kyc_level(ports.policy, user), read_compartments(ports.store, ports.ledger, user))?;
	let mut views = vec![view(Compartment::Main, MAIN_DISPLAY_NAME.to_owned(), read.main, None)];
	for (scope, balance) in read.scopes.into_iter().zip(read.compartments) {
		if scope.state == ScopeState::Closed && balance.posted == 0 {
			continue;
		}
		views.push(view(Compartment::Scope(scope.id.clone()), scope.display_name.clone(), balance, Some(scope)));
	}
	Ok((views, kyc_level))
}

/// The caller's balance in one compartment. A scope banking does not know — or a closed one
/// the caller holds nothing in, which `list_compartments` hides — is `NotFound`.
pub async fn get_compartment(ports: &CompartmentPorts<'_>, user: UserId, scope: &Compartment) -> Result<(CompartmentView, u32), DomainError> {
	let (views, kyc_level) = list_compartments(ports, user).await?;
	views
		.into_iter()
		.find(|view| view.compartment == *scope)
		.map(|view| (view, kyc_level))
		.ok_or_else(|| DomainError::NotFound {
			entity: "compartment",
			id: scope.as_str().to_owned(),
		})
}

/// What the caller holds in `main` and, summed, in every scope compartment — the split the
/// wallet and the balance RPCs add up. Every scope banking knows is summed, closed ones
/// included: money there is still the user's.
#[derive(Clone, Copy, Debug, Default)]
pub struct Holdings {
	pub main: LedgerBalanceTotals,
	pub compartments: LedgerBalanceTotals,
}

/// A sum of ledger balances, on each account's natural side.
#[derive(Clone, Copy, Debug, Default)]
pub struct LedgerBalanceTotals {
	pub posted: Usdt,
	pub pending: Usdt,
	pub locked: Usdt,
}

impl LedgerBalanceTotals {
	fn add(self, balance: &LedgerBalance) -> Result<Self, DomainError> {
		let sum = |a: Usdt, b: u128| {
			a.checked_add(Usdt::from_base_units(b))
				.ok_or_else(|| DomainError::Repository("compartment totals overflow".into()))
		};
		Ok(Self {
			posted: sum(self.posted, balance.posted)?,
			pending: sum(self.pending, balance.pending)?,
			locked: sum(self.locked, balance.locked)?,
		})
	}

	/// Settled and not reserved by an in-flight spend.
	pub fn available(self) -> Usdt {
		self.posted.checked_sub(self.locked).unwrap_or(Usdt::ZERO)
	}
}

/// The caller's `main` claim and the sum of their compartments, read in one ledger lookup.
pub async fn holdings(store: &dyn CompartmentStore, ledger: &dyn Ledger, user: UserId) -> Result<Holdings, DomainError> {
	let read = read_compartments(store, ledger, user).await?;
	Ok(Holdings {
		main: LedgerBalanceTotals::default().add(&read.main)?,
		compartments: read.compartments.iter().try_fold(LedgerBalanceTotals::default(), LedgerBalanceTotals::add)?,
	})
}

/// Every scope banking knows, with the caller's `main` balance and one balance per scope in
/// the same order — the one read behind the listing and the totals.
struct CompartmentRead {
	scopes: Vec<ScopeRecord>,
	main: LedgerBalance,
	compartments: Vec<LedgerBalance>,
}

async fn read_compartments(store: &dyn CompartmentStore, ledger: &dyn Ledger, user: UserId) -> Result<CompartmentRead, DomainError> {
	let scopes = store.scopes().await?;
	let keys: Vec<_> = std::iter::once(Compartment::Main)
		.chain(scopes.iter().map(|scope| Compartment::Scope(scope.id.clone())))
		.map(|compartment| compartment.claim_key(user))
		.collect();
	let mut balances = ledger.balances(&keys).await?;
	if balances.len() != keys.len() {
		return Err(DomainError::Repository(format!("asked the ledger for {} balances, got {}", keys.len(), balances.len())));
	}
	let main = balances.remove(0);
	Ok(CompartmentRead {
		scopes,
		main,
		compartments: balances,
	})
}

/// One scope's policy. `main` has none, and an unknown scope is the same NOT_FOUND.
pub async fn get_scope_policy(ports: &CompartmentPorts<'_>, user: UserId, scope: &ScopeId) -> Result<PolicyView, DomainError> {
	let (record, kyc_level) = tokio::try_join!(known_scope(ports.store, scope), kyc_level(ports.policy, user))?;
	Ok(PolicyView {
		caller_eligible: record.policy.admits_kyc(kyc_level),
		scope: record,
	})
}

/// Move `amount` of the caller's money from `from` to `to`, exactly one of them `main`.
pub async fn move_funds(ports: &CompartmentPorts<'_>, user: UserId, from: Compartment, to: Compartment, amount: Usdt, idempotency_key: &str) -> Result<RecordedMove, MoveError> {
	if idempotency_key.is_empty() || idempotency_key.len() > MAX_IDEMPOTENCY_KEY {
		return Err(DomainError::Validation(format!("idempotency_key must be 1..{MAX_IDEMPOTENCY_KEY} bytes")).into());
	}
	if amount < MIN_MOVE {
		return Err(MoveError::Refused(MoveRefusal::AmountBelowMinimum));
	}
	let mut movement = CompartmentMove::request(CompartmentMoveId::new(), user, from, to, amount)?;
	if let Some(original) = ports.store.find_move(user, idempotency_key).await? {
		return repeat(original, &movement);
	}
	// Unknown is answered here; everything that can change — the scope's state and policy,
	// the account's freeze — is read again under the locks, in `admit`.
	known_scope(ports.store, movement.scope()).await?;
	let admission = Admission { ports, movement: movement.clone() };
	match ports.store.record_move(&mut movement, idempotency_key, &admission).await? {
		MoveOutcome::Recorded(recorded) => {
			ports.relay.notify_one();
			Ok(recorded)
		}
		// Another request with the same key landed between our lookup and our lock.
		MoveOutcome::Existing(original) => repeat(original, &movement),
	}
}

/// The checks, run by the store while it holds the key and claim locks.
struct Admission<'a> {
	ports: &'a CompartmentPorts<'a>,
	movement: CompartmentMove,
}

#[async_trait]
impl MoveAdmission for Admission<'_> {
	async fn admit(&self, in_flight: &InFlight, reads: &mut dyn LockedReads) -> Result<(), MoveError> {
		let (ledger, user, amount) = (self.ports.ledger, self.movement.user(), self.movement.amount());
		if self.movement.is_move_in() {
			// A freeze or a scope state that changed since the request arrived counts.
			let scope = in_flight.scope.as_ref().ok_or_else(|| DomainError::NotFound {
				entity: "scope",
				id: self.movement.scope().to_string(),
			})?;
			if let Some(refusal) = scope.state.refuses_money_in() {
				return Err(MoveError::Refused(refusal));
			}
			let standing = in_flight.standing.ok_or_else(|| DomainError::NotFound {
				entity: "user",
				id: user.to_string(),
			})?;
			if standing.blocked {
				return Err(MoveError::Refused(MoveRefusal::UserFrozen));
			}
			self.admit_source(reads, amount).await?;
			let target = ledger.balance_locked(&self.movement.to().claim_key(user), reads).await.map_err(DomainError::from)?;
			// Inflows still pending count, on the ledger and in the control plane alike: the
			// cap is on what the compartment will hold.
			let balance = Usdt::from_base_units(target.posted.saturating_add(target.pending))
				.checked_add(in_flight.pending_into_target)
				.ok_or_else(|| DomainError::Repository("compartment balance overflows".into()))?;
			return scope
				.policy
				.admit_move_in(MoveInFacts {
					kyc_level: standing.kyc_level,
					amount,
					moved_in_24h: in_flight.moved_in_24h,
					balance,
				})
				.map_err(MoveError::Refused);
		}
		self.admit_source(reads, amount).await
	}
}

impl Admission<'_> {
	/// The source covers the amount — the ledger's spendable, net of everything recorded
	/// against the source and not yet applied, the user's other moves included.
	async fn admit_source(&self, reads: &mut dyn LockedReads, amount: Usdt) -> Result<(), MoveError> {
		let source = self.movement.from().claim_key(self.movement.user());
		let spendable = self.ports.ledger.spendable_locked(&source, reads).await.map_err(DomainError::from)?;
		if Usdt::from_base_units(spendable) < amount {
			return Err(MoveError::Refused(MoveRefusal::InsufficientFunds));
		}
		Ok(())
	}
}

/// A key the caller already used: the original move when the terms match, a conflict when
/// they do not.
fn repeat(original: RecordedMove, asked: &CompartmentMove) -> Result<RecordedMove, MoveError> {
	if original.movement.same_terms(asked.from(), asked.to(), asked.amount()) {
		Ok(original)
	} else {
		Err(DomainError::Conflict("idempotency_key was used for a different move".into()).into())
	}
}

async fn known_scope(store: &dyn CompartmentStore, scope: &ScopeId) -> Result<ScopeRecord, DomainError> {
	store.scope(scope).await?.ok_or_else(|| DomainError::NotFound {
		entity: "scope",
		id: scope.to_string(),
	})
}

/// The caller's mirrored KYC level; a user with no local row yet holds level 0.
async fn kyc_level(policy: &dyn OutflowPolicy, user: UserId) -> Result<u32, DomainError> {
	Ok(policy.standing(user).await?.map_or(0, |standing| standing.kyc_level))
}

fn view(compartment: Compartment, display_name: String, balance: LedgerBalance, scope: Option<ScopeRecord>) -> CompartmentView {
	CompartmentView {
		compartment,
		display_name,
		available: Usdt::from_base_units(balance.available()),
		held: Usdt::from_base_units(balance.locked.min(balance.posted)),
		total: Usdt::from_base_units(balance.posted),
		scope,
	}
}
