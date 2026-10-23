"use client";

import { type ReactNode } from "react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";

import { DUR, EASE } from "@/shared/ui/motion";

/**
 * Presence boundary for a panel that mounts and unmounts — the admin user
 * drawer, an inline detail card, anything rendered behind a `&&`.
 *
 * Without this, closing a panel removes it from the DOM in the same frame and
 * the layout snaps shut. `AnimatePresence` keeps the outgoing node alive long
 * enough for {@link Panel} to play its exit. `initial={false}` so a panel that is
 * already open on first paint (a deep link) does not animate in.
 */
export function PanelPresence({ children }: { children: ReactNode }) {
  return <AnimatePresence initial={false}>{children}</AnimatePresence>;
}

export interface PanelProps {
  /** Which edge the panel comes from. Match it to where the panel sits. */
  from?: "right" | "bottom";
  /**
   * Animate the panel's own height when its content changes size. On by default:
   * switching between two records of different heights otherwise jumps.
   */
  resize?: boolean;
  /**
   * For a panel that shares a flex row with content it displaces — the admin
   * users drawer beside its table. The panel takes its width over the course of
   * the entrance and gives it back over the exit, so the neighbour narrows and
   * widens *with* it rather than snapping a frame before or after.
   *
   * `gap` is the row's gap, so the negative margin can cancel it: the gap does
   * not collapse on its own when the panel reaches zero width, and without this
   * the neighbour would finish one gap short and then jump.
   *
   * `width` is the panel's open width, and has to be passed rather than left to
   * a class. Animating to `auto` would end with `width: auto` inline, which
   * beats the class that was sizing it and leaves the panel content-sized once
   * the animation finishes.
   */
  collapse?: { gap: string; width: string };
  className?: string;
  children: ReactNode;
}

const OFFSET = 12;

/**
 * The panel itself. Must be a direct child of {@link PanelPresence}, and must
 * carry a stable `key` — key it on *whether* the panel is open, not on which
 * record it shows, or every switch plays a full exit + enter. Use
 * {@link PanelSwap} inside for the record-to-record change.
 */
export function Panel({
  from = "right",
  resize = true,
  collapse,
  className,
  children,
}: PanelProps) {
  const reduce = useReducedMotion();
  const shift = reduce ? {} : from === "right" ? { x: OFFSET } : { y: OFFSET };

  // Width is a layout property and animating it is not free — which is exactly
  // why the rest of this slice stays on opacity and transform. It is the right
  // trade here and only here: a transform-based collapse scales the panel, and
  // a scaled panel drags its text with it. One element reflowing a two-column
  // admin screen for 280ms is cheaper than distorted type.
  //
  // Both ends carry it, not just the exit. With it on the exit alone the panel
  // arrived at full width in a single frame and its neighbour jumped narrower
  // before the panel had visibly appeared — the two halves of one movement
  // playing at different times.
  const collapsed = collapse
    ? { width: 0, minWidth: 0, marginLeft: `calc(-1 * ${collapse.gap})` }
    : {};
  const opened = collapse
    ? { width: collapse.width, minWidth: collapse.width, marginLeft: 0 }
    : {};

  return (
    <motion.div
      layout={resize ? "size" : false}
      className={className}
      initial={{ opacity: 0, ...shift, ...collapsed }}
      animate={{ opacity: 1, x: 0, y: 0, ...opened }}
      exit={{ opacity: 0, ...shift, ...collapsed }}
      transition={{ duration: DUR.base, ease: EASE.out }}
    >
      {children}
    </motion.div>
  );
}

export interface PanelSwapProps {
  /** Identity of what is being shown. A change here plays the swap. */
  swapKey: string;
  children: ReactNode;
  className?: string;
}

/**
 * Content changing *inside* a panel that stays put — picking a different user
 * while the drawer is already open. The frame holds its position and the body
 * cross-fades, so the eye tracks "same panel, new record" instead of re-reading
 * the whole thing.
 *
 * `mode="popLayout"`, not `"wait"`. `wait` holds the incoming record until the
 * outgoing one has finished leaving, so there is a stretch with no content at
 * all — and with {@link Panel}'s size animation the frame collapses toward zero
 * and springs back. `popLayout` takes the leaving record out of flow instead, so
 * the two overlap, the new height is known immediately, and the panel resizes
 * once, in one direction.
 *
 * The exit is deliberately shorter than the enter: leaving should read as a
 * dismissal, arriving as a placement.
 */
export function PanelSwap({ swapKey, children, className }: PanelSwapProps) {
  return (
    <AnimatePresence mode="popLayout" initial={false}>
      <motion.div
        key={swapKey}
        className={className}
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        exit={{
          opacity: 0,
          transition: { duration: DUR.fast * 0.6, ease: EASE.out },
        }}
        transition={{ duration: DUR.fast, ease: EASE.out }}
      >
        {children}
      </motion.div>
    </AnimatePresence>
  );
}
