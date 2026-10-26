// The token-addressed owner-removal notice an owner reaches from their mailbox.
//
// Hand-written rather than generated: these are BFF DTOs over a concierge-owned proto,
// shaped for this wallet host. When the surface does get a proto in `./gen`, this file is
// what it replaces.
//
// A vote is a decision, not a number: the public GET carries `attempts_remaining` and a
// settled `decision`, never a running tally the client could add to.
//
// Lifecycle states travel as plain strings on purpose: a closed union here would turn a
// backend addition into a client type error and tempt a view into an exhaustive switch that
// renders nothing for a state it has not met yet.

/** An amount as it crossed the wire: an exact decimal string, never a number. */
export type Decimal = string;

/**
 * Unix SECONDS as a decimal string, `"0"` while unset — what the BFF actually sends (see
 * `shared/lib/datetime.ts`, which is the one place it is parsed). The name is kept for its
 * meaning, not its encoding.
 */
export type Timestamp = string;

export type RemovalDecision = "remove" | "keep";

/**
 * What `GET /api/approval/removal/:token` renders — a redacted summary and nothing else.
 * Unknown, expired, spent and burned tokens all answer with one identical 404, so there is
 * no "which failure was it" field.
 */
export interface RemovalApproval {
  removal_id: string;
  state: string;
  initiator_email: string;
  /** The owner whose seat this is about — the reader. */
  target_email: string;
  reason: string;
  created_at: Timestamp;
  expires_at: Timestamp;
  /**
   * This seat's answer as the wire spells it — NOT a nullable `RemovalDecision`. An
   * unanswered seat arrives as `"pending"` and an absent one as `""`, both truthy, so it is
   * read through `settledRemoval` (`shared/lib/decision.ts`), never a `?? null`.
   */
  decision?: string | null;
  /** Wrong codes left before the token burns. */
  attempts_remaining: number;
}

/**
 * The answer to a cast vote. `invitation` is the authoritative re-read — the view renders
 * that, never its own optimistic guess — and `decided` says whether this POST settled it.
 */
export interface RemovalApprovalResult {
  invitation: RemovalApproval;
  decided: boolean;
}
