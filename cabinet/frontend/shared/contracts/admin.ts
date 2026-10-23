// Admin-console response types — the shapes the BFF's `/api/admin/*` handlers emit
// (see `cabinet/backend/src/dto.rs`). These are hand-written to match the BFF DTOs
// rather than re-exported from `./gen`, because the admin DTOs diverge from the raw
// proto (64-bit ints rendered as strings, the derived `isAdmin` flag, and the combined
// cabinet-config response). Import these from `@/shared/contracts/admin`.

export interface SessionUser {
  userId: string;
  email: string;
  /** Whether the mailbox is proven. A KYC level above 0 needs it. */
  emailVerified: boolean;
  /** A handle beside the email; absent when the account has none. */
  username?: string;
  status: string;
  role: string;
  isAdmin: boolean;
  /**
   * `role` above came from the `OWNER_SUBJECTS` emergency allowlist, not from the
   * persisted `users.role`.
   *
   * camelCase, unlike its snake_case twin on {@link AdminUserSummary} below, and the
   * difference is not a typo. This shape is NOT one of the BFF's: `/api/auth/session` is
   * the site-root auth surface, and ingress sends `/api/auth/*` straight to concierge,
   * whose `SessionUser` carries `#[serde(rename_all = "camelCase")]`. The admin DTOs come
   * from the cabinet's own BFF (`cabinet/backend/src/dto.rs`), which renames nothing.
   *
   * True only while the fund has no persisted owner — the window closes on the first
   * genesis seeding and cannot reopen. Render it as a warning, never as ownership: it is
   * real authority over the console and it seats nobody.
   */
  roleIsBreakGlass: boolean;
}

export interface SessionInfo {
  authenticated: boolean;
  user?: SessionUser;
  /**
   * What the caller's seat holds, concrete — a guest's set when nobody is signed in. What a
   * page shows is decided from this (`shared/config/route-permissions.ts`); the BFF still
   * decides what runs.
   */
  permissions: string[];
}

// ── users ─────────────────────────────────────────────────────────────────────
export interface AdminUserSummary {
  user_id: string;
  email: string;
  status: string;
  kyc_level: number;
  role: string;
  /** @see SessionUser.roleIsBreakGlass — same flag, the BFF's snake_case spelling. */
  role_is_break_glass: boolean;
  token_version: string;
  created_at: string;
  /**
   * WHY the account is disabled — and therefore which control the console may offer.
   * THREE cases, not two:
   *
   *   · `"admin_hold"`  — one operator's brake. Lapses by itself at {@link hold_expires_at},
   *                       and `/api/admin/users/reinstate` lifts it in one act.
   *   · `"governance"`  — the owners' ratified verdict. Never lapses; reinstatement is a
   *                       proposal, and the one-act route refuses it.
   *   · `""`            — empty on an ACTIVE user, and also on one suspended before this
   *                       field existed. Those keep the old one-act, never-lapsing
   *                       semantics they were actually suspended under, so an empty string
   *                       is not a synonym for "active": read it together with `status`.
   *
   * Which is why this is a string and not a boolean. A two-way branch files the third case
   * under whichever of the other two it happened to be compared against, and the account it
   * mis-files is the one nobody can act on correctly.
   */
  suspended_by: string;
  /** Unix seconds an `admin_hold` lapses; `"0"` when nothing lapses. Format through
   *  `shared/lib/datetime`, which is where this wire shape (seconds-as-string, `"0"` for
   *  absent) is already understood. */
  hold_expires_at: string;
}

export interface AdminUserList {
  users: AdminUserSummary[];
  total: string;
}

