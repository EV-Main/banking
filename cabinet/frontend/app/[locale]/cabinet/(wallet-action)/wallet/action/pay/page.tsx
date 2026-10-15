import { I18nScope } from "@evinvest/i18n/react";

import { UnavailableAction } from "@/features/wallet-action";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// `https://evinvest.ltd/wallet/pay?invoice=` — paying an invoice. A placeholder until the
// invoice routes land in the BFF; the amount will come from the server-side invoice, never the URL.
export const dynamic = "force-dynamic";

export default async function WalletPayActionPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(wallet-action)/wallet/action/pay/page.tsx")}>
      <UnavailableAction action="pay" />
    </I18nScope>
  );
}
