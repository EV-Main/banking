import { useT } from "@evinvest/i18n/react";

import { useId } from "react";

import { Button } from "@evinvest/uikit";

/**
 * The pay button a scope host puts next to an invoice. It knows the invoice's id and nothing
 * else: the amount and the payee are read on evinvest.ltd from the invoice banking issued,
 * so nothing the host renders can change what gets paid. Disabled, with the reason beside
 * it, without a usable id — the host SDK would refuse the action anyway.
 */
export function PayInvoiceButton({ invoice, onPay }: { invoice: string | null; onPay: () => void }) {
  const t = useT();
  const reasonId = useId();
  return (
    <span className="inline-flex flex-col gap-1">
      <Button type="button" disabled={invoice === null} aria-describedby={invoice === null ? reasonId : undefined} onClick={onPay}>
        {t("walletMfe.payInvoice", "Pay invoice")}
      </Button>
      {invoice === null && (
        <span id={reasonId} className="text-xs text-ink-soft">
          {t("walletMfe.noInvoice", "There is no invoice to pay here.")}
        </span>
      )}
    </span>
  );
}
