//! Scope ledger use cases — what a scope's own service may do with money in its scope
//! (`scope.v1.ScopeLedgerService`): open an account, read its accounts' balances, and move
//! money from one of them into the scope's `fee` collector within the automatic ceilings.
//!
//! The caller is always a [`ClientId`] the transport proved (mTLS), and every call is first
//! held against banking's [`Grants`]: `main` is never a client's, a scope the client holds no
//! grant in is refused before banking says whether the scope exists, and only then is an
//! unknown scope NOT_FOUND. A mutating call then answers its idempotency key: a repeat with the
//! same terms gets the original result whatever happened since, other terms are a conflict.

use async_trait::async_trait;
use domain::{
	balance::LedgerAccountKey,
	error::DomainError,
	money::Usdt,
	scope_ledger::{AutoTransferFacts, ClientId, Grants, LedgerRefusal, ScopeTransfer, ScopeTransferId, SubjectRef, TransferReason, TransferTerms, canonical},
	scopes::{ScopeAccountLabel, ScopeId, ScopeState},
};
use tokio::sync::Notify;
use uuid::Uuid;

pub use crate::ports::scope_ledger::TransferError;
use crate::ports::{
	compartments::{CompartmentStore, ScopeRecord},
	ledger::{Ledger, LockedReads},
	scope_ledger::{OwnedAccount, RPC_OPEN_ACCOUNT, RPC_TRANSFER, Recorded, RecordedTransfer, RequestKey, ScopeLedgerStore, StoredRequest, TransferAdmission, TransferFacts},
};

/// Longest idempotency key accepted, in bytes (`ledger.proto`).
const MAX_IDEMPOTENCY_KEY: usize = 128;

/// The driven ports and the configuration the scope ledger use cases borrow.
pub struct ScopeLedgerPorts<'a> {
	/// Who may do what where — banking's configuration, loaded and checked at boot.
	pub grants: &'a Grants,
	pub store: &'a dyn ScopeLedgerStore,
	/// The scope registry (which scopes exist, their state and policy).
	pub scopes: &'a dyn CompartmentStore,
	/// Every balance is TigerBeetle's.
	pub ledger: &'a dyn Ledger,
	/// Nudged after a transfer commits so the relay posts it at once.
	pub relay: &'a Notify,
}

/// A scope account the caller owns, with its posted ledger balance.
#[derive(Clone, Debug)]
pub struct AccountView {
	pub account: OwnedAccount,
	pub balance: Usdt,
}

/// What a client asks of `Transfer`, parsed.
#[derive(Clone, Debug)]
pub struct TransferRequest {
	pub scope: ScopeId,
	pub from: ScopeAccountLabel,
	pub to: ScopeAccountLabel,
	pub amount: Usdt,
	pub reason: TransferReason,
	pub subject_ref: SubjectRef,
	pub memo: String,
	pub idempotency_key: String,
}

fn check_key(key: &str) -> Result<(), DomainError> {
	if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY {
		return Err(DomainError::Validation(format!("idempotency_key must be 1..{MAX_IDEMPOTENCY_KEY} bytes")));
	}
	Ok(())
}

/// The gate every call passes first: `main` is banking's, a scope the client holds no grant in
/// is not its business — refused alike, so a client cannot probe which scopes exist — and a
/// granted scope banking has not heard of from concierge is NOT_FOUND.
async fn granted_scope(ports: &ScopeLedgerPorts<'_>, client: &ClientId, scope: &ScopeId) -> Result<ScopeRecord, TransferError> {
	if scope.is_main() || ports.grants.in_scope(client, scope).next().is_none() {
		return Err(TransferError::Refused(LedgerRefusal::NotGranted));
	}
	Ok(ports.scopes.scope(scope).await?.ok_or_else(|| DomainError::NotFound {
		entity: "scope",
		id: scope.to_string(),
	})?)
}

