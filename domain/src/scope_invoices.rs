//! `scope_invoices` bounded context — a scope asking a user to pay into one of its accounts
//! (D10; `scope/v1/invoices.proto` for the issuing service, `banking/v1/invoices.proto` for the
//! payer).
//!
//! An invoice is the one way a service takes money from a user who gave it no mandate: the
//! service issues it, the user pays it on evinvest.ltd straight from `main` or from their
//! compartment in the invoice's scope into the scope account the invoice names — one posted
//! ledger transfer, no move between compartments first. It is paid at most once.
//!
//! Pure and wasm-safe: ids are minted and the clock is read by the layers above.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, SubjectRef, canonical, validate_memo},
	scopes::{Compartment, ScopeAccountLabel, ScopeId},
	users::UserId,
};

/// A unique invoice id (UUID). Minted by the application layer.
pub type InvoiceId = Id<InvoiceTag>;
/// Phantom tag making [`InvoiceId`] a distinct, incompatible identity type.
pub struct InvoiceTag;

/// The furthest ahead an invoice may expire, in seconds (`invoices.proto`: "at most 30 days").
pub const MAX_INVOICE_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// Where an invoice stands (`InvoiceState` on the wire).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvoiceState {
	Open,
	Paid,
	Cancelled,
	Expired,
}

impl InvoiceState {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Open => "open",
			Self::Paid => "paid",
			Self::Cancelled => "cancelled",
			Self::Expired => "expired",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"open" => Ok(Self::Open),
			"paid" => Ok(Self::Paid),
			"cancelled" => Ok(Self::Cancelled),
			"expired" => Ok(Self::Expired),
			other => Err(DomainError::Validation(format!("unknown invoice state: {other}"))),
		}
	}
}

/// Everything a `CreateInvoice` names — what an idempotent repeat must match.
#[derive(Clone, Debug)]
pub struct InvoiceTerms {
	/// The issuing client; it owns `to`.
	pub client: ClientId,
	pub scope: ScopeId,
	pub to: ScopeAccountLabel,
	pub amount: Usdt,
	pub memo: String,
	/// Unix seconds.
	pub expires_at: i64,
	/// Only this user may pay; `None` = anyone signed in.
	pub payer_auth_subject: Option<String>,
	pub subject_ref: SubjectRef,
}

impl InvoiceTerms {
	/// The canonical rendering stored beside the idempotency key: every field the request
	/// carries, length-prefixed.
	pub fn canonical(&self) -> String {
		canonical(&[
			self.to.as_str(),
			&self.amount.base_units().to_string(),
			&self.memo,
			&self.expires_at.to_string(),
			self.payer_auth_subject.as_deref().unwrap_or(""),
			self.subject_ref.as_str(),
		])
	}
}

/// Who paid an invoice, from where and when.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvoicePayment {
	pub payer: UserId,
	/// The payer as the identity plane names them — what the issuing service knows.
	pub payer_auth_subject: String,
	pub from: Compartment,
	/// Unix seconds.
	pub paid_at: i64,
	/// Whether the ledger took it. Until it has, nobody is told the invoice is paid: a leg the
	/// ledger refuses puts the invoice back to open ([`Invoice::fail_payment`]).
	pub posted: bool,
}

/// Why a payment did not happen — each one the whole wire status message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvoiceRefusal {
	/// Paid, cancelled, expired, or past its expiry (FAILED_PRECONDITION).
	NotOpen,
	/// The invoice names another payer. Answered as the same NOT_FOUND as an unknown id, so
	/// nobody learns which invoice ids exist.
	OtherPayer,
	/// The payer is below the receiving scope's `min_kyc_level` (FAILED_PRECONDITION).
	KycLevel,
	ScopeFrozen,
	ScopeClosed,
	UserFrozen,
	InsufficientFunds,
}

impl InvoiceRefusal {
	pub const fn reason(self) -> &'static str {
		match self {
			Self::NotOpen => "invoice_not_open",
			Self::OtherPayer => "not_found",
			Self::KycLevel => "kyc_level",
			Self::ScopeFrozen => "scope_frozen",
			Self::ScopeClosed => "scope_closed",
			Self::UserFrozen => "user_frozen",
			Self::InsufficientFunds => "insufficient_funds",
		}
	}
}

