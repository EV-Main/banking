"use client";

import NextLink from "next/link";
import type { ComponentProps } from "react";

import { useCabinetHref } from "@/shared/lib/cabinet-route";

/**
 * `next/link` for in-cabinet navigation, with the zone prefix applied.
 *
 * This exists because `basePath` is gone. It used to add `/cabinet` to every
 * href for free, but it also would have added it to the locale — turning
 * `/ru/cabinet/wallet` into `/cabinet/ru/cabinet/wallet` — so pages moved to a
 * `[locale]/cabinet` route tree and the prefix became this component's job.
 * (`docs/i18n-cabinet-routing-spike.md` has the measurements.)
 *
 * A bare `next/link` still compiles and still type-checks; it just points at the
 * conductor's origin root, which is a 404 on someone else's page. That is a
 * failure no tool in this repo catches, which is the argument for routing every
 * link through one component rather than 23 call sites remembering a rule.
 *
 * The locale comes from the route rather than a prop so a link does not have to be
 * handed something every caller would forward identically. That resolution now lives in
 * `shared/lib/cabinet-route.ts` next to its reading counterpart, so the writing and
 * reading halves of one rule cannot drift apart.
 *
 * Automatic prefetching is off by default, and the reason is measured, not assumed
 * (banking#349). Every cabinet route is rendered on demand and none has a `loading.tsx`,
 * so a prefetch has nothing to carry: Next answers the viewport prefetch with the route
 * tree alone and then goes back for the page's `<head>` in a second request — two
 * documents per link, and neither one shortens the click, which still has to make the
 * full dynamic request for the page. On the old admin console that was 40 RSC requests
 * per page load for the rail.
 *
 * `shared/ui/cabinet-link.test.ts` pins the premise: the day a route gains a loading
 * boundary, a prefetch starts carrying a real skeleton and this default is due for
 * a rethink. A caller with a reason can still pass `prefetch` explicitly.
 */
export function Link({
  href,
  ...props
}: Omit<ComponentProps<typeof NextLink>, "href"> & { href: `/${string}` }) {
  const toHref = useCabinetHref();
  return <NextLink href={toHref(href)} prefetch={false} {...props} />;
}
