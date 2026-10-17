//! `scope_charges` bounded context — a scope's service taking a one-off amount from a user's
//! compartment into one of its own accounts, on the user's mandate for it
//! (`scope/v1/ledger.proto`, `ScopeLedgerService.Charge`).
//!
//! A charge is one posted ledger transfer `Dr scope_claim:<scope>:<user> / Cr
//! scope_account:<scope>:<to>`, applied by the relay. Unlike a hold it keeps nothing
//! standing: it either lands or is refused, and until the relay decides, the amount is
//! promised away from the compartment. Whether the mandate, the freeze and the balance allow
//! it is decided by the layers above under their locks; this module owns what a charge is.
//!
//! Pure and wasm-safe: ids are minted by the application layer, no clock, no I/O.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, SubjectRef, canonical, validate_memo},
	scopes::{ScopeAccountLabel, ScopeId},
	users::UserId,
};

/// A unique charge id (UUID). Minted by the application layer.
pub type ChargeId = Id<ChargeTag>;
/// Phantom tag making [`ChargeId`] a distinct, incompatible identity type.
pub struct ChargeTag;

/// Everything a `Charge` fixes once it is admitted.
#[derive(Clone, Debug)]
pub struct ChargeTerms {
	/// The charging client; it owns `to`.
	pub client: ClientId,
	pub scope: ScopeId,
	/// The user whose compartment is debited.
	pub user: UserId,
	pub to: ScopeAccountLabel,
	pub amount: Usdt,
	pub memo: String,
	pub subject_ref: SubjectRef,
	/// The mandate the charge drew on.
	pub mandate_id: uuid::Uuid,
}

/// The canonical rendering stored beside a `Charge` idempotency key: every field the request
/// carries, length-prefixed, so a repeat that differs in any of them is a conflict. The user is
/// named as the client named them (`auth_subject`), not by the id banking resolved it to.
pub fn canonical_request(auth_subject: &str, to: &ScopeAccountLabel, amount: Usdt, subject_ref: &SubjectRef, memo: &str) -> String {
	canonical(&[auth_subject, to.as_str(), &amount.base_units().to_string(), subject_ref.as_str(), memo])
}

/// A one-off debit of a user's compartment into a scope account.
#[derive(Clone, Debug)]
pub struct ScopeCharge {
	id: ChargeId,
	terms: ChargeTerms,
	pending: Vec<ChargeEvent>,
}

impl ScopeCharge {
	/// A new charge, raising the event the relay posts. Who may be charged and what the
	/// mandate allows are the store's to check, under its locks.
	pub fn request(id: ChargeId, terms: ChargeTerms) -> Result<Self, DomainError> {
		let mut requested = Self::restore(id, terms)?;
		requested.pending.push(ChargeEvent::Charged {
			charge_id: id,
			scope: requested.terms.scope.clone(),
			user: requested.terms.user,
			to_label: requested.terms.to.clone(),
			amount: requested.terms.amount,
		});
		Ok(requested)
	}

	/// Rebuild from storage (no events).
	pub fn restore(id: ChargeId, terms: ChargeTerms) -> Result<Self, DomainError> {
		if terms.amount.is_zero() {
			return Err(DomainError::Validation("amount must be positive".into()));
		}
		if terms.scope.is_main() {
			return Err(DomainError::Validation("a charge never names main".into()));
		}
		// The domain's own floor under the use case's checks: no charge ever credits one of
		// banking's labels, whatever a row or a grant says.
		if terms.to.is_reserved() {
			return Err(DomainError::Forbidden(format!("{} is banking's own account", terms.to)));
		}
		validate_memo(&terms.memo)?;
		Ok(Self { id, terms, pending: Vec::new() })
	}

	pub fn id(&self) -> ChargeId {
		self.id
	}

	pub fn terms(&self) -> &ChargeTerms {
		&self.terms
	}
}

impl Entity for ScopeCharge {
	type Id = ChargeId;

	fn id(&self) -> ChargeId {
		self.id
	}
}

impl AggregateRoot for ScopeCharge {
	const NAME: &'static str = "scope_charge";
}