/// What an earlier call under the same key did, when it asked for the same thing; a conflict
/// when it did not.
fn same_terms(stored: &StoredRequest, terms: &str) -> Result<(), DomainError> {
	if stored.terms == terms {
		Ok(())
	} else {
		Err(DomainError::Conflict("idempotency_key was used for a different request".into()))
	}
}

/// Open `(scope, label)` for the caller, or answer a repeat. A label the caller already owns
/// is its existing account; one another client owns is ALREADY_EXISTS.
pub async fn open_account(ports: &ScopeLedgerPorts<'_>, client: &ClientId, scope: &ScopeId, label: &ScopeAccountLabel, idempotency_key: &str) -> Result<AccountView, TransferError> {
	check_key(idempotency_key)?;
	granted_scope(ports, client, scope).await?;
	if !ports.grants.may_open(client, scope, label) {
		return Err(TransferError::Refused(LedgerRefusal::NotGranted));
	}
	let terms = canonical(&[label.as_str()]);
	let key = RequestKey {
		client,
		scope,
		rpc: RPC_OPEN_ACCOUNT,
		key: idempotency_key,
	};
	let account = match ports.store.open_account(key, label, &terms).await? {
		Recorded::New(account) => account,
		Recorded::Existing(stored) => {
			same_terms(&stored, &terms)?;
			owned(ports, client, scope, label)
				.await?
				.ok_or_else(|| DomainError::Repository(format!("key {idempotency_key} names account {label}, which the caller does not own")))?
		}
	};
	let balance = posted(ports.ledger, &account).await?;
	Ok(AccountView { account, balance })
}

async fn owned(ports: &ScopeLedgerPorts<'_>, client: &ClientId, scope: &ScopeId, label: &ScopeAccountLabel) -> Result<Option<OwnedAccount>, DomainError> {
	Ok(ports.store.accounts_of(client, scope).await?.into_iter().find(|account| &account.label == label))
}

fn ledger_key(account: &OwnedAccount) -> LedgerAccountKey {
	LedgerAccountKey::ScopeAccount(account.scope.clone(), account.label.clone())
}

async fn posted(ledger: &dyn Ledger, account: &OwnedAccount) -> Result<Usdt, DomainError> {
	Ok(Usdt::from_base_units(ledger.balance(&ledger_key(account)).await?.posted))
}

/// Balances of the caller's accounts in `scope`: every one it owns when `labels` is empty,
/// else exactly those asked, in that order. A label the caller does not own — someone else's,
/// banking's `fee`, or none at all — is the same NOT_FOUND.
pub async fn get_balances(ports: &ScopeLedgerPorts<'_>, client: &ClientId, scope: &ScopeId, labels: &[ScopeAccountLabel]) -> Result<Vec<AccountView>, TransferError> {
	granted_scope(ports, client, scope).await?;
	let owned = ports.store.accounts_of(client, scope).await?;
	let accounts: Vec<OwnedAccount> = if labels.is_empty() {
		owned
	} else {
		labels
			.iter()
			.map(|label| {
				owned.iter().find(|account| &account.label == label).cloned().ok_or_else(|| DomainError::NotFound {
					entity: "scope account",
					id: format!("{scope}:{label}"),
				})
			})
			.collect::<Result<_, _>>()?
	};
	let keys: Vec<LedgerAccountKey> = accounts.iter().map(ledger_key).collect();
	let balances = ports.ledger.balances(&keys).await.map_err(DomainError::from)?;
	if balances.len() != keys.len() {
		return Err(DomainError::Repository(format!("asked the ledger for {} balances, got {}", keys.len(), balances.len())).into());
	}
	Ok(accounts
		.into_iter()
		.zip(balances)
		.map(|(account, balance)| AccountView {
			account,
			balance: Usdt::from_base_units(balance.posted),
		})
		.collect())
}

