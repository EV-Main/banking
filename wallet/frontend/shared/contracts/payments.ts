// The payments surface: a payment ORDER out of the caller's own claim, as they answer it
// signed in.
//
// Hand-written: these are the BFF's DTOs, shaped for this wallet host, not the proto messages
// `./gen` projects (`PaymentEnd.detail`, the lowercase states and the string timestamps are
// all the BFF's doing). When the surface is ever served straight off the proto, this file is
// what `./gen` replaces.
//
// `tier` is a fact the money plane derives from the destination (`external` for an address,
// `internal` otherwise) and is carried here READ-ONLY.
//
// Lifecycle states travel as plain strings: the state machine lives in the money plane, and a
// closed union here would turn a backend addition into a client type error. Views label the
// states they know and fall back to the wire word.

// ── An end as the plane renders it back ────────────────────────────────────────

export type PaymentEndKind = "user" | "treasury" | "external";

/**
 * One end of an order, resolved for a reader.
 *
 * `label` is the canonical name the digest binds; `detail` is what a PERSON recognises the
 * end by — the receiving investor's masked mailbox — so "investor
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

/** Where the money lands, by what it takes to get it there. */
export type PaymentTier = "internal" | "external";

export type ConsentDecision = "approve" | "reject";

/**
 * A payment out of the caller's own claim, as `GET /api/wallet/payments/:id` shows it — the
 * terms the caller answers and nothing about the operator beyond a masked address.
 *
 * `decision` is the wire's spelling and goes through `settledConsent`
 * (`shared/lib/decision.ts`): an unanswered order is the explicit, truthy `"pending"`.
 * `invalidated` is a pin moving (sessions revoked, mailbox changed) — the order can no
 * longer be answered.
 */
export interface PaymentConfirmation {
  payment_id: string;
  state: string;
  tier: PaymentTier | string;
  source: PaymentEnd;
  destination: PaymentEnd;
  amount: string; // an exact decimal, never a number
  reason: string;
  payload_hash: string;
  /** Masked by the plane. */
  initiator_email: string;
  expires_at: string; // unix seconds, parsed only by `shared/lib/datetime.ts`
  decision: string;
  invalidated: boolean;
  failure_reason: string | null;
}
