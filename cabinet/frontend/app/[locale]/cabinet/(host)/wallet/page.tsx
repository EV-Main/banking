import { I18nScope } from "@evinvest/i18n/react";

import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";
import { WalletHostView } from "@/views/wallet/ui/wallet-host-view";

// `/cabinet/wallet`: the same panel the wallet element renders on scope hosts. Wallet notices
// of a scope without its own link base (SA, for one) point here.
export default async function WalletPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(host)/wallet/page.tsx")}>
      <WalletHostView />
    </I18nScope>
  );
}
