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

import {
  adminAllocationsResource,
  cabinetConfigResource,
  fundRevenueResource,
  mfeRegistryResource,
  parkedEventsResource,
  redemptionQueueResource,
  treasuryResource,
  usersResource,
  withdrawalQueueResource,
} from "@/entities/admin/model/admin-resource";
import { paymentsResource } from "@/entities/payment/model/payment-resource";
import { isServiceId, tenantOf } from "@/entities/grant/lib/access";
import { grantHoldersResource } from "@/entities/grant/model/grant-resource";
import { profileResource } from "@/entities/user/model/profile-resource";
import { depositsResource, walletResource, withdrawalsResource } from "@/entities/wallet/model/wallet-resource";

// Zone-relative paths, exactly as the rail writes them (basePath is not part of these).
// Ordered longest-prefix-first so `/wallet/activity` isn't answered by `/wallet`.
const ROUTES: ReadonlyArray<{ prefix: string; warm: (path: string) => void }> = [
  { prefix: "/admin/users", warm: () => usersResource.prefetch({}) },
  {
    prefix: "/admin/cabinet",
    warm: () => {
      cabinetConfigResource.prefetch();
      mfeRegistryResource.prefetch();
    },
  },
  { prefix: "/admin/treasury", warm: () => treasuryResource.prefetch() },
  { prefix: "/admin/withdrawals", warm: () => withdrawalQueueResource.prefetch() },
  { prefix: "/admin/outbox", warm: () => parkedEventsResource.prefetch() },
  {
    prefix: "/admin/payments",
    warm: () => {
      // The unfiltered list, plus what the form's pickers read: the product list and the
      // configured rails (the treasury read carries them).
      paymentsResource.prefetch();
      adminAllocationsResource.prefetch();
      treasuryResource.prefetch();
    },
  },
  { prefix: "/admin/revenue", warm: () => fundRevenueResource.prefetch() },
  // `/admin/allocations/<service>` is the panel-access page, open to a delegate who cannot
  // read the registry — warming that would only buy them a 403.
  {
    prefix: "/admin/allocations/",
    warm: (path) => {
      const service = path.slice("/admin/allocations/".length).split("/")[0] ?? "";
      const namespace = isServiceId(service) ? tenantOf(service) : null;
      if (namespace !== null) grantHoldersResource.prefetch(namespace);
    },
  },
  { prefix: "/admin/allocations", warm: () => adminAllocationsResource.prefetch() },
  {
    prefix: "/admin/valuation",
    warm: () => {
      adminAllocationsResource.prefetch();
      redemptionQueueResource.prefetch();
    },
  },
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
