"use client";

// What each screen reads, so the reading can start before the screen does.
//
// The cache in `shared/lib/resource.ts` removes the skeleton on the SECOND visit to a
// screen — the value is already there. This closes the first visit too. `next/link` would
// prefetch the route's RSC payload on hover, but the cabinet turns that off (see
// `shared/ui/cabinet-link.tsx`: a dynamic route with no loading boundary has nothing to
// carry); this is the same idea applied where it pays — the data the screen will ask for,
// driven off the same signal. By the time the click lands the answer
// is usually in the cache, and the screen paints its figures on the first frame.
//
// Two triggers, both cheap and both cancel-free (a warm that turns out to be unnecessary
// costs one GET that a later screen would have made anyway):
//
//   · intent — pointer or keyboard focus on a nav row warms that row's screen;
//   · idle — once the shell is up, warm the handful of reads shared by most screens, at
//     idle priority so they never race the reads the current screen is making for itself.

import { profileResource } from "@/entities/user/model/profile-resource";
import { depositsResource, walletResource, withdrawalsResource } from "@/entities/wallet/model/wallet-resource";

// Zone-relative paths, exactly as the rail writes them (basePath is not part of these).
// Ordered longest-prefix-first so `/wallet/activity` isn't answered by `/wallet`.
const ROUTES: ReadonlyArray<{ prefix: string; warm: (path: string) => void }> = [
  {
    prefix: "/wallet/activity",
    warm: () => {
      withdrawalsResource.prefetch();
      depositsResource.prefetch();
    },
  },
  { prefix: "/wallet", warm: () => walletResource.prefetch() },
  // Home reads the verification standing.
  { prefix: "/", warm: () => profileResource.prefetch() },
];

// "/" is Home, not "every path": it is matched exactly, never as a prefix. Every other entry
// matches its own path or anything nested under it.
function matches(path: string, prefix: string): boolean {
  if (prefix === "/") return path === "/";
  return path === prefix || path.startsWith(prefix.endsWith("/") ? prefix : `${prefix}/`);
}

/** Warm the reads the screen at `href` will make. Safe to call repeatedly — fresh entries are left alone. */
export function prefetchRoute(href: string): void {
  const path = href.split("?")[0] ?? href;
  ROUTES.find((route) => matches(path, route.prefix))?.warm(path);
}

/**
 * Handlers to spread onto a nav link so intent warms its screen. `onFocus` is not optional
 * garnish: a keyboard user tabbing the rail has to get the same head start a pointer does.
 */
export function prefetchOn(href: string) {
  const warm = () => prefetchRoute(href);
  return { onMouseEnter: warm, onFocus: warm, onTouchStart: warm };
}

/**
 * Warm the balance, which both wallet pages and the wallet panel read. Called once when a
 * signed-in shell mounts.
 *
 * At idle, so the current screen's own reads go first — nothing here is needed to paint the
 * page the user actually asked for.
 */
export function warmShell(): () => void {
  const warm = () => walletResource.prefetch();
  if (typeof window === "undefined") return () => undefined;
  if (typeof requestIdleCallback === "function") {
    const handle = requestIdleCallback(warm, { timeout: 2_000 });
    return () => cancelIdleCallback(handle);
  }
  // Safari has no requestIdleCallback; a short timer is close enough for a warm-up.
  const timer = setTimeout(warm, 500);
  return () => clearTimeout(timer);
}
