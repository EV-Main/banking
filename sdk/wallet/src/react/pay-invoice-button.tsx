import type { ReactNode } from 'react';

import { PAY_INVOICE_TAG, type WalletActionMode, type WalletActionOutcome } from '../index';
import { ActionElement } from './action-element';

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
export function PayInvoiceButton({ invoice, ...props }: PayInvoiceButtonProps) {
  return <ActionElement tag={PAY_INVOICE_TAG} target={{ invoice }} {...props} />;
}
