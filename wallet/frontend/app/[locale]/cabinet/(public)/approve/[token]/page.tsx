import { I18nScope } from "@evinvest/i18n/react";

import { PayoutApprovalView } from "@/views/approval/ui/payout-approval-view";
import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// The payout approval an owner opens from their email.
//
// The token is read here and handed down, but it is never used server-side: the summary is
// fetched from the browser, exactly like every other read in this cabinet (there are no
// route handlers and no server-side reads — see `shared/lib/resource.ts`). That also keeps
// the token out of the server's logs and out of the RSC payload's fetch cache.
//
// `dynamic` is asserted rather than inferred. This page must never be prerendered or
// cached: every token is a different page, each is single-use, and a cached one would be
// the worst possible thing to serve to the next reader.
export const dynamic = "force-dynamic";

export default async function PayoutApprovalPage({ params }: { params: Promise<{ token: string }> }) {
  const { token } = await params;
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(public)/approve/[token]/page.tsx")}>
      <PayoutApprovalView token={token} />
    </I18nScope>
  );
}
