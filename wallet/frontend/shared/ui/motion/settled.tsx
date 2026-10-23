"use client";

import { type ReactNode, useState } from "react";
import { useReducedMotion } from "motion/react";
import * as m from "motion/react-m";

import { useEntranceLive } from "./entrance";
import { MotionFeatures } from "./features";
import { DUR, EASE, RISE } from "./tokens";

export interface SettledProps {
  /** True while the data this surface needs is still in flight. */
  loading: boolean;
  /** What stands in for the content meanwhile — a `Skeleton`, or a group of them. */
  skeleton: ReactNode;
  children: ReactNode;
  className?: string;
}

/**
 * The skeleton → content handover. Every data surface in the cabinet swapped its
 * skeleton for real content in a single frame, which reads as a flicker: the eye
 * registers that *something* changed without registering *what*. Here the
 * skeleton is replaced and the content fades and rises the last few pixels into
 * its place.
 *
 * Two things it deliberately does not do:
 *
 * - **No cross-fade.** Skeleton and content would have to overlap, which means
 *   taking one out of flow and animating a height — expensive, and it makes the
 *   surface jump when the real content is a different size.
 * - **No entrance when no skeleton was shown.** Data that is already present on
 *   the first render (a cached or server-supplied value) cuts straight in;
 *   fading it would invent a delay that the data never had.
 *
 * The caller still owns the empty and error states — pass them as `children`,
 * guarded on the same condition as `loading`, and they arrive with the same
 * motion as content would.
 *
 * One more thing it does not do: travel, when the section around it is still
 * arriving. Two 8px rises composed on top of each other make the card stutter
 * as it settles, so a handover caught inside a live entrance keeps the fade and
 * drops the movement. See ./entrance.
 */
export function Settled({
  loading,
  skeleton,
  children,
  className,
}: SettledProps) {
  const reduce = useReducedMotion();
  const entering = useEntranceLive();

  // Adjusting state during render rather than in an effect, which is what lets
  // the entrance play on the very first render after `loading` drops: an effect
  // would paint the content opaque once and only then start the fade, and that
  // flash is more noticeable than the cut it was meant to replace.
  //
  // `flat` is captured at the same instant for the same reason — it has to be
  // the answer as of the handover, not as of whenever this happens to re-render
  // next, by which time the section has landed and the answer has changed.
  const [wasLoading, setWasLoading] = useState(loading);
  const [sawSkeleton, setSawSkeleton] = useState(false);
  const [flat, setFlat] = useState(false);
  if (wasLoading !== loading) {
    setWasLoading(loading);
    if (wasLoading) {
      setSawSkeleton(true);
      setFlat(entering);
    }
  }

  // No entrance on this branch. The skeleton's own fade lives on the primitive
  // (`[data-slot="skeleton"]` in globals.css) so that it reaches the 40-odd
  // skeletons in this app written inline, outside any Settled boundary. Fading
  // the wrapper here as well would compound with it and the scaffolding would
  // take twice as long to arrive as it was tuned for.
  if (loading) return <div className={className}>{skeleton}</div>;
  if (!sawSkeleton) return <div className={className}>{children}</div>;

  // Swapping the plain wrapper for a motion one remounts it, which is what
  // replays `initial` — so a later refetch that shows the skeleton again gets
  // its own handover rather than appearing instantly.
  return (
    <MotionFeatures>
      <m.div
        className={className}
        initial={{ opacity: 0, y: reduce || flat ? 0 : RISE }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ duration: DUR.base, ease: EASE.out }}
      >
        {children}
      </m.div>
    </MotionFeatures>
  );
}
