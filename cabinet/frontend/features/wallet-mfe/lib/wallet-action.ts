// The wallet element's side of the money-action contract (`@evinvest/wallet`, banking
// `sdk/wallet`). On a scope host the element moves no money: every action is a page on the
// wallet origin, offered to the host as a cancelable `ev:wallet-action` so it can open it in
// a popup and hear the result back. Left alone, the element navigates — which is also the
// in-place behaviour on evinvest.ltd itself, where the host does not intercept.
//
// Pure and import-free so `node --test` runs it as is.

/** Cancelable; `detail` is `{ href }`. Name frozen with apiVersion 2 (sdk/wallet `WALLET_ACTION_EVENT`). */
export const WALLET_ACTION_EVENT = "ev:wallet-action";

/** The four action pages, by the exact path the host SDK accepts (`WALLET_ACTION_PATHS`). */
export const WALLET_ACTIONS = {
  pay: "/wallet/pay",
  withdraw: "/wallet/withdraw",
  move: "/wallet/move",
  mandate: "/wallet/mandate",
} as const;

export type WalletAction = keyof typeof WALLET_ACTIONS;

// The SDK refuses an action whose `scope`/`invoice` is not this shape rather than cleaning
// it, so the element never builds one it would refuse.
const ID = /^[A-Za-z0-9_-]{1,128}$/;

/** An id the action URL may carry, or null — the same rule the host SDK applies. */
export function actionId(raw: string | null | undefined): string | null {
  if (typeof raw !== "string") return null;
  const value = raw.trim();
  return ID.test(value) ? value : null;
}

/**
 * The scope a host named on the element, or null for the full wallet. `main` is not a scope
 * host's id — it is the account every compartment hangs off — so naming it reads as absent
 * rather than rendering a "main + main" wallet.
 */
export function readScope(raw: string | null | undefined): string | null {
  const id = actionId(raw);
  return id === null || id === "main" ? null : id;
}

/**
 * The absolute action URL on `origin` (the wallet's own origin, where the bundle came from):
 * the path and nothing but `scope`/`invoice`. Amounts, addresses and recipients never ride
 * in it — the action page reads them from the server.
 */
export function walletActionHref(origin: string, action: WalletAction, params: { scope?: string | null; invoice?: string | null } = {}): string {
  const url = new URL(WALLET_ACTIONS[action], origin);
  const scope = actionId(params.scope);
  const invoice = actionId(params.invoice);
  if (invoice !== null) url.searchParams.set("invoice", invoice);
  if (scope !== null) url.searchParams.set("scope", scope);
  return url.href;
}

/**
 * Offer the action to the host from `source`. True when the host took it (called
 * `preventDefault()`); the caller then does nothing more. Bubbles and is composed so a host
 * that wraps the element in its own shadow root still hears it.
 */
export function offerWalletAction(source: EventTarget, href: string): boolean {
  const event = new CustomEvent<{ href: string }>(WALLET_ACTION_EVENT, {
    bubbles: true,
    composed: true,
    cancelable: true,
    detail: { href },
  });
  return !source.dispatchEvent(event);
}
