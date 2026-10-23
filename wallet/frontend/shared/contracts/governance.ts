// The fund-governance surface: the owner roster, owner removals, the payout consilium, and
// the two token-addressed approval endpoints an owner reaches from their mailbox.
//
// Hand-written rather than generated, like `./notifications` and `./admin`: these are BFF
// DTOs shaped for this wallet host, not a direct projection of a banking proto message (the
// removal half is concierge-owned, the payout half banking-owned, and the BFF joins them —
// see docs/CONSILIUM.md § The two planes). When the surface does get a proto, this file is
// what `./gen` replaces.
//
// A rule from the policy is encoded in the *types* rather than left to a view: a vote is a
// decision, not a number. The public GET carries `attempts_remaining` and a settled
// `decision`, never a running tally the client could add to.
//
// Lifecycle states travel as plain strings on purpose. The state machine lives in the money
// plane and gains members there (`ExecutionFailed` arrived after `Approved`); a closed union
// here would turn a backend addition into a client type error and, worse, tempt a view into
// an exhaustive switch that renders nothing at all for a state it has not met yet. Views
// label the states they know and fall back to the wire value, which is legible even when it
// is new.

import type { FeeTerms } from "./admin";
import type { ConsiliumPaymentTerms } from "./payments";

/** An amount as it crossed the wire: an exact decimal string, never a number. */
export type Decimal = string;

/**
 * Unix SECONDS as a decimal string, `"0"` while unset — what the BFF actually sends (see
 * `shared/lib/datetime.ts`, which is the one place it is parsed). The name is kept for its
 * meaning, not its encoding.
 */
export type Timestamp = string;

// ── The change of terms being authorized ───────────────────────────────────────

/**
 * What a fee-policy consilium asks the owners to carry: one product's terms moved FROM
 * what it charges today TO what the requester proposes (docs/FEES.md § Changing the terms).
 *
 * `from` is absent when the fund charged nothing — a different fact from a policy of
 * zeros, and the approval page says so rather than printing 0%. `effective_from` is what
 * the requester ASKED for (`"0"` = as soon as allowed); the 24h notice is counted from the
 * owners' approval, so the moment the terms actually bind is not known until then.
 * Everything here except `allocation_name` and `holder_count` is inside the hashed
 * subject, so an approval of one change is never a valid signature over another.
 */
export interface ConsiliumFeePolicyTerms {
  change_id: string;
  service: string;
  /** The product's display name, read live from the registry. */
  allocation_name: string;
  from?: FeeTerms | null;
  to: FeeTerms;
  effective_from: Timestamp;
  /** Investors holding units when the change was proposed — who the notice will reach. */
  holder_count: number;
  reason: string;
}

// ── The ownership terms being authorized (#245) ─────────────────────────────────

/**
 * What a holder-grant consilium asks the owners to carry: `units` of the reserved
 * `allocation` (`fee` | `fund`) minted to `user_id`. The only way a person becomes a holder
 * of either — an operator's mint refuses the reserved allocations, and the company is not a
 * holder. All three fields are inside the hashed subject.
 */
export interface HolderGrantTerms {
  allocation: string;
  /** The money-plane id the terms stored — a person, never the company. */
  user_id: string;
  units: Decimal;
}

/**
 * What a seed-capital consilium asks the owners to carry: the chain transfer `tx_ref` on
 * `network`, which the chain must report as exactly `amount` USDT, is `depositor_user_id`'s
 * — booked as their deposit and subscription into `fund` once the quorum carries. The
 * reference is carried in FULL, as an external payment's address is (policy 13).
 */
export interface SeedCapitalTerms {
  tx_ref: string;
  network: string;
  amount: Decimal;
  depositor_user_id: string;
}

// ── Public: the emailed payout approval ────────────────────────────────────────

export type PayoutDecision = "approve" | "reject";

/**
 * What `GET /api/approval/payout/:token` renders — a redacted summary and nothing else.
 * The GET is strictly read-only because mail scanners click every link in a message
 * (policy 5); the vote is the POST below, carrying the secret code.
 *
 * There is no "which failure was it" field, and there must never be one: unknown, expired,
 * spent and burned tokens all answer with one identical 404 (policy 10).
 */
