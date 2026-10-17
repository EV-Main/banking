import { I18nScope } from "@evinvest/i18n/react";

import { PayoutLinesView } from "@/views/approval/ui/payout-lines-view";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// The lines of a pending scope payout, for the people on its approval's roster. Behind the
// session like every `(app)` page; the BFF checks the roster and answers one 404 otherwise.
export const dynamic = "force-dynamic";

export default async function PayoutLinesPage({ params }: { params: Promise<{ operationId: string }> }) {
  const { operationId } = await params;
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(app)/approvals/payout/[operationId]/page.tsx")}>
      <PayoutLinesView operationId={operationId} />
    </I18nScope>
  );
}