export interface AdminUserProfile {
  user_id: string;
  email: string;
  email_verified: boolean;
  status: string;
  token_version: string;
  legal_name: string;
  preferred_name: string;
  phone: string;
  date_of_birth: string;
  nationality: string;
  tax_residence: string;
  residential_address: string;
  language: string;
  base_currency: string;
  timezone: string;
  kyc_level: number;
  role: string;
  /** @see SessionUser.roleIsBreakGlass — same flag, the BFF's snake_case spelling. */
  role_is_break_glass: boolean;
  /**
   * WHY the account is disabled — and therefore which control the console may offer.
   * THREE cases, not two:
   *
   *   · `"admin_hold"`  — one operator's brake. Lapses by itself at {@link hold_expires_at},
   *                       and `/api/admin/users/reinstate` lifts it in one act.
   *   · `"governance"`  — the owners' ratified verdict. Never lapses; reinstatement is a
   *                       proposal, and the one-act route refuses it.
   *   · `""`            — empty on an ACTIVE user, and also on one suspended before this
   *                       field existed. Those keep the old one-act, never-lapsing
   *                       semantics they were actually suspended under, so an empty string
   *                       is not a synonym for "active": read it together with `status`.
   *
   * Which is why this is a string and not a boolean. A two-way branch files the third case
   * under whichever of the other two it happened to be compared against, and the account it
   * mis-files is the one nobody can act on correctly.
   */
  suspended_by: string;
  /** Unix seconds an `admin_hold` lapses; `"0"` when nothing lapses. Format through
   *  `shared/lib/datetime`, which is where this wire shape (seconds-as-string, `"0"` for
   *  absent) is already understood. */
  hold_expires_at: string;
}

/** A user's whole cash: `amount = main + in_compartments` — the `main` claim plus every
 *  scope compartment, which is still the user's money. */
export interface UserBalance {
  amount: string;
  pending: string;
  authoritative: boolean;
  as_of: string;
  main: string;
  in_compartments: string;
}

// ── outbox ────────────────────────────────────────────────────────────────────
export interface ParkedEvent {
  seq: string;
  event_id: string;
  aggregate: string;
  aggregate_id: string;
  kind: string;
  reason: string;
  parked_at: string;
  compensated: boolean;
}

export interface ParkedEventList {
  events: ParkedEvent[];
}

// ── treasury ──────────────────────────────────────────────────────────────────
export interface RailLiquidity {
  network: string;
  custody: string;
  treasury_address: string;
  onchain_usdt: string;
  onchain_gas: string;
  /** The rail's sweep gas-station wallet — fund native coin here (never USDT). Empty when unwired. */
  gas_station_address: string;
  gas_station_gas: string;
  /** Whether the rail's addresses are testnet-tagged — TON's friendly form differs per realm. */
  is_testnet: boolean;
}

/** An allocation's cash claim off one ledger balance: `posted` is settled, `reserved` is
 *  spoken for by approved payments out of it, `available` is the difference. */
export interface AllocationClaim {
  posted: string;
  available: string;
  reserved: string;
}

/** One allocation as the treasury shows it (#245): name, cash, supply, price and who holds
 *  it — the same shape for a product and for the reserved `fee` / `fund` allocations
 *  (`access: "hidden"`), which is also what the Revenue screen reads for `fee`.
 *
 *  Hand-written rather than the generated `BankingV1AllocationTreasury`: the BFF reshapes
 *  the proto's `nav_posted_at_unix` into the string `nav_posted_at` every other stamp here
 *  uses, and fills a missing `claim` with zeros, so this is the shape that actually
 *  arrives. `nav_posted_at` is `"0"` while nothing under it has been marked. */
export interface AllocationTreasury {
  service: string;
  title: string;
  access: AllocationAccessLevel;
  claim: AllocationClaim;
  units_outstanding: string;
  nav: string;
  nav_posted_at: string;
  holders: UnitHolding[];
  /** banking's own treasury (D17), where the retained withdrawal fees land. Set on the
   *  Revenue answer only, beside `fee`; a treasury line never carries it — the treasury
   *  answer has it once, as `Treasury.banking_treasury`. */
  banking_treasury?: AllocationClaim;
}

/** The two-layer treasury picture: per-rail custody, and who the claims on it belong to —
 *  people directly (`held_by_users`, and `held_in_compartments` for their scope
 *  compartments), through the units of an allocation (`allocations`, the hidden `fee` and
 *  `fund` included), or banking itself (`banking_treasury`, D17). No remainder is derived:
 *  each figure is read off its own accounts, and Σ = `total_custody` is checked hub-side. */
export interface Treasury {
  rails: RailLiquidity[];
  bank: string;
  total_custody: string;
  reserved_for_withdrawals: string;
  held_by_users: string;
  allocations: AllocationTreasury[];
  /** Zeros, not absent, from a hub older than D17 — the BFF fills them. */
  banking_treasury: AllocationClaim;
  held_in_compartments: string;
}