/// A scope's request for `amount` into `(scope, to)`, payable once until `expires_at`.
///
/// Only a payment moves money: one posted transfer `Dr <payer's main or compartment> / Cr
/// scope_account:<scope>:<to>`, applied by the relay. Creation, cancellation and expiry are
/// control-plane facts only.
#[derive(Clone, Debug)]
pub struct Invoice {
	id: InvoiceId,
	terms: InvoiceTerms,
	state: InvoiceState,
	/// The version of the invoice's last event; each event is one more.
	version: u64,
	payment: Option<InvoicePayment>,
	pending: Vec<InvoiceEvent>,
}

impl Invoice {
	/// A new open invoice, raising its creation. `now` is the clock the expiry is checked
	/// against: in the future, at most [`MAX_INVOICE_TTL_SECS`] ahead.
	pub fn issue(id: InvoiceId, terms: InvoiceTerms, now: i64) -> Result<Self, DomainError> {
		if terms.expires_at <= now {
			return Err(DomainError::Validation("expires_at must be in the future".into()));
		}
		if terms.expires_at - now > MAX_INVOICE_TTL_SECS {
			return Err(DomainError::Validation("expires_at must be at most 30 days ahead".into()));
		}
		let mut invoice = Self::restore(id, terms, InvoiceState::Open, 1, None)?;
		invoice.pending.push(InvoiceEvent::Created {
			invoice_id: id,
			scope: invoice.terms.scope.clone(),
			to_label: invoice.terms.to.clone(),
			amount: invoice.terms.amount,
			version: 1,
		});
		Ok(invoice)
	}

	/// Rebuild from storage (no events).
	pub fn restore(id: InvoiceId, terms: InvoiceTerms, state: InvoiceState, version: u64, payment: Option<InvoicePayment>) -> Result<Self, DomainError> {
		if terms.amount.is_zero() {
			return Err(DomainError::Validation("amount must be positive".into()));
		}
		if terms.scope.is_main() {
			return Err(DomainError::Validation("an invoice is never issued in main".into()));
		}
		// banking's own labels take no payment from a client's invoice: `fee` is swept by
		// banking alone, and an invoice into it would book a payment as a fee.
		if terms.to.is_reserved() {
			return Err(DomainError::Validation(format!("{} is banking's own account", terms.to)));
		}
		validate_memo(&terms.memo)?;
		if terms.payer_auth_subject.as_deref().is_some_and(str::is_empty) {
			return Err(DomainError::Validation("payer_auth_subject must not be empty when set".into()));
		}
		if (state == InvoiceState::Paid) != payment.is_some() {
			return Err(DomainError::Validation(format!("invoice {id}: a payment belongs to a paid invoice and to it alone")));
		}
		if payment.as_ref().is_some_and(|paid| paid.from.scope().is_some_and(|scope| scope != &terms.scope)) {
			return Err(DomainError::Validation(format!("invoice {id}: paid from another scope's compartment")));
		}
		Ok(Self {
			id,
			terms,
			state,
			version,
			payment,
			pending: Vec::new(),
		})
	}

	pub fn id(&self) -> InvoiceId {
		self.id
	}

	pub fn terms(&self) -> &InvoiceTerms {
		&self.terms
	}

	/// The state as stored. A read shows [`Self::state_at`].
	pub fn state(&self) -> InvoiceState {
		self.state
	}

	pub fn version(&self) -> u64 {
		self.version
	}

	pub fn payment(&self) -> Option<&InvoicePayment> {
		self.payment.as_ref()
	}

	/// Open, but past its expiry: the reaper's to expire, and no longer payable.
	pub fn is_due(&self, now: i64) -> bool {
		self.state == InvoiceState::Open && self.terms.expires_at <= now
	}

	/// The state as anyone reading it at `now` must see it: an open invoice past its expiry is
	/// expired even before the reaper records it, so no reader offers to pay it; a payment the
	/// ledger has not taken yet is not a payment to anyone outside — the invoice reads open
	/// until it posts (and stays open if it is refused).
	pub fn state_at(&self, now: i64) -> InvoiceState {
		if self.is_due(now) {
			return InvoiceState::Expired;
		}
		match (self.state, &self.payment) {
			(InvoiceState::Paid, Some(payment)) if !payment.posted => InvoiceState::Open,
			(state, _) => state,
		}
	}

