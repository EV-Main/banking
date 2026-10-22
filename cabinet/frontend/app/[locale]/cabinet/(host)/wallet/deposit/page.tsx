import { I18nScope } from "@evinvest/i18n/react";

import { DepositView } from "@/views/wallet/ui/deposit-view";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// Top up the balance with crypto; the wallet element's Deposit button links here. `?network=`
// preselects a rail (old links still carry it); it's read here (server side) rather than with
// `useSearchParams` so the view needs no Suspense boundary.
export default async function WalletDepositPage({ searchParams }: { searchParams: Promise<{ network?: string }> }) {
  const { network } = await searchParams;
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(host)/wallet/deposit/page.tsx")}>
      <DepositView initialNetwork={network} />
    </I18nScope>
  );
}
