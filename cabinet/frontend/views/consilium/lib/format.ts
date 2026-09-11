// Display helpers for the owners' room. Plain TypeScript — every helper that produces words
// takes the caller's `t`, the same contract `views/admin/lib/format.ts` follows.

import type { Translate } from "@evinvest/i18n";

import type {
  AdmissionPeer,
  AdmissionVote,
  OwnerAdmission,
  OwnerRemoval,
  RemovalPeer,
  RemovalVote,
} from "@/shared/contracts/governance";
import { admissionVote, peerVote, settledRemoval } from "@/shared/lib/decision";

/**
 * States in which nothing further can be voted.
 *
 * Compared case-insensitively and matched loosely, because the state machine is the money
 * plane's and this list is a mirror of it. The fallback direction is deliberate: a state we
 * do not recognise is treated as still OPEN, so a vote control stays on screen and the
 * server refuses it if it is wrong. The other way round — hiding the control for a state we
 * have not met — would silently remove an owner's ability to vote because a backend release
 * added a word to an enum.
 */
const TERMINAL: ReadonlySet<string> = new Set([
  "approved",
  "rejected",
  "removed",
  "kept",
  "refused",
  "expired",
  "cancelled",
  "canceled",
  "void",
  "voided",
  "executed",
  "executionfailed",
  "failed",
  "complete",
  "completed",
]);

/**
 * uikit's `Empty` draws a dashed frame but leaves the border width to the caller, and
 * doubles its padding at `md`. Stated once for the slice — the dashboard makes the same
 * call for the same reason (`views/dashboard/ui/dashboard-view.tsx`).
 */
export const EMPTY_BOX = "border md:p-6";

const normalise = (state: string | undefined): string => (state ?? "").toLowerCase().replaceAll("_", "");

/** Whether a request is finished with — settled, expired, cancelled or void. */
export function isSettled(state: string | undefined, decidedAt?: string | null): boolean {
  return Boolean(decidedAt) || TERMINAL.has(normalise(state));
}

// States that have a name in the catalogue. Anything else falls back to the bare wire word,
// which is legible — `admin/lib/format.ts` makes the same call and for the same reason: a
// reader seeing `reconciling` is better served than one seeing `consilium.state.reconciling`.
const KNOWN_STATES: ReadonlySet<string> = new Set([
  "open",
  "pending",
  "approved",
  "rejected",
  "removed",
  "kept",
  "expired",
  "cancelled",
  "void",
  "executed",
  "executionfailed",
  "failed",
]);

export function stateLabel(state: string | undefined, t: Translate): string {
  const key = normalise(state);
  return KNOWN_STATES.has(key) ? t(`consilium.state.${key}`) : (state ?? "—");
}

/** Token classes for a state pill. Neutral unless the state carries real news. */
export function stateTone(state: string | undefined): string {
  const key = normalise(state);
  if (key === "approved" || key === "executed" || key === "removed") return "text-positive";
  if (key === "open" || key === "pending") return "text-accent-debug";
  if (key === "rejected" || key === "failed" || key === "executionfailed" || key === "void") return "text-accent-error";
  return "text-ink-soft";
}

// Both take the RAW wire value and normalise on the way in, so no call site can forget to.
export function voteLabel(vote: string | null | undefined, t: Translate): string {
  const cast = peerVote(vote);
  if (cast === "remove") return t("consilium.vote.remove");
  if (cast === "keep") return t("consilium.vote.keep");
  return t("consilium.vote.waiting");
}

export function voteTone(vote: string | null | undefined): string {
  const cast = peerVote(vote);
  if (cast === "remove") return "text-positive";
  if (cast === "keep") return "text-accent-warn";
  return "text-ink-soft";
}

/** The target's own answer, normalised. Null while they have not answered. */
export function targetAnswer(removal: OwnerRemoval): RemovalVote | null {
  return settledRemoval(removal.target_decision);
}

/**
 * How a removal stands on the peers' votes alone.
 *
 * Path (b) is UNANIMITY over the eligible peers, not a majority — so a single `keep` ends
 * it, and the page says so rather than leaving a progress bar creeping towards a total it
 * can no longer reach. Path (a), the target accepting from their own mailbox, is unaffected
 * by any of this and is reported separately (docs/CONSILIUM.md § Owner removal).
 */
export interface PeerTally {
  toRemove: number;
  toKeep: number;
  total: number;
  waiting: number;
  /** Unanimity is still reachable: nobody has voted to keep. */
  unanimityPossible: boolean;
}

// The `?? []` on every list read here and in `standingIn` is not paranoia about our own
// types — it is the wire's. JSON in this stack follows proto3 semantics, where an empty
// repeated field and a zero are simply absent from the payload (`shared/contracts/index.ts`
// says so for the generated half). A removal with no peers yet is exactly the case that
// arrives with `peers` missing, and it is also the case the two-owner rule is about, so it
// is the one that must not throw.
export function peerTally(peers: readonly RemovalPeer[] | undefined): PeerTally {
  const list = peers ?? [];
  const toRemove = list.filter((p) => peerVote(p.vote) === "remove").length;
  const toKeep = list.filter((p) => peerVote(p.vote) === "keep").length;
  return {
    toRemove,
    toKeep,
    total: list.length,
    waiting: list.length - toRemove - toKeep,
    unanimityPossible: toKeep === 0 && list.length > 0,
  };
}

