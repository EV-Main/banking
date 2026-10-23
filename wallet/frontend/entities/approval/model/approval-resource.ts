"use client";

// The token-addressed reads, as resources.
//
// They go through `defineResource` for the same reason every other read in this cabinet
// does: it is the one place a read is started, and it starts it from `useResource`'s
// subscription rather than from a `useEffect`. A hand-rolled fetch-on-mount is a `setState`
// running synchronously inside an effect — cascading renders, and the thing
// `react-hooks/set-state-in-effect` exists to stop.
//
// Two properties of the cache matter here and are worth being explicit about, because these
// resources are keyed by a credential:
//
//   · Memory only. `persist` is off (its default), so nothing is written to
//     sessionStorage. The token lives exactly as long as the tab does, which is no longer
//     than it already lives in `window.location` and the browser's history.
//   · Re-reading is free. The GET is strictly inert — mail scanners fetch it before any
//     human does (docs/CONSILIUM.md, policy 5) — so the focus-driven revalidation that
//     comes with the cache costs a redacted summary and nothing else. It spends no token
//     and casts no vote.
//
// The revalidate window is long because nothing on these pages changes on its own: an
// invitation is settled by the reader in front of it, and the only other transition it can
// make is expiring.

import {
  fetchConsentApproval,
  fetchPayoutApproval,
  fetchRemovalApproval,
  submitConsentDecision,
  submitPayoutDecision,
  submitRemovalDecision,
} from "@/entities/approval/api/approval-client";
import type {
  PayoutApprovalResult,
  PayoutDecision,
  RemovalApprovalResult,
  RemovalDecision,
} from "@/shared/contracts/governance";
import type { ConsentDecision, PaymentConsentResult } from "@/shared/contracts/payments";
import { defineResource } from "@/shared/lib/resource";

const REVALIDATE_S = 60;

export const payoutApprovalResource = defineResource({
  name: "approval.payout",
  fetch: fetchPayoutApproval,
  key: (token: string) => token,
  revalidate: REVALIDATE_S,
  enabled: (token: string) => token.trim().length > 0,
});

export const removalApprovalResource = defineResource({
  name: "approval.removal",
  fetch: fetchRemovalApproval,
  key: (token: string) => token,
  revalidate: REVALIDATE_S,
  enabled: (token: string) => token.trim().length > 0,
});

export const consentApprovalResource = defineResource({
  name: "approval.consent",
  fetch: fetchConsentApproval,
  key: (token: string) => token,
  revalidate: REVALIDATE_S,
  enabled: (token: string) => token.trim().length > 0,
});

/**
 * Cast the payout vote and adopt the answer.
 *
 * The response carries the authoritative invitation — the tally, the settled decision and
 * the attempt counter as the server computed them under the request row's lock — so it is
 * published straight into the cache rather than invalidated. Asking again would be asking
 * a question we have just been given the answer to, and the answer we already hold is the
 * one that counted.
 */
export async function decidePayout(token: string, code: string, decision: PayoutDecision): Promise<PayoutApprovalResult> {
  const result = await submitPayoutDecision(token, code, decision);
  payoutApprovalResource.publish(result.invitation, token);
  return result;
}

/** The same, for the removal notice. */
export async function decideRemoval(token: string, code: string, decision: RemovalDecision): Promise<RemovalApprovalResult> {
  const result = await submitRemovalDecision(token, code, decision);
  removalApprovalResource.publish(result.invitation, token);
  return result;
}

/** The same, for an investor's consent over their own money. */
export async function decideConsent(token: string, code: string, decision: ConsentDecision): Promise<PaymentConsentResult> {
  const result = await submitConsentDecision(token, code, decision);
  consentApprovalResource.publish(result.invitation, token);
  return result;
}
