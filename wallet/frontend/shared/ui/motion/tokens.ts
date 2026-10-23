// Motion tokens — the design-system half of `motion`. Every animation in the
// cabinet picks its curve and duration from here, the same way colour picks from
// the `ev/*` variables: one place to retune the feel of the whole surface, and no
// per-screen easing invented on the spot.
//
// The cabinet is a money surface, so its motion is quieter than the landing's:
// shorter, smaller travel, never decorative. Motion here has one job — to say
// *what changed* — and anything that delays a number appearing is wrong.

/** Cubic-bezier control points, in the shape `motion` wants them. */
export const EASE = {
  /** Default. Fast start, long settle — every entrance. */
  out: [0.22, 1, 0.36, 1],
  /** Symmetric; for state that reverses (a panel opening and closing). */
  inOut: [0.65, 0, 0.35, 1],
} as const;

/** Seconds. `fast` is the house default here — the landing's is twice this. */
export const DUR = {
  fast: 0.18,
  base: 0.28,
  slow: 0.4,
} as const;

/** Pixels a revealing element travels. Deliberately small. */
export const RISE = 8;

/** Seconds between siblings in a staggered list. */
export const STAGGER = 0.035;

/**
 * Seconds between the sections of a screen. Larger than {@link STAGGER} because
 * these are cards, not rows: at 35ms a page of six reads as one block twitching
 * rather than as parts arriving, and the sequence is lost.
 *
 * The ceiling is what it costs the last section. Six at 50ms puts the last one
 * on screen 0.55s after the first moves, which is still inside the window where
 * a page feels like it opened; a step of 0.1 would push that to 0.9s, and by
 * then the reader is waiting for a card rather than watching it arrive. Screens
 * with more sections than that should group them, not slow down.
 */
export const SECTION_STAGGER = 0.05;
