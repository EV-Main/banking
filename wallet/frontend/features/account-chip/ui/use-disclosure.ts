"use client";

import { useCallback, useEffect, useState, type RefObject } from "react";
import { flushSync } from "react-dom";

/**
 * `closed` → hidden and inert; `entering` → laid out in its closed look, one forced style
 * flush long; `open`; `closing` → still painted while the exit transition runs, already
 * inert so Tab and clicks pass through it.
 */
export type DisclosurePhase = "closed" | "entering" | "open" | "closing";

/** Slack past the computed transition before the fallback gives up on `transitionend`. */
const END_SLACK_MS = 50;

/**
 * Open/close state for a panel that animates both ways and stays in the DOM in between.
 *
 * Entering is two commits inside the handler, not an effect: the panel has to be styled
 * once in its closed look before `data-state="open"` lands, or the browser folds both into
 * one frame and nothing transitions. `flushSync` + a forced layout read gives that frame
 * without a `requestAnimationFrame` hop (up to two frames of latency) or a setState in an
 * effect. Leaving waits for the panel's own opacity `transitionend`, with a timer for the
 * cases that never fire one (a host stylesheet zeroing transitions, a hidden tab).
 */
export function useDisclosure(panel: RefObject<HTMLElement | null>) {
  const [phase, setPhase] = useState<DisclosurePhase>("closed");

  // Not memoised: it reads `panel.current`, which the compiler will not track as a dependency,
  // and only handlers call it.
  const open = () => {
    if (phase === "open" || phase === "entering") return;
    if (phase === "closing") {
      // Still painted: reversing the running transition needs no extra frame.
      flushSync(() => setPhase("open"));
      return;
    }
    flushSync(() => setPhase("entering"));
    panel.current?.getBoundingClientRect();
    flushSync(() => setPhase("open"));
  };

  const close = useCallback(() => {
    setPhase((p) => (p === "closed" ? p : "closing"));
  }, []);

  /** Close before the event's default action, so Tab already skips the inert panel. */
  const closeNow = useCallback(() => {
    flushSync(() => setPhase((p) => (p === "closed" ? p : "closing")));
  }, []);

  useEffect(() => {
    const el = panel.current;
    if (phase !== "closing" || !el) return;
    const done = () => setPhase((p) => (p === "closing" ? "closed" : p));
    const onEnd = (e: TransitionEvent) => {
      if (e.target === el && e.propertyName === "opacity") done();
    };
    el.addEventListener("transitionend", onEnd);
    const timer = window.setTimeout(done, longestTransitionMs(el) + END_SLACK_MS);
    return () => {
      el.removeEventListener("transitionend", onEnd);
      window.clearTimeout(timer);
    };
  }, [panel, phase]);

  return { phase, open, close, closeNow };
}

/** The panel's longest duration + delay, so a retuned `--ev-dur-fast` keeps the fallback honest. */
function longestTransitionMs(el: HTMLElement): number {
  const style = getComputedStyle(el);
  const seconds = (list: string) => list.split(",").map((v) => Number.parseFloat(v) || 0);
  const durations = seconds(style.transitionDuration);
  const delays = seconds(style.transitionDelay);
  return Math.max(0, ...durations.map((d, i) => (d + (delays[i % delays.length] ?? 0)) * 1000));
}
