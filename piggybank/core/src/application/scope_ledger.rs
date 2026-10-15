//! Scope ledger use cases — what a scope's own service may do with money in its scope
//! (`scope.v1.ScopeLedgerService`): open an account, read its accounts' balances, move money
//! from one of them into the scope's `fee` collector within the automatic ceilings, hold a
//! user's compartment under their mandate and capture or release the hold — and pull the money
//! events it is party to (`scope.v1.MoneyEvents`).
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
	scope_ledger::{
		AutoTransferFacts, ClientId, Grants, Hold, HoldDestination, HoldError, HoldId, HoldTerms, LedgerRefusal, MandateOperation, ScopeTransfer, ScopeTransferId, SubjectRef,
		TransferReason, TransferTerms, canonical,
	},
	scopes::{ScopeAccountLabel, ScopeId, ScopeState},
	users::UserId,
};
use tokio::sync::Notify;
use uuid::Uuid;

pub use crate::ports::scope_ledger::TransferError;
use crate::ports::{
	compartments::{CompartmentStore, ScopeRecord},
	ledger::{Ledger, LockedReads},
	scope_ledger::{
		CaptureTarget, HoldAdmission, HoldChange, HoldChangeFacts, HoldFacts, HoldStore, MandateBook, MoneyEventRecord, MoneyEventStore, OwnedAccount, RPC_CAPTURE, RPC_OPEN_ACCOUNT,
		RPC_RESERVE, RPC_TRANSFER, RPC_VOID, Recorded, RecordedHold, RecordedTransfer, RequestKey, ScopeLedgerStore, StoredRequest, TransferAdmission, TransferFacts,
	},
};

/// Longest idempotency key accepted, in characters (`ledger.proto`: "1..128 chars").
const MAX_IDEMPOTENCY_KEY: usize = 128;

