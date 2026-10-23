// Server-side renewal of the shell's access cookie, for `proxy.ts`.
//
// Why the proxy does this at all. A server render forwards `ev_access` to the BFF, and the
// JWT inside lapses on a short TTL (300 s deployed) while its cookie lives for the whole
// refresh window — so a tab reopened after a coffee break arrives with a cookie that is
// present and dead. The browser's `SessionKeeper` heals that, but only after hydration,
// which is too late for data the server already tried to read. Renewing here, before the
// render, lets the server read succeed on the first try.
//
// The concierge `GET /auth/session` endpoint is the only thing that renews it, and it is
// safe to call concurrently: it serialises per session and hands back the current token
// without rotating the (single-use) refresh token while the access token still has more
// than 30 s left. The single-flight below only spares it the duplicate calls one burst of
// requests (a page plus its prefetches) would make.
//
// No `next/server` import, so the test runner can reach every branch here.

/** Seconds of remaining life under which the proxy renews before rendering. */
export const RENEW_WITHIN_S = 60;

/** How long a page request may wait on the identity plane before going on without it. */
export const RENEW_TIMEOUT_MS = 1_500;

/**
 * After the identity plane failed (unreachable, timed out, 5xx), how long every request
 * skips renewal. A plane that is down (a dropped packet waits out the whole timeout) would
 * otherwise add `RENEW_TIMEOUT_MS` to every page with a lapsing cookie; this caps it at one
 * such page per window per process. The browser's SessionKeeper renews in the meantime.
 *
 * Only a failure of the PLANE arms it. The backoff is shared by every visitor of this
 * process, so a failure a request brings on itself — a 4xx, a malformed answer — must not
 * switch renewal off for everyone else.
 */
export const RENEW_BACKOFF_MS = 30_000;

// What the cookie values must look like before they are written into an outgoing Cookie
// header. Next hands them over percent-DECODED, so an encoded `;` or `=` would otherwise
// become a separator there, and a CR/LF or non-Latin-1 character makes fetch throw.
//
// The session id is concierge's `random_token(32)`: 32 random bytes, URL-safe base64
// without padding — 43 characters (runner/src/web/session.rs). The access token is a JWT:
// three base64url segments.
const SESSION_ID = /^[A-Za-z0-9_-]{43}$/;
const ACCESS_TOKEN = /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/;

/** Whether this is a session id concierge could have minted. Anything else is no session. */
export function isSessionId(value: string | undefined): value is string {
  return value !== undefined && SESSION_ID.test(value);
}

/** Whether this has the shape of a JWT, and so is safe to forward as the access cookie. */
export function isAccessToken(value: string | undefined): value is string {
  return value !== undefined && ACCESS_TOKEN.test(value);
}

/**
 * Seconds until the JWT's `exp`, or null when the value is not a readable JWT.
 *
 * The signature is NOT checked and does not need to be: this only decides whether to ask
 * the identity plane for a new token. The BFF verifies whatever is finally sent.
 */
export function secondsUntilExpiry(token: string, nowMs: number): number | null {
  const payload = token.split(".")[1];
  if (!payload) return null;
  try {
    const json: unknown = JSON.parse(atob(payload.replaceAll("-", "+").replaceAll("_", "/")));
    if (typeof json !== "object" || json === null || !("exp" in json) || typeof json.exp !== "number") return null;
    return json.exp - Math.floor(nowMs / 1000);
  } catch {
    return null;
  }
}

/** Whether a request carrying this access cookie (or none) should be renewed first. */
export function needsRenewal(access: string | undefined, nowMs: number): boolean {
  if (!isAccessToken(access)) return true;
  const left = secondsUntilExpiry(access, nowMs);
  return left === null || left < RENEW_WITHIN_S;
}

/** The value a `Set-Cookie` line assigns to `name`, or null when it sets another cookie. */
export function setCookieValue(line: string, name: string): string | null {
  const pair = line.split(";", 1)[0] ?? "";
  const eq = pair.indexOf("=");
  if (eq === -1 || pair.slice(0, eq).trim() !== name) return null;
  return pair.slice(eq + 1).trim();
}

/**
 * Whether a Set-Cookie line from the identity plane may be passed to the browser: one of
 * the auth cookies this zone knows by name, host-only (no `Domain`), `Path=/`, and `Secure`
 * whenever the cookies are meant to be. The plane is trusted, but this process is the one
 * writing into the visitor's cookie jar, so it states what it is willing to write.
 */
export function acceptedSetCookie(line: string, names: readonly string[], secure: boolean): boolean {
  const [pair = "", ...attributes] = line.split(";").map((part) => part.trim());
  const eq = pair.indexOf("=");
  if (eq === -1 || !names.includes(pair.slice(0, eq).trim())) return false;
  const attrs = attributes.map((attr) => attr.toLowerCase());
  if (attrs.some((attr) => attr.startsWith("domain"))) return false;
  const paths = attrs.filter((attr) => attr.startsWith("path"));
  if (paths.length !== 1 || paths[0]?.replace(/\s+/g, "") !== "path=/") return false;
  return !secure || attrs.includes("secure");
}

