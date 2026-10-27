// How an in-page action tells the host it ended (apiVersion 3, sdk/wallet). The popup tells
// its opener by postMessage; an action that ran inside the element tells the page around it
// with this event, in the same words. Pure so `node --test` runs it as is.

import type { FlowPhase } from "./escape-closes.ts";
import type { WalletAction } from "./wallet-action.ts";

/** Not cancelable; `detail` is a {@link WalletResultDetail}. Same name as the popup's message type. */
export const WALLET_RESULT_EVENT = "ev:wallet-result";

/** The popup's words (`ActionOutcome`, sdk/wallet `WalletResult`). */
export type WalletOutcome = "done" | "failed" | "cancelled";

/** What the in-page form reports when it closes. */
export interface ClosedAction {
  outcome: WalletOutcome;
  reference: string | null;
}

/** The kind, the outcome, and the hub's reference when there is a usable one. Nothing else. */
export interface WalletResultDetail {
  kind: WalletAction;
  outcome: WalletOutcome;
  reference?: string;
}

// The SDK reads a longer reference as none, so the element never sends one.
const MAX_REFERENCE = 128;

/**
 * The flow as it stands when the form closes, by shape only: `IntentFlow` (model) fits it
 * without this module importing React.
 */
export type FlowAtClose = { phase: "done"; confirmed: { reference: string | null } } | { phase: Exclude<FlowPhase, "done">; error?: unknown };

/**
 * `done` only after the BFF answered the confirm. A form that closes on an error, on terms that
 * ran out, or on a confirm whose outcome is unclear is `failed`: the host re-reads its state
 * either way, and `failed` never claims success. Leaving a clean form is `cancelled`.
 */
export function closedAction(flow: FlowAtClose): ClosedAction {
  if (flow.phase === "done") return { outcome: "done", reference: flow.confirmed.reference };
  const failed = flow.phase === "expired" || flow.phase === "unclear" || (flow.error !== undefined && flow.error !== null);
  return { outcome: failed ? "failed" : "cancelled", reference: null };
}

/**
 * The event's detail. A reference that is empty or over 128 characters is left out, never cut:
 * a truncated reference names a different payment, or none.
 */
export function walletResultDetail(kind: WalletAction, closed: ClosedAction): WalletResultDetail {
  const { outcome, reference } = closed;
  const usable = reference !== null && reference !== "" && reference.length <= MAX_REFERENCE;
  return usable ? { kind, outcome, reference } : { kind, outcome };
}

/** Bubbles and is composed, like the offer, so a host that wraps the element in a shadow root hears it. */
export function dispatchWalletResult(source: EventTarget, detail: WalletResultDetail): void {
  source.dispatchEvent(new CustomEvent<WalletResultDetail>(WALLET_RESULT_EVENT, { bubbles: true, composed: true, cancelable: false, detail }));
}
