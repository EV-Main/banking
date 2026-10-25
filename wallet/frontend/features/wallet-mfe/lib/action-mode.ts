// Where a money action runs. Pure so `node --test` runs it as is.

import type { IntentKind } from "./intent.ts";

export type ActionMode = "in_page" | "offer";

// The params a move (`to_scope`) and a payment (`pay_from`) send name the host's scope, which
// only the element's `scope` attribute tells the page. A withdrawal and a mandate need none.
const NEEDS_SCOPE: Readonly<Record<IntentKind, boolean>> = { move: true, pay: true, withdraw: false, mandate: false };

/**
 * In the page only for a kind the BFF listed for this host (`GET /api/wallet/csrf` →
 * `kinds`), and only when the page can build its params. `null` is a list not known yet:
 * the click does not wait for it, so the action stays the `ev:wallet-action` offer, which a
 * host must receive inside the click to open its popup.
 */
export function inPageMode(enabled: readonly IntentKind[] | null, action: IntentKind, scope: string | null): ActionMode {
  if (enabled === null || !enabled.includes(action)) return "offer";
  return NEEDS_SCOPE[action] && scope === null ? "offer" : "in_page";
}

/**
 * Whether the host asked for in-page actions with `actions="inline"`. A host that does not
 * ask (an `@evinvest/wallet` 0.1.x) listens for no `ev:wallet-result`: an action that ran
 * in its page would end without a word, and the host would never re-read its state.
 */
export function inlineOptIn(actions: string | null): boolean {
  return actions === "inline";
}

/** `inPageMode` behind the host's opt-in: without it, every action stays the offer. */
export function actionMode(actions: string | null, enabled: readonly IntentKind[] | null, action: IntentKind, scope: string | null): ActionMode {
  return inlineOptIn(actions) ? inPageMode(enabled, action, scope) : "offer";
}
