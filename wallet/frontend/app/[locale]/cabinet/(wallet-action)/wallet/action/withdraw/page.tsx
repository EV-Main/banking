import { I18nScope } from "@evinvest/i18n/react";

import { WithdrawActionView } from "@/views/wallet/ui/withdraw-action-view";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// `https://evinvest.ltd/wallet/withdraw` — a withdrawal opened by a scope host. Dynamic: the
// page reads its opener and nonce from the URL and must never be served from a cache.
export const dynamic = "force-dynamic";

export default async function WalletWithdrawActionPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(wallet-action)/wallet/action/withdraw/page.tsx")}>
      <WithdrawActionView />
    </I18nScope>
  );
}