/** The seed-capital body, exactly as the BFF reads it (`POST /api/admin/treasury/
 *  seed-capital`). A seed is a proposal, not a booking: the chain transfer `tx_ref` on
 *  `network` is put to the owners as `expected_amount` USDT of `depositor_user_id`'s
 *  (the caller when omitted), and carries as their deposit plus a `fund` subscription. */
export interface SeedCapitalBody {
  tx_ref: string;
  network: string;
  /** Decimal USDT — the figure the owners approve; the chain must report exactly it. */
  expected_amount: string;
  /** The id the console carries; omitted = the caller. */
  depositor_user_id?: string;
}

/** What a seed proposal answers: `recorded` is always false (the booking is the
 *  consilium's execution), `consilium_id` is where the proposal now lives. */
export interface SeedCapitalProposal {
  recorded: boolean;
  amount: string;
  consilium_id: string;
}

// ── allocations (the registry of investable products) ───────────────────────────

/** `draft` — registered, takes no money yet. `open` — subscribe + redeem. `closed` —
 *  redeem only, so winding a product down never traps an investor. */
export type AllocationState = "draft" | "open" | "closed";

/** The picture an operator chose for a product, in the wire's snake_case.
 *
 *  A closed set mirrored from `icon::ALL` in `contracts/src/allocation.rs`; the hub's CHECK
 *  constraint and the BFF both reject anything outside it (the BFF answers 400), so this
 *  union and that list have to be edited together. `fund` is the hub's default. */
export type AllocationIcon = "fund" | "real_estate" | "trading" | "yield" | "venture" | "treasury" | "commodity" | "credit" | "index" | "arbitrage";

/** `hidden` — not in the caller's catalog at all. `view` — listed, `Subscribe` refused.
 *  `invest` — listed and open to new money (subject to `state` and the unit cap). Ranked
 *  `hidden < view < invest`, mirroring `contracts::allocation::access::ALL`. */
export type AllocationAccessLevel = "hidden" | "view" | "invest";

/** The levels a per-investor grant may carry — everything above the `hidden` floor.
 *  Mirrors `contracts::allocation::access::GRANTABLE`. */
export type AllocationGrantLevel = "view" | "invest";

/** What stands behind a product's units. `cash` — paid for into the fund's claim, and a
 *  redemption pays out of it. `in_kind` — the units stand for an asset the holders own
 *  off-platform; the fund holds no cash for them, so `Redeem` is refused (412) and holders
 *  exit through the book. The hub flips a product to `in_kind` on its first in-kind mint;
 *  only `SetAllocationBacking` takes it back. A registration lands on `cash`. */
export type AllocationBacking = "cash" | "in_kind";

// ── fees ─────────────────────────────────────────────────────────────────────

/** The five-field schedule a fund charges by — one statement, never patched a leg at a
 *  time. Rates are basis points; `basis` and `crystallization` are the money plane's closed
 *  vocabularies (`shared/lib/fee-terms.ts` has the words for them). */
export interface FeeTerms {
  management_bps: number;
  performance_bps: number;
  hurdle_bps: number;
  basis: string;
  crystallization: string;
}

/** A fund's terms in force. `configured` false means no policy row exists, which is a
 *  different fact from a policy whose rates are zero — the first can never charge. */
export interface FeePolicy extends FeeTerms {
  service: string;
  configured: boolean;
  /** Unix seconds as a string; the moment the row was last promoted. */
  updated_at: string;
  /** Which version of the fund's history these terms are; `0` when unconfigured. */
  version: number;
  /** Unix seconds as a string since which they bind; `"0"` when unconfigured. */
  effective_from: string;
  /** The change on its way — awaiting the owners or scheduled — or absent. Shown to
   *  holders and non-holders alike: the terms coming are part of deciding to stay in. */
  pending?: FeePolicyChange | null;
}

export interface FeePolicyList {
  policies: FeePolicy[];
}

/** Where a change stands. Typed as an open string like every other lifecycle on this wire
 *  (`./governance`): a member the plane grows later must render as its wire word, not
 *  break the build. The names this build knows are `admin.feeChange.state.*`. */
export type FeePolicyChangeState = "awaiting_consilium" | "scheduled" | "active" | "superseded" | "rejected" | "cancelled" | string;

/** Who had to agree: one `AllocationManage` holder, or the owners' consilium. */
export type FeePolicyRequirement = "admin" | "owner_consilium" | string;

