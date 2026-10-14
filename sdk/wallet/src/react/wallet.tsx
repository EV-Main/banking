import { API_VERSION, NAVIGATE_EVENT } from '@evinvest/mfe';
import { navigateListener, RemoteElement, type NavigateHandler } from '@evinvest/mfe/react';
import type { ReactNode } from 'react';

import { WALLET_ACTION_EVENT, WALLET_TAG, type WalletActionOutcome } from '../index';
import { actionInPlace, walletActionListener } from './listeners';

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
  /** Scope hosts: a money action finished in its popup; refresh what the host shows. */
  onResult?: (outcome: WalletActionOutcome) => void;
  onNavigate?: NavigateHandler;
  /** Where action results must come from; only for a staging stand. */
  origin?: string;
  minApiVersion?: number;
  className?: string;
  fallback?: ReactNode;
}

/** The banking wallet. With `scope`, its money actions run on evinvest.ltd, in a popup. */
export function Wallet({ scriptUrl, scope, onResult, onNavigate, origin, minApiVersion = API_VERSION, className, fallback = null }: WalletProps) {
  return (
    <RemoteElement
      tag={WALLET_TAG}
      scriptUrl={scriptUrl}
      minApiVersion={minApiVersion}
      {...(className === undefined ? {} : { className })}
      attributes={{ scope }}
      events={{
        [NAVIGATE_EVENT]: navigateListener(onNavigate),
        // The name stays registered either way: RemoteElement binds the set of names once.
        [WALLET_ACTION_EVENT]: scope === undefined ? actionInPlace : walletActionListener(onResult, origin),
      }}
      fallback={fallback}
    />
  );
}
