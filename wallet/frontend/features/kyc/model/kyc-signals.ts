"use client";

// The funnel's view of verification: `kyc_completed`, recorded once per tab when a tier
// this tab has been watching reaches the entry tier. The rule itself is in
// `../lib/kyc-completion`; this is its React face, mounted by `useKycStatus` so every
// screen that reads the tier is a screen that can notice the verdict.

import { useAnalytics } from "@evinvest/analytics/react";
import { useEffect, useRef } from "react";

import { completesVerification } from "@/features/kyc/lib/kyc-completion";
import { ACTIVATION, marked, once, unmark } from "@/shared/analytics";

/** Tab-scoped mark `kyc_started` sets, so the verdict is recognised after the vendor round-trip. */
export const KYC_PENDING_MARK = "kyc_pending";

export function useKycCompletedSignal(level: number, loading: boolean): void {
  const capture = useAnalytics();
  const seen = useRef<number | null>(null);
  useEffect(() => {
    if (loading) return;
    const before = seen.current;
    seen.current = level;
    if (!completesVerification(before, level, marked(KYC_PENDING_MARK))) return;
    unmark(KYC_PENDING_MARK);
    // Several screens mount this at once (Home's checklist + profile row); the mark makes it one event.
    if (once(ACTIVATION.kycCompleted)) capture(ACTIVATION.kycCompleted, { level });
  }, [level, loading, capture]);
}