/**
 * One row of a fund's fee-policy history — what `POST /api/admin/fees/policy` and
 * `/policy/cancel` answer with, and the rows of `GET /api/admin/fees/changes`.
 *
 * Timestamps are unix seconds as strings, `"0"` where the moment has not come:
 * `effective_from` is provisional while the change awaits the owners (the 24h notice is
 * counted from their approval, not from the request) and fixed once scheduled.
 */
export interface FeePolicyChange extends FeeTerms {
  id: string;
  service: string;
  version: number;
  state: FeePolicyChangeState;
  effective_from: string;
  requirement: FeePolicyRequirement;
  /** The consilium this change waits on; `null` for an administrator's change. */
  consilium_id?: string | null;
  requested_by: string;
  requested_at: string;
  scheduled_at: string;
  applied_at: string;
  reason: string;
  /**
   * The operator who took responsibility for the holders who could not be told
   * (`/policy/acknowledge-notices`) — the latest to, when a later act extended the record;
   * `null` until someone did. Operators only — blank for everyone else even once given.
   */
  notices_waived_by: string | null;
  /**
   * The same operator by the email the directory knows them by — what a reader is shown,
   * the id being a fact for the record and a tooltip. Always present on the wire; `null`
   * when nobody acknowledged, when the reader is not an operator, or when the directory
   * cannot name the id (a directory outage never fails the page).
   */
  notices_waived_by_email: string | null;
  /** Unix seconds as a string the (latest) acknowledgement was given; `"0"` until someone gave it. */
  notices_waived_at: string;
  /** The banking user ids whose notice the mailer had given up on by that moment — exactly
   *  the holders the change binds over untold. A holder given up on later is not among them
   *  until a further act adds them. Operators only. */
  notices_waived_users: string[];
  /**
   * Notices not yet delivered to a CURRENT holder, how many of those the mailer has given
   * up on, and how many of the given-up ones the acknowledgement does not cover (all of
   * them while there is none) — the holders a (further) act would name, so it is worth
   * offering exactly while this is non-zero. Counted only while `scheduled` — `0` in every
   * other state, and `0` for everyone but operators — so a non-zero figure is always about
   * a change still on its way.
   */
  undelivered_notices: number;
  notices_given_up: number;
  notices_unacknowledged: number;
}

export interface FeePolicyChangeList {
  changes: FeePolicyChange[];
}

/** `POST /api/admin/fees/policy`. `effective_from` is unix seconds as a NUMBER — the BFF
 *  reads it with `as_i64` — and `0` asks for the earliest moment the notice allows.
 *  `reason` is required by the hub exactly when the owners must approve. */
export interface ScheduleFeePolicyRequest extends FeeTerms {
  service: string;
  effective_from: number;
  reason: string;
}

/** `POST /api/admin/fees/policy/cancel`. Idempotent on an already-cancelled change. */
export interface CancelFeePolicyChangeRequest {
  service: string;
  change_id: string;
}

/** `POST /api/admin/fees/policy/acknowledge-notices`. Idempotent on a change already
 *  acknowledged (the first record stands). 409 when the change is not `scheduled` or every
 *  notice has been delivered; 403 for a caller who is neither the requester nor an owner. */
export interface AcknowledgeUndeliveredNoticesRequest {
  service: string;
  change_id: string;
}

/** Uncollected fee units in one fund. `value` is what a settlement would convert them to
 *  at the current NAV. */
export interface FeeShares {
  service: string;
  units: string;
  value: string;
}

export interface FeeSettlement {
  service: string;
  units: string;
  nav: string;
  cash: string;
}

/** One charge against one holding. `charged_cash` falls short of
 *  `management + performance` exactly when the rest went to `debt_carried`. */
export interface FeeAssessment {
  service: string;
  trigger: string;
  nav: string;
  management: string;
  performance: string;
  debt_opening: string;
  charged_units: string;
  charged_cash: string;
  debt_carried: string;
  high_water_mark: string;
  assessed_at: string;
}

export interface FeeAssessmentList {
  assessments: FeeAssessment[];
}

