//! A user's mandate — the only consent a scope's service debits their compartment on
//! (`banking.v1.CompartmentService.GrantMandate`, `ledger.proto` USER CONSENT): which client,
//! in which scope, for which operations, within which caps, until when.
//!
//! The caps are decided here over facts the store reads under its locks ([`MandateUsage`]):
//! the store answers "how much is drawn", this module answers "may this draw happen". What
//! counts as drawn is the client's draws on the user's compartment in the scope, whichever of
//! the user's mandates they were made under, so granting again never resets a cap.

use crate::{
	error::DomainError,
	money::Usdt,
	scope_ledger::{ClientId, LedgerRefusal, MandateOperation},
	scopes::ScopeId,
	users::UserId,
};

impl MandateOperation {
	/// Every operation, in wire order.
	pub const ALL: [Self; 3] = [Self::Reserve, Self::Charge, Self::BookFill];

	/// The stored name. FROZEN: rows and their CHECK constraint carry it.
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Reserve => "reserve",
			Self::Charge => "charge",
			Self::BookFill => "book_fill",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"reserve" => Ok(Self::Reserve),
			"charge" => Ok(Self::Charge),
			"book_fill" => Ok(Self::BookFill),
			other => Err(DomainError::Validation(format!("unknown mandate operation: {other}"))),
		}
	}
}

/// The furthest a dated mandate may end from its grant: five years.
pub const MAX_MANDATE_SECONDS: i64 = 5 * 366 * 24 * 60 * 60;

/// What a mandate allows the client to draw. `book_fill_24h` is set exactly when the mandate
/// lists BOOK_FILL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MandateCaps {
	pub per_op: Usdt,
	pub per_24h: Usdt,
	/// Σ of the client's ACTIVE holds at once — the cap a hold without a TTL cannot slip past
	/// by aging out of the 24 h window while it still holds the money.
	pub outstanding: Usdt,
	pub book_fill_24h: Option<Usdt>,
}

/// Everything a user grants, as asked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MandateTerms {
	pub user: UserId,
	pub scope: ScopeId,
	pub client: ClientId,
	pub caps: MandateCaps,
	/// Each operation once, in [`MandateOperation::ALL`] order.
	pub operations: Vec<MandateOperation>,
	/// Unix seconds; `None` = until revoked.
	pub expires_at: Option<i64>,
}

/// The rules every set of terms obeys, whoever it is for — a user's grant and a service's
/// request for one are checked alike. Returns the operations in [`MandateOperation::ALL`]
/// order.
fn check_terms(scope: &ScopeId, caps: &MandateCaps, operations: &[MandateOperation]) -> Result<Vec<MandateOperation>, DomainError> {
	if scope.is_main() {
		return Err(DomainError::Validation("a mandate is never granted in main".into()));
	}
	if caps.per_op.is_zero() {
		return Err(DomainError::Validation("cap_per_op must be positive".into()));
	}
	if caps.per_24h < caps.per_op {
		return Err(DomainError::Validation("cap_24h must be at least cap_per_op".into()));
	}
	if caps.outstanding < caps.per_op {
		return Err(DomainError::Validation("cap_outstanding must be at least cap_per_op".into()));
	}
	let mut ordered = Vec::with_capacity(operations.len());
	for operation in MandateOperation::ALL {
		match operations.iter().filter(|listed| **listed == operation).count() {
			0 => {}
			1 => ordered.push(operation),
			_ => return Err(DomainError::Validation(format!("operation {} is listed twice", operation.as_str()))),
		}
	}
	if ordered.is_empty() {
		return Err(DomainError::Validation("a mandate lists at least one operation".into()));
	}
	match (ordered.contains(&MandateOperation::BookFill), caps.book_fill_24h) {
		(true, None) => return Err(DomainError::Validation("cap_book_fill_24h is required with BOOK_FILL".into())),
		(true, Some(cap)) if cap.is_zero() => return Err(DomainError::Validation("cap_book_fill_24h must be positive".into())),
		(false, Some(_)) => return Err(DomainError::Validation("cap_book_fill_24h is set only with BOOK_FILL".into())),
		(true, Some(_)) | (false, None) => {}
	}
	Ok(ordered)
}

