import { I18nScope } from "@evinvest/i18n/react";

import { RemovalApprovalView } from "@/views/approval/ui/removal-approval-view";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// The "your ownership is being ended" notice, opened by the owner it is about.
// Same contract as the payout approval next door: the token is handed to the client and
// read from the browser, and the page is never prerendered or cached.
export const dynamic = "force-dynamic";

export default async function OwnerRemovalApprovalPage({ params }: { params: Promise<{ token: string }> }) {
  const { token } = await params;
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(public)/owner-removal/[token]/page.tsx")}>
      <RemovalApprovalView token={token} />
    </I18nScope>
  );
}
