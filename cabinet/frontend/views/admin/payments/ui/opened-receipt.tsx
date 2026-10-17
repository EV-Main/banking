"use client";

// What just happened, said accurately.
//
// The operator has clicked a button on a money surface, and the natural reading of any
// receipt is "it is done". This one leads with OPENED, says in plain words that nothing
// has moved, and names who has been asked — because that is the plane's answer, not the
// form's preview: an owner consilium (with the room linked, where the live tally is) or
// one investor's mailbox. It offers no "view payment" affordance beyond the list below,
// because until someone answers there is nothing more to see.

import { useLocale, useT } from "@evinvest/i18n/react";
import { Alert, AlertDescription, AlertTitle, Button } from "@evinvest/uikit";
import { CheckCircle2 } from "lucide-react";

import type { Payment } from "@/shared/contracts/payments";
import { expiresIn, formatMoment } from "@/shared/lib/datetime";
import { Link } from "@/shared/ui/cabinet-link";

export function OpenedReceipt({ payment, onDismiss }: { payment: Payment; onDismiss: () => void }) {
  const t = useT();
  const locale = useLocale();
  const consent = payment.consent;
  return (
    <Alert role="status" variant="success">
      <CheckCircle2 className="size-4" />
      <AlertTitle>{t("admin.payments.openedTitle", "Opened — no money has moved")}</AlertTitle>
      <AlertDescription className="gap-3">
        <p className="leading-relaxed">
          {consent
            ? t("admin.payments.openedConsentBody", "{email} has been emailed a link and a code. Nothing moves until they agree — this is their money, and only they can say so.", { email: consent.subject_email })
            : payment.seat === "session"
              ? t("admin.payments.openedSessionBody", "The investor has been told in their cabinet. Nothing moves until they confirm it there — this is their money, and only they can say so.")
              : payment.seat === "concierge_approval"
                ? t("admin.payments.openedApprovalBody", "The payment is proposed to the platform owners as an approval. Nothing moves until more than half of them approve it and its veto delay has passed.")
                : t("admin.payments.openedConsiliumBody", "The owners have each been emailed a link. Nothing moves until more than half of them confirm from their own mailboxes; the live tally is in the consilium.")}
        </p>
        <p className="text-xs tabular-nums text-ink-soft">
          {t("admin.payments.openedExpires", "Expires {at} · {left}", { at: formatMoment(payment.expires_at, locale), left: expiresIn(payment.expires_at, t) })}
        </p>
        <div className="flex flex-wrap gap-2 text-ink">
          {payment.consilium_id && (
            <Button asChild size="sm" variant="outline">
              <Link href="/consilium">{t("admin.payments.openConsilium", "Open the consilium")}</Link>
            </Button>
          )}
          <Button type="button" size="sm" variant="ghost" onClick={onDismiss}>
            {t("ui.close", "Close")}
          </Button>
        </div>
      </AlertDescription>
    </Alert>
  );
}
