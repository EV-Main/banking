//! The `Charge` use case — a scope's service taking a one-off amount from a user's compartment
//! into an account it owns, on the user's mandate for it (`scope.v1.ScopeLedgerService.Charge`).
//!
//! Every call first passes the scope ledger's gate ([`granted_scope`]), then answers its
//! idempotency key: a repeat with the same terms gets the original charge whatever happened
//! since, other terms are a conflict. A new charge is decided by the store under the key's lock,
//! the compartment's write lock and the mandate row's lock — the grant, the credited account's
//! owner, the scope's and the user's freeze, the mandate's caps and the compartment's spendable
//! — and is recorded `pending`; the posted transfer is the relay's, and only a posted charge is
//! announced (`KIND_CHARGED`).

use async_trait::async_trait;
use domain::{
	balance::LedgerAccountKey,
	error::DomainError,
	money::Usdt,
	scope_charges::{ChargeId, ChargeTerms, ScopeCharge, canonical_request},
	scope_ledger::{ClientId, LedgerRefusal, MandateOperation, SubjectRef, validate_memo},
	scopes::{ScopeAccountLabel, ScopeId, ScopeState},
};
use uuid::Uuid;

use crate::{
	application::scope_ledger::{ScopeLedgerPorts, check_key, check_subject, granted_scope, same_terms},
	ports::{
		ledger::LockedReads,
		scope_charges::{ChargeAdmission, ChargeFacts, RPC_CHARGE, RecordedCharge},
		scope_ledger::{Recorded, RequestKey, TransferError},
	},
};

/// The smallest charge: 0.000001 USDT (10^12 of the ledger's 18-dp base units).
const MIN_CHARGE: Usdt = Usdt::from_base_units(1_000_000_000_000);

/// What a client asks of `Charge`, parsed.
#[derive(Clone, Debug)]
pub struct ChargeRequest {
	pub scope: ScopeId,
	/// The user, as the identity plane names them.
	pub auth_subject: String,
	pub amount: Usdt,
	/// The scope account credited; the caller must own it.
	pub to: ScopeAccountLabel,
	pub memo: String,
	pub subject_ref: SubjectRef,
	pub idempotency_key: String,
}

/// Charge `amount` of a user's compartment in the scope into `to`, under the user's mandate
/// for the caller (operation CHARGE).
pub async fn charge(ports: &ScopeLedgerPorts<'_>, client: &ClientId, request: ChargeRequest) -> Result<RecordedCharge, TransferError> {
	check_key(&request.idempotency_key)?;
	check_subject(&request.auth_subject)?;
	validate_memo(&request.memo)?;
	if request.amount.is_zero() {
		return Err(DomainError::Validation("amount must be positive".into()).into());
	}
	// Below this a charge is dust: it costs a ledger transfer, a row and an event for nothing
	// a user could see in their history.
	if request.amount < MIN_CHARGE {
		return Err(DomainError::Validation("amount must be at least 0.000001".into()).into());
	}
	granted_scope(ports, client, &request.scope).await?;
	// By the label, before anything is read: no client credits banking's own accounts, whatever
	// a row or a grant says.
	if request.to.is_reserved() {
		return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
	}
	let terms = canonical_request(&request.auth_subject, &request.to, request.amount, &request.subject_ref, &request.memo);
	let key = RequestKey {
		client,
		scope: &request.scope,
		rpc: RPC_CHARGE,
		key: &request.idempotency_key,
	};
	let admission = ChargeAdmitter { ports, client, request: &request };
	match ports.store.record_charge(key, &terms, &request.auth_subject, &request.to, &admission).await? {
		Recorded::New(recorded) => {
			ports.relay.notify_one();
			Ok(recorded)
		}
		Recorded::Existing(stored) => {
			same_terms(&stored, &terms)?;
			let id = Uuid::parse_str(&stored.result_id).map_err(|_| DomainError::Repository(format!("key {} names charge {:?}, not a uuid", request.idempotency_key, stored.result_id)))?;
			Ok(ports
				.store
				.charge(client, &request.scope, id)
				.await?
				.ok_or_else(|| DomainError::Repository(format!("key {} names charge {id}, which is gone", request.idempotency_key)))?)
		}
	}
}

struct ChargeAdmitter<'a> {
	ports: &'a ScopeLedgerPorts<'a>,
	client: &'a ClientId,
	request: &'a ChargeRequest,
}

#[async_trait]
impl ChargeAdmission for ChargeAdmitter<'_> {
	async fn admit(&self, facts: &ChargeFacts, reads: &mut dyn LockedReads) -> Result<ScopeCharge, TransferError> {
		let request = self.request;
		let scope = facts.scope.as_ref().ok_or_else(|| DomainError::NotFound {
			entity: "scope",
			id: request.scope.to_string(),
		})?;
		// Checked for a new charge only, so a repeat of a key used before the flag was taken
		// away still gets its original result.
		if !self.ports.grants.compartments(self.client, &request.scope) {
			return Err(TransferError::Refused(LedgerRefusal::NotGranted));
		}
		// Banking's own accounts are owned by banking, so this also keeps every charge out of
		// `fee` and `treasury` should the label check upstream ever be bypassed.
		if facts.destination_owner.as_ref() != Some(self.client) {
			return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
		}
		// A charge is money into the scope's account: a frozen scope takes no new debit, and a
		// closed one takes nothing in (as an invoice payment into it).
		match scope.state {
			ScopeState::Active => {}
			ScopeState::Frozen => return Err(TransferError::Refused(LedgerRefusal::ScopeFrozen)),
			ScopeState::Closed => return Err(TransferError::Refused(LedgerRefusal::ScopeClosed)),
		}
		// A user banking does not know has no mandate either, and the same answer keeps a
		// client from learning which subjects have a wallet.
		let user = facts.user.ok_or(TransferError::Refused(LedgerRefusal::MandateRequired))?;
		let draw = facts.mandate.as_ref().ok_or(TransferError::Refused(LedgerRefusal::MandateRequired))?;
		// The mandate row and the compartment are both locked, so two charges racing for the
		// last of a cap cannot both see room for themselves.
		draw.mandate
			.admit_draw(MandateOperation::Charge, request.amount, draw.usage, facts.now)
			.map_err(TransferError::Refused)?;
		// After the mandate: a client the user never consented to learns nothing of the freeze.
		if user.blocked {
			return Err(TransferError::Refused(LedgerRefusal::UserFrozen));
		}
		let compartment = LedgerAccountKey::ScopeClaim(user.id, request.scope.clone());
		let spendable = self.ports.ledger.spendable_locked(&compartment, reads).await.map_err(DomainError::from)?;
		if Usdt::from_base_units(spendable) < request.amount {
			return Err(TransferError::Refused(LedgerRefusal::InsufficientFunds));
		}
		Ok(ScopeCharge::request(
			ChargeId::new(),
			ChargeTerms {
				client: self.client.clone(),
				scope: request.scope.clone(),
				user: user.id,
				to: request.to.clone(),
				amount: request.amount,
				memo: request.memo.clone(),
				subject_ref: request.subject_ref.clone(),
				mandate_id: draw.mandate.id,
			},
		)?)
	}
}