export interface PayoutApproval {
  consilium_id: string;
  state: string;
  /** Present for a payment consilium, and what the page renders when it is set
   *  (docs/CONSILIUM.md § Payments). Exactly one of the siblings below describes the
   *  subject. */
  payment?: ConsiliumPaymentTerms | null;
  /** Present for a NAV mark past the move guard (banking#232). */
  valuation_override?: ValuationOverride | null;
  /** Present for a fee-policy consilium (docs/FEES.md § Changing the terms). */
  fee_policy?: ConsiliumFeePolicyTerms | null;
  /** Present for a holder grant — a person seated on `fee` / `fund` (#245). */
  holder_grant?: HolderGrantTerms | null;
  /** Present for a seed of the platform's capital (#245). */
  seed_capital?: SeedCapitalTerms | null;
  /** Full hash; the page shows a short prefix of it. */
  payload_hash: string;
  initiator_email: string;
  /** The seat this token was minted for — the "you" the page addresses. */
  voter_email: string;
  threshold: number;
  approvals: number;
  owner_count: number;
  created_at: Timestamp;
  expires_at: Timestamp;
  /**
   * This seat's answer as the wire spells it — NOT a nullable `PayoutDecision`.
   *
   * An unanswered seat is an explicit `PENDING` in the money plane and reaches the client
   * as the string `"pending"`; an absent one reaches it as `""`. Both are truthy, so this
   * must go through `settledPayout` (`shared/lib/decision.ts`) rather than a `?? null`.
   * Typing it as the narrow union is what made every fresh invitation look decided.
   */
  decision?: string | null;
  /** Wrong codes left before the token burns and every owner is notified (policy 7). */
  attempts_remaining: number;
}

/**
 * The answer to a cast vote. `invitation` is the authoritative re-read — the view renders
 * that, never its own optimistic guess — and `decided` says whether this POST is what
 * settled the seat.
 */
export interface PayoutApprovalResult {
  invitation: PayoutApproval;
  decided: boolean;
}

// ── Public: the emailed "your seat is being removed" notice ────────────────────

export type RemovalDecision = "remove" | "keep";

/** What `GET /api/approval/removal/:token` renders. Same contract shape, different content. */
export interface RemovalApproval {
  removal_id: string;
  state: string;
  initiator_email: string;
  /** The owner whose seat this is about — the reader. */
  target_email: string;
  reason: string;
  created_at: Timestamp;
  expires_at: Timestamp;
  /** As the wire spells it — read through `settledRemoval`. See {@link PayoutApproval.decision}. */
  decision?: string | null;
  attempts_remaining: number;
}

export interface RemovalApprovalResult {
  invitation: RemovalApproval;
  decided: boolean;
}

// ── Authenticated: the owners' room ────────────────────────────────────────────

export interface Owner {
  user_id: string;
  email: string;
  display_name: string;
  owner_since: Timestamp;
}

/**
 * What a user proposal decides. Three kinds share one message because all three ask the
 * same question — "do the owners agree to change this person's standing?" — differing only
 * in what the verdict writes.
 */
export type UserProposalKind = "suspension" | "reinstatement" | "admin_admission";

/** One eligible owner voter and their answer. Same wire caveat as {@link RemovalPeer}: an
 *  unanswered peer arrives as `""` or `"pending"`, never as null. */
export interface UserProposalPeer {
  user_id: string;
  email: string;
  vote?: string | null;
  voted_at?: Timestamp | null;
}

/**
 * The owners' verdict over one PERSON's standing.
 *
 * Two things separate it from {@link OwnerAdmission}, and neither is cosmetic:
 *
 *   · `subject_user_id` is ANY user, not necessarily an owner — which is why none of the
 *     admission's roster checks have an analogue here.
 *   · It passes on a MAJORITY of the snapshotted voters, not unanimity. Unanimity guards
 *     the owner roster because a minority able to add owners by majority grows itself into
 *     a majority; neither thing decided here has that property, and unanimity would COST
 *     safety — a hold lapses in 24h, so one unreachable owner would not delay a
 *     ratification, they would decide it by releasing a compromised account at the deadline.
 *
 * `threshold` is frozen at open and must be READ, never re-derived from `peers.length` or
 * the current roster: it is the bar this proposal is actually measured against, and a
 * roster that moved underneath it does not move the bar.
 */
export interface UserProposal {
  id: string;
  kind: UserProposalKind | string;
  state: string;
  subject_user_id: string;
  subject_email: string;
  initiator_user_id: string;
  initiator_email: string;
  reason: string;
  /** Every owner except the initiator, frozen at open. Membership IS the eligibility test. */
  peers: UserProposalPeer[];
  owner_count: number;
  /** How many of `peers` must vote FOR. Frozen at open — read it, do not recompute it. */
  threshold: number;
  created_at: Timestamp;
  expires_at: Timestamp;
  decided_at?: Timestamp | null;
  void_reason?: string | null;
  /** Serialised as a string by the BFF's integer encoding; never do arithmetic on it here. */
  version: number | string;
}

// ── The NAV mark being authorized ──────────────────────────────────────────────

/**
 * What a valuation-override consilium authorizes: marking one fund at an AUM the NAV-move
 * guard refuses. There is no per-request "override" flag any more (banking#232) — a move
 * past the guard is only ever recorded by executing one of these, at exactly this AUM.
 * Both fields are covered by `payload_hash`, so both are shown in full wherever the
 * request is decided.
 */
export interface ValuationOverride {
  service: string;
  aum: Decimal;
}
