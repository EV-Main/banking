// The payments surface: a payment ORDER between two named ends of the platform, and the
// emailed consent an investor gives when the money is their own.
//
// Hand-written like `./governance`: these are the BFF's DTOs, shaped for this cabinet, not
// the proto messages `./gen` projects (`PaymentEnd.detail`, the nested `consent`, the
// lowercase states and the string timestamps are all the BFF's doing — docs/CONSILIUM.md
// § Payments). When the surface is ever served straight off the proto, this file is what
// `./gen` replaces.
//
// Two facts the money plane derives are carried here as READ-ONLY fields, never inputs:
//
//   · `tier` is a function of the destination (`external` for an address, `service` for a
//     product's pooled claim, `internal` for everything else). The open request carries no
//     tier; a caller-supplied one would only ever be interesting when it disagreed.
//   · `requirement` is a function of the SOURCE and nothing else: fund-owned money asks the
//     owner consilium, an investor's own claim asks that investor. There is no cell in
//     which one admin moves fund money alone, and no field here that could ask for one.
//
// A form may preview both before the confirm click, but the plane's answer is the one that
// counts.
//
// Lifecycle states travel as plain strings, for the reason `./governance` gives: the state
// machine lives in the money plane, and a closed union here would turn a backend addition
// into a client type error. Views label the states they know and fall back to the wire
// word.

import type { Decimal, Timestamp } from "./governance";

// ── Naming an end ──────────────────────────────────────────────────────────────

/** An internal claim as the caller names it when OPENING an order. The platform's own
 *  money is the reserved `fee` / `fund` allocations, addressed as `service` with that id
 *  (#245); the `piggybank` / `revenue` singletons this once carried are refused by the
 *  plane and survive only as {@link PaymentEndKind} on orders opened before. */
export type PartyKind = "service" | "user";

/**
 * One internal end of an order. `id` is the product id for `service` (`fee` / `fund`
 * for the reserved allocations), and for `user` the CONCIERGE user id the console
 * carries (`AdminUserSummary.user_id`) — the plane resolves it to its own id and echoes
 * THAT back in `PaymentEnd.id`, so the two never round-trip (payments.proto).
 */
export interface Party {
  kind: PartyKind;
  id: string;
}

/** An on-chain destination. The only external shape; an external SOURCE is unrepresentable. */
export interface ExternalDestination {
  network: string;
  /** Rendered in full wherever it is shown — see `FullAddress` (policy 13). */
  address: string;
}

/** Exactly one of the two: either an internal party or an external address. */
export interface PaymentDestination {
  internal?: Party;
  external?: ExternalDestination;
}

export interface OpenPaymentRequest {
  source: Party;
  destination: PaymentDestination;
  amount: Decimal;
  /** Required and bounded (≤ 500 bytes); it is what the approvers read. */
  reason: string;
}

// ── An end as the plane renders it back ────────────────────────────────────────

export type PaymentEndKind = PartyKind | "external" | "piggybank" | "revenue";

/**
 * One end of an order, resolved for a reader.
 *
 * `label` is the canonical name the digest binds; `detail` is what a PERSON recognises the
 * end by — the receiving investor's masked mailbox, the product's title — so "investor
 * 8f3e…" is not approved for the wrong person. The BFF sends EMPTY STRINGS, not null, for
 * the fields a kind does not carry (`network`/`address` on an internal end, `detail` when
 * there is nothing to add).
 */
export interface PaymentEnd {
  label: string;
  kind: PaymentEndKind | string;
  id: string;
  network: string;
  address: string;
  detail: string;
}

// ── The two derived facts ──────────────────────────────────────────────────────

/** Where the money lands, by what it takes to get it there (docs/CONSILIUM.md § Three tiers). */
export type PaymentTier = "internal" | "service" | "external";

/** Who must agree, decided by the source alone. */
export type PaymentRequirement = "owner_consilium" | "subject_consent";

// ── The consent seat ───────────────────────────────────────────────────────────

/**
 * The one emailed seat a `subject_consent` order carries — the investor whose money it is.
 *
 * `decision` is the wire's spelling and goes through `settledConsent`
 * (`shared/lib/decision.ts`): an unanswered seat is the explicit `"pending"`, which is
 * truthy, and reading it as an answer is the bug that once hid every vote form.
 * `invalidated` is the pins moving (a token revoked, a mailbox changed) — the seat can no
 * longer be answered, whatever `decision` says.
 */