/// The driven ports and the configuration the scope ledger use cases borrow.
pub struct ScopeLedgerPorts<'a> {
	/// Who may do what where — banking's configuration, loaded and checked at boot.
	pub grants: &'a Grants,
	pub store: &'a dyn ScopeLedgerStore,
	pub holds: &'a dyn HoldStore,
	/// Users' consent to holds — refuses everything until mandates are granted
	/// ([`NoMandates`]).
	pub mandates: &'a dyn MandateBook,
	pub events: &'a dyn MoneyEventStore,
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
	let chars = key.chars().count();
	if chars == 0 || chars > MAX_IDEMPOTENCY_KEY {
		return Err(DomainError::Validation(format!("idempotency_key must be 1..{MAX_IDEMPOTENCY_KEY} characters")));
	}
	// The key is echoed into logs and compared byte for byte; a control character there is a
	// client bug, and two keys differing only by one would read alike to an operator.
	if key.chars().any(char::is_control) {
		return Err(DomainError::Validation("idempotency_key must not contain control characters".into()));
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
/// else each label once, in first-asked order (the service deduplicates). A label the caller
/// does not own — someone else's, banking's `fee`, or none at all — is the same NOT_FOUND.
/// Reading needs only the scope grant: an account whose prefix was taken away stays readable,
/// only its debit goes.
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
	// Owning the account is not enough: the label must still be inside one of the client's
	// grants as configured NOW, so taking a prefix away also takes away the debit of every
	// account opened under it (the money stays, readable, for an approved transfer). Checked
	// for a NEW transfer only — in the admission — so a repeat of a key used before the
	// prefix went still gets its original result, as the contract promises.
	let admission = Admission {
		ledger: ports.ledger,
		client,
		transfer: &asked,
		label_granted: ports.grants.may_open(client, &request.scope, asked.from()),
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
	/// Whether a current grant of the client covers the source's label.
	label_granted: bool,
}

#[async_trait]
impl TransferAdmission for Admission<'_> {
	async fn admit(&self, facts: &TransferFacts, reads: &mut dyn LockedReads) -> Result<(), TransferError> {
		let transfer = self.transfer;
		let scope = facts.scope.as_ref().ok_or_else(|| DomainError::NotFound {
			entity: "scope",
			id: transfer.scope().to_string(),
		})?;
		if !self.label_granted {
			return Err(TransferError::Refused(LedgerRefusal::NotGranted));
		}
		if facts.source_owner.as_ref() != Some(self.client) {
			return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
		}
		// banking opens `(<scope>, fee)` when it learns the scope; a scope without it is
		// banking's fault, not the caller's, and nothing is moved into an account nobody owns.
		if !facts.destination_owner.as_ref().is_some_and(ClientId::is_banking) {
			return Err(TransferError::Internal(format!("scope {} has no fee collector owned by banking", transfer.scope())));
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

/// The mandate book while users cannot grant mandates yet (`GrantMandate` is a later part):
/// no user has consented to anything, so every hold is `mandate_required`. Wiring anything
/// else in production would let a client debit users on its own word.
pub struct NoMandates;

#[async_trait]
impl MandateBook for NoMandates {
	async fn authorize(&self, _: &ClientId, _: &ScopeId, _: UserId, _: MandateOperation, _: Usdt) -> Result<Option<Uuid>, DomainError> {
		Ok(None)
	}

	async fn lists(&self, _: Uuid, _: MandateOperation) -> Result<bool, DomainError> {
		Ok(false)
	}
}

/// What a client asks of `Reserve`, parsed.
#[derive(Clone, Debug)]
pub struct ReserveRequest {
	pub scope: ScopeId,
	/// The user, as the identity plane names them.
	pub auth_subject: String,
	pub amount: Usdt,
	/// 0 = no expiry.
	pub ttl_seconds: u32,
	pub subject_ref: SubjectRef,
	pub idempotency_key: String,
}

/// Longest `auth_subject` accepted, in bytes — what `users.auth_subject` was ever given.
const MAX_AUTH_SUBJECT: usize = 255;

fn check_subject(raw: &str) -> Result<(), DomainError> {
	if raw.is_empty() || raw.len() > MAX_AUTH_SUBJECT || raw.chars().any(char::is_control) {
		return Err(DomainError::Validation(format!("auth_subject must be 1..{MAX_AUTH_SUBJECT} bytes of plain text")));
	}
	Ok(())
}

fn refused(err: HoldError) -> TransferError {
	match err {
		HoldError::Refused(refusal) => TransferError::Refused(refusal),
		HoldError::Invalid(err) => TransferError::Domain(err),
	}
}

/// The hold an earlier call under the same key recorded, when it asked for the same thing.
async fn repeated_hold(ports: &ScopeLedgerPorts<'_>, client: &ClientId, scope: &ScopeId, stored: &StoredRequest, terms: &str, key: &str) -> Result<RecordedHold, TransferError> {
	same_terms(stored, terms)?;
	let id = Uuid::parse_str(&stored.result_id).map_err(|_| DomainError::Repository(format!("key {key} names hold {:?}, not a uuid", stored.result_id)))?;
	Ok(ports
		.holds
		.hold(client, scope, id)
		.await?
		.ok_or_else(|| DomainError::Repository(format!("key {key} names hold {id}, which is gone")))?)
}

/// Hold `amount` of a user's compartment in the scope for the caller, under the user's
/// mandate. The compartments grant, the scope's and the user's freeze, the mandate and the
/// compartment's spendable are all decided under the compartment's write lock; a repeat of
/// the key gets the original hold whatever happened since.
pub async fn reserve(ports: &ScopeLedgerPorts<'_>, client: &ClientId, request: ReserveRequest) -> Result<RecordedHold, TransferError> {
	check_key(&request.idempotency_key)?;
	check_subject(&request.auth_subject)?;
	if request.amount.is_zero() {
		return Err(DomainError::Validation("amount must be positive".into()).into());
	}
	granted_scope(ports, client, &request.scope).await?;
	let terms = canonical(&[
		&request.auth_subject,
		&request.amount.base_units().to_string(),
		&request.ttl_seconds.to_string(),
		request.subject_ref.as_str(),
	]);
	let key = RequestKey {
		client,
		scope: &request.scope,
		rpc: RPC_RESERVE,
		key: &request.idempotency_key,
	};
	let admission = ReserveAdmission { ports, client, request: &request };
	match ports.holds.record_hold(key, &terms, &request.auth_subject, &admission).await? {
		Recorded::New(recorded) => {
			ports.relay.notify_one();
			Ok(recorded)
		}
		Recorded::Existing(stored) => repeated_hold(ports, client, &request.scope, &stored, &terms, &request.idempotency_key).await,
	}
}

struct ReserveAdmission<'a> {
	ports: &'a ScopeLedgerPorts<'a>,
	client: &'a ClientId,
	request: &'a ReserveRequest,
}

#[async_trait]
impl HoldAdmission for ReserveAdmission<'_> {
	async fn admit(&self, facts: &HoldFacts, reads: &mut dyn LockedReads) -> Result<Hold, TransferError> {
		let request = self.request;
		let scope = facts.scope.as_ref().ok_or_else(|| DomainError::NotFound {
			entity: "scope",
			id: request.scope.to_string(),
		})?;
		// Checked for a new hold only, like a transfer's label grant, so a repeat of a key used
		// before the flag was taken away still gets its original result.
		if !self.ports.grants.compartments(self.client, &request.scope) {
			return Err(TransferError::Refused(LedgerRefusal::NotGranted));
		}
		match scope.state {
			ScopeState::Frozen => return Err(TransferError::Refused(LedgerRefusal::ScopeFrozen)),
			ScopeState::Active | ScopeState::Closed => {}
		}
		// A user banking does not know has no mandate either, and the same answer keeps a
		// client from learning which subjects have a wallet.
		let user = facts.user.ok_or(TransferError::Refused(LedgerRefusal::MandateRequired))?;
		let mandate = self
			.ports
			.mandates
			.authorize(self.client, &request.scope, user.id, MandateOperation::Reserve, request.amount)
			.await?
			.ok_or(TransferError::Refused(LedgerRefusal::MandateRequired))?;
		// After the mandate: a client the user never consented to learns nothing of the freeze.
		if user.blocked {
			return Err(TransferError::Refused(LedgerRefusal::UserFrozen));
		}
		let compartment = LedgerAccountKey::ScopeClaim(user.id, request.scope.clone());
		let spendable = self.ports.ledger.spendable_locked(&compartment, reads).await.map_err(DomainError::from)?;
		if Usdt::from_base_units(spendable) < request.amount {
			return Err(TransferError::Refused(LedgerRefusal::InsufficientFunds));
		}
		let expires_at = match request.ttl_seconds {
			0 => None,
			ttl => Some(facts.now.saturating_add(i64::from(ttl))),
		};
		Ok(Hold::place(
			HoldId::new(),
			HoldTerms {
				client: self.client.clone(),
				scope: request.scope.clone(),
				user: user.id,
				amount: request.amount,
				expires_at,
				subject_ref: request.subject_ref.clone(),
				mandate_id: mandate,
			},
		)?)
	}
}

/// What a client asks of `Capture`, parsed.
#[derive(Clone, Debug)]
pub struct CaptureRequest {
	pub scope: ScopeId,
	pub hold_id: Uuid,
	pub to: CaptureTarget,
	/// `None` = everything still held.
	pub amount: Option<Usdt>,
	pub subject_ref: SubjectRef,
	pub idempotency_key: String,
}

/// Move (part of) a hold the caller placed to a scope account it owns, or — under a mandate
/// that lists BOOK_FILL — to another user's compartment in the scope.
pub async fn capture(ports: &ScopeLedgerPorts<'_>, client: &ClientId, request: CaptureRequest) -> Result<RecordedHold, TransferError> {
	check_key(&request.idempotency_key)?;
	if let CaptureTarget::User(subject) = &request.to {
		check_subject(subject)?;
	}
	granted_scope(ports, client, &request.scope).await?;
	let (target_kind, target) = match &request.to {
		CaptureTarget::Account(label) => ("account", label.as_str()),
		CaptureTarget::User(subject) => ("user", subject.as_str()),
	};
	let amount = request.amount.map(|amount| amount.base_units().to_string()).unwrap_or_default();
	let hold_id = request.hold_id.to_string();
	let terms = canonical(&[&hold_id, target_kind, target, &amount, request.subject_ref.as_str()]);
	let key = RequestKey {
		client,
		scope: &request.scope,
		rpc: RPC_CAPTURE,
		key: &request.idempotency_key,
	};
	let change = CaptureChange { ports, client, request: &request };
	match ports.holds.change_hold(key, &terms, request.hold_id, Some(&request.to), &change).await? {
		Recorded::New(recorded) => {
			ports.relay.notify_one();
			Ok(recorded)
		}
		Recorded::Existing(stored) => repeated_hold(ports, client, &request.scope, &stored, &terms, &request.idempotency_key).await,
	}
}

struct CaptureChange<'a> {
	ports: &'a ScopeLedgerPorts<'a>,
	client: &'a ClientId,
	request: &'a CaptureRequest,
}

