"use client";

// Starting verification, as a state machine the presentations share.
//
// There are two of them — a row inside the profile's Verification card, and a block standing
// where a money surface would be for a caller the hub has not cleared yet — and they differ
// only in their frame. What must not differ is any of this: that a start in flight disables
// its own control, that `started` leaves the page instead of returning to idle, and which
// outcomes are worth a sentence of their own.

import { useAnalytics } from "@evinvest/analytics/react";
import { useState } from "react";

import { startVerification, type KycStart } from "@/features/kyc/api/kyc-client";
import { KYC_PENDING_MARK } from "@/features/kyc/model/kyc-signals";
import { ACTIVATION, mark, once } from "@/shared/analytics";

/** Idle, in flight, or what came back — every outcome but `started`, which leaves this page. */
export type StartState = { kind: "idle" | "starting" } | Exclude<KycStart, { kind: "started" }>;

export interface StartVerification {
  state: StartState;
  starting: boolean;
  begin: () => Promise<void>;
  /**
   * Forget the last outcome.
   *
   * The dialog presentation is mounted beside its trigger and stays mounted while closed, so
   * this state outlives a close — and `unavailable` is what nearly every reader gets today.
   * Without this, opening the dialog a minute later re-announced a 503 from an attempt that
   * had already been read and dismissed, as if a new one had just failed.
   */
  reset: () => void;
}

export function useStartVerification(): StartVerification {
  const [state, setState] = useState<StartState>({ kind: "idle" });
  const starting = state.kind === "starting";
  const capture = useAnalytics();

  async function begin() {
    if (starting) return;
    setState({ kind: "starting" });
    const result = await startVerification();
    if (result.kind === "started") {
      // The funnel step is "a vendor session was opened", not "the button was pressed": a
      // refused start (503, throttled) began nothing. Once per tab — a repeat start resumes
      // the same case (concierge#55), so it is the same step. The pending mark is what lets
      // `kyc_completed` recognise the verdict when this tab comes back from the vendor.
      mark(KYC_PENDING_MARK);
      if (once(ACTIVATION.kycStarted)) capture(ACTIVATION.kycStarted);
      // Deliberately NOT back to idle first: the provider's page is already loading over
      // this one, and re-enabling the control would flash a second chance at someone who
      // is leaving — and buy a duplicate case if they took it.
      window.location.assign(result.redirectUrl);
      return;
    }
    setState(result);
  }

  function reset() {
    // Not while a start is in flight: `begin` writes the outcome when it lands, and clearing
    // in between would only mean the answer arrives into a state nobody is expecting.
    if (!starting) setState({ kind: "idle" });
  }

  return { state, starting, begin, reset };
}
