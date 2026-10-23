"use client";

// The consent page's answer, as state: what is in flight, what the server said about the
// last attempt, and whether this very page recorded the answer.
//
// Everything counted here is read back from the server. A wrong code is counted in the
// same transaction as the comparison (policy 7), so a number this hook worked out for
// itself would at best duplicate the server's and at worst contradict it.

import { useState } from "react";

import { ApprovalUnavailableError } from "@/entities/approval/api/approval-client";
import { consentApprovalResource, decideConsent } from "@/entities/approval/model/approval-resource";
import type { ConsentDecision, PaymentConsentInvitation } from "@/shared/contracts/payments";
import { settledConsent } from "@/shared/lib/decision";

export function useConsentDecision(token: string, invitation: PaymentConsentInvitation | null, refresh: () => Promise<unknown>) {
  const [pending, setPending] = useState<ConsentDecision | null>(null);
  const [error, setError] = useState<unknown>(null);
  /** Set only from a server figure that actually moved. */
  const [rejectedAttempts, setRejectedAttempts] = useState<number | null>(null);
  const [justDecided, setJustDecided] = useState(false);
  /** An answer met the single 404: the token is spent, burned or gone. Which is not ours to know. */
  const [spent, setSpent] = useState(false);

  const decide = async (decision: ConsentDecision, code: string) => {
    const attemptsBefore = invitation?.attempts_remaining ?? null;
    setPending(decision);
    setError(null);
    // Cleared per attempt: left standing it would show the previous try's count AND
    // suppress this one's error banner, so a network failure after a wrong code says nothing.
    setRejectedAttempts(null);
    try {
      const result = await decideConsent(token, code, decision);
      if (result.decided) setJustDecided(true);
      else setRejectedAttempts(result.invitation.attempts_remaining);
    } catch (cause) {
      if (cause instanceof ApprovalUnavailableError) {
        setSpent(true);
        return;
      }
      setError(cause);
      // The authority on attempts left is the server; a network error consumed none.
      await refresh();
      const fresh = consentApprovalResource.peek(token);
      if (fresh && attemptsBefore !== null && fresh.attempts_remaining < attemptsBefore && !settledConsent(fresh.decision)) {
        setRejectedAttempts(fresh.attempts_remaining);
      }
    } finally {
      setPending(null);
    }
  };

  return { pending, error, rejectedAttempts, justDecided, spent, decide };
}