#[async_trait]
impl HoldChange for CaptureChange<'_> {
	async fn apply(&self, hold: &mut Hold, facts: &HoldChangeFacts) -> Result<(), TransferError> {
		let request = self.request;
		let scope = facts.scope.as_ref().ok_or_else(|| DomainError::NotFound {
			entity: "scope",
			id: request.scope.to_string(),
		})?;
		let destination = match &request.to {
			// Only into an account the caller owns — never one of banking's (their owner is
			// banking), whatever the label.
			CaptureTarget::Account(label) => {
				if facts.destination_owner.as_ref() != Some(self.client) {
					return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
				}
				HoldDestination::Account(label.clone())
			}
			// The cash leg of a book fill: a debit of the held user towards someone else.
			CaptureTarget::User(_) => {
				if !self.ports.grants.compartments(self.client, &request.scope) {
					return Err(TransferError::Refused(LedgerRefusal::NotGranted));
				}
				match scope.state {
					ScopeState::Frozen => return Err(TransferError::Refused(LedgerRefusal::ScopeFrozen)),
					ScopeState::Active | ScopeState::Closed => {}
				}
				// Decided by the mandate the hold was placed under, for the hold's whole life.
				if !self.ports.mandates.lists(hold.terms().mandate_id, MandateOperation::BookFill).await? {
					return Err(TransferError::Refused(LedgerRefusal::MandateRequired));
				}
				let other = facts.destination_user.ok_or_else(|| DomainError::NotFound {
					entity: "user",
					id: target_subject(&request.to).to_owned(),
				})?;
				if facts.held_user.is_none_or(|held| held.blocked) {
					return Err(TransferError::Refused(LedgerRefusal::UserFrozen));
				}
				HoldDestination::User(other.id)
			}
		};
		hold.capture(request.amount, destination, request.subject_ref.clone(), facts.now).map_err(refused)?;
		Ok(())
	}
}