export type Renewal =
  /** Signed in. `access` is the token to render with, or null if none was re-set. */
  | { kind: "alive"; access: string | null; setCookies: readonly string[] }
  /** The identity plane says the session is gone; `setCookies` clear the cookies. */
  | { kind: "gone"; setCookies: readonly string[] }
  /** No verdict (timeout, 5xx, network) — go on without renewing; the browser heals later. */
  | { kind: "unknown" };

export interface RenewalRequest {
  /** Base URL of the concierge web surface (`AUTH_WEB_URL`). */
  authWebUrl: string;
  sessionCookie: { name: string; value: string };
  accessCookieName: string;
  /** The CSRF cookie's name — the third cookie the plane may (re)set or clear. */
  csrfCookieName?: string;
  /** Require `Secure` on every Set-Cookie line passed through (`AUTH_COOKIE_SECURE`). */
  secure?: boolean;
  timeoutMs?: number;
  fetch?: typeof fetch;
}

const UNKNOWN: Renewal = Object.freeze({ kind: "unknown" });

const inflight = new Map<string, Promise<Renewal>>();
let backoffUntil = 0;

/**
 * Ask the identity plane for the session's current access token. Never rejects.
 *
 * Concurrent calls for the same session share one request, so a burst from one browser
 * costs the identity plane one call per process rather than one per request.
 */
export function renewAccess(request: RenewalRequest, nowMs = Date.now()): Promise<Renewal> {
  if (nowMs < backoffUntil) return Promise.resolve(UNKNOWN);
  const key = request.sessionCookie.value;
  const existing = inflight.get(key);
  if (existing) return existing;
  const pending = callSession(request)
    .then(({ renewal, planeFailed }) => {
      if (planeFailed) backoffUntil = Date.now() + RENEW_BACKOFF_MS;
      return renewal;
    })
    .finally(() => inflight.delete(key));
  inflight.set(key, pending);
  return pending;
}

interface Outcome {
  renewal: Renewal;
  /** The plane itself failed (network, timeout, 5xx) — see {@link RENEW_BACKOFF_MS}. */
  planeFailed: boolean;
}

const NO_VERDICT: Outcome = { renewal: UNKNOWN, planeFailed: false };
const PLANE_FAILED: Outcome = { renewal: UNKNOWN, planeFailed: true };

async function callSession({ authWebUrl, sessionCookie, accessCookieName, csrfCookieName, secure = false, timeoutMs = RENEW_TIMEOUT_MS, fetch: fetchImpl = fetch }: RenewalRequest): Promise<Outcome> {
  // Only the session id: the endpoint needs nothing else, and nothing else of the
  // browser's should leave this process.
  const headers = { accept: "application/json", cookie: `${sessionCookie.name}=${sessionCookie.value}` };
  try {
    // Validated apart from the call, so a value that cannot be a header fails here, on
    // this request alone, instead of reading as the plane being down.
    new Headers(headers);
  } catch {
    return NO_VERDICT;
  }
  let res: Response;
  try {
    res = await fetchImpl(`${authWebUrl.replace(/\/+$/, "")}/auth/session`, { headers, cache: "no-store", redirect: "manual", signal: AbortSignal.timeout(timeoutMs) });
  } catch {
    return PLANE_FAILED;
  }
  if (res.status >= 500) return PLANE_FAILED;
  if (!res.ok) return NO_VERDICT;
  let body: unknown;
  try {
    body = await res.json();
  } catch (cause) {
    // A body that stalls past the timeout is the plane being slow; one that is not JSON
    // is something answering in its place, which says nothing about the plane.
    return cause instanceof Error && cause.name === "TimeoutError" ? PLANE_FAILED : NO_VERDICT;
  }
  if (typeof body !== "object" || body === null || !("authenticated" in body) || typeof body.authenticated !== "boolean") return NO_VERDICT;
  const names = [sessionCookie.name, accessCookieName, ...(csrfCookieName ? [csrfCookieName] : [])];
  const setCookies = res.headers.getSetCookie().filter((line) => acceptedSetCookie(line, names, secure));
  if (!body.authenticated) return { renewal: { kind: "gone", setCookies }, planeFailed: false };
  const access = setCookies.map((line) => setCookieValue(line, accessCookieName) ?? undefined).find(isAccessToken) ?? null;
  return { renewal: { kind: "alive", access, setCookies }, planeFailed: false };
}

/** Test seam: forget in-flight renewals and any backoff so each case starts cold. */
export function resetRenewalsForTests(): void {
  inflight.clear();
  backoffUntil = 0;
}
