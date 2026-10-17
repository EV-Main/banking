import { I18nScope } from "@evinvest/i18n/react";

import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";
import { PayInvoiceActionView } from "@/views/wallet/ui/pay-invoice-action-view";

// `https://evinvest.ltd/wallet/pay?invoice=` — paying an invoice. The amount comes from the
// server-side invoice, never the URL.
export const dynamic = "force-dynamic";

export default async function WalletPayActionPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(wallet-action)/wallet/action/pay/page.tsx")}>
      <PayInvoiceActionView />
    </I18nScope>
  );
}