export interface PaymentConsent {
  subject_email: string;
  decision: string;
  notified: boolean;
  attempts_remaining: number;
  invalidated: boolean;
  invalidation_reason: string;
}

// ── The order ──────────────────────────────────────────────────────────────────

export interface Payment {
  id: string;
  /** pending · approved · executed · execution_failed · rejected · expired · cancelled — open. */
  state: string;
  tier: PaymentTier | string;
  source: PaymentEnd;
  destination: PaymentEnd;
  amount: Decimal;
  reason: string;
  requirement: PaymentRequirement | string;
  payload_hash: string;
  initiator_email: string;
  /** Set for an `owner_consilium` order — the room where the tally lives. */
  consilium_id: string | null;
  /** Set for a `subject_consent` order seated on the emailed token. */
  consent: PaymentConsent | null;
  /** The seat that decides the order: `consilium` · `consent` · `concierge_approval` (a
   *  `bank:payment` approval of the platform owners) · `session` (the investor's own answer
   *  in the cabinet). Null for a row older than seats. */
  seat: string | null;
  /** Concierge's id of the approval, once opened; `concierge_approval` only. */
  approval_id: string | null;
  /** The investor's answer, `session` only; wire spelling, read through `settledConsent`. */
  session_decision: string | null;
  created_at: Timestamp;
  expires_at: Timestamp;
  decided_at: Timestamp | null;
  /** The withdrawal an executed L1 order became; null for the internal tiers. */
  executed_withdrawal_id: string | null;
  /** Why an approved order did not move money. Terminal; retried by nothing. */
  failure_reason: string | null;
  version: number;
}

export interface PaymentList {
  items: Payment[];
}

// ── Public: the emailed consent ────────────────────────────────────────────────

export type ConsentDecision = "approve" | "reject";

/**
 * What `GET /api/approval/consent/:token` renders — the terms and nothing else. Same rules
 * as the payout approval: the GET is inert because mail scanners click every link, and
 * every dead token answers one identical 404 (policy 5, 10).
 */
export interface PaymentConsentInvitation {
  payment_id: string;
  state: string;
  tier: PaymentTier | string;
  source: PaymentEnd;
  destination: PaymentEnd;
  amount: Decimal;
  reason: string;
  payload_hash: string;
  initiator_email: string;
  /** The reader — whose money this is. */
  subject_email: string;
  expires_at: Timestamp;
  /** Wire spelling; read through `settledConsent`. */
  decision: string;
  attempts_remaining: number;
}

/** The answer to a cast consent. `invitation` is the authoritative re-read. */
export interface PaymentConsentResult {
  invitation: PaymentConsentInvitation;
  decided: boolean;
}

// ── The investor's own answer, signed in ───────────────────────────────────────

/**
 * A payment out of the caller's own claim, as `GET /api/wallet/payments/:id` shows it — the
 * terms the caller answers and nothing about the operator beyond a masked address. It
 * replaces the emailed token and code: the session is the credential, and approving asks
 * for a fresh sign-in (`step_up_required`) while declining never does.
 *
 * `decision` is the wire's spelling and goes through `settledConsent`; `invalidated` is a
 * pin moving (sessions revoked, mailbox changed) — the order can no longer be answered.
 */
export interface PaymentConfirmation {
  payment_id: string;
  state: string;
  tier: PaymentTier | string;
  source: PaymentEnd;
  destination: PaymentEnd;
  amount: Decimal;
  reason: string;
  payload_hash: string;
  /** Masked by the plane. */
  initiator_email: string;
  expires_at: Timestamp;
  decision: string;
  invalidated: boolean;
  failure_reason: string | null;
}

export interface PaymentConfirmationList {
  items: PaymentConfirmation[];
}

// ── The terms as a consilium carries them ──────────────────────────────────────

/**
 * What a payment consilium authorizes — a sibling of the other kinds' terms on a
 * `Consilium` and on its emailed invitation. Two ends in words rather than a rail and an address; an
 * external destination still carries both inside `destination`.
 */
export interface ConsiliumPaymentTerms {
  payment_id: string;
  tier: PaymentTier | string;
  source: PaymentEnd;
  destination: PaymentEnd;
  amount: Decimal;
  reason: string;
}