export interface Allocation {
  service: string;
  title: string;
  summary: string;
  state: AllocationState;
  /** Unix seconds; `"0"` on the write responses, which return the clock-free aggregate. */
  created_at: string;
  updated_at: string;
  /** Authorised unit supply, decimal. Subscribe refuses a mint that would pass it. */
  unit_cap: string;
  /**
   * Optional on READ, and deliberately so rather than as a hedge: `allocationsResource`
   * carries `persist: true`, so a returning user's first frame is rehydrated from a
   * sessionStorage object serialised before this field existed. Optionality alone is not
   * the fix — a renderer must land both a missing value and one newer than this build on
   * `fund`.
   *
   * Required on WRITE by this client's own choice, not by the hub — see
   * `AllocationWrite`. On the wire the field carries presence, so an update that omits
   * it changes nothing.
   */
  icon?: AllocationIcon;
  /**
   * access
   *
   * The product's default access level — what a caller with no grant holds. A
   * registration lands on `view`: listed, locked. Optional on READ for the same reason
   * `icon` is above — `allocationsResource` persists to sessionStorage, and a returning
   * user's first frame may rehydrate an object serialised before this field existed.
   */
  access?: AllocationAccessLevel;
  /**
   * caller_access
   *
   * The level THIS caller effectively holds — `max(access, their own grant)`. The
   * subscribe control and the "locked" badge key off this, never off `access` or `state`
   * alone: an `open` product at `view` is visible and not investable. Honest for an
   * `AllocationManage` holder too — their permission does not make them an investor. Same
   * optionality caveat as `access`.
   */
  caller_access?: AllocationAccessLevel;
  /**
   * backing
   *
   * Optional on READ for the same reason `access` is — a persisted catalog object may
   * predate the field. Absent reads as `cash`, the hub's own default for an unset product.
   */
  backing?: AllocationBacking;
}

export interface AllocationList {
  allocations: Allocation[];
}

/** One investor raised above a product's default access level. */
export interface AllocationAccessGrant {
  service: string;
  /**
   * The investor — the id the console carries: concierge-first, banking when no mirror
   * exists. Revoke sends it back exactly as received; the BFF owns the mapping.
   */
  user_id: string;
  /**
   * The investor's email, for display. `null` when the hub could not match `user_id` to
   * an identity (a banking-only id with no concierge mirror, or a mirror that has since
   * gone); absent on a payload serialised before the field existed. Fall back to the id.
   */
  email?: string | null;
  level: AllocationGrantLevel;
  /** The `AllocationManage` holder who granted it — console-facing id, same rule as `user_id`. */
  granted_by: string;
  /** Unix seconds. */
  granted_at: string;
}

export interface AllocationAccessGrantList {
  grants: AllocationAccessGrant[];
}

// ── in-kind issuance (units with no cash leg) ───────────────────────────────────

/** Who holds units: a person (`user`, `id` is the banking user id) or the reserved `fee`
 *  allocation (`allocation`, `id` is its slug — never a user to look up). The company is
 *  not a holder (#245). */
export type UnitHolderKind = "user" | "allocation";

/** One holder of an allocation's units, as the cap table and the treasury name them. */
export interface UnitHolderRef {
  kind: UnitHolderKind;
  id: string;
}

/** One line of a cap table: who, and how many units (decimal). A zero holding is not
 *  listed, and a line never lacks a holder — the BFF drops one rather than render it. */
export interface UnitHolding {
  holder: UnitHolderRef;
  units: string;
}

/** `queued` until the relay posts the mint, then `applied`. A `queued` row is real — the
 *  hub has accepted it — but the units are not on the ledger yet, so a holders read
 *  taken straight after the POST still shows the supply as it was. */
export type UnitIssuanceState = "queued" | "applied";

/** Where the units came from — or went: `mint` (`/allocations/issue` or an executed
 *  holder grant — the supply grew by `units`) or `retire` (`/allocations/retire` — burned
 *  out of a holder, the supply SHRANK by `units`). Always populated; a row that predates
 *  the field reads as `mint`. */
export type UnitIssuanceSource = "mint" | "retire";

/** One in-kind issuance — a mint or a retirement — as the hub recorded it. */
export interface UnitIssuance {
  id: string;
  service: string;
  holder_kind: UnitHolderKind;
  /** The banking user id for `user`; the holding allocation's slug for `allocation`. */
  holder_id: string;
  units: string;
  /** Decimal USDT per unit the mint was recorded at. */
  nav: string;
  /** Decimal USDT the holder is deemed to have paid — what P&L and the management fee
   *  are measured from. */
  cost_basis: string;
  state: UnitIssuanceState;
  /** Unix seconds. */
  created_at: string;
  source: UnitIssuanceSource;
}

