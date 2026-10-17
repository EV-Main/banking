import { I18nScope } from "@evinvest/i18n/react";

import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";
import { MoveActionView } from "@/views/wallet/ui/move-action-view";

// `https://evinvest.ltd/wallet/move` — main ⇄ one scope's compartment. The amount is typed on the page;
// the URL may only preselect a scope the server lists.
export const dynamic = "force-dynamic";

export default async function WalletMoveActionPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(wallet-action)/wallet/action/move/page.tsx")}>
      <MoveActionView />
    </I18nScope>
  );
}
