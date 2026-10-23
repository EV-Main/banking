// The fund-governance surface: the owner roster, owner removals, the payout consilium, and
// the two token-addressed approval endpoints an owner reaches from their mailbox.
//
// Hand-written rather than generated, like `./notifications` and `./admin`: these are BFF
// DTOs shaped for this wallet host, not a direct projection of a banking proto message (the
// removal half is concierge-owned, the payout half banking-owned, and the BFF joins them —
// see docs/CONSILIUM.md § The two planes). When the surface does get a proto, this file is
// what `./gen` replaces.
//
// Two rules from the policy are encoded in the *types* rather than left to a view:
//
//   · A vote is a decision, not a number. The public GET carries `attempts_remaining` and a
//     settled `decision`, never a running tally the client could add to.
//   · A socket frame carries a REVISION and nothing else. There is no tally on the wire,
//     so there is no type here that could accidentally be rendered as one.
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
 * The roster, plus the one derived fact the page has to lead with.
 *
 * `below_payout_floor` is computed by the plane that owns the rule, not re-derived here
 * from `items.length`. A fund under three owners can never authorize a payout again
 * (policy: quorum arithmetic), and that is the sort of arithmetic a client must read
 * rather than reimplement.
 */
export interface OwnerList {
  items: Owner[];
  below_payout_floor: boolean;
}

export type RemovalVote = "remove" | "keep";

/**
 * One eligible peer voter and their answer.
 *
 * A peer who has not voted arrives with `vote` as an empty string, not as null or absent —
 * so this is the wire's spelling and must go through `peerVote` (`shared/lib/decision.ts`).
 * Reading `""` as a cast vote both mislabels them in the roster and removes the vote
 * buttons from someone who still has a vote to give.
 */
export interface RemovalPeer {
  user_id: string;
  email: string;
  vote?: string | null;
  voted_at?: Timestamp | null;
}

/**
 * An open (or settled) proposal to remove an owner.
 *
 * `peers` is the snapshot of who may vote, frozen when the removal was opened — the
 * initiator and the target are both absent from it by construction, so membership of this
 * array IS the eligibility test. A view must not re-derive eligibility from the roster:
 * an owner added since the removal opened is not a voter (policy 3), and the roster does
 * not record that.
 */
export interface OwnerRemoval {
  id: string;
  state: string;
  target_user_id: string;
  target_email: string;
  initiator_user_id: string;
  initiator_email: string;
  reason: string;
  peers: RemovalPeer[];
  /** The target's own answer from their mailbox — the other path to a decision. Wire
   *  spelling; read through `settledRemoval`. */
  target_decision?: string | null;
  target_decided_at?: Timestamp | null;
  target_notified: boolean;
  owner_count: number;
  created_at: Timestamp;
  expires_at: Timestamp;
  decided_at?: Timestamp | null;
  void_reason?: string | null;
  /** Serialised as a string by the BFF's integer encoding; never do arithmetic on it here. */
  version: number | string;
}

export interface OwnerRemovalList {
  items: OwnerRemoval[];
}

/**
 * An answer to an admission. Its own union, not a reuse of {@link RemovalVote}, mirroring
 * the plane's own split — `reject` here refuses a candidate, while `reject` on a removal
 * keeps an owner. A single shared type would make posting the wrong verb a type-correct
 * mistake, and this is the one surface where that mistake seats or unseats someone.
 */
export type AdmissionVote = "admit" | "reject";

/**
 * One eligible peer voter on an admission and their answer.
 *
 * Same wire caveat as {@link RemovalPeer}: an unanswered peer arrives as `""` or
 * `"pending"`, so `vote` goes through `admissionVote` (`shared/lib/decision.ts`), never a
 * `?? null`.
 */
export interface AdmissionPeer {
  user_id: string;
  email: string;
  vote?: string | null;
  voted_at?: Timestamp | null;
}

/**
 * A proposal to grant someone a seat — the ONLY way `Role::Owner` is granted once the fund
 * holds two owners (`SetRole` refuses it outside an executed admission).
 *
 * Two differences from {@link OwnerRemoval}, and neither is cosmetic:
 *
 *   · There is no target-decision trio. The candidate is not an owner yet, so they have no
 *     say in their own admission and there is no mailbox path that could carry it.
 *   · `peers` is every owner except the initiator, and admission passes only on UNANIMITY
 *     of that set with at least one member in it. Removal has a second path (the target's
 *     own acceptance); admission has none, so an absent peer is a proposal that cannot
 *     pass rather than one that passes another way.
 *
 * As with removals, membership of `peers` IS the eligibility test — it is the voter set
 * frozen at open, and an owner seated since is not in it (docs/CONSILIUM.md, policy 3).
 */
