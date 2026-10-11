// The chip's sign-out, with every side effect passed in so `node --test` can drive it.
//
// The logout is shell-owned (site-root /api/auth): it revokes the shared session and clears
// its cookies for every zone at once. Only a 2xx means that happened — a CSRF refusal, a
// store error or a dropped connection leave the session alive, and landing on /loggedout
// then would lie to someone who walks away from a shared machine.

import type { Locale } from "@evinvest/i18n";

// Relative on purpose: `node --test` runs this file and cannot resolve the `@/` alias.
import { cabinetPath } from "../../../shared/config/base-path.ts";

export const LOGOUT_ENDPOINT = "/api/auth/logout";

export type SignOutDeps = {
  fetch: (input: string, init: { method: "POST"; headers: Record<string, string> }) => Promise<{ ok: boolean }>;
  csrfHeader: () => Record<string, string>;
  clearIdentity: () => void;
  clearResources: () => void;
  navigate: (href: string) => void;
  locale: Locale;
};

/**
 * True once the shell confirmed the logout and the page is navigating to /loggedout; false
 * when it did not (non-2xx or a rejected fetch), with nothing navigated or cleared beyond the
 * identity cache. Never throws.
 *
 * The identity cache goes first, before the request: whatever the shell answers, the name of
 * whoever pressed Sign out should not be what the next page paints.
 */
export async function performSignOut({ fetch, csrfHeader, clearIdentity, clearResources, navigate, locale }: SignOutDeps): Promise<boolean> {
  clearIdentity();
  try {
    const res = await fetch(LOGOUT_ENDPOINT, { method: "POST", headers: csrfHeader() });
    if (!res.ok) return false;
  } catch {
    return false;
  }
  // The hard navigation clears the in-memory reads anyway; this clears the persisted ones.
  clearResources();
  navigate(cabinetPath(locale, "/loggedout"));
  return true;
}
