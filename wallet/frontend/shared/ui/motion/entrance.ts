"use client";

// Whether the section around you is still arriving.
//
// The seam this closes. A cold page does two things at once: the section fades
// and rises into place, and — a moment later, when the read lands — the skeleton
// inside it hands over to the real figure, which fades and rises as well. Both
// movements are correct on their own. Together they compose: the content starts
// its 8px travel from wherever the section has got to, so the figure sets off
// again just as the card is settling, and the card visibly stutters.
//
// So a handover that happens *while* the section is still arriving keeps the
// fade and drops the travel. One movement carries the section, and the content
// simply resolves inside it. Once the section has landed, `Settled` is back to
// its full entrance — a refetch an hour later is a change worth showing, not
// part of a page arriving.
//
// The same rule covers one entrance nested inside another, which is how the
// settings panes work: a `Reveal` that mounts inside a live section fades
// without travelling, and the one that plays later, when the operator picks a
// different pane, gets its full movement.
//
// Cost, since this reaches every section: the flag lives in the section's own
// state, so flipping it re-renders the section component — but `children` came
// in as a prop and its element identity has not changed, so React reuses that
// whole subtree untouched. Only the handful of components that actually read
// this context re-render, which is the set that needs to.

import { createContext, useContext } from "react";

export const EntranceContext = createContext(false);

/**
 * True when an enclosing `Reveal` / `StaggerItem` is still animating in.
 *
 * Read it to decide *how* to animate at the moment you start, not to render
 * with: what matters is the state of the world when the thing appeared. Both
 * callers capture it into state at that instant rather than following it.
 */
export function useEntranceLive(): boolean {
  return useContext(EntranceContext);
}
