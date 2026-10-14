//! Compartment use cases — list the caller's compartments, read a scope's policy, and move
//! the caller's own money between `main` and one scope (D9).
//!
//! `move_funds` is a command with the house Read-First: the idempotency key first (a
//! repeat returns the original move, whatever happened since), then the scope's state and
//! the account's freeze, then — for a move in — the scope's policy, then whether the source
//! compartment covers the amount right now. The move is recorded with its event on the
//! outbox and the relay posts it; the source claim's non-negative flag is the backstop for
//! a check that lost a race. No fee, no approval: it is the user's money on both sides.

use domain::{
	error::DomainError,
	money::Usdt,
	scopes::{Compartment, CompartmentMove, CompartmentMoveId, MoveInFacts, MoveRefusal, ScopeId},
	users::UserId,
};
use tokio::sync::Notify;

use crate::ports::{
	OutflowPolicy,
	compartments::{CompartmentStore, MoveOutcome, RecordedMove, ScopeRecord},
	ledger::{Ledger, LedgerBalance},
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

/// Why a move did not happen: a refusal the wire names by its reason, or anything else.
#[derive(Debug)]
pub enum MoveError {
	Refused(MoveRefusal),
	Domain(DomainError),
}

impl From<DomainError> for MoveError {
	fn from(err: DomainError) -> Self {
		Self::Domain(err)
	}
}

/// `main` first, then every scope banking knows with the caller's balance there (zero if
/// never entered). A closed scope is listed only while the caller still holds money in it
/// — the one reason left to see it is to move that money out.
pub async fn list_compartments(ports: &CompartmentPorts<'_>, user: UserId) -> Result<(Vec<CompartmentView>, u32), DomainError> {
	let kyc_level = kyc_level(ports.policy, user).await?;
	let main = view(ports.ledger, user, Compartment::Main, MAIN_DISPLAY_NAME.to_owned(), None).await?;
	let mut views = vec![main];
	// A handful of scopes, read one after another: bounded by construction.
	for scope in ports.store.scopes().await? {
		let compartment = Compartment::Scope(scope.id.clone());
		let entry = view(ports.ledger, user, compartment, scope.display_name.clone(), Some(scope)).await?;
		let closed = entry.scope.as_ref().is_some_and(|scope| scope.state == domain::scopes::ScopeState::Closed);
		if closed && entry.total.is_zero() {
			continue;
		}
		views.push(entry);
	}
	Ok((views, kyc_level))
}

/// One scope's policy. `main` has none, and an unknown scope is the same NOT_FOUND.
pub async fn get_scope_policy(ports: &CompartmentPorts<'_>, user: UserId, scope: &ScopeId) -> Result<PolicyView, DomainError> {
	let record = known_scope(ports.store, scope).await?;
	let kyc_level = kyc_level(ports.policy, user).await?;
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
	let mut movement = CompartmentMove::request(CompartmentMoveId::new(), user, from, to, amount)?;
	if let Some(original) = ports.store.find_move(user, idempotency_key).await? {
		return repeat(original, &movement);
	}

	let scope = known_scope(ports.store, movement.scope()).await?;
	let standing = ports.policy.standing(user).await?.ok_or_else(|| DomainError::NotFound {
		entity: "user",
		id: user.to_string(),
	})?;
	if standing.blocked {
		return Err(MoveError::Refused(MoveRefusal::UserFrozen));
	}
	if movement.is_move_in() {
		if let Some(refusal) = scope.state.refuses_money_in() {
			return Err(MoveError::Refused(refusal));
		}
		let compartment = ports.ledger.balance(&movement.to().claim_key(user)).await.map_err(DomainError::from)?;
		let facts = MoveInFacts {
			kyc_level: standing.kyc_level,
			amount,
			moved_in_24h: ports.store.moved_in_24h(user, movement.scope()).await?,
			// Inflows still pending count: the cap is on what the compartment will hold.
			balance: Usdt::from_base_units(compartment.posted.saturating_add(compartment.pending)),
		};
		scope.policy.admit_move_in(facts).map_err(MoveError::Refused)?;
	}
	let source = ports.ledger.balance(&movement.from().claim_key(user)).await.map_err(DomainError::from)?;
	if source.available() < amount.base_units() {
		return Err(MoveError::Refused(MoveRefusal::InsufficientFunds));
	}

	match ports.store.record_move(&mut movement, idempotency_key).await? {
		MoveOutcome::Recorded(recorded) => {
			ports.relay.notify_one();
			Ok(recorded)
		}
		// Another request with the same key landed between our lookup and our write.
		MoveOutcome::Existing(original) => repeat(original, &movement),
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

async fn view(ledger: &dyn Ledger, user: UserId, compartment: Compartment, display_name: String, scope: Option<ScopeRecord>) -> Result<CompartmentView, DomainError> {
	let balance: LedgerBalance = ledger.balance(&compartment.claim_key(user)).await?;
	Ok(CompartmentView {
		compartment,
		display_name,
		available: Usdt::from_base_units(balance.available()),
		held: Usdt::from_base_units(balance.locked.min(balance.posted)),
		total: Usdt::from_base_units(balance.posted),
		scope,
	})
}
