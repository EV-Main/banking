//! Invoice use cases — a scope's service issuing, cancelling and reading its invoices
//! (`scope.v1.InvoiceService`), and a user reading and paying one
//! (`banking.v1.InvoicePaymentService`).
//!
//! The issuing side passes the same gates as every scope-ledger call ([`granted_in`]) and the
//! same idempotency keys. The paying side acts on the caller's own id. An invoice restricted to
//! another payer is the same NOT_FOUND as an unknown id. A payment is decided under the
//! invoice's row lock and the source claim's write lock — the lock every spender of that claim
//! takes — against the source's spendable net of everything recorded and not yet on the
//! ledger, so two payments of one invoice cannot both pass and a payment cannot spend what a
//! move, a hold or a withdrawal already took. Whichever source pays, the RECEIVING scope's
//! `min_kyc_level` applies; the scope's limits on moves in do not (D24 (6)): the money goes to
//! the scope's account, not into the payer's compartment.

use async_trait::async_trait;
use domain::{
	balance::LedgerAccountKey,
	error::DomainError,
	money::Usdt,
	scope_invoices::{Invoice, InvoiceId, InvoicePayment, InvoiceRefusal, InvoiceState, InvoiceTerms},
	scope_ledger::{ClientId, Grants, IDEMPOTENCY_CONFLICT, LedgerRefusal, SubjectRef, validate_memo},
	scopes::{Compartment, ScopeAccountLabel, ScopeId, ScopeState},
	users::UserId,
};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{
	application::scope_ledger::{check_key, check_subject, granted_in, same_terms},
	ports::{
		compartments::CompartmentStore,
		ledger::{Ledger, LockedReads},
		scope_invoices::{InvoiceIssue, InvoiceStore, IssueFacts, Paid, PaymentAdmission, PaymentError, PaymentFacts, RPC_CREATE_INVOICE, RecordedInvoice},
		scope_ledger::{Recorded, RequestKey, TransferError},
	},
};

/// Where every invoice is paid. First-party only: a scope's own host never takes the payment,
/// and nothing a client sends changes this.
/// The page is `@evinvest/wallet`'s pay action (`sdk/wallet`): it takes the invoice id and
/// reads everything else from the server's invoice.
pub const PAY_URL_BASE: &str = "https://evinvest.ltd/wallet/pay?invoice=";

pub fn pay_url(id: InvoiceId) -> String {
	format!("{PAY_URL_BASE}{id}")
}

/// The ports the issuing side borrows.
pub struct InvoicePorts<'a> {
	pub grants: &'a Grants,
	pub invoices: &'a dyn InvoiceStore,
	pub scopes: &'a dyn CompartmentStore,
}

/// What a client asks of `CreateInvoice`, parsed.
#[derive(Clone, Debug)]
pub struct CreateInvoiceRequest {
	pub scope: ScopeId,
	pub to: ScopeAccountLabel,
	pub amount: Usdt,
	pub memo: String,
	/// Unix seconds.
	pub expires_at: i64,
	pub payer_auth_subject: Option<String>,
	pub subject_ref: SubjectRef,
	pub idempotency_key: String,
}

/// Issue an invoice payable into `(scope, to)`, an account the caller owns under a current
/// grant. A repeat of the key gets the original invoice as it stands now.
pub async fn create_invoice(ports: &InvoicePorts<'_>, client: &ClientId, request: CreateInvoiceRequest) -> Result<RecordedInvoice, TransferError> {
	check_key(&request.idempotency_key)?;
	if let Some(payer) = &request.payer_auth_subject {
		check_subject(payer)?;
	}
	validate_memo(&request.memo)?;
	if request.amount.is_zero() {
		return Err(DomainError::Validation("amount must be positive".into()).into());
	}
	granted_in(ports.grants, ports.scopes, client, &request.scope).await?;
	let terms = InvoiceTerms {
		client: client.clone(),
		scope: request.scope.clone(),
		to: request.to.clone(),
		amount: request.amount,
		memo: request.memo.clone(),
		expires_at: request.expires_at,
		payer_auth_subject: request.payer_auth_subject.clone(),
		subject_ref: request.subject_ref.clone(),
	};
	let canonical = terms.canonical();
	let key = RequestKey {
		client,
		scope: &request.scope,
		rpc: RPC_CREATE_INVOICE,
		key: &request.idempotency_key,
	};
	let admission = IssueAdmission {
		terms,
		// Checked for a new invoice only, in the admission, so a repeat of a key used before a
		// prefix was taken away still gets its original result.
		label_granted: ports.grants.may_open(client, &request.scope, &request.to),
	};
	match ports.invoices.record_invoice(key, &canonical, &request.to, &admission).await? {
		Recorded::New(recorded) => Ok(recorded),
		Recorded::Existing(stored) => {
			same_terms(&stored, &canonical)?;
			let id = Uuid::parse_str(&stored.result_id).map_err(|_| DomainError::Repository(format!("key {} names invoice {:?}, not a uuid", request.idempotency_key, stored.result_id)))?;
			Ok(ports
				.invoices
				.issued_invoice(client, &request.scope, id)
				.await?
				.ok_or_else(|| DomainError::Repository(format!("key {} names invoice {id}, which is gone", request.idempotency_key)))?)
		}
	}
}

