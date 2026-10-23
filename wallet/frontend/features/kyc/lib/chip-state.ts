/**
 * What the shell's verification chip says, decided once and away from React.
 *
 * Four states, ordered by what the reader can do about them. The tier is the verdict the hub
 * acts on, so it outranks every case: a verified caller is verified whatever a stale case row
 * says. Below the entry tier the running case decides — and one of its statuses is not a wait
 * but a request: `resubmitted` means a reviewer sent specific steps back (concierge migration
 * 0010), so the reader is the one holding it up, and the chip has to say so. A decided case
 * — approved, declined, expired — never reaches the browser: the plane drops it from
 * `/kyc/status` the moment it resolves (`../api/kyc-contract`), so a declined attempt looks
 * exactly like no attempt, and the chip offers a start again rather than inventing a verdict
 * it cannot read.
 *
 * `null` when nothing could be read: `useKycStatus` answers `loading: false, level: 0` once
 * BOTH reads have failed, and a chip that took that for tier 0 would call a verified reader
 * "Not verified" over a network blip.
 *
 * Pure, so `node --test` can run it: the wallet host has no component tests.
 */
export type KycChipState = "notStarted" | "review" | "verified" | "attention";

/** A `PillTone` of `@/shared/ui/list-card` — named here so the test needs no React import. */
export type KycChipTone = "neutral" | "pending" | "success" | "error";

export interface KycChip {
  state: KycChipState;
  tone: KycChipTone;
  labelKey: `kyc.chip.${KycChipState}`;
}

/** Structural on purpose: the chip needs one field of the case, not the whole contract. */
export interface KycChipRead {
  level: number;
  runningCase: { status: string } | null;
  /** Whether either source answered at all — `false` is "unknown", never "tier 0". */
  settled: boolean;
}

/** The running statuses that wait on the READER rather than on a reviewer. */
const ATTENTION_STATUSES: readonly string[] = ["resubmitted"];

export function kycChipState({ level, runningCase, settled }: KycChipRead): KycChip | null {
  if (!settled) return null;
  if (level > 0) return { state: "verified", tone: "success", labelKey: "kyc.chip.verified" };
  if (runningCase === null) return { state: "notStarted", tone: "neutral", labelKey: "kyc.chip.notStarted" };
  if (ATTENTION_STATUSES.includes(runningCase.status)) return { state: "attention", tone: "error", labelKey: "kyc.chip.attention" };
  // The same tone /profile gives the running case (`profile.kyc.casePill`).
  return { state: "review", tone: "pending", labelKey: "kyc.chip.review" };
}
