import { I18nScope } from "@evinvest/i18n/react";

import { UnavailableAction } from "@/features/wallet-action";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// `https://evinvest.ltd/wallet/mandate` — a service mandate over a compartment. A placeholder
// until the BFF routes CompartmentService.GrantMandate.
export const dynamic = "force-dynamic";

export default async function WalletMandateActionPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(wallet-action)/wallet/action/mandate/page.tsx")}>
      <UnavailableAction action="mandate" />
    </I18nScope>
  );
}