export interface OwnerAdmission {
  id: string;
  state: string;
  candidate_user_id: string;
  candidate_email: string;
  initiator_user_id: string;
  initiator_email: string;
  reason: string;
  peers: AdmissionPeer[];
  owner_count: number;
  created_at: Timestamp;
  expires_at: Timestamp;
  decided_at?: Timestamp | null;
  void_reason?: string | null;
  /** Serialised as a string by the BFF's integer encoding; never do arithmetic on it here. */
  version: number | string;
}

export interface OwnerAdmissionList {
  items: OwnerAdmission[];
}

/**
 * What a user proposal decides. Three kinds share one message because all three ask the
 * same question — "do the owners agree to change this person's standing?" — differing only
 * in what the verdict writes.
 */
export type UserProposalKind = "suspension" | "reinstatement" | "admin_admission";

/**
 * An answer to a user proposal. Deliberately NEUTRAL, where {@link RemovalVote} says
 * remove/keep and {@link AdmissionVote} says admit/reject.
 *
 * Three kinds share this vote, so a kind-specific verb would only mean something read
 * against `kind` — and a vocabulary that is correct only when cross-referenced is one that
 * eventually gets rendered wrong. The kind-specific verb belongs on the SURFACE, which
 * knows the kind; this is which way the voter pushed.
 */
export type ProposalVote = "for" | "against";

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

export interface UserProposalList {
  items: UserProposal[];
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

/**
 * A consilium as the owners' room sees it. Five kinds: a payment order (`payment`), a NAV
 * mark past the move guard (`valuation_override`), a change of a product's fee terms
 * (`fee_policy`), a person seated on a reserved allocation (`holder_grant`) and a seed of
 * the platform's capital (`seed_capital`), told apart by which sibling is set — exactly one is, and a kind is
 * never expressed by widening another one's field.
 *
 * The fields past `expires_at` are the ones the money plane records as a request settles.
 * They are optional because an open request carries none of them, and because a client
 * that hard-requires an outcome field cannot render a request that has no outcome yet.
 */
export interface Consilium {
  id: string;
  state: string;
  payment?: ConsiliumPaymentTerms | null;
  valuation_override?: ValuationOverride | null;
  fee_policy?: ConsiliumFeePolicyTerms | null;
  holder_grant?: HolderGrantTerms | null;
  seed_capital?: SeedCapitalTerms | null;
  payload_hash: string;
  initiator_user_id?: string;
  initiator_email: string;
  threshold: number;
  approvals: number;
  owner_count: number;
  created_at: Timestamp;
  expires_at: Timestamp;
  decided_at?: Timestamp | null;
  /** Why a request that reached `Approved` did not move money (policy 16). */
  failure_reason?: string | null;
  executed_withdrawal_id?: string | null;
  /** The payment order a payment consilium carried into execution. */
  executed_payment_id?: string | null;
  /** The valuation a NAV-mark consilium recorded on execution. */
  executed_valuation_id?: string | null;
  /** The change a fee-policy consilium scheduled when the owners carried it. */
  executed_fee_policy_change_id?: string | null;
  /** The in-kind issuance a holder-grant consilium minted when the owners carried it. */
  executed_issuance_id?: string | null;
  /** The `fund` subscription a seed-capital consilium booked when the owners carried it. */
  executed_subscription_id?: string | null;
}

export interface ConsiliumList {
  items: Consilium[];
}

// ── The realtime frame ─────────────────────────────────────────────────────────

/**
 * Everything the websocket is allowed to say.
 *
 * A frame is a doorbell, not a delivery: `revision` moves, the client re-reads the
 * authoritative snapshot over REST, and a stale or spoofed frame can therefore only ever
 * cause a redundant fetch — never a wrong count on screen (policy 21). A `heartbeat` frame
 * proves the connection is alive and carries no revision change.
 */
export interface ConsiliumFrame {
  revision?: number;
  at?: Timestamp;
  heartbeat?: boolean;
}
