import { useT } from "@evinvest/i18n/react";

import { Button } from "@evinvest/uikit";

/**
 * The pay button a scope host puts next to an invoice. It knows the invoice's id and nothing
 * else: the amount and the payee are read on evinvest.ltd from the invoice banking issued,
 * so nothing the host renders can change what gets paid. Disabled without a usable id —
 * the host SDK would refuse the action anyway.
 */
export function PayInvoiceButton({ invoice, onPay }: { invoice: string | null; onPay: () => void }) {
  const t = useT();
  return (
    <Button type="button" disabled={invoice === null} onClick={onPay}>
      {t("walletMfe.payInvoice", "Pay invoice")}
    </Button>
  );
}