	/// The payment, once the ledger took it — the only payment a reader is shown.
	pub fn posted_payment(&self) -> Option<&InvoicePayment> {
		self.payment.as_ref().filter(|payment| payment.posted)
	}

	/// The ledger refused (or the relay parked) the payment's leg: the invoice is open again,
	/// with nothing paid, and may be paid anew. `false` when there is no payment waiting for
	/// the ledger — a posted one is final.
	pub fn fail_payment(&mut self, reason: &str) -> bool {
		if self.state != InvoiceState::Paid || self.payment.as_ref().is_none_or(|payment| payment.posted) {
			return false;
		}
		self.version += 1;
		self.state = InvoiceState::Open;
		self.payment = None;
		self.pending.push(InvoiceEvent::PaymentFailed {
			invoice_id: self.id,
			scope: self.terms.scope.clone(),
			version: self.version,
			reason: reason.to_owned(),
		});
		true
	}

	/// Whether the user named `auth_subject` may see and pay it.
	pub fn payable_by(&self, auth_subject: &str) -> bool {
		self.terms.payer_auth_subject.as_deref().is_none_or(|payer| payer == auth_subject)
	}

	/// Pay it once. `from` must be `main` or the payer's compartment in the invoice's scope;
	/// whether the payer may pay at all (freeze, KYC, funds) is decided by the caller, under
	/// its locks.
	pub fn pay(&mut self, payment: InvoicePayment) -> Result<(), InvoiceRefusal> {
		if !self.payable_by(&payment.payer_auth_subject) {
			return Err(InvoiceRefusal::OtherPayer);
		}
		if self.state != InvoiceState::Open || self.is_due(payment.paid_at) {
			return Err(InvoiceRefusal::NotOpen);
		}
		if payment.from.scope().is_some_and(|scope| scope != &self.terms.scope) {
			return Err(InvoiceRefusal::NotOpen);
		}
		self.version += 1;
		self.state = InvoiceState::Paid;
		let payment = InvoicePayment { posted: false, ..payment };
		self.pending.push(InvoiceEvent::Paid {
			invoice_id: self.id,
			scope: self.terms.scope.clone(),
			to_label: self.terms.to.clone(),
			amount: self.terms.amount,
			payer: payment.payer,
			from: payment.from.clone(),
			version: self.version,
		});
		self.payment = Some(payment);
		Ok(())
	}

	/// Withdraw an open invoice: `true` when it is cancelled now, `false` when it already was.
	/// Paid, expired, or past its expiry is [`InvoiceRefusal::NotOpen`].
	pub fn cancel(&mut self, now: i64) -> Result<bool, InvoiceRefusal> {
		match self.state {
			InvoiceState::Cancelled => Ok(false),
			InvoiceState::Open if !self.is_due(now) => {
				self.close(false);
				Ok(true)
			}
			InvoiceState::Open | InvoiceState::Paid | InvoiceState::Expired => Err(InvoiceRefusal::NotOpen),
		}
	}

	/// The reaper's expiry of an open invoice past `expires_at`; `false` when it is not due.
	pub fn expire(&mut self, now: i64) -> bool {
		if !self.is_due(now) {
			return false;
		}
		self.close(true);
		true
	}

	fn close(&mut self, expired: bool) {
		self.version += 1;
		let (invoice_id, scope, version) = (self.id, self.terms.scope.clone(), self.version);
		if expired {
			self.state = InvoiceState::Expired;
			self.pending.push(InvoiceEvent::Expired { invoice_id, scope, version });
		} else {
			self.state = InvoiceState::Cancelled;
			self.pending.push(InvoiceEvent::Cancelled { invoice_id, scope, version });
		}
	}
}

impl Entity for Invoice {
	type Id = InvoiceId;

	fn id(&self) -> InvoiceId {
		self.id
	}
}

impl AggregateRoot for Invoice {
	const NAME: &'static str = "scope_invoice";
}

