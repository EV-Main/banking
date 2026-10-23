// Pure rules of the move page, kept out of the component so they run under `node --test`.
// Import-light on purpose: only the wallet host's money module.

import { isPositiveWireDecimal, toBaseUnits } from "../../../shared/lib/money.ts";

export const MAIN = "main";

export type Direction = "in" | "out";

export interface MoveCompartment {
  scope?: string;
  available?: string;
}

/** The compartments a move may name: every scope but `main` itself. */
export function movableScopes<T extends MoveCompartment>(list: readonly T[]): T[] {
  return list.filter((c) => c.scope !== undefined && c.scope !== "" && c.scope !== MAIN);
}

/**
 * The scope the URL asked for, only if the server's list offers it. The URL is a hint for
 * what to highlight — an unknown or crafted value selects nothing, and the reader picks.
 */
export function preselectedScope(urlScope: string | null, scopes: readonly MoveCompartment[]): string | null {
  if (urlScope === null) return null;
  return scopes.some((c) => c.scope === urlScope) ? urlScope : null;
}

/** The two ends of a move. Exactly one is `main`, so the direction fixes which. */
export function moveEnds(scope: string, direction: Direction): { from_scope: string; to_scope: string } {
  return direction === "in" ? { from_scope: MAIN, to_scope: scope } : { from_scope: scope, to_scope: MAIN };
}

export type AmountProblem = "empty" | "invalid" | "exceeds";

/**
 * Whether a typed amount can be sent: a positive wire decimal not above what the source
 * holds. The hub re-checks all of it; this only saves the round trip.
 */
export function amountProblem(raw: string, available: string | undefined): AmountProblem | null {
  const text = raw.trim();
  if (text === "") return "empty";
  if (!isPositiveWireDecimal(text)) return "invalid";
  return toBaseUnits(text) > toBaseUnits(available) ? "exceeds" : null;
}
