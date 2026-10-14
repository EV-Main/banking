import { API_VERSION } from '@evinvest/mfe';
import { RemoteElement } from '@evinvest/mfe/react';
import type { ReactNode } from 'react';

import { PAY_INVOICE_TAG, WALLET_ACTION_EVENT, type WalletActionOutcome } from '../index';
import { walletActionListener } from './listeners';

export interface PayInvoiceButtonProps {
  /** Bundle URL from the wallet registry (`walletEntry(entries, 'payInvoice')`). */
  scriptUrl: string;
  /** The invoice banking issued for this payment. */
  invoice: string;
  /** The scope the invoice belongs to, which pays from its own compartment. */
  scope?: string;
  /** The payment finished, failed or was abandoned in its popup. */
  onResult: (outcome: WalletActionOutcome) => void;
  /** Where results must come from; only for a staging stand. */
  origin?: string;
  minApiVersion?: number;
  className?: string;
  fallback?: ReactNode;
}

/**
 * Pay a banking invoice. The element renders the button; the payment itself
 * happens on evinvest.ltd, in a popup, and its outcome comes back to `onResult`.
 */
export function PayInvoiceButton({ scriptUrl, invoice, scope, onResult, origin, minApiVersion = API_VERSION, className, fallback = null }: PayInvoiceButtonProps) {
  return (
    <RemoteElement
      tag={PAY_INVOICE_TAG}
      scriptUrl={scriptUrl}
      minApiVersion={minApiVersion}
      {...(className === undefined ? {} : { className })}
      attributes={{ invoice, scope }}
      events={{ [WALLET_ACTION_EVENT]: walletActionListener(onResult, origin) }}
      fallback={fallback}
    />
  );
}
