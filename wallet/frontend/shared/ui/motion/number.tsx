"use client";

import { useLayoutEffect, useRef } from "react";
import { animateValue, useReducedMotion } from "motion/react";

import { useHydrated } from "@/shared/lib/use-hydrated";

import { driveNumber, type NumberDriverDeps } from "./number-driver";
import { DUR, EASE } from "./tokens";

export interface AnimatedNumberProps {
  /** The figure to display. Animates from whatever is on screen to this. */
  value: number;
  /**
   * How to render the running figure. Must be referentially stable — a module
   * function, or something memoised — because a new identity restarts the
   * animation. Every money formatter in `shared/lib/money` qualifies.
   */
  format: (n: number) => string;
  className?: string;
}

// The browser-side deps of the driver, wired once. The fallback sits past the
// count's own duration by a margin wide enough that a long task during
// hydration cannot let the timer beat the finishing frame: in the normal case
// the animation completes first and the timer is cancelled without firing.
//
// `animateValue`, not `animate(from, to, …)`. For a bare number `animate` ends in
// exactly this object — a `JSAnimation` on the same frame loop, started at the
// same instant — but reaches it through the element/sequence machinery, which is
// ~2.6 KB gz on every page holding a figure. Going straight to the engine keeps
// the curve and the frames and drops the wrapper. Its units are the engine's,
// milliseconds, where `animate` takes seconds.
const DOM_DEPS: NumberDriverDeps = {
  animate: (from, to, { onUpdate, onComplete }) =>
    animateValue({
      keyframes: [from, to],
      duration: DUR.slow * 1000,
      ease: EASE.out,
      onUpdate,
      onComplete,
    }),
  isHidden: () => document.visibilityState === "hidden",
  onHidden: (cb) => {
    const listener = () => {
      if (document.visibilityState === "hidden") cb();
    };
    document.addEventListener("visibilitychange", listener);
    return () => document.removeEventListener("visibilitychange", listener);
  },
  setTimer: (cb, ms) => {
    const id = setTimeout(cb, ms);
    return () => clearTimeout(id);
  },
  fallbackMs: DUR.slow * 1000 + 250,
};

/**
 * A figure that travels to its new value instead of being replaced by it.
 *
 * On a balance this is the difference between "the number is 12,480" and "the
 * number *became* 12,480" — after a deposit settles or a poll lands, the motion
 * is the only thing that says which digits changed and in which direction.
 *
 * Three deliberate implementation choices:
 *
 * - **The DOM is written directly, not through React.** `animateValue()` drives a
 *   plain number and `onUpdate` writes the text. A 60fps count that went
 *   through `setState` would re-render this component ~40 times per second and,
 *   through it, everything the parent re-renders with it. Nothing here needs to
 *   be in the React tree, so it isn't. The write sets `nodeValue` on the text
 *   node already there rather than `textContent`, which would discard it and
 *   create a new one on every frame — the same in-place update React itself
 *   makes for a string child. `textContent` is only the fallback for a span
 *   with no text node to update.
 * - **`useLayoutEffect`, not `useEffect`.** The JSX renders the *final* string
 *   so that SSR, a crawler, or a JS failure all show the true figure. The layout
 *   effect overwrites it with the starting figure before the browser paints, so
 *   the correct-but-not-yet-animated value is never visible. In a plain effect
 *   that overwrite lands after a paint and the number visibly snaps back to the
 *   start before counting up.
 * - **The final figure never depends on an animation frame.** The count runs on
 *   `requestAnimationFrame`, which a hidden tab does not get; left to the
 *   animation alone, a page opened in the background reads "$0.00" until it is
 *   brought to the front (banking#346). `driveNumber` snaps to the final figure
 *   when the page is hidden at the start, when it goes hidden mid-count, or on a
 *   timer set just past the count's own duration — whichever comes first.
 *
 * Pair with `tabular-nums` at the call site. Proportional digits change width as
 * they cycle, and a figure that jitters sideways while it counts is worse than
 * one that simply appears.
 */
export function AnimatedNumber({
  value,
  format,
  className,
}: AnimatedNumberProps) {
  const reduce = useReducedMotion();
  const ref = useRef<HTMLSpanElement>(null);
  // What is currently on screen. Starts at 0 so the first appearance counts up
  // from nothing; afterwards it is wherever the last animation finished, so a
  // refresh travels the actual delta rather than restarting from zero. A figure
  // hydrated from server HTML is the exception: it has been on screen since the
  // HTML arrived, and counting it up from 0 would take a number the reader
  // already saw away from them for the length of the count.
  const hydrated = useHydrated();
  const shown = useRef(hydrated ? 0 : value);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const write = (text: string) => {
      const node = el.firstChild;
      if (node?.nodeType === Node.TEXT_NODE) node.nodeValue = text;
      else el.textContent = text;
    };
    const onShown = (n: number) => {
      shown.current = n;
    };
    return driveNumber(
      { from: reduce ? value : shown.current, to: value, format, write, onShown },
      DOM_DEPS,
    );
  }, [value, format, reduce]);

  return (
    <span ref={ref} className={className}>
      {format(value)}
    </span>
  );
}
