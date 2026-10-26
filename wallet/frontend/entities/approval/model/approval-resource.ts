"use client";

// The token-addressed read, as a resource.
//
// It goes through `defineResource` for the same reason every other read in this wallet host
// does: it is the one place a read is started, and it starts it from `useResource`'s
// subscription rather than from a `useEffect`. A hand-rolled fetch-on-mount is a `setState`
// running synchronously inside an effect — cascading renders, and the thing
// `react-hooks/set-state-in-effect` exists to stop.
//
// Two properties of the cache matter here and are worth being explicit about, because this
// resource is keyed by a credential:
//
//   · Memory only. `persist` is off (its default), so nothing is written to
//     sessionStorage. The token lives exactly as long as the tab does, which is no longer
//     than it already lives in `window.location` and the browser's history.
//   · Re-reading is free. The GET is strictly inert — mail scanners fetch it before any
//     human does — so the focus-driven revalidation that comes with the cache costs a
//     redacted summary and nothing else. It spends no token and casts no vote.
//
// The revalidate window is long because nothing on this page changes on its own: an
// invitation is settled by the reader in front of it, and the only other transition it can
// make is expiring.

import { fetchRemovalApproval, submitRemovalDecision } from "@/entities/approval/api/approval-client";
import type { RemovalApprovalResult, RemovalDecision } from "@/shared/contracts/governance";
import { defineResource } from "@/shared/lib/resource";

export const removalApprovalResource = defineResource({
  name: "approval.removal",
  fetch: fetchRemovalApproval,
  key: (token: string) => token,
  revalidate: 60,
  enabled: (token: string) => token.trim().length > 0,
});

/**
 * Cast the removal answer and adopt the result.
 *
 * The response carries the authoritative invitation — the settled decision and the attempt
 * counter as the server computed them — so it is published straight into the cache rather
 * than invalidated. Asking again would be asking a question we have just been given the
 * answer to.
 */
export async function decideRemoval(token: string, code: string, decision: RemovalDecision): Promise<RemovalApprovalResult> {
  const result = await submitRemovalDecision(token, code, decision);
  removalApprovalResource.publish(result.invitation, token);
  return result;
}