/** The retirement body, exactly as the BFF reads it (`POST /api/admin/allocations/
 *  retire`). The mirror of a mint: units are burned out of ONE person — the reserved
 *  allocations are never burned from here — and the supply shrinks by them; no cash moves
 *  either way. `cost_basis` follows the mint's rule (absent = `units × NAV`, an empty
 *  string is malformed). `force` is the operator's explicit override to burn out of a
 *  live (draft or open) product — omitted, not `false`, when the product is closed and
 *  needs none. */
export interface RetireUnitsBody {
  service: string;
  /** Whose units are burnt — the id the console carries. */
  user_id: string;
  /** Decimal units, > 0, at most what the holder has available. */
  units: string;
  /** Decimal USDT of book value written off; omitted = `units × NAV`. */
  cost_basis?: string;
  /** 1..64 chars, in the same per-product key space as `/allocations/issue`. */
  idempotency_key: string;
  force?: true;
}

/** A product's cap table (#245): the settled supply and every holder of it, largest first
 *  — people and the `fee` allocation alike, each a line. Σ `holders[].units` =
 *  `units_outstanding` is the supply invariant the hub checks; it is read here, never
 *  derived. All units decimal. */
export interface UnitHolders {
  service: string;
  units_outstanding: string;
  /** In-kind mints the relay has accepted but not yet posted — not part of
   *  `units_outstanding` until they land, so a cap pinned to the settled figure alone
   *  would leave them to land above it. */
  queued_units: string;
  holders: UnitHolding[];
}

// ── valuation + redemptions ─────────────────────────────────────────────────────
export interface FundNav {
  service: string;
  nav: string;
  aum: string;
  /** The settled supply — the denominator NAV is derived against. */
  units_outstanding: string;
  posted_at: string;
  stale: boolean;
  unit_cap: string;
  /** Units still issuable. Already nets off in-flight mints, so offering this figure can
   *  never offer more than the hub will accept. */
  remaining_capacity: string;
}

export interface RedemptionQueueItem {
  redemption_id: string;
  user_id: string;
  email: string;
  service: string;
  units: string;
  created_at: string;
}

export interface RedemptionQueue {
  items: RedemptionQueueItem[];
}

export interface Redemption {
  id: string;
  service: string;
  units: string;
  nav: string;
  cash: string;
  state: string;
}

// ── withdrawals (operator queue) ─────────────────────────────────────────────────
export interface WithdrawalQueueItem {
  withdrawal_id: string;
  /** Which account funds it. A `treasury` row is an approved payment out of banking's own
   *  treasury (D17), so it carries empty `user_id`/`email` — label it rather than rendering
   *  a blank investor. */
  source: "user" | "treasury";
  user_id: string;
  email: string;
  network: string;
  address: string;
  amount: string;
  net_amount: string;
  state: string;
  created_at: string;
}

export interface WithdrawalQueue {
  items: WithdrawalQueueItem[];
}

// ── revenue (the reserved `fee` allocation) ─────────────────────────────────────
//
// `GET /api/admin/revenue` answers an `AllocationTreasury` — the `fee` allocation in the
// treasury's own shape (#245): its cash claim, supply, price and the people who hold it,
// plus `banking_treasury` beside it — the withdrawal fees go there since D17, not to `fee`.
// Nothing pays it out from that surface any more: a holder redeems, or the owners approve
// a payment out of `service:fee` on the Payments screen.

// ── cabinet (platform config + money-plane read-only) ───────────────────────────
export interface FeatureFlag {
  key: string;
  description: string;
  enabled: boolean;
  rollout: number;
}

export interface PlatformConfig {
  maintenance_mode: boolean;
  announcement_title: string;
  announcement_body: string;
  announcement_active: boolean;
  flags: FeatureFlag[];
}

export interface OperationsMode {
  read_only: boolean;
}

export interface Rail {
  network: string;
  configured: boolean;
  frozen: boolean;
  gas_coin: string;
}

export interface CabinetConfig {
  platform: PlatformConfig;
  read_only: boolean;
}
