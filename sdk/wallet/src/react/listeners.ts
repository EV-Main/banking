import type { RemoteEventHandler } from '@evinvest/mfe/react';

import { openWalletAction, walletActionFromEvent, walletNavigationHref, type WalletActionOutcome } from '../index';

/**
 * The `ev:wallet-action` listener of a scope host. Runs inside the element's
 * click, so the popup keeps the user's activation; when the popup is blocked
 * anyway, the action becomes a full navigation that returns here. A second click
 * while a popup is open only focuses it (`busy`): one action, one `onResult`.
 * Every `ev:wallet-action` is cancelled here: left to the element, one that
 * failed the path or id check would navigate this tab to it unfiltered.
 */
export function walletActionListener(onResult: ((outcome: WalletActionOutcome) => void) | undefined, origin: string | undefined): RemoteEventHandler {
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
