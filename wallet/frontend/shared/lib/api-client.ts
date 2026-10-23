"use client";

// The one browser→BFF JSON transport. Every entity client goes through it, so session
// handling and error wording live HERE instead of in six copies of `fetch`-and-throw.
//
// Why it does more than fetch: the shared access cookie lapses on a short TTL while the
// page still looks signed in (see `shared/lib/session.ts`). Left alone, a tab that idled
// past that TTL renders fine and then fails every call with a bare `unauthenticated` —
// which is what the user saw, and why "click around a few times or reload" appeared to
// fix it. Two layers close that:
//
//   · pre-flight — a request issued after the session went unconfirmed for STALE_MS
//     rotates the cookie FIRST (coalesced with the session fetch the shell chrome is
//     already making), so coming back to an idle tab never produces a 401 at all;
//   · heal — a 401 that slips through anyway (the cookie lapsed mid-page, or a request
//     raced the pre-flight) refreshes the session and replays the request ONCE. Replaying
//     is safe for mutations too: a 401 is only ever an auth verdict, raised by the BFF's
//     gate before any upstream call (see backend/src/routes) or by the owning plane's own
//     gate before it applies anything — never by a request that half-executed.
//
// Only a shell that answers "genuinely signed out" ends as SessionExpiredError (the
// keeper picks the same answer up and moves to /login). An unreachable session endpoint
// is left as an ordinary failure, so a blip never bounces anyone out of the cabinet.

import type { Translate } from "@evinvest/i18n";

import { apiPath } from "@/shared/config/base-path";
import { csrfHeader } from "@/shared/lib/csrf-client";
import { replaying } from "@/shared/lib/replay";
import { cachedSession, refreshIfStale, refreshSession, sessionGeneration } from "@/shared/lib/session";
import { wordFor } from "@/shared/lib/wire-words";

/** The session is provably gone — offer sign-in, not a retry. */
export class SessionExpiredError extends Error {
  readonly status = 401;
  readonly code = "err.sessionExpired" satisfies ErrorCode;
  constructor() {
    super("Your session has ended. Sign in again to continue.");
    this.name = "SessionExpiredError";
  }
}

/**
 * A failed BFF call, carrying the HTTP status for callers that branch on it.
 *
 * `code` is a catalogue key, not prose — this module runs in the fetch layer,
 * far below any component that could hold a translator, so it names the message
 * and leaves the wording to the render boundary (see {@link errorMessage}).
 * `message` stays populated with the English text so a `console.error`, a Sentry
 * breadcrumb, or a caller that never learned about `code` still reads sensibly.
 *
 * A `code` of `null` means the BFF sent free-form prose of its own. That text is
 * already client-safe (see backend/src/error.rs) but it is not ours to key, so it
 * passes through in whatever language the backend chose.
 */
export class RequestError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code: ErrorCode | null = null,
    /**
     * The parsed error body, as it arrived. `message` and `code` are derived from `error`
     * alone, so a plane that deliberately sends a machine-readable code ALONGSIDE data the
     * screen needs — `/kyc/start`'s `{ error, contact }` — used to have that data dropped
     * here and could not be rendered at all. `{}` when the body was absent or not JSON.
     */
    readonly body: unknown = {},
  ) {
    super(message);
    this.name = "RequestError";
  }
}

/**
 * The one place an error becomes words a reader sees.
 *
 * Views render this rather than `error.message`: the message was fixed in English
 * when the request failed, whereas the key can still be resolved in the reader's
 * locale at paint time. Anything that is not one of our errors — a thrown string,
 * a bug — degrades to the generic key rather than leaking a stack trace into the UI.
 */
export function errorMessage(error: unknown, t: Translate): string {
  if (error instanceof SessionExpiredError) return errorWords(t)[error.code];
  // `wordFor`, not a bare index: `code` is typed, but any caller can cast into the
  // constructor, and an unknown code must still say its English message, never `undefined`.
  if (error instanceof RequestError) return (error.code && wordFor(errorWords(t), error.code)) || error.message;
  return t("err.requestFailed", "Something went wrong on our side. Please try again.");
}

/**
 * Every code a {@link RequestError} may carry. Closed, so each one is a literal `t()` the
 * catalogue extractor can read — a code minted outside this list is a type error, not a
 * raw key on screen.
 */
export type ErrorCode =
  | "err.authNotConfigured"
  | "err.csrf"
  | "err.forbidden"
  | "err.fundServiceRequired"
  | "err.kycSelf"
  | "err.kycStartFailed"
  | "err.kycThrottled"
  | "err.kycUnavailable"
  | "err.network"
  | "err.notFound"
  | "err.rateLimited"
  | "err.requestFailed"
  | "err.serverUnavailable"
  | "err.sessionExpired"
  | "err.unauthenticated"
  | "err.unparkDeclined"
  | "err.verificationRequired";