/// Facts raised by a charge; internally tagged so the stored JSON describes itself. The event
/// names the user and the amount for the relay's plan; a pending charge is counted against the
/// compartment from the `scope_charges` row (its kind is left out of the outbox scan), not from
/// this event.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChargeEvent {
	/// Relay: post `Dr scope_claim:<scope>:<user> / Cr scope_account:<scope>:<to_label>` for
	/// `amount`.
	Charged {
		charge_id: ChargeId,
		scope: ScopeId,
		user: UserId,
		to_label: ScopeAccountLabel,
		amount: Usdt,
	},
}

impl DomainEvent for ChargeEvent {
	const KIND: &'static str = "scope_charges";
}

impl EmitsEvents for ScopeCharge {
	type Event = ChargeEvent;

	fn drain_events(&mut self) -> Vec<ChargeEvent> {
		core::mem::take(&mut self.pending)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn terms(to: &str, amount: &str, memo: &str) -> ChargeTerms {
		ChargeTerms {
			client: ClientId::parse("panel").unwrap(),
			scope: ScopeId::parse("sa").unwrap(),
			user: UserId::new(),
			to: ScopeAccountLabel::parse(to).unwrap(),
			amount: Usdt::parse_decimal(amount).unwrap(),
			memo: memo.to_owned(),
			subject_ref: SubjectRef::parse("order:4f0c1b2e-0000-4000-8000-000000000001").unwrap(),
			mandate_id: uuid::Uuid::new_v4(),
		}
	}

	#[test]
	fn a_charge_raises_the_one_event_the_relay_posts() {
		let mut charge = ScopeCharge::request(ChargeId::new(), terms("sales", "12.5", "Monthly plan")).unwrap();
		let events = charge.drain_events();
		let [ChargeEvent::Charged { amount, to_label, .. }] = events.as_slice() else {
			panic!("one Charged event expected, got {events:?}");
		};
		assert_eq!((amount.to_decimal_string().as_str(), to_label.as_str()), ("12.5", "sales"));
		assert!(charge.drain_events().is_empty(), "drained once");
		assert!(
			ScopeCharge::restore(charge.id, terms("sales", "12.5", "")).unwrap().pending.is_empty(),
			"a restore raises nothing"
		);
	}

	#[test]
	fn a_charge_is_refused_what_cannot_be_one() {
		for (to, amount, memo, needle) in [
			("sales", "0", "", "positive"),
			("fee", "1", "", "banking's own"),
			("treasury", "1", "", "banking's own"),
			("sales", "1", "see https://example.com", "link"),
		] {
			let err = ScopeCharge::request(ChargeId::new(), terms(to, amount, memo)).expect_err(needle).to_string();
			assert!(err.contains(needle), "{needle}: {err}");
		}
		let mut in_main = terms("sales", "1", "");
		in_main.scope = ScopeId::main();
		assert!(ScopeCharge::request(ChargeId::new(), in_main).is_err());
	}

	#[test]
	fn the_canonical_request_tells_every_field_apart() {
		let to = ScopeAccountLabel::parse("sales").unwrap();
		let other = ScopeAccountLabel::parse("sale").unwrap();
		let one = Usdt::parse_decimal("1").unwrap();
		let reference = SubjectRef::parse("order:4f0c1b2e-0000-4000-8000-000000000001").unwrap();
		let base = canonical_request("user-a", &to, one, &reference, "m");
		assert_eq!(base, canonical_request("user-a", &to, one, &reference, "m"));
		for different in [
			canonical_request("user-b", &to, one, &reference, "m"),
			canonical_request("user-a", &other, one, &reference, "m"),
			canonical_request("user-a", &to, Usdt::parse_decimal("2").unwrap(), &reference, "m"),
			canonical_request("user-a", &to, one, &SubjectRef::parse("order:4f0c1b2e-0000-4000-8000-000000000002").unwrap(), "m"),
			canonical_request("user-a", &to, one, &reference, "n"),
		] {
			assert_ne!(base, different);
		}
	}
}
