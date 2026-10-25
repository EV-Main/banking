import { RemoteElement } from '@evinvest/mfe/react';
import type { ReactNode } from 'react';

import { INLINE_ACTIONS_ATTRIBUTE, PAY_INVOICE_TAG, WALLET_API_VERSION, type WalletActionMode, type WalletActionOutcome } from '../index';
import { actionEvents, inlineActions } from './listeners';

export interface PayInvoiceButtonProps {
  /** Bundle URL from the wallet registry (`walletEntry(entries, 'payInvoice')`). */
  scriptUrl: string;
  /** The invoice banking issued for this payment. */
  invoice: string;
  /** The scope the invoice belongs to, which pays from its own compartment. */
  scope?: string;
  /**
   * Default `'popup'`. `'inline'` lets the element take the payment in this page when
   * banking allows it for this host; otherwise it still goes to the popup.
   */
  mode?: WalletActionMode;
  /** The payment finished, failed or was abandoned, in its popup or in the page. */
  onResult: (outcome: WalletActionOutcome) => void;
  /** Where results must come from; only for a staging stand. */
  origin?: string;
  /** Default `WALLET_API_VERSION`: an older element is refused and `fallback` stays. */
  minApiVersion?: number;
  className?: string;
  fallback?: ReactNode;
}

/**
 * Pay a banking invoice. The element renders the button; the payment happens on
 * evinvest.ltd in a popup, or in this page in inline mode, and its outcome comes back to
 * `onResult`.
 */
export function PayInvoiceButton({
  scriptUrl,
  invoice,
  scope,
  mode = 'popup',
  onResult,
  origin,
  minApiVersion = WALLET_API_VERSION,
  className,
  fallback = null,
}: PayInvoiceButtonProps) {
  return (
    <RemoteElement
      tag={PAY_INVOICE_TAG}
      scriptUrl={scriptUrl}
      minApiVersion={minApiVersion}
      {...(className === undefined ? {} : { className })}
      attributes={{ invoice, scope, [INLINE_ACTIONS_ATTRIBUTE]: inlineActions(mode) }}
      events={actionEvents({ onResult, origin, mode })}
      fallback={fallback}
    />
  );
}