const errorWords = (t: Translate): Record<ErrorCode, string> => ({
  "err.authNotConfigured": t("err.authNotConfigured", "Sign-in is unavailable right now. Please try again shortly."),
  "err.csrf": t("err.csrf", "This page went stale. Reload it and try again."),
  "err.forbidden": t("err.forbidden", "You don't have access to this."),
  "err.fundServiceRequired": t("err.fundServiceRequired", "Select a fund to continue."),
  "err.kycSelf": t("err.kycSelf", "You can't set your own verification tier — another holder of KycManage has to."),
  "err.kycStartFailed": t("err.kycStartFailed", "We couldn't start verification. Please try again."),
  "err.kycThrottled": t("err.kycThrottled", "You've started verification too many times today. Try again tomorrow."),
  "err.kycUnavailable": t("err.kycUnavailable", "Verification is unavailable right now. Please contact support."),
  "err.network": t("err.network", "Can't reach the server. Check your connection and try again."),
  "err.notFound": t("err.notFound", "Not found."),
  "err.rateLimited": t("err.rateLimited", "Too many requests — give it a moment and try again."),
  "err.requestFailed": t("err.requestFailed", "Something went wrong on our side. Please try again."),
  "err.serverUnavailable": t("err.serverUnavailable", "The service is temporarily unavailable. Please try again."),
  "err.sessionExpired": t("err.sessionExpired", "Your session has ended. Sign in again to continue."),
  "err.unauthenticated": t("err.unauthenticated", "We couldn't confirm your session. Reload the page or sign in again."),
  "err.unparkDeclined": t("err.unparkDeclined", "The hub declined the unpark."),
  "err.verificationRequired": t("err.verificationRequired", "Verify your identity to continue."),
});

type Method = "GET" | "POST" | "PATCH" | "DELETE";

interface JsonRequest {
  method?: Method;
  body?: unknown;
  /** `zone` = this cabinet's BFF (basePath-prefixed); `shell` = the site-root auth surface. */
  scope?: "zone" | "shell";
}

// The BFF's fixed error strings, mapped to catalogue keys and their English text.
// Everything else the BFF sends is already client-safe prose (see backend/src/error.rs)
// and passes through untouched — with a null key, since we did not author it.
/**
 * The English source for `err.csrf`. Module-local on purpose: a caller that meets a stale
 * token outside this table — the identity plane answers `csrf check failed` in plain text,
 * which no lookup below can match — renders the KEY, not this string, so the reader gets
 * the sentence in their own language rather than the one that was fixed at fetch time.
 */
const STALE_PAGE_MESSAGE = "This page went stale. Reload it and try again.";

/**
 * The English source for `err.serverUnavailable`. Shared by the status fallback below and by
 * the identity plane's `internal` code in the table — they are the same sentence, and a
 * caller that copied it out to re-key a code was the third copy of it in the repo.
 */
const SERVER_UNAVAILABLE_MESSAGE = "The service is temporarily unavailable. Please try again.";

/** See {@link isVerificationRequired}. Named so the table and the predicate cannot drift. */
const VERIFICATION_REQUIRED = "err.verificationRequired" satisfies ErrorCode;

const FRIENDLY: Record<string, { code: ErrorCode; en: string }> = {
  unauthenticated: {
    code: "err.unauthenticated",
    en: "We couldn't confirm your session. Reload the page or sign in again.",
  },
  csrf: { code: "err.csrf", en: STALE_PAGE_MESSAGE },
  "auth not configured": {
    code: "err.authNotConfigured",
    en: "Sign-in is unavailable right now. Please try again shortly.",
  },
  "request failed": {
    code: "err.requestFailed",
    en: "Something went wrong on our side. Please try again.",
  },
  // The one refusal the BFF names rather than flattening into a read failure (see
  // backend/src/routes/money.rs): below KYC tier 1 the hub hands out no deposit address.
  // Keyed here so a screen that only renders the sentence still shows it in the reader's
  // language — the screens that can act on it offer verification instead.
  verification_required: {
    code: VERIFICATION_REQUIRED,
    en: "Verify your identity to continue.",
  },
  // The identity plane's closed dictionary (`features/kyc/api/kyc-contract`). It is a
  // SECOND backend answering this transport, and its codes were unknown to this table, so
  // every one of them reached the reader as the literal word `internal` / `throttled` /
  // `kyc_unavailable` in all five locales. Keyed here rather than in the slice: one policy
  // per code is what this table is, and a screen that re-words a code is a second place the
  // wording can drift from (`unauthenticated` and `csrf` above already serve both planes).
  internal: { code: "err.serverUnavailable", en: SERVER_UNAVAILABLE_MESSAGE },
  throttled: {
    code: "err.kycThrottled",
    en: "You've started verification too many times today. Try again tomorrow.",
  },
  kyc_unavailable: {
    code: "err.kycUnavailable",
    en: "Verification is unavailable right now. Please contact support.",
  },
  // The BFF's own refusal, not a plane's: an operator may not set their own KYC tier
  // (`cabinet/backend/src/routes/admin.rs::set_kyc`). It is keyed rather than sent as
  // prose — which is what the neighbouring role gates do — because this one is read by
  // an operator who is mid-task and needs to know WHO can do it instead, and a sentence
  // fixed in English at the BFF would say that in English to all five locales.
  kyc_self: {
    code: "err.kycSelf",
    en: "You can't set your own verification tier — another holder of KycManage has to.",
  },
};

