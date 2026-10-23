// The post-login redirect target, from the query string to the shell.
//
// Inside the cabinet `returnTo` is ZONE-RELATIVE — `/`, `/wallet`, `/invest/x?y=1` —
// the same shape the nav and the gate reason in. Both writers (the proxy's signed-out
// bounce and `SessionKeeper`) produce that shape, and this module is the one reader:
// the shell's `/api/auth/login` redirects to `returnTo` as a site-root path, so the
// `/{locale}/cabinet` prefix is put on here, exactly once. It used to be put on by the
// writer AND the reader, which sent every deep link to `/cabinet/{locale}/cabinet/…`
// (#390) — the same class of bug as #147 and #153.
//
// Pure and browser-safe. The cabinet backend re-validates the target server-side; the
// same-origin check here is the UX/defense-in-depth copy.

import type { Locale } from "@evinvest/i18n";

// Relative on purpose: `node --test` runs this file and cannot resolve the `@/` alias.
import { cabinetPath, zonePathname } from "../../../shared/config/base-path.ts";

/** A same-origin path, or `/` when `raw` could escape to another origin. */
export function safeReturnTo(raw: string | null): string {
  if (!raw || !raw.startsWith("/")) return "/";
  // Reject protocol-relative ("//evil", "/\evil") and any backslash, which some browsers
  // normalize to "/" — both can escape to another origin.
  if (raw[1] === "/" || raw[1] === "\\" || raw.includes("\\")) return "/";
  return raw;
}

/**
 * The site-root page the shell should land on after login, for a `returnTo` read off
 * the login page's URL.
 *
 * A full `/{locale}/cabinet/…` (an old bookmark, or a link written on the public site
 * before this convention) is accepted and reduced to its zone-relative form first, so the
 * prefix is never doubled whichever shape arrived. The login page's own locale wins over
 * one carried in the target: the proxy mints both from the same request, so they only
 * differ on a hand-written link.
 */
export function loginReturnTo(locale: Locale, raw: string | null | undefined): string {
  const dest = safeReturnTo(raw ?? null);
  // `zonePathname` reads a path, not a URL — a `?` or `#` glued to the last segment would
  // stop the zone root `/cabinet?x` from being recognised, so the tail comes off first.
  const cut = dest.search(/[?#]/);
  const pathname = cut === -1 ? dest : dest.slice(0, cut);
  const tail = cut === -1 ? "" : dest.slice(cut);
  return cabinetPath(locale, zonePathname(pathname)) + tail;
}

/**
 * The `href` of the sign-in button: a full navigation to the SHELL-owned login
 * (site-root `/api/auth`, not the zone's BFF — the cabinet runs no OAuth).
 */
export function loginHref(locale: Locale, returnTo: string | null | undefined, provider: "google" | "github"): string {
  return `/api/auth/login?provider=${provider}&returnTo=${encodeURIComponent(loginReturnTo(locale, returnTo))}`;
}
