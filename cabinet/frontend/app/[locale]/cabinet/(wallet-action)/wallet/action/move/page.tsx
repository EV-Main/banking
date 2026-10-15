import { I18nScope } from "@evinvest/i18n/react";

import { UnavailableAction } from "@/features/wallet-action";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// `https://evinvest.ltd/wallet/move` — main ⇄ compartment. A placeholder until the BFF routes
// CompartmentService.MoveFunds; the path exists so the host contract is whole.
export const dynamic = "force-dynamic";

export default async function WalletMoveActionPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(wallet-action)/wallet/action/move/page.tsx")}>
      <UnavailableAction action="move" />
    </I18nScope>
  );
}
