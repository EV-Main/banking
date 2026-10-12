"use client";

import { useEffect } from "react";

import { useLocale } from "@evinvest/i18n/react";

import { decideLocaleSync } from "@/entities/user/lib/locale-sync";
import { profileResource } from "@/entities/user/model/profile-resource";
import { relocalise } from "@/shared/config/base-path";
import { writeLocaleCookie } from "@/shared/lib/locale-cookie";
import { useResource } from "@/shared/lib/resource";

// When the proxy had to guess this page's locale from Accept-Language and the account
// has a language of its own, re-enters the page under the stored one. That is all it
// does: a URL never writes to the account — settings is the only writer (#347).
//
// Renders nothing and mounts once, in the signed-in `(app)` layout: the profile only
// exists for a signed-in reader, and `(auth)` deliberately has no session to read one
// with. The rule itself is `decideLocaleSync` in `entities/user/lib/locale-sync.ts`;
// this file only gathers its inputs and carries out the verdict.
export function LocaleSync() {
  const locale = useLocale();
  const { data: profile } = useResource(profileResource);

  useEffect(() => {
    if (!profile) return;
    // Cleared unconditionally: the mark describes the entry that minted it, and this is
    // the page that entry led to.
    const decision = decideLocaleSync({
      locale,
      stored: profile.language ?? "",
      marker: takeGuessMarker(),
    });
    if (decision.kind !== "adopt-stored") return;

    // Not `confirmLocaleCookie`: adopting the account's language is not the reader's
    // choice on this device, so the cookie stays unconfirmed and the next unprefixed entry
    // is re-checked against the account.
    writeLocaleCookie(decision.to);
    // Hard navigation: the locale is a root layout segment, so the catalogue is chosen
    // server-side and `router.replace` would re-render the same one. `location.replace`
    // rather than an assignment, so the guessed URL is not a back-button stop the reader
    // has to click past twice.
    window.location.replace(relocalise(decision.to, window.location));
  }, [profile, locale]);

  return null;
}

/**
 * Reads the proxy's "this locale came from Accept-Language" mark and clears it,
 * returning the value it was minted with — raw, since the policy matches it against
 * the current locale rather than trusting it to be one.
 *
 * The name is derived from the protocol for the same reason as the locale cookie
 * itself — see `shared/lib/locale-cookie.ts`. Expired by setting `max-age=0` on the
 * exact same path the proxy wrote it with; anything else leaves the original in place.
 */
function takeGuessMarker(): string | null {
  const name = (location.protocol === "https:" ? "__Host-" : "") + "ev_locale_guessed";
  let value: string | null = null;
  for (const part of document.cookie.split(";")) {
    const [k, ...rest] = part.trim().split("=");
    if (k === name) value = decodeURIComponent(rest.join("="));
  }
  if (value !== null) {
    document.cookie = `${name}=;path=/;max-age=0${location.protocol === "https:" ? ";secure" : ""}`;
  }
  return value;
}
