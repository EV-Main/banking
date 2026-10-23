"use client";

import { useSyncExternalStore } from "react";

// Whether this render may use what only the browser knows — a figure's motion, a stage
// kept in localStorage. (Times are not here: `shared/lib/time-zone.tsx` renders them in the
// reader's zone from the first HTML.) `false` on the server AND in the render that hydrates the server's HTML
// (React hands that render the server snapshot), so the two agree; `true` from the next
// render on, and for anything mounted in the browser after hydration.
//
// For a screen whose figures were read on the server. Without a seed nothing here was in
// the HTML, every value mounts after hydration, and this is always `true`.

const subscribeNever = () => () => undefined;

export function useHydrated(): boolean {
  return useSyncExternalStore(
    subscribeNever,
    () => true,
    () => false,
  );
}
