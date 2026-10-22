// Display helpers for the owners' room. Plain TypeScript — every helper that produces words
// takes the caller's `t`.

import type { Translate } from "@evinvest/i18n";

import type {
  AdmissionPeer,
  AdmissionVote,
  Consilium,
  OwnerAdmission,
  OwnerRemoval,
  ProposalVote,
  RemovalPeer,
  RemovalVote,
  UserProposal,
  UserProposalPeer,
} from "@/shared/contracts/governance";
// Relative `.ts` paths, not the `@/` alias, for the same reason `views/trade/lib/order-state.ts`
// uses them: `node --test` does not resolve the alias for value imports, and this module's
// rules (#318) are exactly the ones a unit test has to be able to reach.
import { admissionVote, peerVote, proposalVote, settledRemoval } from "../../../shared/lib/decision.ts";
import { hasUnixStamp } from "../../../shared/lib/unix-stamp.ts";
import { wordFor } from "../../../shared/lib/wire-words.ts";

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

/**
 * Whether a request is finished with — settled, expired, cancelled or void.
 *
 * `hasUnixStamp`, not truthiness: an undecided request arrives with `decided_at` as the
 * string "0", which is truthy, and reading it raw filed every OPEN consilium under "Earlier
 * requests" while the live card said none was open (#318).
 */
export function isSettled(state: string | undefined, decidedAt?: string | null): boolean {
  return hasUnixStamp(decidedAt) || TERMINAL.has(normalise(state));
}

// States that have a name. Anything else falls back to the bare wire word,
// which is legible — `admin/lib/format.ts` makes the same call and for the same reason: a
// reader seeing `reconciling` is better served than one seeing `consilium.state.reconciling`.
const stateWords = (t: Translate): Readonly<Record<string, string>> => ({
  open: t("consilium.state.open", "Open"),
  pending: t("consilium.state.pending", "Pending"),
  approved: t("consilium.state.approved", "Approved"),
  rejected: t("consilium.state.rejected", "Rejected"),
  removed: t("consilium.state.removed", "Removed"),
  kept: t("consilium.state.kept", "Seat kept"),
  expired: t("consilium.state.expired", "Expired"),
  cancelled: t("consilium.state.cancelled", "Cancelled"),
  void: t("consilium.state.void", "Void"),
  executed: t("consilium.state.executed", "Paid out"),
  executionfailed: t("consilium.state.executionfailed", "Payout failed"),
  failed: t("consilium.state.failed", "Failed"),
});

export function stateLabel(state: string | undefined, t: Translate): string {
  return wordFor(stateWords(t), normalise(state)) ?? (state ?? "—");
}

// ── the kinds of consilium ────────────────────────────────────────────────────
// Told apart by which sibling is set — a kind is never expressed by widening another
// one's field (`shared/contracts/governance.ts`). The order matches the views' own
// branches, so a row that somehow carries two is read the same way everywhere. A row with
// no sibling set is a kind this client does not know yet: it reads as no kind rather than
// being passed off as one of the five.

export type ConsiliumKind = "valuation_override" | "payment" | "fee_policy" | "holder_grant" | "seed_capital";

type Kinded = Pick<Consilium, "valuation_override" | "payment" | "fee_policy" | "holder_grant" | "seed_capital">;

export function consiliumKind(consilium: Kinded): ConsiliumKind | null {
  if (consilium.valuation_override) return "valuation_override";
  if (consilium.payment) return "payment";
  if (consilium.fee_policy) return "fee_policy";
  if (consilium.holder_grant) return "holder_grant";
  if (consilium.seed_capital) return "seed_capital";
  return null;
}

const kindWords = (t: Translate): Record<ConsiliumKind, string> => ({
  valuation_override: t("consilium.kind.valuation_override", "NAV mark"),
  payment: t("consilium.kind.payment", "Payment"),
  fee_policy: t("consilium.kind.fee_policy", "Fee terms"),
  holder_grant: t("consilium.kind.holder_grant", "Holder grant"),
  seed_capital: t("consilium.kind.seed_capital", "Seed capital"),
});

export function consiliumKindLabel(kind: ConsiliumKind | null, t: Translate): string {
  return kind ? kindWords(t)[kind] : "—";
}

/** A state pill that knows what was decided: `executed` on a fee-policy consilium means
 *  the change was SCHEDULED, on a holder grant that the units were MINTED, on a seed that
 *  the deposit and subscription were BOOKED — not that anything was paid out. */
