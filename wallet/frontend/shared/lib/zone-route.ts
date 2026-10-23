"use client";

// Reading the current route, with the zone prefix off.
//
// `shared/ui/zone-link.tsx` covers the writing half — every in-zone href goes
// through it so nothing has to remember that pages live under `/{locale}/cabinet`.
// This is the same rule pointed the other way: `usePathname()` returns the real
// request path, so anything comparing it against a plain `/wallet` needs the prefix
// taken off first. Both halves exist because `basePath` used to do this for free and
// no longer can (it would have prefixed the locale too — see
// `docs/i18n-wallet-routing-spike.md`).
//
// The locale resolution is shared with the Link rather than written twice: English is
// the floor, because a row highlighted in the wrong language is recoverable and a hook
// that throws while rendering the shell is not.

import { useParams, usePathname } from "next/navigation";
import { useCallback } from "react";

import { isLocale, type Locale } from "@evinvest/i18n";

import { zonePagePath, zonePathname } from "@/shared/config/base-path";

/** The active locale from the route, falling back to English. */
export function useLocale(): Locale {
  const raw = useParams()?.locale;
  return isLocale(raw) ? raw : "en";
}

/** The current path with `/{locale}/cabinet` removed — what the nav compares against. */
export function useZonePathname(): `/${string}` {
  return zonePathname(usePathname() ?? "/");
}

/** Maps a zone-relative path to a real href — for `router.push`/`replace`, which cannot
 *  go through the Link component. */
export function useZoneHref(): (path: `/${string}`) => string {
  const locale = useLocale();
  // Stable per locale: callers put this in effect dependency arrays, and a fresh
  // closure each render would re-run them on every render.
  return useCallback((path: `/${string}`) => zonePagePath(locale, path), [locale]);
}
