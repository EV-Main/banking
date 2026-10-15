//! Invoices' control plane — who issued which invoice under which idempotency key, where it
//! stands, and who paid it from where. The payment's money is TigerBeetle's, posted by the relay
//! from the outbox event written with the paid row.

use async_trait::async_trait;
use domain::{
	error::DomainError,
	scope_invoices::{Invoice, InvoiceRefusal},
	scope_ledger::ClientId,
	scopes::{Compartment, ScopeAccountLabel, ScopeId},
	users::UserId,
};
use uuid::Uuid;

use crate::ports::{
	compartments::ScopeRecord,
	ledger::LockedReads,
	outflow::PayoutStanding,
	scope_ledger::{Recorded, RequestKey, TransferError},
};

/// The RPC name `CreateInvoice` keys are unique under, as stored.
pub const RPC_CREATE_INVOICE: &str = "create_invoice";

/// An invoice as recorded.
#[derive(Clone, Debug)]
pub struct RecordedInvoice {
	pub invoice: Invoice,
	/// Unix seconds.
	pub created_at: i64,
	/// The database's clock when it was read, unix seconds — what a reader decides whether it
	/// is past its expiry by ([`Invoice::state_at`]).
	pub read_at: i64,
}

/// What the control plane says about a new invoice, read under the key's lock.
#[derive(Clone, Debug)]
pub struct IssueFacts {
	/// The scope as it stands now; `None` when banking does not know it.
	pub scope: Option<ScopeRecord>,
	/// Who owns the account the invoice pays into; `None` when it does not exist.
	pub destination_owner: Option<ClientId>,
	/// The database's clock, unix seconds: what the expiry is checked against.
	pub now: i64,
}

/// Decides a new invoice under the store's lock and returns it issued — or refuses.
#[async_trait]
pub trait InvoiceIssue: Send + Sync {
	async fn admit(&self, facts: &IssueFacts) -> Result<Invoice, TransferError>;
}

/// Why a payment did not happen: a refusal the wire names by its reason, or anything else.
#[derive(Debug)]
pub enum PaymentError {
	Refused(InvoiceRefusal),
	Domain(DomainError),
}

impl From<DomainError> for PaymentError {
	fn from(err: DomainError) -> Self {
		Self::Domain(err)
	}
}

/// What the control plane says about a payment, read with the invoice's row and the source
/// claim locked, on the connection that holds them.
#[derive(Clone, Debug)]
pub struct PaymentFacts {
	/// The receiving scope as it stands now.
	pub scope: Option<ScopeRecord>,
	/// The payer's freeze and mirrored KYC level now; `None` when banking has no such user.
	pub payer: Option<PayoutStanding>,
	/// The database's clock, unix seconds.
	pub now: i64,
}

/// Pays the locked invoice, or refuses. The source's spendable is read inside, through
/// `reads`.
#[async_trait]
pub trait PaymentAdmission: Send + Sync {
	async fn admit(&self, invoice: &mut Invoice, facts: &PaymentFacts, reads: &mut dyn LockedReads) -> Result<(), PaymentError>;
}

/// What [`InvoiceStore::pay_invoice`] did.
#[derive(Debug)]
pub enum Paid {
	/// Paid now; the payment's event is on the outbox.
	Now(RecordedInvoice),
	/// The payer already used the key: the invoice it paid, with where from. The caller
	/// decides whether it is a repeat or a conflict.
	Earlier(RecordedInvoice),
}

#[async_trait]
pub trait InvoiceStore: Send + Sync {
	/// The invoice `id`, whoever issued it.
	async fn invoice(&self, id: Uuid) -> Result<Option<RecordedInvoice>, DomainError>;

	/// The invoice `id` if `client` issued it in `scope`; `None` for anyone else's, as for one
	/// that does not exist.
	async fn issued_invoice(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<Option<RecordedInvoice>, DomainError>;

	/// The user's `auth_subject`, `None` when banking has no such user.
	async fn auth_subject(&self, user: UserId) -> Result<Option<String>, DomainError>;

	/// Issue an invoice into `(key.scope, to)`. Under a lock on the key it answers a key already
	/// used with what that call recorded; otherwise it asks `admission` with the facts and
	/// records the invoice it returns, with its money event, in one transaction.
	async fn record_invoice(&self, key: RequestKey<'_>, terms: &str, to: &ScopeAccountLabel, admission: &dyn InvoiceIssue) -> Result<Recorded<RecordedInvoice>, TransferError>;

	/// Cancel an open invoice `client` issued in `scope` (NOT_FOUND for anyone else's), under
	/// its row lock. A cancelled one is returned as it is.
	async fn cancel_invoice(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<RecordedInvoice, TransferError>;

	/// Pay invoice `id` as `payer` from `from`. Under a lock on the payer's key it answers a key
	/// already used with the invoice it paid ([`Paid::Earlier`]); otherwise, with the invoice's
	/// row locked and then the source claim's write lock taken, it lets `admission` pay it and
	/// records the payment, its event on the outbox, in one transaction.
	async fn pay_invoice(&self, payer: UserId, id: Uuid, from: &Compartment, idempotency_key: &str, admission: &dyn PaymentAdmission) -> Result<Paid, PaymentError>;
}
