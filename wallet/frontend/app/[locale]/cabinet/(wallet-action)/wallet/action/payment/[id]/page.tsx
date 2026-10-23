import { I18nScope } from "@evinvest/i18n/react";

import { routeMessages } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";
import { PaymentConfirmView } from "@/views/wallet/ui/payment-confirm-view";

// `/cabinet/wallet/action/payment/<id>` — a payment out of the reader's own claim, waiting for
// their answer; the link in the notice that asks for it, or a scope host's popup
// (`/wallet/payment/<id>`, redirected here by the proxy). The route gate sends a guest to sign
// in first. Nothing about the payment is read from the URL but its id: the terms are the
// server's, shown back before anything can be approved.
export const dynamic = "force-dynamic";

export default async function PaymentConfirmPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  const locale = await currentLocale();
  return (
    <I18nScope messages={routeMessages(locale, "app/[locale]/cabinet/(wallet-action)/wallet/action/payment/[id]/page.tsx")}>
      <PaymentConfirmView id={id} />
    </I18nScope>
  );
}