/**
 * Where the caller stands in relation to one removal — the single question the vote UI
 * branches on, answered once so the view does not re-derive it three times.
 *
 * `peer` is looked up in the removal's OWN snapshot of eligible voters rather than in the
 * live roster. The voter set is frozen when the request is opened, so an owner added since
 * is not among them and cannot be made one by refreshing the page (policy 3).
 */
export type Standing =
  | { role: "peer"; vote: RemovalVote | null }
  | { role: "target" }
  | { role: "initiator" }
  | { role: "bystander" };

export function standingIn(removal: OwnerRemoval, userId: string | null): Standing {
  if (!userId) return { role: "bystander" };
  if (removal.target_user_id === userId) return { role: "target" };
  if (removal.initiator_user_id === userId) return { role: "initiator" };
  const peer = (removal.peers ?? []).find((p) => p.user_id === userId);
  // `peerVote`, not `?? null`: an unanswered peer arrives as an empty string, which is
  // truthy — read raw it both labels them as having voted and removes the vote buttons
  // from someone who still has a vote to give.
  if (peer) return { role: "peer", vote: peerVote(peer.vote) };
  return { role: "bystander" };
}

// ── admissions ────────────────────────────────────────────────────────────────
//
// Deliberate near-duplicates of the four helpers above rather than a shared generic over
// both. The vocabularies are different enums with one word in common that means opposite
// things — `reject` KEEPS an owner on a removal and REFUSES a candidate on an admission —
// so a generic parameterised by a vote type would make "which plane am I in?" an argument
// someone can pass wrongly, on the one surface where a wrong verb seats or unseats a
// person. Two short functions that cannot be confused beat one clever one that can.

/** An admission vote as a reader sees it. Takes the RAW wire value and normalises on entry. */
export function admissionVoteLabel(vote: string | null | undefined, t: Translate): string {
  const cast = admissionVote(vote);
  if (cast === "admit") return t("consilium.admissionVote.admit");
  if (cast === "reject") return t("consilium.admissionVote.reject");
  return t("consilium.vote.waiting");
}

export function admissionVoteTone(vote: string | null | undefined): string {
  const cast = admissionVote(vote);
  if (cast === "admit") return "text-positive";
  if (cast === "reject") return "text-accent-error";
  return "text-ink-soft";
}

/**
 * How an admission stands on its peers' votes.
 *
 * Unanimity of every owner except the initiator, with at least one such peer — stricter
 * than removal, which has a second path through the target's own acceptance. An admission
 * has no second path, so `total === 0` is not "waiting for a mailbox" but a proposal that
 * cannot pass at all; the plane refuses to open one, and if a peer somehow leaves the set
 * afterwards the page says so rather than showing a bar creeping toward nothing.
 *
 * A single `reject` ends it, which is why `unanimityPossible` is tracked separately from
 * the count: three of four `admit` votes is not "nearly there" if the fourth said no.
 */
export interface AdmissionTally {
  toAdmit: number;
  toReject: number;
  total: number;
  waiting: number;
  /** Unanimity is still reachable: nobody has voted to reject, and there is someone to agree. */
  unanimityPossible: boolean;
}

export function admissionTally(peers: readonly AdmissionPeer[] | undefined): AdmissionTally {
  // `?? []` for the same wire reason as `peerTally`: proto3 JSON omits an empty repeated
  // field, and the empty case is precisely the one the "at least one peer" rule is about.
  const list = peers ?? [];
  const toAdmit = list.filter((p) => admissionVote(p.vote) === "admit").length;
  const toReject = list.filter((p) => admissionVote(p.vote) === "reject").length;
  return {
    toAdmit,
    toReject,
    total: list.length,
    waiting: list.length - toAdmit - toReject,
    unanimityPossible: toReject === 0 && list.length > 0,
  };
}

/**
 * Where the caller stands in one admission.
 *
 * `candidate` exists even though a candidate is not an owner and cannot open this page:
 * they can be looking at it the moment after their own admission carried, and a seated
 * owner reading "you are not an eligible voter" about their own admission would be told
 * something both cold and untrue. Peers are read from the admission's own frozen set, not
 * the live roster, for the same reason removals are (policy 3).
 */
export type AdmissionStanding =
  | { role: "peer"; vote: AdmissionVote | null }
  | { role: "candidate" }
  | { role: "initiator" }
  | { role: "bystander" };

export function standingInAdmission(admission: OwnerAdmission, userId: string | null): AdmissionStanding {
  if (!userId) return { role: "bystander" };
  if (admission.initiator_user_id === userId) return { role: "initiator" };
  if (admission.candidate_user_id === userId) return { role: "candidate" };
  const peer = (admission.peers ?? []).find((p) => p.user_id === userId);
  if (peer) return { role: "peer", vote: admissionVote(peer.vote) };
  return { role: "bystander" };
}
