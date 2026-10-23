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
/// (`ConsiliumList.items`), and the source message need not be named `<Dto>`.
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
	/// the persisted `users.role`. A client must say so: an "owner" the consilium
	/// has never heard of is exactly the divergence this whole change removes.
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

/// The unified balance by lifecycle: `total = available + in_orders + invested +
/// pending_withdrawal + in_compartments`. `in_orders` is the cash the book holds for the
/// caller's resting buy orders, `in_compartments` the caller's money in scope compartments —
/// both still theirs, so inside `total` and outside `available`.
#[derive(Default, Serialize)]
pub struct Balance {
	pub available: String,
	pub in_orders: String,
	pub invested: String,
	pub pending_withdrawal: String,
	pub in_compartments: String,
	pub total: String,
}

impl From<bk::Balance> for Balance {
	fn from(b: bk::Balance) -> Self {
		Self {
			available: b.available,
			in_orders: b.in_orders,
			invested: b.invested,
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
	pub units: String,
	pub nav: String,
	pub service: String,
	pub network: String,
	pub address: String,
	pub tx_ref: String,
	/// fee only — the two legs of a charge, so a timeline row can say whether it was rent
	/// on the capital or a share of the gain rather than only how much was taken.
	pub management: String,
	pub performance: String,
	/// The compartment the row belongs to (`main` for every kind the feed carries today).
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
			units: o.units,
			nav: o.nav,
			service: o.service,
			network: o.network,
			address: o.address,
			tx_ref: o.tx_ref,
			management: o.management,
			performance: o.performance,
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

// ── consilium: multi-owner authorization (money plane) ───────────────────────

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
pub fn mask_email(email: &str) -> String {
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

#[derive(Serialize)]
pub struct ConsiliumVoter {
	pub user_id: String,
	pub email: String,
	pub decision: String,
	pub decided_at: String,
	pub notified: bool,
}

impl From<bk::ConsiliumVoter> for ConsiliumVoter {
	fn from(v: bk::ConsiliumVoter) -> Self {
		let decision = enum_label(v.decision().as_str_name(), "VOTE_DECISION_");
		Self {
			user_id: v.user_id,
			email: v.email,
			decision,
			decided_at: v.decided_at.to_string(),
			notified: v.notified,
		}
	}
}

/// The immutable subject of a PAYMENT consilium — a projection of the order, not the
/// order. The plane renders both ends as LABELS (the wording the approval mail carries),
/// so each is delivered as a [`PaymentEnd`] with only `label` populated: the consilium
/// screen and the payments screen then read one shape, and the label is the only part of
/// it an approval is bound to.
#[derive(Serialize)]
pub struct ConsiliumPaymentTerms {
	pub payment_id: String,
	pub tier: String,
	pub source: PaymentEnd,
	pub destination: PaymentEnd,
	pub amount: String,
	pub reason: String,
}

impl From<bk::ConsiliumPaymentTerms> for ConsiliumPaymentTerms {
	fn from(t: bk::ConsiliumPaymentTerms) -> Self {
		Self {
			payment_id: t.payment_id,
			tier: t.tier,
			source: PaymentEnd::labelled(t.source),
			destination: PaymentEnd::labelled(t.destination),
			amount: t.amount,
			reason: t.reason,
		}
	}
}

/// The immutable subject of a VALUATION-OVERRIDE consilium (banking#232): the fund and the
/// AUM to mark it at, past the NAV-move guard. Both are covered by `payload_hash`.
#[derive(Serialize)]
pub struct ValuationOverrideTerms {
	pub service: String,
	pub aum: String,
}

impl From<bk::ValuationOverrideTerms> for ValuationOverrideTerms {
	fn from(t: bk::ValuationOverrideTerms) -> Self {
		Self { service: t.service, aum: t.aum }
	}
}

/// One set of fee terms as a consilium states them.
#[derive(Serialize)]
pub struct FeeTerms {
	pub management_bps: u32,
	pub performance_bps: u32,
	pub hurdle_bps: u32,
	pub basis: String,
	pub crystallization: String,
}

/// The immutable subject of a FEE-POLICY consilium: which change, over which fund, from
/// what terms (`null` when the fund charged nothing) to what terms, and why. `holder_count`
/// and `allocation_name` are read live beside the hashed subject.
#[derive(Serialize)]
pub struct ConsiliumFeePolicyTerms {
	pub change_id: String,
	pub service: String,
	pub allocation_name: String,
	pub from: Option<FeeTerms>,
	pub to: FeeTerms,
	/// Unix seconds the requester asked the change to bind from; `"0"` = as soon as allowed.
	pub effective_from: String,
	pub holder_count: u32,
	pub reason: String,
}

impl From<bk::ConsiliumFeePolicyTerms> for ConsiliumFeePolicyTerms {
	fn from(t: bk::ConsiliumFeePolicyTerms) -> Self {
		Self {
			change_id: t.change_id,
			service: t.service,
			allocation_name: t.allocation_name,
			from: t.from_configured.then_some(FeeTerms {
				management_bps: t.from_management_bps,
				performance_bps: t.from_performance_bps,
				hurdle_bps: t.from_hurdle_bps,
				basis: t.from_basis,
				crystallization: t.from_crystallization,
			}),
			to: FeeTerms {
				management_bps: t.to_management_bps,
				performance_bps: t.to_performance_bps,
				hurdle_bps: t.to_hurdle_bps,
				basis: t.to_basis,
				crystallization: t.to_crystallization,
			},
			effective_from: t.effective_from.to_string(),
			holder_count: t.holder_count,
			reason: t.reason,
		}
	}
}

/// The immutable subject of a holder-grant consilium (#245): `units` of the reserved
/// `allocation` (`fee` | `fund`) for `user_id` — the money-plane id the terms stored.
#[derive(Serialize)]
pub struct HolderGrantTerms {
	pub allocation: String,
	pub user_id: String,
	pub units: String,
}

impl From<bk::HolderGrantTerms> for HolderGrantTerms {
	fn from(t: bk::HolderGrantTerms) -> Self {
		Self {
			allocation: t.allocation,
			user_id: t.user_id,
			units: t.units,
		}
	}
}

/// The immutable subject of a seed-capital consilium (#245): the chain transfer
/// `tx_ref` on `network`, worth `amount`, is `depositor_user_id`'s — booked as their
/// deposit and subscription into `fund` once the quorum carries. The reference is
/// carried in FULL, as a payout's address is.
#[derive(Serialize)]
pub struct SeedCapitalTerms {
	pub tx_ref: String,
	pub network: String,
	pub amount: String,
	pub depositor_user_id: String,
}

impl From<bk::SeedCapitalTerms> for SeedCapitalTerms {
	fn from(t: bk::SeedCapitalTerms) -> Self {
		Self {
			tx_ref: t.tx_ref,
			network: t.network,
			amount: t.amount,
			depositor_user_id: t.depositor_user_id,
		}
	}
}

/// One consilium in full — the owner-only view, with the per-voter breakdown.
///
/// Exactly one of `payment`, `valuation_override`, `fee_policy`, `holder_grant` and
/// `seed_capital` describes the subject; the others are `null` — never an empty object,
/// which the invitation page treats as unrenderable — so a screen tells the kinds apart by
/// which sibling is set. (`revenue_payout` left with the kind, #245 / C-9.)
#[derive(Serialize)]
pub struct Consilium {
	pub id: String,
	pub state: String,
	pub payment: Option<ConsiliumPaymentTerms>,
	pub valuation_override: Option<ValuationOverrideTerms>,
	pub fee_policy: Option<ConsiliumFeePolicyTerms>,
	/// Set exactly when this is a HOLDER_GRANT consilium.
	pub holder_grant: Option<HolderGrantTerms>,
	/// Set exactly when this is a SEED_CAPITAL consilium.
	pub seed_capital: Option<SeedCapitalTerms>,
	pub payload_hash: String,
	pub initiator_user_id: String,
	pub initiator_email: String,
	pub owner_count: u32,
	pub threshold: u32,
	pub approvals: u32,
	pub rejections: u32,
	pub voters: Vec<ConsiliumVoter>,
	pub created_at: String,
	pub expires_at: String,
	pub decided_at: String,
	pub executed_withdrawal_id: String,
	/// The order an executed PAYMENT consilium carried; `null` otherwise.
	pub executed_payment_id: Option<String>,
	/// The mark an executed VALUATION-OVERRIDE consilium recorded; `null` otherwise.
	pub executed_valuation_id: Option<String>,
	/// The change an executed FEE_POLICY consilium scheduled; `null` otherwise.
	pub executed_fee_policy_change_id: Option<String>,
	/// The in-kind issuance an executed HOLDER_GRANT consilium minted; `null` otherwise.
	pub executed_issuance_id: Option<String>,
	/// The `fund` subscription an executed SEED_CAPITAL consilium booked; `null` otherwise.
	pub executed_subscription_id: Option<String>,
	pub failure_reason: String,
	/// Monotonic per consilium. The live page watches this and refetches when it moves.
	pub version: String,
}

impl From<bk::Consilium> for Consilium {
	fn from(c: bk::Consilium) -> Self {
		let state = enum_label(c.state().as_str_name(), "CONSILIUM_STATE_");
		Self {
			id: c.id,
			state,
			payment: c.payment.map(ConsiliumPaymentTerms::from),
			valuation_override: c.valuation_override.map(ValuationOverrideTerms::from),
			fee_policy: c.fee_policy.map(ConsiliumFeePolicyTerms::from),
			holder_grant: c.holder_grant.map(HolderGrantTerms::from),
			seed_capital: c.seed_capital.map(SeedCapitalTerms::from),
			payload_hash: c.payload_hash,
			initiator_user_id: c.initiator_user_id,
			initiator_email: c.initiator_email,
			owner_count: c.owner_count,
			threshold: c.threshold,
			approvals: c.approvals,
			rejections: c.rejections,
			voters: c.voters.into_iter().map(ConsiliumVoter::from).collect(),
			created_at: c.created_at.to_string(),
			expires_at: c.expires_at.to_string(),
			decided_at: c.decided_at.to_string(),
			executed_withdrawal_id: c.executed_withdrawal_id,
			executed_payment_id: non_empty(c.executed_payment_id),
			executed_valuation_id: non_empty(c.executed_valuation_id),
			executed_fee_policy_change_id: non_empty(c.executed_fee_policy_change_id),
			executed_issuance_id: non_empty(c.executed_issuance_id),
			executed_subscription_id: non_empty(c.executed_subscription_id),
			failure_reason: c.failure_reason,
			version: c.version.to_string(),
		}
	}
}

list_dto! { ConsiliumList from bk::ConsiliumList { items: Vec<Consilium> } }

/// What the emailed owner is shown before voting. Deliberately narrower than
/// [`Consilium`]: no other owner's identity and no vote-by-vote breakdown — and both
/// addresses are masked, because this is the one surface reachable with no session.
#[derive(Default, Serialize)]
pub struct ConsiliumInvitation {
	pub consilium_id: String,
	pub state: String,
	/// Set exactly when this is a PAYMENT consilium — see [`Consilium`].
	pub payment: Option<ConsiliumPaymentTerms>,
	/// Set exactly when this is a VALUATION-OVERRIDE consilium — see [`Consilium`].
	pub valuation_override: Option<ValuationOverrideTerms>,
	/// Set exactly when this is a FEE_POLICY consilium — see [`Consilium`].
	pub fee_policy: Option<ConsiliumFeePolicyTerms>,
	/// Set exactly when this is a HOLDER_GRANT consilium — see [`Consilium`].
	pub holder_grant: Option<HolderGrantTerms>,
	/// Set exactly when this is a SEED_CAPITAL consilium — see [`Consilium`].
	pub seed_capital: Option<SeedCapitalTerms>,
	pub payload_hash: String,
	pub initiator_email: String,
	pub voter_email: String,
	pub threshold: u32,
	pub approvals: u32,
	pub owner_count: u32,
	pub created_at: String,
	pub expires_at: String,
	pub decision: String,
	pub attempts_remaining: u32,
}

impl From<bk::ConsiliumInvitation> for ConsiliumInvitation {
	fn from(i: bk::ConsiliumInvitation) -> Self {
		let state = enum_label(i.state().as_str_name(), "CONSILIUM_STATE_");
		let decision = enum_label(i.decision().as_str_name(), "VOTE_DECISION_");
		Self {
			consilium_id: i.consilium_id,
			state,
			payment: i.payment.map(ConsiliumPaymentTerms::from),
			valuation_override: i.valuation_override.map(ValuationOverrideTerms::from),
			fee_policy: i.fee_policy.map(ConsiliumFeePolicyTerms::from),
			holder_grant: i.holder_grant.map(HolderGrantTerms::from),
			seed_capital: i.seed_capital.map(SeedCapitalTerms::from),
			payload_hash: i.payload_hash,
			initiator_email: mask_email(&i.initiator_email),
			voter_email: mask_email(&i.voter_email),
			threshold: i.threshold,
			approvals: i.approvals,
			owner_count: i.owner_count,
			created_at: i.created_at.to_string(),
			expires_at: i.expires_at.to_string(),
			decision,
			attempts_remaining: i.attempts_remaining,
		}
	}
}

/// The invitation as it stands after a vote, plus whether this vote carried the verdict.
#[derive(Serialize)]
pub struct ConsiliumDecision {
	pub invitation: ConsiliumInvitation,
	pub decided: bool,
}

impl From<bk::SubmitDecisionResponse> for ConsiliumDecision {
	fn from(r: bk::SubmitDecisionResponse) -> Self {
		Self {
			invitation: r.invitation.map(ConsiliumInvitation::from).unwrap_or_default(),
			decided: r.decided,
		}
	}
}

// ── payments: the order that moves money between two named ends ──────────────
//
// The one family on this wire whose browser contract was fixed BEFORE the DTOs were
// written — the payments and consent screens are built to it in parallel — and it differs
// from the older passthroughs in two ways a reader of the neighbours will notice: unix
// stamps cross as RFC 3339 strings rather than as unix-seconds strings, and the proto's
// "empty means unset" sentinels cross as `null`. Both are deliberate: the screen renders
// what it is given and never has to know which zero means "not yet".

/// Unix seconds → RFC 3339 in UTC (`2025-09-12T09:30:00Z`). Written by hand because this
/// crate declares `time` without its `formatting` feature: the feature happens to be
/// unified in through `cookie` and `sentry-types` today, but leaning on a transitive
/// feature would make one fixed layout hostage to a dependency's own flags.
fn rfc3339(secs: i64) -> String {
	let Ok(t) = time::OffsetDateTime::from_unix_timestamp(secs) else {
		// Unrepresentable stamps cannot come from the plane (it writes `now()`); an
		// empty string is the honest rendering of one rather than a made-up date.
		return String::new();
	};
	format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
}

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
	/// `service` | `user`, or `external` for an address (the retired `piggybank` /
	/// `revenue` kinds are refused at the hub and no order carries them).
	pub kind: String,
	pub id: String,
	pub network: String,
	pub address: String,
	/// What a person recognises this end BY: the receiving investor's masked mailbox, the
	/// product's title. Never part of the digest.
	pub detail: String,
}

impl PaymentEnd {
	/// An end the plane rendered as words only — the consilium projection.
	fn labelled(label: String) -> Self {
		Self { label, ..Default::default() }
	}
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
			expires_at: rfc3339(c.expires_at),
			decision,
			invalidated: c.invalidated,
			failure_reason: non_empty(c.failure_reason),
		}
	}
}

list_dto! { PaymentConfirmationList from bk::PaymentConfirmationList { items: Vec<PaymentConfirmation> } }

/// What the emailed investor is shown before answering. Deliberately narrow: the terms
/// they are consenting to and nothing about the operator beyond a
/// masked address — and both addresses are masked here as well as by the plane, because
/// this is the one payments surface reachable with no session.
#[derive(Default, Serialize)]
pub struct PaymentConsentInvitation {
	pub payment_id: String,
	pub state: String,
	pub tier: String,
	pub source: PaymentEnd,
	pub destination: PaymentEnd,
	pub amount: String,
	pub reason: String,
	pub payload_hash: String,
	pub initiator_email: String,
	pub subject_email: String,
	pub expires_at: String,
	/// This seat's own answer, so a reopened link shows what was already said.
	pub decision: String,
	pub attempts_remaining: u32,
}

impl From<bk::PaymentConsentInvitation> for PaymentConsentInvitation {
	fn from(i: bk::PaymentConsentInvitation) -> Self {
		let state = enum_label(i.state().as_str_name(), "PAYMENT_STATE_");
		let decision = enum_label(i.decision().as_str_name(), "CONSENT_DECISION_");
		Self {
			payment_id: i.payment_id,
			state,
			tier: i.tier,
			source: i.source.map(PaymentEnd::from).unwrap_or_default(),
			destination: i.destination.map(PaymentEnd::from).unwrap_or_default(),
			amount: i.amount,
			reason: i.reason,
			payload_hash: i.payload_hash,
			initiator_email: mask_email(&i.initiator_email),
			subject_email: mask_email(&i.subject_email),
			expires_at: rfc3339(i.expires_at),
			decision,
			attempts_remaining: i.attempts_remaining,
		}
	}
}

/// The invitation as it stands after an answer, plus whether this answer decided the
/// order.
#[derive(Serialize)]
pub struct PaymentConsentDecision {
	pub invitation: PaymentConsentInvitation,
	pub decided: bool,
}

impl From<bk::SubmitConsentResponse> for PaymentConsentDecision {
	fn from(r: bk::SubmitConsentResponse) -> Self {
		Self {
			invitation: r.invitation.map(PaymentConsentInvitation::from).unwrap_or_default(),
			decided: r.decided,
		}
	}
}

// ── governance: an owner's removal, answered from the mailbox (ownership plane) ──

/// What the TARGET is shown before answering: no peer identities, no vote breakdown.
/// Both addresses are masked — this surface, too, needs no session.
#[derive(Default, Serialize)]
pub struct OwnerRemovalInvitation {
	pub removal_id: String,
	pub state: String,
	pub initiator_email: String,
	pub target_email: String,
	pub reason: String,
	pub created_at: String,
	pub expires_at: String,
	pub decision: String,
	pub attempts_remaining: u32,
}

impl From<cc::OwnerRemovalInvitation> for OwnerRemovalInvitation {
	fn from(i: cc::OwnerRemovalInvitation) -> Self {
		let state = enum_label(i.state().as_str_name(), "OWNER_REMOVAL_STATE_");
		let decision = enum_label(i.decision().as_str_name(), "REMOVAL_VOTE_");
		Self {
			removal_id: i.removal_id,
			state,
			initiator_email: mask_email(&i.initiator_email),
			target_email: mask_email(&i.target_email),
			reason: i.reason,
			created_at: i.created_at.to_string(),
			expires_at: i.expires_at.to_string(),
			decision,
			attempts_remaining: i.attempts_remaining,
		}
	}
}

#[derive(Serialize)]
pub struct RemovalDecision {
	pub invitation: OwnerRemovalInvitation,
	pub decided: bool,
}

impl From<cc::SubmitSelfDecisionResponse> for RemovalDecision {
	fn from(r: cc::SubmitSelfDecisionResponse) -> Self {
		Self {
			invitation: r.invitation.map(OwnerRemovalInvitation::from).unwrap_or_default(),
			decided: r.decided,
		}
	}
}

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

