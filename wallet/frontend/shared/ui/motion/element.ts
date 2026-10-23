"use client";

// Turning an arbitrary element or component into a motion one, without adding a
// wrapper around it.
//
// Why this exists. The cabinet's sections are grid and flex items that carry
// their own placement — `xl:col-start-1`, `lg:order-3`, `flex-1`. Wrapping such
// a section in a `<motion.div>` moves the section one level away from the track
// it was placed on: the wrapper becomes the item, the placement classes stay on
// the child, and the layout silently changes. So the section itself has to be
// the thing that animates.
//
// `m.create` builds that component, and the cache is the point: creating it
// during render would hand React a new component type on every pass, which
// unmounts and remounts the subtree — losing state, restarting the very
// animation this exists to play, and (in a card holding a form) throwing away
// what the user typed.
//
// Keyed by the tag name or the component reference, both of which are stable
// module-level values, so the map is bounded by the number of distinct section
// elements in the app — a handful.

// `m`, not `motion`: the component carries no features of its own, so the
// caller renders it under `MotionFeatures` (see ./features).
import * as m from "motion/react-m";
import type { ElementType } from "react";

type MotionElement = typeof m.div;

const cache = new Map<ElementType, MotionElement>();

/**
 * The motion version of `as`, memoised. Call it during render as often as you
 * like; the component is built once.
 *
 * The result is typed as `m.div` deliberately. Every caller in this slice
 * passes the same prop surface — `className`, `style`, `children` and the motion
 * props — and every element it is used with (a `div`, a `header`, a `section`,
 * uikit's `Card`) accepts exactly that. Widening it to a generic polymorphic
 * type would buy nothing and cost the call sites their inference.
 */
export function asMotion(as: ElementType): MotionElement {
  const hit = cache.get(as);
  if (hit) return hit;
  // `m.create` takes a tag name or a component; the cast pins its generic to
  // the div prop surface described above rather than resolving it per call site.
  const made = m.create(as as "div") as MotionElement;
  cache.set(as, made);
  return made;
}
