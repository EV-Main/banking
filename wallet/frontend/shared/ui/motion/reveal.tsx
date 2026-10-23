"use client";

/* eslint-disable react-hooks/static-components --
   Each of the three components below resolves `as` through `asMotion`, which the
   rule reads as building a component mid-render. It is not: `asMotion` is a
   memo over a stable key and hands back the *same* component every time (see
   ./element, where the cache and the reason for it are spelled out). The failure
   the rule guards against — a fresh component type on each render, remounting
   the subtree and restarting the animation — is precisely what that cache
   exists to prevent, and there is no way to express "already memoised" to it.
   Scoped to this file because these three are the only place it applies. */

import { useCallback, useState, type ElementType, type ReactNode } from "react";
import { useReducedMotion, type HTMLMotionProps } from "motion/react";

import { asMotion } from "./element";
import { EntranceContext, useEntranceLive } from "./entrance";
import { MotionFeatures } from "./features";
import { DUR, EASE, RISE, STAGGER } from "./tokens";

/**
 * How far this entrance travels, given where it is.
 *
 * Nested inside an entrance that is still running — a settings pane inside the
 * page section that holds it — it fades without moving, for the same reason
 * `Settled` does: two rises composed on one element make it stutter as it
 * settles. Captured at mount and kept, because what matters is the state of the
 * world when this element appeared, not when it next re-renders.
 */
function useRise(): number {
  const reduce = useReducedMotion();
  const outerLive = useEntranceLive();
  const [nested] = useState(outerLive);
  return reduce || nested ? 0 : RISE;
}

/**
 * Every entrance in this file publishes "I am still arriving" to everything
 * inside it, and withdraws it when it lands. `Settled` is the consumer — see
 * ./entrance for the seam that closes, and for why the re-render this costs
 * reaches only the components that read it.
 *
 * If the completion callback never fires — a section unmounted mid-flight, a
 * browser that skipped the animation — the flag simply stays raised, and the
 * worst that follows is a handover that fades without travelling. Failing
 * towards the calmer of the two is the right way round.
 */
function useEntranceFlag() {
  const [live, setLive] = useState(true);
  const settle = useCallback(() => setLive(false), []);
  return [live, settle] as const;
}

// `children` is narrowed back to plain React on the two components that pass it
// through a context provider: motion's own signature also admits a MotionValue,
// which is meaningful as the sole text child of a leaf and meaningless for a
// section that holds a card.
export interface RevealProps extends Omit<HTMLMotionProps<"div">, "ref" | "children"> {
  children?: ReactNode;
  /** Seconds before this element moves. */
  delay?: number;
  /**
   * What to render. Defaults to a `div`; pass the section's own element —
   * `"header"`, `"section"`, uikit's `Card` — and it animates in place instead
   * of gaining a wrapper. See ./element for why that distinction matters.
   *
   * The props accepted are still a `div`'s, so a component with its own — an
   * `Alert` and its `variant` — goes inside rather than through `as`. That is
   * only ever a nuisance when the element in question is a grid or flex item
   * carrying placement, and an `Alert` never is.
   *
   * A component passed here **must spread the props it doesn't name onto its
   * DOM node**, `ref` and `style` included — that pair is how motion reaches
   * the element at all. uikit's components are written that way (`{ className,
   * ...props }` onto a `div`); one that dropped either would not animate, and
   * would be left sitting at the `initial` opacity of zero rather than merely
   * cutting in. Prefer a tag name or a uikit primitive.
   */
  as?: ElementType;
}

/**
 * A single block arriving — an alert that just appeared, a receipt, a panel
 * section, or the one card a short screen is made of. Fades and rises
 * {@link RISE}px, once, on mount.
 *
 * Unlike the landing's equivalent this does **not** trigger on scroll. The
 * cabinet's surfaces are short and dense; a reveal that waits for the viewport
 * would leave a signed-in user looking at blank cards.
 */
export function Reveal({ delay = 0, as = "div", children, onAnimationComplete, ...props }: RevealProps) {
  const rise = useRise();
  const [live, settle] = useEntranceFlag();
  const Comp = asMotion(as);
  return (
    <MotionFeatures>
      <Comp
        initial={{ opacity: 0, y: rise }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ duration: DUR.base, ease: EASE.out, delay }}
        {...props}
        onAnimationComplete={(definition) => {
          settle();
          onAnimationComplete?.(definition);
        }}
      >
        <EntranceContext.Provider value={live}>{children}</EntranceContext.Provider>
      </Comp>
    </MotionFeatures>
  );
}

export interface StaggerProps extends Omit<HTMLMotionProps<"div">, "ref"> {
  /** Seconds before the first child moves. */
  delay?: number;
  /** Seconds between siblings. Default {@link STAGGER}. */
  step?: number;
  /** What to render. Defaults to a `div`. See {@link RevealProps.as}. */
  as?: ElementType;
}

/**
 * A list, a grid, or a whole screen whose parts arrive in sequence. Wrap each
 * part in {@link StaggerItem}; timing is the parent's, never the part's, so
 * reordering or adding one needs no delay arithmetic.
 *
 * This is also how a page arrives. Put it on the container the screen already
 * has — the grid, the flex column — rather than adding one, and give each
 * section its own element through `as`. Nothing about the layout changes; the
 * same elements simply start 8px low and transparent.
 *
 * Keep groups short. Past ~12 rows the tail is still arriving after the reader
 * has started reading the head, which is worse than no stagger at all — for a
 * long table, reveal the table as one block instead.
 */
export function Stagger({ delay = 0, step = STAGGER, as = "div", children, ...props }: StaggerProps) {
  const Comp = asMotion(as);
  return (
    <MotionFeatures>
      <Comp
        initial="hidden"
        animate="shown"
        variants={{
          hidden: {},
          shown: { transition: { staggerChildren: step, delayChildren: delay } },
        }}
        {...props}
      >
        {children}
      </Comp>
    </MotionFeatures>
  );
}

export interface StaggerItemProps extends Omit<HTMLMotionProps<"div">, "ref" | "children"> {
  children?: ReactNode;
  /** What to render. Defaults to a `div`. See {@link RevealProps.as}. */
  as?: ElementType;
}

/**
 * One member of a {@link Stagger}.
 *
 * Carries no `initial` or `animate` of its own on purpose: those come from the
 * parent through the variant tree, which is what lets the parent own the
 * sequence. Used without a `Stagger` above it the item just renders, unanimated
 * — a missing parent leaves content visible rather than hidden.
 */
export function StaggerItem({ as = "div", children, onAnimationComplete, ...props }: StaggerItemProps) {
  const rise = useRise();
  const [live, settle] = useEntranceFlag();
  const Comp = asMotion(as);
  return (
    <MotionFeatures>
      <Comp
        variants={{
          hidden: { opacity: 0, y: rise },
          shown: {
            opacity: 1,
            y: 0,
            transition: { duration: DUR.base, ease: EASE.out },
          },
        }}
        {...props}
        onAnimationComplete={(definition) => {
          settle();
          onAnimationComplete?.(definition);
        }}
      >
        <EntranceContext.Provider value={live}>{children}</EntranceContext.Provider>
      </Comp>
    </MotionFeatures>
  );
}