	/// The wallet's five lifecycle figures cross one-to-one — `in_orders` in particular,
	/// which the cabinet shows beside `available` so a resting buy order explains the dip.
	#[test]
	fn the_balance_carries_every_lifecycle_figure() {
		let balance = Balance::from(bk::Balance {
			available: "87.88".into(),
			in_orders: "12.12".into(),
			invested: "50".into(),
			pending_withdrawal: "0".into(),
			total: "160".into(),
			in_compartments: "10".into(),
		});
		assert_eq!(
			(
				balance.available.as_str(),
				balance.in_orders.as_str(),
				balance.invested.as_str(),
				balance.pending_withdrawal.as_str(),
				balance.in_compartments.as_str(),
				balance.total.as_str()
			),
			("87.88", "12.12", "50", "0", "10", "160")
		);
	}

	/// The two #245 kinds cross as their own siblings, `null` on every other kind, and
	/// their effect ids follow the same empty-means-null rule as the older ones.
	#[test]
	fn a_holder_grant_and_a_seed_are_their_own_terms_siblings() {
		let grant = Consilium::from(bk::Consilium {
			holder_grant: Some(bk::HolderGrantTerms {
				allocation: "fund".into(),
				user_id: "d4d6".into(),
				units: "1000".into(),
			}),
			executed_issuance_id: "7c1e".into(),
			..Default::default()
		});
		let terms = grant.holder_grant.as_ref().expect("the grant terms");
		assert_eq!((terms.allocation.as_str(), terms.user_id.as_str(), terms.units.as_str()), ("fund", "d4d6", "1000"));
		assert!(grant.seed_capital.is_none() && grant.payment.is_none() && grant.valuation_override.is_none() && grant.fee_policy.is_none());
		assert_eq!(grant.executed_issuance_id.as_deref(), Some("7c1e"));
		assert_eq!(grant.executed_subscription_id, None);

		let seed = ConsiliumInvitation::from(bk::ConsiliumInvitation {
			seed_capital: Some(bk::SeedCapitalTerms {
				tx_ref: "0xabc:0".into(),
				network: "bep20".into(),
				amount: "250.5".into(),
				depositor_user_id: "d4d6".into(),
			}),
			..Default::default()
		});
		let terms = seed.seed_capital.as_ref().expect("the seed terms");
		assert_eq!(
			(terms.tx_ref.as_str(), terms.network.as_str(), terms.amount.as_str(), terms.depositor_user_id.as_str()),
			("0xabc:0", "bep20", "250.5", "d4d6")
		);
		assert!(seed.holder_grant.is_none());
	}

