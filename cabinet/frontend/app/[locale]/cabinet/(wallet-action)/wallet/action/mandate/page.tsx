import { I18nScope } from "@evinvest/i18n/react";

import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";
import { MandateActionView } from "@/views/wallet/ui/mandate-action-view";

// `https://evinvest.ltd/wallet/mandate` — the services allowed to draw on the reader's compartment,
// and ending that permission. Nothing about a service or its caps is read from the URL.
export const dynamic = "force-dynamic";

export default async function WalletMandateActionPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(wallet-action)/wallet/action/mandate/page.tsx")}>
      <MandateActionView />
    </I18nScope>
  );
}
