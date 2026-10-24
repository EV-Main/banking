// When Escape closes an in-page form. Pure and import-free so `node --test` runs it as is.

/** The phases of one in-page action (`model/use-intent-flow.ts`). */
export type FlowPhase = "editing" | "creating" | "review" | "expired" | "unclear" | "done";

/**
 * Escape is Cancel where the form offers Cancel or Close: never while a create or a confirm is
 * in flight, nor on the terms, where leaving would drop an intent the person is reading.
 */
export function escapeCloses(phase: FlowPhase): boolean {
  return phase === "editing" || phase === "done";
}
