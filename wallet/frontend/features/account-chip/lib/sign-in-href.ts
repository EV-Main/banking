// The signed-out chip's destination: the cabinet's login page, carrying whatever the host
// page said about WHY the visitor is coming (`intent`) and WHERE they were headed
// (`returnTo`). Both arrive as data attributes on the custom element — the only channel a
// light-DOM remote has from a host that does not share its React tree — and both are
// optional: without them this is the plain `/{locale}/cabinet/login` the chip always
// linked to.
//
// `returnTo` stays ZONE-RELATIVE here (`/invest`, not `/en/cabinet/invest`). The login
// page is the one reader that puts the `/{locale}/cabinet` prefix on (#390), and it does
// so for every writer alike — the proxy's bounce, `SessionKeeper`, and now the chip. The
// same-origin check is the auth slice's `safeReturnTo`, reused rather than copied so the
// open-redirect rule has one definition.

import type { Locale } from "@evinvest/i18n";

// Relative on purpose: `node --test` runs this file and cannot resolve the `@/` alias.
import { cabinetPath } from "../../../shared/config/base-path.ts";
import { safeReturnTo } from "../../auth/lib/return-to.ts";

/** What the host says the visitor came to do; anything else is dropped, not forwarded. */
export const SIGN_IN_INTENTS = ["signup", "login"] as const;
export type SignInIntent = (typeof SIGN_IN_INTENTS)[number];

export function signInIntent(raw: string | null | undefined): SignInIntent | null {
  return (SIGN_IN_INTENTS as readonly string[]).includes(raw ?? "") ? (raw as SignInIntent) : null;
}

export function signInHref(
  locale: Locale,
  { intent, returnTo }: { intent?: string | null; returnTo?: string | null } = {},
): string {
  const params = new URLSearchParams();
  const known = signInIntent(intent);
  if (known) params.set("intent", known);
  // An absent or empty attribute means "no opinion", which is today's behaviour; a
  // present one is forwarded only in its same-origin form.
  if (returnTo) params.set("returnTo", safeReturnTo(returnTo));
  const query = params.toString();
  return cabinetPath(locale, "/login") + (query ? `?${query}` : "");
}