struct IssueAdmission {
	terms: InvoiceTerms,
	label_granted: bool,
}

#[async_trait]
impl InvoiceIssue for IssueAdmission {
	async fn admit(&self, facts: &IssueFacts) -> Result<Invoice, TransferError> {
		let scope = facts.scope.as_ref().ok_or_else(|| DomainError::NotFound {
			entity: "scope",
			id: self.terms.scope.to_string(),
		})?;
		if !self.label_granted {
			return Err(TransferError::Refused(LedgerRefusal::NotGranted));
		}
		// banking's own accounts are owned by banking, so this also keeps every invoice out of
		// `fee` and `treasury`.
		if facts.destination_owner.as_ref() != Some(&self.terms.client) {
			return Err(TransferError::Refused(LedgerRefusal::NotAccountOwner));
		}
		match scope.state {
			ScopeState::Frozen => return Err(TransferError::Refused(LedgerRefusal::ScopeFrozen)),
			// A closed scope may still ask; its invoices are refused to every payer
			// (`scope_closed`), like any money into it.
			ScopeState::Active | ScopeState::Closed => {}
		}
		Ok(Invoice::issue(InvoiceId::new(), self.terms.clone(), facts.now)?)
	}
}

/// Withdraw an open invoice the caller issued. A cancelled one is returned unchanged.
pub async fn cancel_invoice(ports: &InvoicePorts<'_>, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<RecordedInvoice, TransferError> {
	granted_in(ports.grants, ports.scopes, client, scope).await?;
	ports.invoices.cancel_invoice(client, scope, id).await
}

/// One invoice the caller issued in `scope`; anyone else's is the same NOT_FOUND as none.
pub async fn get_invoice(ports: &InvoicePorts<'_>, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<RecordedInvoice, TransferError> {
	granted_in(ports.grants, ports.scopes, client, scope).await?;
	Ok(ports.invoices.issued_invoice(client, scope, id).await?.ok_or_else(|| not_found(id))?)
}

fn not_found(id: Uuid) -> DomainError {
	DomainError::NotFound {
		entity: "invoice",
		id: id.to_string(),
	}
}

/// The ports the paying side borrows.
pub struct PayerPorts<'a> {
	pub invoices: &'a dyn InvoiceStore,
	pub scopes: &'a dyn CompartmentStore,
	/// Every balance is TigerBeetle's.
	pub ledger: &'a dyn Ledger,
	/// Nudged after a payment commits so the relay posts it at once.
	pub relay: &'a Notify,
}

/// An invoice as its payer sees it, with what they could pay it from right now.
#[derive(Clone, Debug)]
pub struct PayableView {
	pub recorded: RecordedInvoice,
	/// Who is looking: the payment's details are the payer's alone.
	pub viewer: UserId,
	/// The scope's human name.
	pub display_name: String,
	pub available_main: Usdt,
	pub available_scope: Usdt,
}

impl PayableView {
	/// The state the payer is shown: an open invoice past its expiry reads expired.
	pub fn state(&self) -> InvoiceState {
		self.recorded.invoice.state_at(self.recorded.read_at)
	}

	/// The payment, when the ledger took it and the viewer is the one who paid — anyone else
	/// with the id learns only that the invoice is paid, not by whom, when or from where.
	pub fn own_payment(&self) -> Option<&InvoicePayment> {
		self.recorded.invoice.posted_payment().filter(|payment| payment.payer == self.viewer)
	}
}

/// The invoice `id` if `user` may see it — unknown and restricted to another payer alike are
/// NOT_FOUND — with the payer's `auth_subject`.
async fn visible(ports: &PayerPorts<'_>, user: UserId, id: Uuid) -> Result<(RecordedInvoice, String), DomainError> {
	let recorded = ports.invoices.invoice(id).await?.ok_or_else(|| not_found(id))?;
	let subject = ports.invoices.auth_subject(user).await?.ok_or_else(|| not_found(id))?;
	if !recorded.invoice.payable_by(&subject) {
		return Err(not_found(id));
	}
	Ok((recorded, subject))
}

