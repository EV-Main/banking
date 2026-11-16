// Inline mode: the host lets the element run a money action inside the host page, through
// banking's intent routes on evinvest.ltd, instead of offering it for a popup. Both sides
// must agree: the host opts in with an attribute, and banking's BFF lists the action kinds
// it allows for that host. When either says no, the element still fires the cancelable
// `ev:wallet-action` offer, so a host in inline mode keeps its popup listener.
//
// Server-safe: nothing here touches the DOM.

import { walletOutcome, walletReference, type WalletResult } from './action';

/**
 * Not cancelable, bubbles and is composed: the in-page form of an action closed. `detail`
 * is `{ kind, outcome, reference? }`. The same string as `WALLET_RESULT_MESSAGE` on
 * purpose, but a different channel: this one is a DOM event on the element, that one is
 * the popup's `postMessage`. A popup fallback fires no event; its result comes by message.
 */
export const WALLET_RESULT_EVENT = 'ev:wallet-result';

/** The element attribute a host sets to {@link INLINE_ACTIONS_VALUE} to opt into inline mode. */
export const INLINE_ACTIONS_ATTRIBUTE = 'actions';
export const INLINE_ACTIONS_VALUE = 'inline';

/** Where the host lets a money action run: a popup on evinvest.ltd (the default), or the host page. */
export type WalletActionMode = 'popup' | 'inline';

/** The actions that can run in the host page. */
export const WALLET_INLINE_KINDS = ['move', 'withdraw', 'pay', 'mandate'] as const;
export type WalletInlineKind = (typeof WALLET_INLINE_KINDS)[number];

/**
 * How an in-page action ended. As with the popup, `done` is a hint for the UI, not proof
 * that money moved: the host re-reads the state from its own backend.
 */
export interface WalletInlineResult extends WalletResult {
  kind: WalletInlineKind;
}

/**
 * The detail of a {@link WALLET_RESULT_EVENT}, or null for anything else: not a
 * `CustomEvent` of that name, no detail, an unknown `kind` or `outcome`. The detail comes
 * from a bundle the host does not build, so it is read as untrusted. The rule for
 * `reference` is `parseWalletMessage`'s: an unusable one (not a string, empty, over 128
 * characters) reads as none and the outcome stands — the action may have moved money —
 * and a long one is never cut.
 */
export function parseWalletResultEvent(event: unknown): WalletInlineResult | null {
  if (typeof CustomEvent === 'undefined' || !(event instanceof CustomEvent)) return null;
  if (event.type !== WALLET_RESULT_EVENT) return null;
  const detail: unknown = event.detail;
  if (typeof detail !== 'object' || detail === null) return null;
  const rawKind: unknown = Reflect.get(detail, 'kind');
  const kind = WALLET_INLINE_KINDS.find(known => known === rawKind);
  const outcome = walletOutcome(Reflect.get(detail, 'outcome'));
  if (kind === undefined || outcome === undefined) return null;
  return { kind, outcome, reference: walletReference(Reflect.get(detail, 'reference')) };
}