/**
 * The verification gate specifically, as opposed to every other 403.
 *
 * A screen branches on this to offer the way out; `err.forbidden` — "you don't have access
 * to this" — is the settled refusal it must NOT be confused with.
 */
export function isVerificationRequired(error: unknown): boolean {
  return error instanceof RequestError && error.code === VERIFICATION_REQUIRED;
}

// Fallbacks when the response carries no `{ error }` body at all.
function statusMessage(status: number): { code: ErrorCode; en: string } {
  if (status === 403) return { code: "err.forbidden", en: "You don't have access to this." };
  if (status === 404) return { code: "err.notFound", en: "Not found." };
  if (status === 429)
    return { code: "err.rateLimited", en: "Too many requests — give it a moment and try again." };
  if (status >= 500) return { code: "err.serverUnavailable", en: SERVER_UNAVAILABLE_MESSAGE };
  return { code: "err.requestFailed", en: `Request failed (${status}).` };
}

export async function requestJson<T>(path: `/${string}`, req: JsonRequest = {}): Promise<T> {
  const { method = "GET", body, scope = "zone" } = req;
  const url = scope === "zone" ? apiPath(path) : path;
  const init: RequestInit = {
    method,
    headers:
      body === undefined
        ? { accept: "application/json", ...(method === "GET" ? {} : csrfHeader()) }
        : { accept: "application/json", "content-type": "application/json", ...csrfHeader() },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  };

  // Pre-flight: rotate a cookie that is old enough to be at risk before spending the
  // request on it. Cheap — single-flight with every other session reader on the page.
  await refreshIfStale();
  const generation = sessionGeneration();

  // A rollout blip (502/503/504, dropped connection) is retried for reads only — a
  // mutation that half-executed must not run twice; the why lives in `./replay.ts`.
  const replayable = method === "GET";
  let res = await replaying(() => send(url, init), replayable);
  if (res.status === 401) {
    // Heal: if the shell already answered while this request was in flight, the replay
    // just uses the cookie that answer re-set; otherwise refresh first.
    const session = sessionGeneration() > generation ? cachedSession() : await refreshSession();
    if (session && !session.authenticated) throw new SessionExpiredError();
    res = await replaying(() => send(url, init), replayable);
  }

  const data = (await res.json().catch(() => ({}))) as T & { error?: string };
  if (!res.ok) {
    // A BFF string we recognise becomes a key; one we don't stays the backend's own
    // prose, unkeyed. No `{ error }` body at all falls back to the status.
    //
    // `Object.hasOwn`, not a bare index: `FRIENDLY` is an object literal, so
    // `FRIENDLY["toString"]` resolves up the prototype chain to a function. That is
    // truthy, so `??` would not fire, `detail.en` would be `undefined`, and the
    // banner would render empty — a silent blank alert for any BFF error string that
    // happens to name an Object.prototype member.
    const known =
      data.error !== undefined
        ? Object.hasOwn(FRIENDLY, data.error)
          ? FRIENDLY[data.error]
          : undefined
        : statusMessage(res.status);
    const detail = known ?? { code: null, en: data.error as string };
    throw new RequestError(detail.en, res.status, detail.code, data);
  }
  return data;
}

/** Network-level failure → the same friendly surface as an error response. */
async function send(url: string, init: RequestInit): Promise<Response> {
  try {
    return await fetch(url, init);
  } catch {
    throw new RequestError(
      "Can't reach the server. Check your connection and try again.",
      0,
      "err.network",
    );
  }
}

export const getJson = <T>(path: `/${string}`): Promise<T> => requestJson<T>(path);

export const postJson = <T>(path: `/${string}`, body: unknown): Promise<T> => requestJson<T>(path, { method: "POST", body });