async fn view(ports: &PayerPorts<'_>, user: UserId, recorded: RecordedInvoice) -> Result<PayableView, DomainError> {
	let scope = recorded.invoice.terms().scope.clone();
	let display_name = ports.scopes.scope(&scope).await?.map(|record| record.display_name).unwrap_or_else(|| scope.to_string());
	let keys = [LedgerAccountKey::UserClaim(user), LedgerAccountKey::ScopeClaim(user, scope)];
	let balances = ports.ledger.balances(&keys).await?;
	let [main, compartment] = balances.as_slice() else {
		return Err(DomainError::Repository(format!("asked the ledger for 2 balances, got {}", balances.len())));
	};
	Ok(PayableView {
		recorded,
		viewer: user,
		display_name,
		available_main: Usdt::from_base_units(main.available()),
		available_scope: Usdt::from_base_units(compartment.available()),
	})
}

/// One invoice as the caller, its would-be payer, sees it.
pub async fn get_payable(ports: &PayerPorts<'_>, user: UserId, id: Uuid) -> Result<PayableView, DomainError> {
	let (recorded, _) = visible(ports, user, id).await?;
	view(ports, user, recorded).await
}

/// Longest idempotency key accepted, in characters (`invoices.proto`, like every key).
const MAX_PAY_KEY: usize = 128;

/// Pay an open invoice from `from` — `main`, or the caller's compartment in the invoice's
/// scope. A repeat of the key returns the invoice it paid; the key used for another invoice or
/// another source is ALREADY_EXISTS.
pub async fn pay(ports: &PayerPorts<'_>, user: UserId, id: Uuid, from: Compartment, idempotency_key: &str) -> Result<PayableView, PaymentError> {
	let chars = idempotency_key.chars().count();
	if chars == 0 || chars > MAX_PAY_KEY || idempotency_key.chars().any(char::is_control) {
		return Err(DomainError::Validation(format!("idempotency_key must be 1..{MAX_PAY_KEY} characters of plain text")).into());
	}
	let (recorded, subject) = visible(ports, user, id).await?;
	if from.scope().is_some_and(|scope| scope != &recorded.invoice.terms().scope) {
		return Err(DomainError::Validation("pay_from must be main or the invoice's scope".into()).into());
	}
	let admission = Admission {
		ledger: ports.ledger,
		user,
		subject,
		from: from.clone(),
	};
	let paid = match ports.invoices.pay_invoice(user, id, &from, idempotency_key, &admission).await? {
		Paid::Now(recorded) => {
			ports.relay.notify_one();
			recorded
		}
		Paid::Earlier(earlier) => {
			let same = earlier.invoice.id().raw() == id && earlier.invoice.payment().is_some_and(|payment| payment.from == from);
			if !same {
				return Err(DomainError::Conflict(IDEMPOTENCY_CONFLICT.into()).into());
			}
			earlier
		}
	};
	Ok(view(ports, user, paid).await?)
}

/// The checks, run by the store while it holds the invoice's row and the source claim's lock.
struct Admission<'a> {
	ledger: &'a dyn Ledger,
	user: UserId,
	subject: String,
	from: Compartment,
}

#[async_trait]
impl PaymentAdmission for Admission<'_> {
	async fn admit(&self, invoice: &mut Invoice, facts: &PaymentFacts, reads: &mut dyn LockedReads) -> Result<(), PaymentError> {
		// What is no longer payable says so before anything about the payer: a second payer of a
		// paid invoice learns that, not why they could not have paid it anyway.
		// The stored state, not the read view: a payment still waiting for the ledger reads open
		// but is a payment all the same.
		if invoice.state() != InvoiceState::Open || invoice.is_due(facts.now) {
			return Err(PaymentError::Refused(InvoiceRefusal::NotOpen));
		}
		let scope = facts.scope.as_ref().ok_or_else(|| DomainError::NotFound {
			entity: "scope",
			id: invoice.terms().scope.to_string(),
		})?;
		match scope.state {
			ScopeState::Active => {}
			ScopeState::Frozen => return Err(PaymentError::Refused(InvoiceRefusal::ScopeFrozen)),
			ScopeState::Closed => return Err(PaymentError::Refused(InvoiceRefusal::ScopeClosed)),
		}
		let standing = facts.payer.ok_or_else(|| DomainError::NotFound {
			entity: "user",
			id: self.user.to_string(),
		})?;
		if standing.blocked {
			return Err(PaymentError::Refused(InvoiceRefusal::UserFrozen));
		}
		if !scope.policy.admits_kyc(standing.kyc_level) {
			return Err(PaymentError::Refused(InvoiceRefusal::KycLevel));
		}
		let spendable = self.ledger.spendable_locked(&self.from.claim_key(self.user), reads).await.map_err(DomainError::from)?;
		if Usdt::from_base_units(spendable) < invoice.terms().amount {
			return Err(PaymentError::Refused(InvoiceRefusal::InsufficientFunds));
		}
		invoice
			.pay(InvoicePayment {
				payer: self.user,
				payer_auth_subject: self.subject.clone(),
				from: self.from.clone(),
				paid_at: facts.now,
				posted: false,
			})
			.map_err(PaymentError::Refused)
	}
}
