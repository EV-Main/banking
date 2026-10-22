import { I18nScope } from "@evinvest/i18n/react";

import { ActivityView } from "@/views/wallet/ui/activity-view";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// The merged deposit + withdrawal feed, with cancel on rows still sitting in the queue.
export default async function WalletActivityPage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(host)/wallet/activity/page.tsx")}>
      <ActivityView />
    </I18nScope>
  );
}