/// An invoice's facts. Only [`Self::Paid`] reaches the relay ([`Self::moves_money`]); the
/// others are the control plane's alone. A payment names the payer, so a scan of unapplied
/// debits of the payer's claim finds it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InvoiceEvent {
	Created {
		invoice_id: InvoiceId,
		scope: ScopeId,
		to_label: ScopeAccountLabel,
		amount: Usdt,
		version: u64,
	},
	/// Relay: post `Dr <from claim of payer> / Cr scope_account:<scope>:<to_label>` for
	/// `amount`.
	Paid {
		invoice_id: InvoiceId,
		scope: ScopeId,
		to_label: ScopeAccountLabel,
		amount: Usdt,
		payer: UserId,
		from: Compartment,
		version: u64,
	},
	Cancelled {
		invoice_id: InvoiceId,
		scope: ScopeId,
		version: u64,
	},
	Expired {
		invoice_id: InvoiceId,
		scope: ScopeId,
		version: u64,
	},
	/// The payment's leg was refused or parked; the invoice is open again. The issuer was never
	/// told of the payment, so it is told nothing of this either.
	PaymentFailed {
		invoice_id: InvoiceId,
		scope: ScopeId,
		version: u64,
		reason: String,
	},
}

impl InvoiceEvent {
	pub fn moves_money(&self) -> bool {
		match self {
			Self::Paid { .. } => true,
			Self::Created { .. } | Self::Cancelled { .. } | Self::Expired { .. } | Self::PaymentFailed { .. } => false,
		}
	}
}

impl DomainEvent for InvoiceEvent {
	const KIND: &'static str = "scope_invoices";
}

impl EmitsEvents for Invoice {
	type Event = InvoiceEvent;

