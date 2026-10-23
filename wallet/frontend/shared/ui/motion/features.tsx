"use client";

// What the slice's `m` components can do, loaded once and synchronously.
//
// Why `m` and not `motion`. `motion.div` carries every feature the library has
// — layout projection, drag, pan — and a page pays for all of it the moment one
// motion component is on it. The entrances, the skeleton handover and the tab
// marker use none of that: they animate opacity and transform on mount and
// exit. `m` renders the same element with the same visual state but brings no
// features of its own, and this provider hands it `domAnimation` (animations,
// exit, hover/tap/focus/in-view). Same animation engine, same code path from the
// first frame; only the projection and drag code stays off the page.
//
// Why synchronous, not `features={() => import(...)}`. A lazily loaded bundle
// arrives after hydration, and until it does every `m` element sits at its
// `initial` — opacity 0 — so on a slow connection the cards would appear later
// than they do with `motion`. Here the features are on the page before the
// first render, and an entrance starts on exactly the frame it always did.
//
// Why per primitive rather than once at the root. The root providers wrap
// routes with no motion at all (the logged-out page, the MFE mount); a provider
// there would put `domAnimation` on them too. Each primitive wraps its own `m`
// element instead, so the features travel with the code that needs them. The
// provider is a context value and a loop over a handful of keys, per instance.
//
// `Panel` is the exception and stays on `motion`: it animates its own size
// (`layout="size"`), which is the projection code this file exists to leave
// out. It lives behind its own entry point, `@/shared/ui/motion-panel`, so the
// full feature set reaches only the routes that open a panel; nothing here
// conflicts with it there, because `strict` is off.

import type { ReactNode } from "react";
import { LazyMotion, domAnimation } from "motion/react";

/** Supplies `domAnimation` to the `m` components below it. */
export function MotionFeatures({ children }: { children: ReactNode }) {
  return <LazyMotion features={domAnimation}>{children}</LazyMotion>;
}
