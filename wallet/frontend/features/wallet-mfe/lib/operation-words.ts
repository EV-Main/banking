// Operation kinds and states in words, for the wallet element. The keys and English are the
// operations timeline's (`views/operations/lib/format.ts`) on purpose: same word, same
// translation, and the element's catalogue is cut from the cabinet's at build time, so it
// carries them without a second copy. A view cannot be imported from a feature, hence the
// small mirror rather than a shared import.

import type { Translate } from "@evinvest/i18n";

export type Direction = "in" | "out" | "move";

const DIRECTIONS: Readonly<Record<string, Direction>> = {
  deposit: "in",
  withdrawal: "out",
  fee: "out",
  subscription: "move",
  redemption: "move",
};

export function operationDirection(kind: string): Direction {
  return Object.hasOwn(DIRECTIONS, kind) ? DIRECTIONS[kind] : "move";
}

export function operationKindLabel(kind: string, t: Translate): string {
  switch (kind) {
    case "deposit":
      return t("ops.kind.deposit", "Deposit");
    case "withdrawal":
      return t("ops.kind.withdrawal", "Withdrawal");
    case "subscription":
      return t("ops.kind.subscription", "Subscription");
    case "redemption":
      return t("ops.kind.redemption", "Redemption");
    case "fee":
      return t("ops.kind.fee", "Fee");
    default:
      // A kind the hub adds later is still a legible row: its wire id, never translated.
      return kind || t("ops.kind.unknown", "Operation");
  }
}

const SETTLED = new Set(["credited", "completed", "charged", "partly_deferred"]);

/** Done and nothing left to say about it: the row needs no state badge. */
export function isSettled(state: string): boolean {
  return SETTLED.has(state);
}

export function operationStateLabel(state: string, t: Translate): string {
  switch (state) {
    case "queued":
      return t("ops.state.queued", "Queued");
    case "processing":
      return t("ops.state.processing", "Processing");
    case "failed":
      return t("ops.state.failed", "Failed");
    case "cancelled":
      return t("ops.state.cancelled", "Cancelled");
    default:
      return state.replace(/_/g, " ");
  }
}
