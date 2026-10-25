import { RemoteElement } from '@evinvest/mfe/react';
import type { ReactNode } from 'react';

import { INLINE_ACTIONS_ATTRIBUTE, MANDATE_TAG, WALLET_API_VERSION, type WalletActionMode, type WalletActionOutcome } from '../index';
import { actionEvents, inlineActions } from './listeners';

export interface MandateButtonProps {
  /** Bundle URL from the wallet registry (`walletEntry(entries, 'mandate')`). */
  scriptUrl: string;
  /** The mandate request banking stored for the service; its terms are read from it, server-side. */
  request: string;
  /** The scope the mandate is for. */
  scope?: string;
  /**
   * Default `'popup'`. `'inline'` lets the element take the grant in this page when
   * banking allows it for this host; otherwise it still goes to the popup.
   */
  mode?: WalletActionMode;
  /** The grant finished, failed or was abandoned, in its popup or in the page. */
  onResult: (outcome: WalletActionOutcome) => void;
  /** Where results must come from; only for a staging stand. */
  origin?: string;
  /** Default `WALLET_API_VERSION`: an older element is refused and `fallback` stays. */
  minApiVersion?: number;
  className?: string;
  fallback?: ReactNode;
}

/** Grant the mandate a service asked for. Like `PayInvoiceButton`, the element renders the button. */
export function MandateButton({
  scriptUrl,
  request,
  scope,
  mode = 'popup',
  onResult,
  origin,
  minApiVersion = WALLET_API_VERSION,
  className,
  fallback = null,
}: MandateButtonProps) {
  return (
    <RemoteElement
      tag={MANDATE_TAG}
      scriptUrl={scriptUrl}
      minApiVersion={minApiVersion}
      {...(className === undefined ? {} : { className })}
      attributes={{ request, scope, [INLINE_ACTIONS_ATTRIBUTE]: inlineActions(mode) }}
      events={actionEvents({ onResult, origin, mode })}
      fallback={fallback}
    />
  );
}
