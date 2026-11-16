import type { RemoteEventHandler } from '@evinvest/mfe/react';

import {
  INLINE_ACTIONS_VALUE,
  openWalletAction,
  parseWalletResultEvent,
  WALLET_ACTION_EVENT,
  WALLET_RESULT_EVENT,
  walletActionFromEvent,
  walletNavigationHref,
  type WalletActionMode,
  type WalletActionOutcome,
} from '../index';

type OnResult = ((outcome: WalletActionOutcome) => void) | undefined;

/**
 * The `ev:wallet-action` listener of a scope host. Runs inside the element's
 * click, so the popup keeps the user's activation; when the popup is blocked
 * anyway, the action becomes a full navigation that returns here. A second click
 * while a popup is open only focuses it (`busy`): one action, one `onResult`.
 * Every `ev:wallet-action` is cancelled here: left to the element, one that
 * failed the path or id check would navigate this tab to it unfiltered.
 */
export function walletActionListener(onResult: OnResult, origin: string | undefined): RemoteEventHandler {
  return event => {
    const action = walletActionFromEvent(event, origin);
    event.preventDefault();
    if (!action) {
      onResult?.({ status: 'invalid_url' });
      return;
    }
    void openWalletAction(action, origin === undefined ? {} : { origin }).then(outcome => {
      if (outcome.status === 'blocked') {
        const href = walletNavigationHref(action, undefined, origin);
        if (href !== null) window.location.assign(href);
      } else if (outcome.status !== 'busy') onResult?.(outcome);
    });
  };
}

/** Leaves the action to the element: on evinvest.ltd itself it runs in place. */
export const actionInPlace: RemoteEventHandler = () => {};

/**
 * The `ev:wallet-result` listener: an in-page form closed. Reported as the `result`
 * variant, like a popup's. A detail that does not parse is ignored, not reported as
 * `invalid_url`: nothing was opened or refused, and the host has nothing it can trust.
 */
export function walletResultListener(onResult: OnResult): RemoteEventHandler {
  return event => {
    const result = parseWalletResultEvent(event);
    if (result) onResult?.({ status: 'result', result });
  };
}

interface ActionEventsOptions {
  onResult: OnResult;
  origin: string | undefined;
  mode: WalletActionMode;
  /** The full wallet on evinvest.ltd itself: the element runs every offered action in place. */
  inPlace?: boolean;
}

/**
 * Both action events, always: `RemoteElement` binds the set of event names once, so a mode
 * that changes after mount switches the handlers, not the names. `ev:wallet-action` is
 * still taken in inline mode — a kind the BFF does not list for this host comes as an offer.
 */
export function actionEvents({ onResult, origin, mode, inPlace = false }: ActionEventsOptions): Record<string, RemoteEventHandler> {
  return {
    [WALLET_ACTION_EVENT]: inPlace ? actionInPlace : walletActionListener(onResult, origin),
    [WALLET_RESULT_EVENT]: mode === 'inline' ? walletResultListener(onResult) : actionInPlace,
  };
}

/** The `actions` attribute value: set only in inline mode, absent otherwise. */
export function inlineActions(mode: WalletActionMode): string | undefined {
  return mode === 'inline' ? INLINE_ACTIONS_VALUE : undefined;
}