	fn drain_events(&mut self) -> Vec<InvoiceEvent> {
		core::mem::take(&mut self.pending)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const NOW: i64 = 1_800_000_000;

	fn terms(payer: Option<&str>) -> InvoiceTerms {
		InvoiceTerms {
			client: ClientId::parse("panel").unwrap(),
			scope: ScopeId::parse("sa").unwrap(),
			to: ScopeAccountLabel::parse("sales").unwrap(),
			amount: Usdt::parse_decimal("25").unwrap(),
			memo: "Monthly plan".into(),
			expires_at: NOW + 3600,
			payer_auth_subject: payer.map(str::to_owned),
			subject_ref: SubjectRef::parse("order:4f0c2d1e-0000-4000-8000-0123456789ab").unwrap(),
		}
	}

	fn payment(subject: &str, from: Compartment, at: i64) -> InvoicePayment {
		InvoicePayment {
			payer: UserId::new(),
			payer_auth_subject: subject.into(),
			from,
			paid_at: at,
			posted: false,
		}
	}

	fn open(payer: Option<&str>) -> Invoice {
		let mut invoice = Invoice::issue(InvoiceId::new(), terms(payer), NOW).unwrap();
		invoice.drain_events();
		invoice
	}

	#[test]
	fn an_expiry_must_be_ahead_and_within_thirty_days() {
		let at = |expires_at| Invoice::issue(InvoiceId::new(), InvoiceTerms { expires_at, ..terms(None) }, NOW);
		assert!(at(NOW).is_err());
		assert!(at(NOW + MAX_INVOICE_TTL_SECS + 1).is_err());
		assert!(at(NOW + MAX_INVOICE_TTL_SECS).is_ok());
	}

	#[test]
	fn a_memo_with_a_link_and_an_invoice_into_a_reserved_label_are_refused() {
		assert!(
			Invoice::issue(
				InvoiceId::new(),
				InvoiceTerms {
					memo: "pay at https://evil.example/x".into(),
					..terms(None)
				},
				NOW
			)
			.is_err()
		);
		let fee = ScopeAccountLabel::parse("fee").unwrap();
		assert!(Invoice::issue(InvoiceId::new(), InvoiceTerms { to: fee, ..terms(None) }, NOW).is_err());
	}

	#[test]
	fn an_invoice_is_paid_once() {
		let mut invoice = open(None);
		invoice.pay(payment("u-1", Compartment::Main, NOW)).unwrap();
		assert_eq!(invoice.state(), InvoiceState::Paid);
		assert!(matches!(invoice.drain_events().as_slice(), [InvoiceEvent::Paid { version: 2, .. }]));
		assert_eq!(invoice.pay(payment("u-2", Compartment::Main, NOW)), Err(InvoiceRefusal::NotOpen));
	}

	#[test]
	fn a_restricted_invoice_is_another_payers_and_a_due_one_is_not_open() {
		let mut invoice = open(Some("u-1"));
		assert_eq!(invoice.pay(payment("u-2", Compartment::Main, NOW)), Err(InvoiceRefusal::OtherPayer));
		assert_eq!(invoice.pay(payment("u-1", Compartment::Main, NOW + 3600)), Err(InvoiceRefusal::NotOpen));
		assert_eq!(invoice.state_at(NOW + 3600), InvoiceState::Expired);
		assert_eq!(invoice.state(), InvoiceState::Open, "a read never writes the expiry");
	}

	#[test]
	fn only_the_invoices_own_scope_compartment_pays_it() {
		let mut invoice = open(None);
		let other = Compartment::Scope(ScopeId::parse("invest").unwrap());
		assert_eq!(invoice.pay(payment("u-1", other, NOW)), Err(InvoiceRefusal::NotOpen));
		invoice.pay(payment("u-1", Compartment::Scope(ScopeId::parse("sa").unwrap()), NOW)).unwrap();
	}

	#[test]
	fn cancelling_is_idempotent_but_a_paid_or_due_invoice_is_not_open() {
		let mut invoice = open(None);
		assert_eq!(invoice.cancel(NOW), Ok(true));
		assert_eq!(invoice.cancel(NOW), Ok(false));
		assert!(matches!(invoice.drain_events().as_slice(), [InvoiceEvent::Cancelled { version: 2, .. }]));

		let mut paid = open(None);
		paid.pay(payment("u-1", Compartment::Main, NOW)).unwrap();
		assert_eq!(paid.cancel(NOW), Err(InvoiceRefusal::NotOpen));

		let mut due = open(None);
		assert_eq!(due.cancel(NOW + 3600), Err(InvoiceRefusal::NotOpen));
		assert!(due.expire(NOW + 3600));
		assert!(!due.expire(NOW + 3600));
		assert!(matches!(due.drain_events().as_slice(), [InvoiceEvent::Expired { version: 2, .. }]));
	}

	#[test]
	fn a_payment_reads_open_until_the_ledger_takes_it_and_a_refused_one_reopens_the_invoice() {
		let mut invoice = open(None);
		invoice.pay(payment("u-1", Compartment::Main, NOW)).unwrap();
		invoice.drain_events();
		assert_eq!((invoice.state(), invoice.state_at(NOW)), (InvoiceState::Paid, InvoiceState::Open));
		assert!(invoice.posted_payment().is_none());
		assert!(invoice.fail_payment("insufficient_funds"));
		assert_eq!((invoice.state(), invoice.version()), (InvoiceState::Open, 3));
		assert!(invoice.payment().is_none());
		assert!(matches!(invoice.drain_events().as_slice(), [InvoiceEvent::PaymentFailed { version: 3, .. }]));
		assert!(!invoice.fail_payment("again"), "nothing waits for the ledger now");
		invoice.pay(payment("u-2", Compartment::Main, NOW)).unwrap();
		let posted = Invoice::restore(
			invoice.id(),
			invoice.terms().clone(),
			InvoiceState::Paid,
			invoice.version(),
			invoice.payment().cloned().map(|payment| InvoicePayment { posted: true, ..payment }),
		)
		.unwrap();
		assert_eq!(posted.state_at(NOW), InvoiceState::Paid);
		assert!(!posted.clone().fail_payment("late"), "a posted payment is final");
	}

	#[test]
	fn only_a_payment_reaches_the_relay() {
		let mut invoice = Invoice::issue(InvoiceId::new(), terms(None), NOW).unwrap();
		invoice.pay(payment("u-1", Compartment::Main, NOW)).unwrap();
		let relayed: Vec<bool> = invoice.drain_events().iter().map(InvoiceEvent::moves_money).collect();
		assert_eq!(relayed, [false, true]);
	}

	#[test]
	fn the_canonical_terms_cover_every_field() {
		let base = terms(None).canonical();
		assert_ne!(base, terms(Some("u-1")).canonical());
		assert_ne!(
			base,
			InvoiceTerms {
				memo: "Other".into(),
				..terms(None)
			}
			.canonical()
		);
		assert_ne!(
			base,
			InvoiceTerms {
				expires_at: NOW + 7200,
				..terms(None)
			}
			.canonical()
		);
	}
}
