import { I18nScope } from "@evinvest/i18n/react";

import { LoggedOutView } from "@/views/logged-out/ui/logged-out";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

export default async function Page() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(auth)/loggedout/page.tsx")}>
      <LoggedOutView />
    </I18nScope>
  );
}
