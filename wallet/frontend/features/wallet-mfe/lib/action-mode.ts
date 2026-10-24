// Where a money action runs. Pure and import-free so `node --test` runs it as is.

export type ActionMode = "in_page" | "offer";

/**
 * In the page only when the build turned the in-page forms on AND the element names a scope:
 * the intent routes serve trusted scope hosts alone, and without a scope the element is on
 * evinvest.ltd itself, where the action page runs in place. Everything else keeps the
 * `ev:wallet-action` offer exactly as before.
 */
export function actionMode(inPageEnabled: boolean, scope: string | null): ActionMode {
  return inPageEnabled && scope !== null ? "in_page" : "offer";
}