/// A grant that is over before it starts is a mistake, not a mandate; one that ends past
/// [`MAX_MANDATE_SECONDS`] is "until revoked" in disguise and must say so.
fn check_expiry_at(expires_at: Option<i64>, now: i64) -> Result<(), DomainError> {
	match expires_at {
		Some(at) if at <= now => Err(DomainError::Validation("expires_at must be in the future".into())),
		Some(at) if at > now.saturating_add(MAX_MANDATE_SECONDS) => Err(DomainError::Validation("expires_at must be within 5 years".into())),
		Some(_) | None => Ok(()),
	}
}

fn canonical_terms(scope: &ScopeId, client: &ClientId, caps: &MandateCaps, operations: &[MandateOperation], expires_at: Option<i64>) -> String {
	let operations: Vec<&str> = operations.iter().map(|operation| operation.as_str()).collect();
	[
		scope.as_str().to_owned(),
		client.as_str().to_owned(),
		caps.per_op.base_units().to_string(),
		caps.per_24h.base_units().to_string(),
		caps.outstanding.base_units().to_string(),
		caps.book_fill_24h.map(|cap| cap.base_units().to_string()).unwrap_or_default(),
		operations.join(","),
		expires_at.map(|at| at.to_string()).unwrap_or_default(),
	]
	.join("\n")
}

impl MandateTerms {
	/// Checks what holds whenever the terms are read: a mandate in `main` is meaningless (no
	/// client is ever granted there), caps must be positive and consistent, the operations a
	/// non-empty set, and a BOOK_FILL cap present exactly when BOOK_FILL is listed — sending
	/// the user's money to other users is never allowed by default.
	pub fn new(user: UserId, scope: ScopeId, client: ClientId, caps: MandateCaps, operations: &[MandateOperation], expires_at: Option<i64>) -> Result<Self, DomainError> {
		let ordered = check_terms(&scope, &caps, operations)?;
		Ok(Self {
			user,
			scope,
			client,
			caps,
			operations: ordered,
			expires_at,
		})
	}

	pub fn check_expiry(&self, now: i64) -> Result<(), DomainError> {
		check_expiry_at(self.expires_at, now)
	}

	pub fn lists(&self, operation: MandateOperation) -> bool {
		self.operations.contains(&operation)
	}

	/// Everything the terms say, rendered for the idempotency comparison: a repeat of the key
	/// with different terms is a conflict, not a repeat.
	pub fn canonical(&self) -> String {
		canonical_terms(&self.scope, &self.client, &self.caps, &self.operations, self.expires_at)
	}
}

/// What a scope's service asks a user to grant: a [`MandateTerms`] before there is a user.
/// The same rules hold, so a request that could never be granted is refused when it is made.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MandateRequestTerms {
	pub scope: ScopeId,
	pub client: ClientId,
	pub caps: MandateCaps,
	/// Each operation once, in [`MandateOperation::ALL`] order.
	pub operations: Vec<MandateOperation>,
	/// The proposed end of the mandate itself, unix seconds; `None` = until revoked.
	pub mandate_expires_at: Option<i64>,
}

impl MandateRequestTerms {
	pub fn new(scope: ScopeId, client: ClientId, caps: MandateCaps, operations: &[MandateOperation], mandate_expires_at: Option<i64>) -> Result<Self, DomainError> {
		let ordered = check_terms(&scope, &caps, operations)?;
		Ok(Self {
			scope,
			client,
			caps,
			operations: ordered,
			mandate_expires_at,
		})
	}

	pub fn check_expiry(&self, now: i64) -> Result<(), DomainError> {
		check_expiry_at(self.mandate_expires_at, now)
	}

	/// For the idempotency comparison, as [`MandateTerms::canonical`].
	pub fn canonical(&self) -> String {
		canonical_terms(&self.scope, &self.client, &self.caps, &self.operations, self.mandate_expires_at)
	}
}

/// A mandate request as recorded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MandateRequest {
	pub id: uuid::Uuid,
	pub terms: MandateRequestTerms,
	/// Unix seconds.
	pub created_at: i64,
	/// Unix seconds: until when the user may grant it.
	pub expires_at: i64,
}