	/// `/api/users` carries the caller's permissions from GetMe, so the cabinet can offer
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
		assert_eq!(enum_label(bk::ConsiliumState::Open.as_str_name(), "CONSILIUM_STATE_"), "open");
		assert_eq!(enum_label(bk::ConsiliumState::ExecutionFailed.as_str_name(), "CONSILIUM_STATE_"), "execution_failed");
		assert_eq!(enum_label(bk::VoteDecision::Approve.as_str_name(), "VOTE_DECISION_"), "approve");
	}

	/// The redacted invitation is the one surface a stranger holding a link can read: no
	/// address on it may be renderable back to a real mailbox.
	#[test]
	fn the_public_invitation_masks_both_addresses() {
		let invitation = ConsiliumInvitation::from(bk::ConsiliumInvitation {
			initiator_email: "ada@example.com".into(),
			voter_email: "grace@example.com".into(),
			..Default::default()
		});
		assert_eq!(invitation.initiator_email, "a***@example.com");
		assert_eq!(invitation.voter_email, "g***@example.com");
	}

	/// The same rule on the consent page: the initiator and the subject are the only two
	/// addresses on it, and a stranger with the link may read neither.
	#[test]
	fn the_consent_invitation_masks_both_addresses() {
		let invitation = PaymentConsentInvitation::from(bk::PaymentConsentInvitation {
			initiator_email: "ada@example.com".into(),
			subject_email: "grace@example.com".into(),
			..Default::default()
		});
		assert_eq!(invitation.initiator_email, "a***@example.com");
		assert_eq!(invitation.subject_email, "g***@example.com");
	}

	/// The hand-written layout against known dates, including a leap day and the end of a
	/// year — the two places a homegrown calendar goes wrong.
	#[test]
	fn unix_seconds_render_as_rfc3339_utc() {
		assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
		assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
		assert_eq!(rfc3339(1_757_669_400), "2025-09-12T09:30:00Z");
		assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
	}

	/// A payment consilium's ends arrive from the plane as words; they land in `label`,
	/// and the structured fields stay empty rather than invented.
	#[test]
	fn a_payment_consilium_carries_its_terms_as_labelled_ends() {
		let consilium = Consilium::from(bk::Consilium {
			payment: Some(bk::ConsiliumPaymentTerms {
				payment_id: "p-1".into(),
				tier: "external".into(),
				source: "the fund's pooled capital".into(),
				destination: "0xabc on BEP20".into(),
				amount: "10.00".into(),
				reason: "rent".into(),
			}),
			executed_payment_id: "p-1".into(),
			..Default::default()
		});
		let terms = consilium.payment.expect("a payment consilium carries its terms");
		assert_eq!(terms.source.label, "the fund's pooled capital");
		assert_eq!(terms.destination.label, "0xabc on BEP20");
		assert_eq!(terms.destination.kind, "");
		assert_eq!(consilium.executed_payment_id.as_deref(), Some("p-1"));

		let revenue = Consilium::from(bk::Consilium::default());
		assert!(revenue.payment.is_none() && revenue.executed_payment_id.is_none());
	}

	/// The third sibling: set on a valuation override, `null` — never an empty object, which
	/// the invitation page treats as unrenderable — on every other kind, on the room's view
	/// and on the public invitation alike.
	#[test]
	fn a_valuation_override_consilium_carries_its_terms_and_nulls_elsewhere() {
		let consilium = Consilium::from(bk::Consilium {
			valuation_override: Some(bk::ValuationOverrideTerms {
				service: "svc-arb".into(),
				aum: "16250".into(),
			}),
			executed_valuation_id: "v-1".into(),
			..Default::default()
		});
		let terms = consilium.valuation_override.expect("a valuation override carries its terms");
		assert_eq!((terms.service.as_str(), terms.aum.as_str()), ("svc-arb", "16250"));
		assert_eq!(consilium.executed_valuation_id.as_deref(), Some("v-1"));
		assert!(consilium.payment.is_none());

		let other = Consilium::from(bk::Consilium::default());
		assert!(other.valuation_override.is_none() && other.executed_valuation_id.is_none());
		let invitation = ConsiliumInvitation::from(bk::ConsiliumInvitation {
			valuation_override: Some(bk::ValuationOverrideTerms {
				service: "svc-arb".into(),
				aum: "1".into(),
			}),
			..Default::default()
		});
		assert_eq!(invitation.valuation_override.map(|t| t.service).as_deref(), Some("svc-arb"));
		assert!(ConsiliumInvitation::from(bk::ConsiliumInvitation::default()).valuation_override.is_none());
	}
}
