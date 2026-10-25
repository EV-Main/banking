import { NAVIGATE_EVENT } from '@evinvest/mfe';
import { navigateListener, RemoteElement, type NavigateHandler } from '@evinvest/mfe/react';
import type { ReactNode } from 'react';

import { INLINE_ACTIONS_ATTRIBUTE, WALLET_API_VERSION, WALLET_TAG, type WalletActionMode, type WalletActionOutcome } from '../index';
import { actionEvents, inlineActions } from './listeners';

export interface WalletProps {
  /** Bundle URL from the wallet registry (`walletEntry(entries, 'wallet')`). */
  scriptUrl: string;
  /**
   * A scope host's id: the element shows that scope's compartment and main,
   * balances and history only, and its money actions go to a popup on
   * evinvest.ltd. Absent, the full wallet — mounted on evinvest.ltd itself,
   * where the element carries out actions in place.
   */
  scope?: string;
  /**
   * Default `'popup'`. `'inline'` lets the element run an action in this page when
   * banking allows that kind for this host; any other action still goes to the popup.
   */
  mode?: WalletActionMode;
  /** A money action finished, in its popup or in the page; refresh what the host shows. */
  onResult?: (outcome: WalletActionOutcome) => void;
  onNavigate?: NavigateHandler;
  /** Where action results must come from; only for a staging stand. */
  origin?: string;
  /** Default `WALLET_API_VERSION`: an older element is refused and `fallback` stays. */
  minApiVersion?: number;
  className?: string;
  fallback?: ReactNode;
}

/** The banking wallet. With `scope`, its money actions run on evinvest.ltd in a popup, or in this page in inline mode. */
export function Wallet({
  scriptUrl,
  scope,
  mode = 'popup',
  onResult,
  onNavigate,
  origin,
  minApiVersion = WALLET_API_VERSION,
  className,
  fallback = null,
}: WalletProps) {
  return (
    <RemoteElement
      tag={WALLET_TAG}
      scriptUrl={scriptUrl}
      minApiVersion={minApiVersion}
      {...(className === undefined ? {} : { className })}
      attributes={{ scope, [INLINE_ACTIONS_ATTRIBUTE]: inlineActions(mode) }}
      events={{
        [NAVIGATE_EVENT]: navigateListener(onNavigate),
        // The names stay registered either way: RemoteElement binds the set of names once.
        ...actionEvents({ onResult, origin, mode, inPlace: scope === undefined }),
      }}
      fallback={fallback}
    />
  );
}