fn target_subject(target: &CaptureTarget) -> &str {
	match target {
		CaptureTarget::Account(label) => label.as_str(),
		CaptureTarget::User(subject) => subject,
	}
}

/// What a client asks of `Void`, parsed.
#[derive(Clone, Debug)]
pub struct VoidRequest {
	pub scope: ScopeId,
	pub hold_id: Uuid,
	pub subject_ref: SubjectRef,
	pub idempotency_key: String,
}

/// Release what a hold the caller placed still holds. One no longer active is returned as it
/// is; one past its expiry expires.
pub async fn void(ports: &ScopeLedgerPorts<'_>, client: &ClientId, request: VoidRequest) -> Result<RecordedHold, TransferError> {
	check_key(&request.idempotency_key)?;
	granted_scope(ports, client, &request.scope).await?;
	let terms = canonical(&[&request.hold_id.to_string(), request.subject_ref.as_str()]);
	let key = RequestKey {
		client,
		scope: &request.scope,
		rpc: RPC_VOID,
		key: &request.idempotency_key,
	};
	let change = VoidChange { subject_ref: &request.subject_ref };
	match ports.holds.change_hold(key, &terms, request.hold_id, None, &change).await? {
		Recorded::New(recorded) => {
			ports.relay.notify_one();
			Ok(recorded)
		}
		Recorded::Existing(stored) => repeated_hold(ports, client, &request.scope, &stored, &terms, &request.idempotency_key).await,
	}
}

struct VoidChange<'a> {
	subject_ref: &'a SubjectRef,
}

#[async_trait]
impl HoldChange for VoidChange<'_> {
	async fn apply(&self, hold: &mut Hold, facts: &HoldChangeFacts) -> Result<(), TransferError> {
		// A release is a credit back to the user: a frozen scope or user does not stop it.
		hold.void(self.subject_ref.clone(), facts.now);
		Ok(())
	}
}

/// One hold the caller placed in `scope`; anyone else's is the same NOT_FOUND as none.
pub async fn get_hold(ports: &ScopeLedgerPorts<'_>, client: &ClientId, scope: &ScopeId, hold_id: Uuid) -> Result<RecordedHold, TransferError> {
	granted_scope(ports, client, scope).await?;
	Ok(ports.holds.hold(client, scope, hold_id).await?.ok_or_else(|| DomainError::NotFound {
		entity: "hold",
		id: hold_id.to_string(),
	})?)
}

/// Events per `Pull` when the client asks for none in particular, and the most it may ask.
pub const DEFAULT_PULL_LIMIT: u32 = 100;
pub const MAX_PULL_LIMIT: u32 = 500;

/// A page of the caller's money events and the cursor after it.
#[derive(Debug)]
pub struct PulledEvents {
	pub events: Vec<MoneyEventRecord>,
	pub next_position: i64,
}

/// The money events delivered to the caller past `after`, in `scope` or — when `None` — in
/// every scope it holds a grant in now. A scope outside its grants, `main` included, is
/// refused; a granted scope banking does not know simply has no events. Balances of accounts
/// the caller does not own are dropped from each event.
pub async fn pull(ports: &ScopeLedgerPorts<'_>, client: &ClientId, scope: Option<&ScopeId>, after: i64, limit: u32) -> Result<PulledEvents, TransferError> {
	if after < 0 {
		return Err(DomainError::Validation("after_position must not be negative".into()).into());
	}
	let scopes = match scope {
		Some(scope) if scope.is_main() || ports.grants.in_scope(client, scope).next().is_none() => return Err(TransferError::Refused(LedgerRefusal::NotGranted)),
		Some(scope) => vec![scope.clone()],
		None => ports.grants.scopes_of(client),
	};
	if scopes.is_empty() {
		return Ok(PulledEvents {
			events: Vec::new(),
			next_position: after,
		});
	}
	let limit = match limit {
		0 => DEFAULT_PULL_LIMIT,
		asked => asked.min(MAX_PULL_LIMIT),
	};
	let mut events = ports.events.pull(client, &scopes, after, limit).await?;
	for event in &mut events {
		event.balances.retain(|entry| &entry.owner == client);
	}
	let next_position = events.last().map_or(after, |event| event.position);
	Ok(PulledEvents { events, next_position })
}
