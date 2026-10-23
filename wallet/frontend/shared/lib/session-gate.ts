// The proxy's session decision, without the proxy.
//
// `proxy.ts` imports `next/server` and so cannot be loaded by the test runner. Everything
// it decides about the session — which cookies the forwarded request carries, which
// Set-Cookie lines reach the browser, and where `/login` sends the browser — is decided
// here from plain values, so that decision can be pinned without a Next runtime.
//
// Nothing here keeps a guest out: every page renders for anyone, and what a caller may see
// on it is the route gate's (`application/layout/route-gate.tsx`), from the session's
// permissions. `/login` is no page of its own — it is the page to return to, with the
// sign-in dialog open over it (`?login`), or that page outright for a session.
// Relative imports only: the runner resolves no `@/` alias.

import { BASE_PATH, zonePathname } from "../config/base-path.ts";
import type { Renewal } from "./access-renewal.ts";

export interface SessionGateInput {
  pathname: string;
  /** The query string as the request carried it, `?` included, or "". */
  search: string;
  /** The locale segment of the path, or null when there is none. */
  locale: string | null;
  /** Whether the request carries a non-empty session cookie. */
  hasSession: boolean;
  /** The renewal attempt's outcome, or null when none was made. */
  renewal: Renewal | null;
  cookieNames: { session: string; access: string };
}

export interface SessionDecision {
  /** Where to send the browser instead of rendering — a path and its query — or null to render. */
  redirect: { pathname: string; search: string } | null;
  /** Cookie values to set on the forwarded request (the renewed access token). */
  requestCookies: Readonly<Record<string, string>>;
  /** Cookies to remove from the forwarded request (a session the identity plane says is gone). */
  deleteCookies: readonly string[];
  /** Set-Cookie lines for the response, verbatim from the identity plane. */
  setCookies: readonly string[];
}

/** The query flag that opens the sign-in dialog over whatever page carries it. */
export const SIGN_IN_PARAM = "login";

export function decideSession({ pathname, search, locale, hasSession, renewal, cookieNames }: SessionGateInput): SessionDecision {
  const requestCookies: Record<string, string> = {};
  const deleteCookies: string[] = [];
  let signedIn = hasSession;
  if (renewal?.kind === "alive" && renewal.access) requestCookies[cookieNames.access] = renewal.access;
  if (renewal?.kind === "gone") {
    signedIn = false;
    deleteCookies.push(cookieNames.session, cookieNames.access);
  }
  const setCookies = renewal && renewal.kind !== "unknown" ? renewal.setCookies : [];
  const base = { requestCookies, deleteCookies, setCookies };

  if (zonePathname(pathname) !== "/login") return { ...base, redirect: null };
  const target = new URL(returnTarget(new URLSearchParams(search).get("returnTo")), "http://zone");
  if (!signedIn) target.searchParams.set(SIGN_IN_PARAM, "");
  const path = target.pathname === "/" ? "" : target.pathname;
  return { ...base, redirect: { pathname: `/${locale ?? "en"}${BASE_PATH}${path}`, search: target.search } };
}

/** A zone-relative path to return to: same-origin only, and never a second `/login`. */
function returnTarget(raw: string | null): string {
  if (!raw || !raw.startsWith("/") || raw.startsWith("//") || raw.includes("\\")) return "/";
  const zoneRelative = zonePathname(raw);
  return zoneRelative.startsWith("/login") ? "/" : zoneRelative;
}
