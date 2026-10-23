import { I18nScope } from "@evinvest/i18n/react";

import { ConsentApprovalView } from "@/views/approval/ui/consent-approval-view";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// The payment consent an investor opens from their email — their own money, their answer.
// Same contract as the payout approval next door: the token is handed to the client and
// read from the browser, and the page is never prerendered or cached — every token is a
// different page, each is single-use, and a cached one would be served to the next reader.
export const dynamic = "force-dynamic";

export default async function ConsentApprovalPage({ params }: { params: Promise<{ token: string }> }) {
  const { token } = await params;
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(public)/consent/[token]/page.tsx")}>
      <ConsentApprovalView token={token} />
    </I18nScope>
  );
}