/// A mandate as recorded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mandate {
	pub id: uuid::Uuid,
	pub terms: MandateTerms,
	/// Unix seconds.
	pub created_at: i64,
	pub revoked_at: Option<i64>,
	/// When a later grant for the same (user, scope, client) took its place.
	pub replaced_at: Option<i64>,
}

/// What the client has drawn on the user's compartment, read under the locks the draw is
/// decided under.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MandateUsage {
	/// Debits in the rolling 24 h: a hold placed in it at its full amount until it is
	/// released, then at what was captured of it; and what was captured in it of an older hold.
	pub drawn_24h: Usdt,
	/// What the client's active holds hold now.
	pub outstanding: Usdt,
	/// Captured from the client's holds on the compartment into other users' compartments in
	/// the rolling 24 h, under whichever mandate.
	pub book_filled_24h: Usdt,
}

impl Mandate {
	/// Neither revoked, nor replaced, nor past its expiry.
	pub fn is_live(&self, now: i64) -> bool {
		self.revoked_at.is_none() && self.replaced_at.is_none() && self.terms.expires_at.is_none_or(|at| at > now)
	}

	/// Whether `amount` may be drawn now by `operation` (a hold or a charge). Every refusal is
	/// `mandate_required`: the wire does not tell a client which cap it hit, so it cannot map
	/// the user's limits by probing.
	pub fn admit_draw(&self, operation: MandateOperation, amount: Usdt, usage: MandateUsage, now: i64) -> Result<(), LedgerRefusal> {
		let refuse = Err(LedgerRefusal::MandateRequired);
		if operation == MandateOperation::BookFill || !self.is_live(now) || !self.terms.lists(operation) || amount > self.terms.caps.per_op {
			return refuse;
		}
		if usage.drawn_24h.checked_add(amount).is_none_or(|drawn| drawn > self.terms.caps.per_24h) {
			return refuse;
		}
		// A charge leaves at once and holds nothing; only a hold adds to what stands.
		if operation == MandateOperation::Reserve && usage.outstanding.checked_add(amount).is_none_or(|held| held > self.terms.caps.outstanding) {
			return refuse;
		}
		Ok(())
	}

	/// The BOOK_FILL cap, when this mandate grants it and is not revoked.
	fn book_fill_cap(&self) -> Option<Usdt> {
		self.terms
			.caps
			.book_fill_24h
			.filter(|_| self.terms.lists(MandateOperation::BookFill) && self.revoked_at.is_none())
	}

	/// Whether `amount` of a hold placed under this mandate may go to another user. Both the
	/// mandate the hold was placed under and the user's `current` one for the client must grant
	/// BOOK_FILL, and the tighter of their caps bounds the rolling 24 h: a re-grant that drops
	/// BOOK_FILL or lowers its cap takes effect on holds already standing.
	pub fn admit_book_fill(&self, current: Option<&Mandate>, amount: Usdt, usage: MandateUsage, now: i64) -> Result<(), LedgerRefusal> {
		let refuse = Err(LedgerRefusal::MandateRequired);
		let Some(current) = current.filter(|current| current.is_live(now)) else { return refuse };
		let (Some(placed), Some(now_cap)) = (self.book_fill_cap(), current.book_fill_cap()) else {
			return refuse;
		};
		if usage.book_filled_24h.checked_add(amount).is_none_or(|filled| filled > placed.min(now_cap)) {
			return refuse;
		}
		Ok(())
	}

