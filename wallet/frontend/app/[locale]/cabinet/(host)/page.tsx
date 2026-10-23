import { I18nScope } from "@evinvest/i18n/react";

import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";
import { HomeView } from "@/views/home/ui/home-view";

// `/cabinet`: the sign-in host and the place identity verification starts and returns to
// (concierge `KYC_RETURN_URL`). A guest gets the route gate's sign-in wall.
export default async function HomePage() {
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(host)/page.tsx")}>
      <HomeView />
    </I18nScope>
  );
}