/// Move money from an account the caller owns into the scope's `fee` collector, within the
/// automatic ceilings of the scope's policy for `(client, scope)`. Anything else — another
/// destination, an amount past a ceiling — is `approval_required`: the client asks with
/// `RequestTransfer`.
pub async fn transfer(ports: &ScopeLedgerPorts<'_>, client: &ClientId, request: TransferRequest) -> Result<RecordedTransfer, TransferError> {
	check_key(&request.idempotency_key)?;
	granted_scope(ports, client, &request.scope).await?;
	// By the label, before anything is read: no client debits banking's own accounts, whatever
	// a row or a grant says.
	if request.from.is_reserved() {
		return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
	}
	// The one automatic destination (`ledger.proto`, the fee route).
	if request.to != ScopeAccountLabel::fee() {
		return Err(TransferError::Refused(LedgerRefusal::ApprovalRequired));
	}
	let terms = TransferTerms {
		client: client.clone(),
		scope: request.scope.clone(),
		from: request.from,
		to: request.to,
		amount: request.amount,
		reason: request.reason,
		subject_ref: request.subject_ref,
		memo: request.memo,
	};
	let canonical_terms = terms.canonical();
	let mut transfer = ScopeTransfer::request(ScopeTransferId::new(), terms)?;
	let key = RequestKey {
		client,
		scope: &request.scope,
		rpc: RPC_TRANSFER,
		key: &request.idempotency_key,
	};
	// The store drains the transfer it records; the checks read an untouched copy.
	let asked = transfer.clone();
	let admission = Admission {
		ledger: ports.ledger,
		client,
		transfer: &asked,
	};
	match ports.store.record_transfer(&mut transfer, key, &canonical_terms, &admission).await? {
		Recorded::New(recorded) => {
			ports.relay.notify_one();
			Ok(recorded)
		}
		Recorded::Existing(stored) => {
			same_terms(&stored, &canonical_terms)?;
			let id = Uuid::parse_str(&stored.result_id).map_err(|_| DomainError::Repository(format!("key {} names transfer {:?}, not a uuid", request.idempotency_key, stored.result_id)))?;
			Ok(ports
				.store
				.transfer(id)
				.await?
				.ok_or_else(|| DomainError::Repository(format!("key {} names transfer {id}, which is gone", request.idempotency_key)))?)
		}
	}
}

/// The checks, run by the store while it holds the key, ceiling and account locks.
struct Admission<'a> {
	ledger: &'a dyn Ledger,
	client: &'a ClientId,
	transfer: &'a ScopeTransfer,
}

#[async_trait]
impl TransferAdmission for Admission<'_> {
	async fn admit(&self, facts: &TransferFacts, reads: &mut dyn LockedReads) -> Result<(), TransferError> {
		let transfer = self.transfer;
		let scope = facts.scope.as_ref().ok_or_else(|| DomainError::NotFound {
			entity: "scope",
			id: transfer.scope().to_string(),
		})?;
		if facts.source_owner.as_ref() != Some(self.client) {
			return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
		}
		// banking opens `(<scope>, fee)` when it learns the scope; a scope without it is
		// banking's fault, not the caller's, and nothing is moved into an account nobody owns.
		if !facts.destination_owner.as_ref().is_some_and(ClientId::is_banking) {
			return Err(DomainError::Repository(format!("scope {} has no fee collector owned by banking", transfer.scope())).into());
		}
		// A freeze stops new debits; a closed scope still lets its accounts be swept.
		match scope.state {
			ScopeState::Frozen => return Err(TransferError::Refused(LedgerRefusal::ScopeFrozen)),
			ScopeState::Active | ScopeState::Closed => {}
		}
		scope
			.policy
			.admit_auto_transfer(AutoTransferFacts {
				amount: transfer.amount(),
				transferred_24h: facts.transferred_24h,
			})
			.map_err(TransferError::Refused)?;
		let source = LedgerAccountKey::ScopeAccount(transfer.scope().clone(), transfer.from().clone());
		let spendable = self.ledger.spendable_locked(&source, reads).await.map_err(DomainError::from)?;
		if Usdt::from_base_units(spendable) < transfer.amount() {
			return Err(TransferError::Refused(LedgerRefusal::InsufficientFunds));
		}
		Ok(())
	}
}