export function consiliumStateLabel(consilium: Kinded & Pick<Consilium, "state">, t: Translate): string {
  if (normalise(consilium.state) === "executed") {
    const kind = consiliumKind(consilium);
    if (kind === "fee_policy") return t("consilium.feePolicy.carried", "Carried");
    if (kind === "holder_grant") return t("consilium.holderGrant.carried", "Minted");
    if (kind === "seed_capital") return t("consilium.seedCapital.carried", "Booked");
  }
  return stateLabel(consilium.state, t);
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
  if (cast === "remove") return t("consilium.vote.remove", "To remove");
  if (cast === "keep") return t("consilium.vote.keep", "To keep");
  return t("consilium.vote.waiting", "Not answered");
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
  if (cast === "admit") return t("consilium.admissionVote.admit", "Admit");
  if (cast === "reject") return t("consilium.admissionVote.reject", "Refuse");
  return t("consilium.vote.waiting", "Not answered");
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

// ── user proposals ────────────────────────────────────────────────────────────
//
// A third near-duplicate set, for a sharper version of the same reason. The vote here is
// NEUTRAL — `for`/`against` — because one message carries three kinds, so the words say
// which way someone pushed and nothing about what it does. That makes the verb a property
// of the SURFACE, and these helpers are where the surface decides it.

/** The three kinds, as a reader sees them. An unrecognised kind falls back to its wire
 *  word, the same call `roleLabel` makes: `reconciling` beats `consilium.kind.reconciling`. */
const proposalKindWords = (t: Translate): Readonly<Record<string, string>> => ({
  suspension: t("consilium.proposalKind.suspension", "Permanent block"),
  reinstatement: t("consilium.proposalKind.reinstatement", "Letting back in"),
  admin_admission: t("consilium.proposalKind.admin_admission", "Admin seat"),
});

export function proposalKindLabel(kind: string, t: Translate): string {
  return wordFor(proposalKindWords(t), kind) ?? kind;
}

const proposalVerbs = (t: Translate): Readonly<Record<string, Record<ProposalVote, string>>> => ({
  suspension: { for: t("consilium.proposalVerb.suspension.for", "Block permanently"), against: t("consilium.proposalVerb.suspension.against", "Do not block") },
  reinstatement: { for: t("consilium.proposalVerb.reinstatement.for", "Let back in"), against: t("consilium.proposalVerb.reinstatement.against", "Keep blocked") },
  admin_admission: { for: t("consilium.proposalVerb.admin_admission.for", "Grant admin"), against: t("consilium.proposalVerb.admin_admission.against", "Refuse") },
});

/**
 * The kind's own verb for a vote, in the direction the voter is pushing.
 *
 * This is the whole reason the wire word is neutral: "for" on a suspension means *block
 * this person* and on a reinstatement means *let them back in*, and a button reading "For"
 * over either would be asking someone to agree with something unnamed. An unrecognised kind
 * falls back to the neutral words rather than guessing — a wrong verb on a governance
 * ballot is how a vote gets cast by mistake.
 */
export function proposalVoteLabel(kind: string, direction: ProposalVote, t: Translate): string {
  const verbs = proposalVerbs(t);
  if (Object.hasOwn(verbs, kind)) return verbs[kind][direction];
  return direction === "for" ? t("consilium.proposalVote.for", "For") : t("consilium.proposalVote.against", "Against");
}

export function proposalVoteTone(vote: string | null | undefined): string {
  const cast = proposalVote(vote);
  if (cast === "for") return "text-positive";
  if (cast === "against") return "text-accent-error";
  return "text-ink-soft";
}

/** How an owner's cast vote reads in the roster. Neutral here on purpose: the roster says
 *  which way each owner pushed, and the kind is stated once on the card above it. */
export function proposalPeerLabel(vote: string | null | undefined, t: Translate): string {
  const cast = proposalVote(vote);
  if (cast === "for") return t("consilium.proposalVote.for", "For");
  if (cast === "against") return t("consilium.proposalVote.against", "Against");
  return t("consilium.vote.waiting", "Not answered");
}

/**
 * How a user proposal stands.
 *
 * A MAJORITY of the snapshotted voters, not the unanimity the two owner consilia demand —
 * so unlike {@link admissionTally} a single AGAINST does not end it, and the page must not
 * imply that it does. What ends it is AGAINST passing the point where FOR can still reach
 * the threshold, which is why `stillReachable` is computed against `threshold` rather than
 * against the head count.
 *
 * `threshold` is READ, never derived. It is frozen when the proposal opens, so a roster
 * that moved underneath it does not move the bar — and a page that recomputed it from
 * `peers.length` would show owners a target their proposal is not measured against.
 */
export interface ProposalTally {
  forVotes: number;
  againstVotes: number;
  total: number;
  waiting: number;
  threshold: number;
  /** The threshold can still be met by the owners who have not answered. */
  stillReachable: boolean;
}

export function proposalTally(proposal: UserProposal): ProposalTally {
  // `?? []` for the wire reason `peerTally` gives: proto3 JSON omits an empty repeated
  // field, so an absent `peers` means none arrived, not that the shape is wrong.
  const list: readonly UserProposalPeer[] = proposal.peers ?? [];
  const forVotes = list.filter((p) => proposalVote(p.vote) === "for").length;
  const againstVotes = list.filter((p) => proposalVote(p.vote) === "against").length;
  const waiting = list.length - forVotes - againstVotes;
  return {
    forVotes,
    againstVotes,
    total: list.length,
    waiting,
    threshold: proposal.threshold,
    stillReachable: forVotes + waiting >= proposal.threshold,
  };
}

/**
 * Where the caller stands in one user proposal.
 *
 * `subject` is any user, not necessarily an owner — which is the difference from
 * {@link standingInAdmission}, and the reason an owner can be the subject of a proposal they
 * are also not eligible to vote on. Peers come from the proposal's OWN frozen set rather
 * than the live roster, as everywhere else here (docs/CONSILIUM.md, policy 3).
 */
export type ProposalStanding =
  | { role: "peer"; vote: ProposalVote | null }
  | { role: "subject" }
  | { role: "initiator" }
  | { role: "bystander" };

export function standingInProposal(proposal: UserProposal, userId: string | null): ProposalStanding {
  if (!userId) return { role: "bystander" };
  if (proposal.initiator_user_id === userId) return { role: "initiator" };
  const peer = (proposal.peers ?? []).find((p) => p.user_id === userId);
  if (peer) return { role: "peer", vote: proposalVote(peer.vote) };
  // Checked AFTER the peer lookup: the subject may also be an owner, and if they are an
  // eligible voter on some other person's proposal that is the role that matters here.
  // Being the subject only excludes them when they are not in the frozen voter set.
  if (proposal.subject_user_id === userId) return { role: "subject" };
  return { role: "bystander" };
}
