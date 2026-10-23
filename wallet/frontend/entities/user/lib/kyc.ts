import type { UserProfile } from "@/shared/contracts";

/** The only tier `/kyc/start` opens a case for, and the one the hub gates money on. */
export const ENTRY_TIER = 1;

/**
 * The tier the hub is holding for this caller.
 *
 * An absent `kyc_level` counts as 0 rather than unknown — proto3 omits zero-valued scalars
 * from JSON, so "no field" is exactly how an unverified user arrives.
 */
export function kycLevel(profile: UserProfile | null | undefined): number {
  return profile?.kyc_level ?? 0;
}

/**
 * The tier gate: below {@link ENTRY_TIER} the hub issues no deposit address and refuses every
 * withdrawal, so "is refused a deposit address" and "has not been verified" are one predicate,
 * kept here in the entity that owns the field rather than copied into each screen.
 *
 * An absent PROFILE answers `false`: nothing is known about that caller's tier yet, and a
 * screen that guessed would hide a deposit address from someone entitled to it.
 */
export function isUnverified(profile: UserProfile | null | undefined): boolean {
  return profile != null && kycLevel(profile) === 0;
}

/**
 * A verification attempt that is still running, as far as this predicate cares. Structural on
 * purpose: the full shape belongs to `features/kyc`, and an entity may not reach into a
 * feature to borrow it.
 */
export interface RunningCase {
  /** Whether the vendor session behind the case can still be re-entered. */
  resumable: boolean;
}

/**
 * May this caller be offered a start — the question every surface that shows the button has to
 * answer, and the one that used to be answered from the tier alone (#190).
 *
 * Three states, not two:
 *
 *  · **no case** — offer the start; this is the only state the cabinet modelled before.
 *  · **a running case that is resumable** — still offer it. A repeat start returns the SAME
 *    vendor session rather than buying a new one (concierge#55), so the action is a "continue",
 *    and refusing it would strand a user who closed the vendor tab by accident.
 *  · **a running case that is NOT resumable** — refuse. The case is alive and holds the start
 *    gate, but nothing on the plane can send the browser back to the vendor, so a start here
 *    can only fail. That user is waiting on a decision, and the screen owes them a sentence,
 *    not a button.
 *
 * Above the entry tier there is nothing to start: `/kyc/start` opens a case for tier 1 and
 * nothing higher, so a press would spend a paid session on a verdict that cannot raise anyone.
 */
export function canStartVerification(level: number, runningCase: RunningCase | null): boolean {
  return level === 0 && (runningCase === null || runningCase.resumable);
}
