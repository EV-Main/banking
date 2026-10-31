//! Browser-facing JSON DTOs. These reproduce the exact wire shape the old TS BFF
//! emitted (proto-loader with `keepCase` + `longs: String`): snake_case fields, with
//! 64-bit integers rendered as strings so the committed `shared/contracts/gen` types
//! stay valid and the React fetch code is unchanged.

use evbanking_contracts::banking::v1 as bk;
use evconcierge_contracts::concierge::v1 as cc;
use serde::Serialize;

/// Declares one of the list DTOs: a `Vec` of an already-converted element type, the scalar
/// fields the wire shape carries alongside it, and the `From` that builds it from the proto
/// message. Every name is spelled out at the call site rather than derived from the DTO's,
/// because they need not line up — the field is not always the plural of the element type
/// (`PaymentConfirmationList.items`), and the source message need not be named `<Dto>`.
///
/// The second form names the source binding so extra fields can be given as expressions over
/// it, which is what a field needing more than a move requires (`total: r.total.to_string()`).
macro_rules! list_dto {
	($(#[$meta:meta])* $name:ident from $src:path { $field:ident: Vec<$elem:ty> $(,)? }) => {
		list_dto! { $(#[$meta])* $name from $src as l { $field: Vec<$elem> } }
	};
	(
		$(#[$meta:meta])*
		$name:ident from $src:path as $msg:ident {
			$field:ident: Vec<$elem:ty>
			$(, $(#[$xmeta:meta])* $extra:ident: $xty:ty = $value:expr)* $(,)?
		}
	) => {
		$(#[$meta])*
		#[derive(Serialize)]
		pub struct $name {
			pub $field: Vec<$elem>,
			$($(#[$xmeta])* pub $extra: $xty,)*
		}

		impl From<$src> for $name {
			fn from($msg: $src) -> Self {
				Self {
					$field: $msg.$field.into_iter().map(<$elem>::from).collect(),
					$($extra: $value,)*
				}
			}
		}
	};
}

// ── /api/auth/session ────────────────────────────────────────────────────────
// Note the camelCase principal: the old BFF mapped the proto `user_id` → `userId`
// for this endpoint only (the session principal), unlike the snake_case passthroughs.

// ── concierge: user profile + sessions ───────────────────────────────────────

#[derive(Serialize)]
pub struct UserProfile {
	pub user_id: String,
	pub email: String,
	pub email_verified: bool,
	pub status: String,
	pub token_version: String,
	pub legal_name: String,
	pub preferred_name: String,
	pub phone: String,
	pub date_of_birth: String,
	pub nationality: String,
	pub tax_residence: String,
	pub residential_address: String,
	pub language: String,
	pub base_currency: String,
	pub timezone: String,
	pub kyc_level: u32,
	pub role: String,
	/// True when `role` came from the OWNER_SUBJECTS emergency allowlist rather than
	/// the persisted `users.role`; a client must say so.
	pub role_is_break_glass: bool,
	/// WHY the account is disabled — and therefore which control a client may offer.
	/// `"admin_hold"` lapses and one admin lifts it; `"governance"` never lapses and only
	/// the owners lift it; EMPTY on an active user and also on one suspended before the
	/// field existed, which carries the old one-act, never-lapsing semantics. Three cases,
	/// not two: a boolean here would silently file the third under whichever of the other
	/// two it was compared against.
	pub suspended_by: String,
	/// Unix seconds an `admin_hold` lapses; `"0"` when nothing lapses. A string like every
	/// other stamp the BFF emits.
	pub hold_expires_at: String,
	/// What the caller may do, concrete (no alias, no wildcard). Only `/api/users` (GetMe)
	/// fills it; every other profile the BFF relays carries it empty, which means "not
	/// reported", not "holds none".
	pub permissions: Vec<String>,
}

impl From<cc::UserProfile> for UserProfile {
	fn from(p: cc::UserProfile) -> Self {
		Self {
			user_id: p.user_id,
			email: p.email,
			email_verified: p.email_verified,
			status: p.status,
			token_version: p.token_version.to_string(),
			legal_name: p.legal_name,
			preferred_name: p.preferred_name,
			phone: p.phone,
			date_of_birth: p.date_of_birth,
			nationality: p.nationality,
			tax_residence: p.tax_residence,
			residential_address: p.residential_address,
			language: p.language,
			base_currency: p.base_currency,
			timezone: p.timezone,
			kyc_level: p.kyc_level,
			role: p.role,
			role_is_break_glass: p.role_is_break_glass,
			suspended_by: p.suspended_by,
			hold_expires_at: p.hold_expires_at.to_string(),
			permissions: p.permissions,
		}
	}
}

// ── piggybank: wallet ────────────────────────────────────────────────────────

/// The unified balance by lifecycle: `total = available + pending_withdrawal +
/// in_compartments`. `in_compartments` is the caller's money in scope compartments — still
/// theirs, so inside `total` and outside `available`.
#[derive(Default, Serialize)]
pub struct Balance {
	pub available: String,
	pub pending_withdrawal: String,
	pub in_compartments: String,
	pub total: String,
}

impl From<bk::Balance> for Balance {
	fn from(b: bk::Balance) -> Self {
		Self {
			available: b.available,
			pending_withdrawal: b.pending_withdrawal,
			in_compartments: b.in_compartments,
			total: b.total,
		}
	}
}

#[derive(Serialize)]
pub struct DepositAddress {
	pub network: String,
	pub address: String,
	pub min_confirmations: u32,
	pub is_testnet: bool,
}

impl From<bk::DepositAddress> for DepositAddress {
	fn from(d: bk::DepositAddress) -> Self {
		Self {
			network: d.network,
			address: d.address,
			min_confirmations: d.min_confirmations,
			is_testnet: d.is_testnet,
		}
	}
}

#[derive(Serialize)]
pub struct NetworkWithdrawable {
	pub network: String,
	pub withdrawable: String,
	pub instant: String,
	pub min_withdrawal: String,
	pub withdrawal_fee: String,
}

impl From<bk::NetworkWithdrawable> for NetworkWithdrawable {
	fn from(n: bk::NetworkWithdrawable) -> Self {
		Self {
			network: n.network,
			withdrawable: n.withdrawable,
			instant: n.instant,
			min_withdrawal: n.min_withdrawal,
			withdrawal_fee: n.withdrawal_fee,
		}
	}
}

#[derive(Serialize)]
pub struct Wallet {
	pub balance: Balance,
	pub deposit_addresses: Vec<DepositAddress>,
	pub withdrawable: Vec<NetworkWithdrawable>,
}

impl From<bk::Wallet> for Wallet {
	fn from(w: bk::Wallet) -> Self {
		Self {
			balance: w.balance.map(Balance::from).unwrap_or_default(),
			deposit_addresses: w.deposit_addresses.into_iter().map(DepositAddress::from).collect(),
			withdrawable: w.withdrawable.into_iter().map(NetworkWithdrawable::from).collect(),
		}
	}
}

#[derive(Serialize)]
pub struct Withdrawal {
	pub id: String,
	pub network: String,
	pub address: String,
	pub amount: String,
	pub fee: String,
	pub net_amount: String,
	pub state: String,
	pub tx_ref: String,
}

impl From<bk::Withdrawal> for Withdrawal {
	fn from(w: bk::Withdrawal) -> Self {
		Self {
			id: w.id,
			network: w.network,
			address: w.address,
			amount: w.amount,
			fee: w.fee,
			net_amount: w.net_amount,
			state: w.state,
			tx_ref: w.tx_ref,
		}
	}
}

list_dto! { WithdrawalList from bk::WithdrawalList { withdrawals: Vec<Withdrawal> } }

#[derive(Serialize)]
pub struct Deposit {
	pub tx_ref: String,
	pub network: String,
	pub amount: String,
	pub created_at: String,
}

impl From<bk::Deposit> for Deposit {
	fn from(d: bk::Deposit) -> Self {
		Self {
			tx_ref: d.tx_ref,
			network: d.network,
			amount: d.amount,
			created_at: d.created_at.to_string(),
		}
	}
}

list_dto! { DepositList from bk::DepositList { deposits: Vec<Deposit> } }

/// A scope's invoice as its would-be payer sees it. The amount and the payee are the
/// server's, never the page's: the browser only names the invoice.
#[derive(Serialize)]
pub struct PayableInvoice {
	pub invoice_id: String,
	pub scope: String,
	pub display_name: String,
	pub amount: String,
	pub memo: String,
	pub state: String,
	pub expires_at: String,
	pub paid_at: String,
	pub paid_from: String,
	pub available_main: String,
	pub available_scope: String,
}

impl From<bk::PayableInvoice> for PayableInvoice {
	fn from(i: bk::PayableInvoice) -> Self {
		Self {
			invoice_id: i.invoice_id,
			scope: i.scope,
			display_name: i.display_name,
			amount: i.amount,
			memo: i.memo,
			state: i.state,
			expires_at: i.expires_at.to_string(),
			paid_at: i.paid_at.to_string(),
			paid_from: i.paid_from,
			available_main: i.available_main,
			available_scope: i.available_scope,
		}
	}
}

/// One of the caller's mandates: what a scope's service may draw on their compartment there.
/// Caps and usage are decimal USDT strings; `operations` are `reserve`, `charge`, `book_fill`.
#[derive(Serialize)]
pub struct Mandate {
	pub mandate_id: String,
	pub scope: String,
	pub client_id: String,
	pub client_display_name: String,
	pub cap_per_op: String,
	pub cap_24h: String,
	pub cap_outstanding: String,
	pub cap_book_fill_24h: String,
	pub operations: Vec<&'static str>,
	/// Unix seconds as strings; `"0"` = none.
	pub expires_at: String,
	pub created_at: String,
	pub revoked_at: String,
	pub replaced_at: String,
	pub used_24h: String,
	pub outstanding: String,
	pub book_filled_24h: String,
}

/// The wire name of a mandate operation, as the page sends it back on a grant.
pub fn mandate_operation_name(raw: i32) -> &'static str {
	match bk::MandateOperation::try_from(raw) {
		Ok(bk::MandateOperation::Reserve) => "reserve",
		Ok(bk::MandateOperation::Charge) => "charge",
		Ok(bk::MandateOperation::BookFill) => "book_fill",
		Ok(bk::MandateOperation::Unspecified) | Err(_) => "unspecified",
	}
}

impl From<bk::Mandate> for Mandate {
	fn from(m: bk::Mandate) -> Self {
		Self {
			mandate_id: m.mandate_id,
			scope: m.scope,
			client_id: m.client_id,
			client_display_name: m.client_display_name,
			cap_per_op: m.cap_per_op,
			cap_24h: m.cap_24h,
			cap_outstanding: m.cap_outstanding,
			cap_book_fill_24h: m.cap_book_fill_24h,
			operations: m.operations.into_iter().map(mandate_operation_name).collect(),
			expires_at: m.expires_at.to_string(),
			created_at: m.created_at.to_string(),
			revoked_at: m.revoked_at.to_string(),
			replaced_at: m.replaced_at.to_string(),
			used_24h: m.used_24h,
			outstanding: m.outstanding,
			book_filled_24h: m.book_filled_24h,
		}
	}
}

/// What a scope's service asks the caller to grant, as banking stored it — the whole consent
/// screen: the page names only the request id, never the limits. Caps are decimal USDT strings
/// (`cap_book_fill_24h` `""` unless `operations` lists `book_fill`); times are unix seconds as
/// strings, `mandate_expires_at` `"0"` = until revoked.
#[derive(Serialize)]
pub struct MandateRequest {
	pub request_id: String,
	pub scope: String,
	pub client_id: String,
	pub client_display_name: String,
	pub cap_per_op: String,
	pub cap_24h: String,
	pub cap_outstanding: String,
	pub cap_book_fill_24h: String,
	pub operations: Vec<&'static str>,
	pub mandate_expires_at: String,
	pub expires_at: String,
}

impl From<bk::MandateRequest> for MandateRequest {
	fn from(r: bk::MandateRequest) -> Self {
		Self {
			request_id: r.request_id,
			scope: r.scope,
			client_id: r.client_id,
			client_display_name: r.client_display_name,
			cap_per_op: r.cap_per_op,
			cap_24h: r.cap_24h,
			cap_outstanding: r.cap_outstanding,
			cap_book_fill_24h: r.cap_book_fill_24h,
			operations: r.operations.into_iter().map(mandate_operation_name).collect(),
			mandate_expires_at: r.mandate_expires_at.to_string(),
			expires_at: r.expires_at.to_string(),
		}
	}
}

list_dto! { MandateList from bk::MandateList { mandates: Vec<Mandate> } }

/// What it takes to move money into a scope. Limits are decimal USDT strings, `""` = none.
#[derive(Serialize)]
pub struct ScopePolicy {
	pub scope: String,
	pub min_kyc_level: u32,
	pub max_per_move: String,
	pub max_moved_in_24h: String,
	pub max_balance: String,
	pub caller_eligible: bool,
	/// Unix seconds as a string.
	pub updated_at: String,
	pub auto_payout_per_op: String,
	pub auto_payout_24h: String,
	pub auto_transfer_24h: String,
}

impl From<bk::ScopePolicy> for ScopePolicy {
	fn from(p: bk::ScopePolicy) -> Self {
		Self {
			scope: p.scope,
			min_kyc_level: p.min_kyc_level,
			max_per_move: p.max_per_move,
			max_moved_in_24h: p.max_moved_in_24h,
			max_balance: p.max_balance,
			caller_eligible: p.caller_eligible,
			updated_at: p.updated_at.to_string(),
			auto_payout_per_op: p.auto_payout_per_op,
			auto_payout_24h: p.auto_payout_24h,
			auto_transfer_24h: p.auto_transfer_24h,
		}
	}
}

/// One compartment of the caller: `main` or a scope. `policy` is `null` for `main`.
#[derive(Serialize)]
pub struct Compartment {
	pub scope: String,
	pub display_name: String,
	pub available: String,
	pub held: String,
	pub total: String,
	pub policy: Option<ScopePolicy>,
}

impl From<bk::Compartment> for Compartment {
	fn from(c: bk::Compartment) -> Self {
		Self {
			scope: c.scope,
			display_name: c.display_name,
			available: c.available,
			held: c.held,
			total: c.total,
			policy: c.policy.map(ScopePolicy::from),
		}
	}
}

list_dto! { CompartmentList from bk::CompartmentList { compartments: Vec<Compartment> } }

/// A move between `main` and one scope. `state` is `pending`, `posted` or `failed`;
/// `failure_reason` is set only when `failed`.
#[derive(Serialize)]
pub struct CompartmentMove {
	pub id: String,
	pub from_scope: String,
	pub to_scope: String,
	pub amount: String,
	/// Unix seconds as a string.
	pub created_at: String,
	pub state: String,
	pub failure_reason: String,
}

impl From<bk::CompartmentMove> for CompartmentMove {
	fn from(m: bk::CompartmentMove) -> Self {
		Self {
			id: m.id,
			from_scope: m.from_scope,
			to_scope: m.to_scope,
			amount: m.amount,
			created_at: m.created_at.to_string(),
			state: m.state,
			failure_reason: m.failure_reason,
		}
	}
}

// ── piggybank: operations (the unified activity timeline) ────────────────────

/// One row of the caller's activity timeline. `kind` is the discriminator: it decides
/// which of the remaining fields carry a value, and the rest arrive as empty strings
/// (proto3 defaults). Passed through verbatim — the BFF adds no interpretation to a
/// read model that already has exactly one shape.
#[derive(Serialize)]
pub struct Operation {
	pub id: String,
	pub kind: String,
	pub state: String,
	/// Unix seconds, rendered as a string like every other timestamp on this surface —
	/// an i64 does not survive JSON's 2^53 in every client.
	pub created_at: String,
	pub amount: String,
	pub fee: String,
	pub net_amount: String,
	pub network: String,
	pub address: String,
	pub tx_ref: String,
	/// The compartment the row belongs to (`main` for every kind the feed carries).
	pub scope: String,
}

impl From<bk::Operation> for Operation {
	fn from(o: bk::Operation) -> Self {
		Self {
			id: o.id,
			kind: o.kind,
			state: o.state,
			created_at: o.created_at.to_string(),
			amount: o.amount,
			fee: o.fee,
			net_amount: o.net_amount,
			network: o.network,
			address: o.address,
			tx_ref: o.tx_ref,
			scope: o.scope,
		}
	}
}

list_dto! {
	OperationList from bk::OperationList as l {
		operations: Vec<Operation>,
		/// True when the history is longer than the page returned.
		truncated: bool = l.truncated,
	}
}

// ── shared by the redacted surfaces ──────────────────────────────────────────

/// A proto enum on the browser wire: the generated `as_str_name()` minus its
/// `SCREAMING_PREFIX_`, lowercased. Derived rather than hand-matched so a variant added
/// to the proto reaches the browser as itself instead of falling through a stale arm.
fn enum_label(name: &str, prefix: &str) -> String {
	name.strip_prefix(prefix).unwrap_or(name).to_ascii_lowercase()
}

/// `ada@example.com` → `a***@example.com`. Applied on every redacted surface a non-owner
/// can reach; idempotent, so re-masking what the plane already masked is safe and this
/// stays a second line of defense rather than a substitute for the plane's own. A value
/// with no `@`, or with no local part, is replaced wholesale — something that is not an
/// address is not something to reveal a prefix of.
fn mask_email(email: &str) -> String {
	if email.is_empty() {
		return String::new();
	}
	match email.split_once('@') {
		// `chars().next()` and not `&local[..1]`: a non-ASCII local part would put that
		// slice inside a UTF-8 character and panic.
		Some((local, domain)) if !domain.is_empty() => match local.chars().next() {
			Some(first) => format!("{first}***@{domain}"),
			None => "***".to_string(),
		},
		_ => "***".to_string(),
	}
}

// ── payments: the order that moves money between two named ends ──────────────
//
// Unlike the older passthroughs, the proto's "empty means unset" sentinels cross as `null`
// here: the screen renders what it is given and never has to know which zero means "not yet".

/// A string the proto leaves empty when the field does not apply.
fn non_empty(value: String) -> Option<String> {
	(!value.is_empty()).then_some(value)
}

/// One end of a payment, as every approval surface describes it. `label` is the wording
/// shared with the mails; the structured fields let a screen link to the account, and
/// stay EMPTY (not `null`) where they do not apply, exactly as the proto carries them.
#[derive(Default, Serialize)]
pub struct PaymentEnd {
	pub label: String,
	/// `user` | `treasury`, or `external` for an address.
	pub kind: String,
	pub id: String,
	pub network: String,
	pub address: String,
	/// What a person recognises this end BY: the receiving investor's masked mailbox. Never
	/// part of the digest.
	pub detail: String,
}

impl From<bk::PaymentEnd> for PaymentEnd {
	fn from(e: bk::PaymentEnd) -> Self {
		Self {
			label: e.label,
			kind: e.kind,
			id: e.id,
			network: e.network,
			address: e.address,
			detail: e.detail,
		}
	}
}

/// A payment out of the caller's own claim, as they are shown it to answer: the terms and
/// nothing about the operator beyond a masked address. `decision` is `pending` until they
/// answer; `invalidated` means the order can no longer be answered or executed.
#[derive(Default, Serialize)]
pub struct PaymentConfirmation {
	pub payment_id: String,
	pub state: String,
	pub tier: String,
	pub source: PaymentEnd,
	pub destination: PaymentEnd,
	pub amount: String,
	pub reason: String,
	pub payload_hash: String,
	pub initiator_email: String,
	pub expires_at: String,
	pub decision: String,
	pub invalidated: bool,
	pub failure_reason: Option<String>,
}

impl From<bk::PaymentConfirmation> for PaymentConfirmation {
	fn from(c: bk::PaymentConfirmation) -> Self {
		let state = enum_label(c.state().as_str_name(), "PAYMENT_STATE_");
		let decision = enum_label(c.decision().as_str_name(), "CONSENT_DECISION_");
		Self {
			payment_id: c.payment_id,
			state,
			tier: c.tier,
			source: c.source.map(PaymentEnd::from).unwrap_or_default(),
			destination: c.destination.map(PaymentEnd::from).unwrap_or_default(),
			amount: c.amount,
			reason: c.reason,
			payload_hash: c.payload_hash,
			initiator_email: mask_email(&c.initiator_email),
			expires_at: c.expires_at.to_string(),
			decision,
			invalidated: c.invalidated,
			failure_reason: non_empty(c.failure_reason),
		}
	}
}

list_dto! { PaymentConfirmationList from bk::PaymentConfirmationList { items: Vec<PaymentConfirmation> } }

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn masking_keeps_one_character_and_is_idempotent() {
		assert_eq!(mask_email("ada@example.com"), "a***@example.com");
		assert_eq!(mask_email("a***@example.com"), "a***@example.com");
		assert_eq!(mask_email(""), "");
		assert_eq!(mask_email("not-an-address"), "***");
		assert_eq!(mask_email("@example.com"), "***");
		assert_eq!(mask_email("ada@"), "***");
	}

	/// The wallet's lifecycle figures cross one-to-one.
	#[test]
	fn the_balance_carries_every_lifecycle_figure() {
		let balance = Balance::from(bk::Balance {
			available: "87.88".into(),
			pending_withdrawal: "2".into(),
			total: "99.88".into(),
			in_compartments: "10".into(),
		});
		assert_eq!(
			(
				balance.available.as_str(),
				balance.pending_withdrawal.as_str(),
				balance.in_compartments.as_str(),
				balance.total.as_str()
			),
			("87.88", "2", "10", "99.88")
		);
	}

	/// `/api/users` carries the caller's permissions from GetMe, so the wallet host can offer
	/// the panel link without a second round trip.
	#[test]
	fn the_profile_carries_the_callers_permissions() {
		let profile = UserProfile::from(cc::UserProfile {
			permissions: vec!["sa:work:leads:read".into(), "bank:treasury:read".into()],
			..Default::default()
		});
		assert_eq!(profile.permissions, ["sa:work:leads:read", "bank:treasury:read"]);
	}

	/// A non-ASCII local part must not be sliced mid-character — a byte slice would panic
	/// on one, and a panic inside a handler is a 500 on a page a stranger can reach.
	#[test]
	fn masking_survives_a_non_ascii_local_part() {
		assert_eq!(mask_email("ада@example.com"), "а***@example.com");
	}

	/// The browser vocabulary is derived from the proto, so a state added upstream
	/// arrives as itself rather than through a stale hand-written arm.
	#[test]
	fn enum_labels_are_the_proto_name_without_its_prefix() {
		assert_eq!(enum_label(bk::PaymentState::Pending.as_str_name(), "PAYMENT_STATE_"), "pending");
		assert_eq!(enum_label(bk::PaymentState::ExecutionFailed.as_str_name(), "PAYMENT_STATE_"), "execution_failed");
		assert_eq!(enum_label(bk::ConsentDecision::Approve.as_str_name(), "CONSENT_DECISION_"), "approve");
	}

	/// The wallet host parses every stamp as unix seconds (`shared/lib/unix-stamp.ts`).
	#[test]
	fn a_payment_deadline_crosses_as_unix_seconds() {
		let confirmation = PaymentConfirmation::from(bk::PaymentConfirmation {
			expires_at: 1_757_669_400,
			..Default::default()
		});
		assert_eq!(confirmation.expires_at, "1757669400");
	}
}
