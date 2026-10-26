"use client";

// The token-addressed approval endpoint, reached from a mailbox: an owner's answer to their
// own removal.
//
// These deliberately do NOT go through `shared/lib/api-client.ts`, and the reason is the
// whole character of these pages. `requestJson` is built around a session: it rotates the
// access cookie before a request that might present a stale one, and it heals a 401 by
// refreshing the session and replaying once. Every one of those steps is right for the
// signed-in wallet host and wrong here:
//
//   · There is no session. The reader followed a link from an email, quite possibly on a
//     phone that has never signed in to this wallet host at all. A pre-flight to the shell's
//     session endpoint is a round-trip that can only ever answer "no", and it makes an
//     inert, read-only page look like it is doing authentication.
//   · A 401 here is not a lapsed session. The credential on these routes is the token plus
//     the code, so healing-and-replaying would replay a *vote* against a fresh cookie —
//     and the error the reader would be shown is "Sign in again to continue", which is
//     advice they cannot take and which is not true.
//
// What IS shared is the error vocabulary: failures come back as the same `RequestError`
// every other screen reports, so `errorMessage(error, t)` translates them at the render
// boundary exactly as it does elsewhere, and no new dialect of "something went wrong"
// appears on the one surface a reader may only ever see once.

import type { RemovalApproval, RemovalApprovalResult, RemovalDecision } from "@/shared/contracts/governance";
import { apiPath } from "@/shared/config/base-path";
import { type ErrorCode, RequestError } from "@/shared/lib/api-client";
import { csrfHeader } from "@/shared/lib/csrf-client";

/**
 * The one answer for every token that cannot be acted on.
 *
 * Unknown, expired, already-spent and burned tokens all produce an identical 404 from the
 * BFF, by design — a caller must not be able to tell them apart, because telling them apart
 * is an enumeration oracle. This error carries the same
 * single meaning up to the view, which is why it is one class with no `reason` field: there
 * is nothing to put in one, and a field would invite a view to guess.
 */
export class ApprovalUnavailableError extends Error {
  readonly status = 404;
  constructor() {
    super("This approval link can no longer be used.");
    this.name = "ApprovalUnavailableError";
  }
}

/** The BFF's fixed error strings that these routes can produce, keyed for translation. */
const FRIENDLY: Record<string, { code: ErrorCode; en: string }> = {
  csrf: { code: "err.csrf", en: "This page went stale. Reload it and try again." },
  "request failed": { code: "err.requestFailed", en: "Something went wrong on our side. Please try again." },
};

function statusError(status: number, prose: string | undefined): RequestError {
  if (prose !== undefined) {
    const known = Object.hasOwn(FRIENDLY, prose) ? FRIENDLY[prose] : undefined;
    // Prose we did not author passes through unkeyed, in whatever language the BFF chose —
    // the same contract `shared/lib/api-client.ts` documents.
    return known ? new RequestError(known.en, status, known.code) : new RequestError(prose, status, null);
  }
  if (status === 429) return new RequestError("Too many requests — give it a moment and try again.", status, "err.rateLimited");
  if (status >= 500) return new RequestError("The service is temporarily unavailable. Please try again.", status, "err.serverUnavailable");
  return new RequestError(`Request failed (${status}).`, status, "err.requestFailed");
}

async function approvalJson<T>(path: `/${string}`, body?: unknown): Promise<T> {
  let res: Response;
  try {
    res = await fetch(apiPath(path), {
      method: body === undefined ? "GET" : "POST",
      // `same-origin` is the default, but it is stated because it is load-bearing here:
      // this page must never be the thing that sends an ambient credential somewhere.
      credentials: "same-origin",
      headers:
        body === undefined
          ? { accept: "application/json" }
          : { accept: "application/json", "content-type": "application/json", ...csrfHeader() },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
  } catch {
    throw new RequestError("Can't reach the server. Check your connection and try again.", 0, "err.network");
  }

  const data = (await res.json().catch(() => ({}))) as T & { error?: string };
  if (res.status === 404) throw new ApprovalUnavailableError();
  if (!res.ok) throw statusError(res.status, data.error);
  return data;
}

// Tokens are opaque and arrive from a URL segment. Encoding is belt and braces — Next has
// already decoded the segment for us, and re-encoding keeps a token containing a `/` or a
// `?` from silently addressing a different route.
const seg = (token: string) => encodeURIComponent(token);

/**
 * Read the removal notice addressed to this owner. Strictly inert: it casts no vote, spends
 * no token and changes nothing, which is precisely what makes it safe for the mail scanners
 * that will fetch it before any human does.
 */
export function fetchRemovalApproval(token: string): Promise<RemovalApproval> {
  return approvalJson<RemovalApproval>(`/api/approval/removal/${seg(token)}`);
}

/**
 * Answer a removal from the target's own mailbox — `remove` accepts it, `keep` refuses it.
 *
 * The wire vocabulary is NOT the domain one. The public approval route parses `approve`
 * and `reject`, and the BFF is what maps them onto Remove and Keep. Sending the domain words
 * here is a 400 every time, permanently — so the translation happens at the edge, in this
 * function, and the rest of the code above it goes on saying what it means.
 */
const REMOVAL_WIRE: Record<RemovalDecision, "approve" | "reject"> = { remove: "approve", keep: "reject" };

export function submitRemovalDecision(token: string, code: string, decision: RemovalDecision): Promise<RemovalApprovalResult> {
  return approvalJson<RemovalApprovalResult>(`/api/approval/removal/${seg(token)}`, { code, decision: REMOVAL_WIRE[decision] });
}