	/// When a hold placed now under this mandate expires: what the client asked
	/// (`ttl_seconds`, 0 = none), but never after the mandate itself.
	pub fn hold_expiry(&self, ttl_seconds: u32, now: i64) -> Option<i64> {
		let asked = (ttl_seconds > 0).then(|| now.saturating_add(i64::from(ttl_seconds)));
		match (asked, self.terms.expires_at) {
			(Some(asked), Some(limit)) => Some(asked.min(limit)),
			(Some(at), None) | (None, Some(at)) => Some(at),
			(None, None) => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn usdt(raw: &str) -> Usdt {
		Usdt::parse_decimal(raw).unwrap()
	}

	fn caps(per_op: &str, per_24h: &str, outstanding: &str, book_fill: Option<&str>) -> MandateCaps {
		MandateCaps {
			per_op: usdt(per_op),
			per_24h: usdt(per_24h),
			outstanding: usdt(outstanding),
			book_fill_24h: book_fill.map(usdt),
		}
	}

	fn terms(caps: MandateCaps, operations: &[MandateOperation], expires_at: Option<i64>) -> Result<MandateTerms, DomainError> {
		MandateTerms::new(UserId::new(), ScopeId::parse("invest").unwrap(), ClientId::parse("invest").unwrap(), caps, operations, expires_at)
	}

	fn mandate(caps: MandateCaps, operations: &[MandateOperation], expires_at: Option<i64>) -> Mandate {
		Mandate {
			id: uuid::Uuid::new_v4(),
			terms: terms(caps, operations, expires_at).unwrap(),
			created_at: 0,
			revoked_at: None,
			replaced_at: None,
		}
	}

	fn usage(drawn_24h: &str, outstanding: &str, book_filled_24h: &str) -> MandateUsage {
		MandateUsage {
			drawn_24h: usdt(drawn_24h),
			outstanding: usdt(outstanding),
			book_filled_24h: usdt(book_filled_24h),
		}
	}

	#[test]
	fn terms_refuse_what_cannot_be_a_mandate() {
		use MandateOperation::*;
		for (caps, operations, needle) in [
			(caps("0", "1", "1", None), vec![Reserve], "cap_per_op"),
			(caps("2", "1", "2", None), vec![Reserve], "cap_24h"),
			(caps("2", "2", "1", None), vec![Reserve], "cap_outstanding"),
			(caps("1", "1", "1", None), vec![], "at least one"),
			(caps("1", "1", "1", None), vec![Reserve, Reserve], "twice"),
			(caps("1", "1", "1", None), vec![BookFill], "required"),
			(caps("1", "1", "1", Some("0")), vec![BookFill], "positive"),
			(caps("1", "1", "1", Some("1")), vec![Reserve], "only with"),
		] {
			let err = terms(caps, &operations, None).expect_err(needle).to_string();
			assert!(err.contains(needle), "{needle}: {err}");
		}
		let main = MandateTerms::new(UserId::new(), ScopeId::main(), ClientId::parse("invest").unwrap(), caps("1", "1", "1", None), &[Reserve], None);
		assert!(main.is_err());
		let ordered = terms(caps("1", "1", "1", Some("1")), &[BookFill, Charge, Reserve], Some(10)).unwrap();
		assert_eq!(ordered.operations, MandateOperation::ALL, "stored in one order, so a repeat compares equal");
		assert!(ordered.check_expiry(10).is_err() && ordered.check_expiry(9).is_ok());
	}

	#[test]
	fn a_draw_stays_inside_every_cap() {
		use MandateOperation::*;
		let m = mandate(caps("10", "25", "15", None), &[Reserve, Charge], None);
		assert_eq!(m.admit_draw(Reserve, usdt("10"), usage("15", "5", "0"), 0), Ok(()));
		assert_eq!(m.admit_draw(Reserve, usdt("10.1"), MandateUsage::default(), 0), Err(LedgerRefusal::MandateRequired), "per op");
		assert_eq!(m.admit_draw(Reserve, usdt("10"), usage("15.1", "0", "0"), 0), Err(LedgerRefusal::MandateRequired), "24 h");
		assert_eq!(m.admit_draw(Reserve, usdt("10"), usage("0", "5.1", "0"), 0), Err(LedgerRefusal::MandateRequired), "outstanding");
		assert_eq!(m.admit_draw(Charge, usdt("10"), usage("0", "15", "0"), 0), Ok(()), "a charge holds nothing");
		assert_eq!(m.admit_draw(BookFill, usdt("1"), MandateUsage::default(), 0), Err(LedgerRefusal::MandateRequired));
		let reserve_only = mandate(caps("10", "25", "15", None), &[Reserve], None);
		assert_eq!(reserve_only.admit_draw(Charge, usdt("1"), MandateUsage::default(), 0), Err(LedgerRefusal::MandateRequired));
	}

	#[test]
	fn a_mandate_ends_by_revoke_replacement_or_expiry() {
		let live = mandate(caps("1", "1", "1", None), &[MandateOperation::Reserve], Some(100));
		assert!(live.is_live(99));
		assert!(!live.is_live(100));
		for ended in [
			Mandate {
				revoked_at: Some(1),
				..live.clone()
			},
			Mandate {
				replaced_at: Some(1),
				..live.clone()
			},
		] {
			assert_eq!(
				ended.admit_draw(MandateOperation::Reserve, usdt("1"), MandateUsage::default(), 0),
				Err(LedgerRefusal::MandateRequired)
			);
		}
	}

	#[test]
	fn a_book_fill_needs_both_the_holds_and_the_current_mandate_within_the_tighter_cap() {
		use MandateOperation::*;
		let m = mandate(caps("5", "5", "5", Some("3")), &[Reserve, BookFill], None);
		assert_eq!(m.admit_book_fill(Some(&m), usdt("1"), usage("0", "0", "2"), 0), Ok(()));
		assert_eq!(m.admit_book_fill(Some(&m), usdt("1.1"), usage("0", "0", "2"), 0), Err(LedgerRefusal::MandateRequired));
		assert_eq!(
			m.admit_book_fill(None, usdt("1"), MandateUsage::default(), 0),
			Err(LedgerRefusal::MandateRequired),
			"no current mandate"
		);
		let replaced = Mandate { replaced_at: Some(1), ..m.clone() };
		let lower = mandate(caps("5", "5", "5", Some("2")), &[Reserve, BookFill], None);
		assert_eq!(replaced.admit_book_fill(Some(&lower), usdt("2"), MandateUsage::default(), 0), Ok(()));
		assert_eq!(
			replaced.admit_book_fill(Some(&lower), usdt("2.1"), MandateUsage::default(), 0),
			Err(LedgerRefusal::MandateRequired),
			"a lower cap granted since binds the standing hold"
		);
		let dropped = mandate(caps("5", "5", "5", None), &[Reserve], None);
		assert_eq!(
			replaced.admit_book_fill(Some(&dropped), usdt("1"), MandateUsage::default(), 0),
			Err(LedgerRefusal::MandateRequired),
			"a re-grant without BOOK_FILL ends it for standing holds"
		);
		let plain = mandate(caps("5", "5", "5", None), &[Reserve], None);
		assert_eq!(plain.admit_book_fill(Some(&m), usdt("1"), MandateUsage::default(), 0), Err(LedgerRefusal::MandateRequired));
		let expiring = mandate(caps("5", "5", "5", Some("3")), &[Reserve, BookFill], Some(10));
		assert_eq!(m.admit_book_fill(Some(&expiring), usdt("1"), MandateUsage::default(), 10), Err(LedgerRefusal::MandateRequired));
	}

	#[test]
	fn a_dated_mandate_ends_within_five_years() {
		let now = 1_000;
		let at = |expires_at| terms(caps("1", "1", "1", None), &[MandateOperation::Reserve], Some(expires_at)).unwrap();
		assert!(at(now + MAX_MANDATE_SECONDS).check_expiry(now).is_ok());
		assert!(at(now + MAX_MANDATE_SECONDS + 1).check_expiry(now).is_err());
	}

	#[test]
	fn a_hold_never_outlives_its_mandate() {
		let open = mandate(caps("1", "1", "1", None), &[MandateOperation::Reserve], None);
		assert_eq!(open.hold_expiry(0, 50), None);
		assert_eq!(open.hold_expiry(10, 50), Some(60));
		let bounded = mandate(caps("1", "1", "1", None), &[MandateOperation::Reserve], Some(55));
		assert_eq!(bounded.hold_expiry(0, 50), Some(55), "no TTL is bounded by the mandate");
		assert_eq!(bounded.hold_expiry(10, 50), Some(55));
		assert_eq!(bounded.hold_expiry(3, 50), Some(53));
	}
}
