import { useT } from "@evinvest/i18n/react";

import { useId } from "react";

import { Button } from "@evinvest/uikit";

/**
 * The button a service puts where it asks for a mandate. It knows the stored request's id and
 * nothing else: the caps, the service and the expiry are read on evinvest.ltd from the request
 * banking holds, so nothing the host renders can widen what gets allowed. Disabled, with the
 * reason beside it, without a usable id — the host SDK would refuse the action anyway.
 */
export function MandateButton({ request, onReview }: { request: string | null; onReview: () => void }) {
  const t = useT();
  const reasonId = useId();
  return (
    <span className="inline-flex flex-col gap-1">
      <Button type="button" disabled={request === null} aria-describedby={request === null ? reasonId : undefined} onClick={onReview}>
        {t("walletMfe.reviewMandate", "Review access request")}
      </Button>
      {request === null && (
        <span id={reasonId} className="text-xs text-ink-soft">
          {t("walletMfe.noMandateRequest", "There is no access request to review here.")}
        </span>
      )}
    </span>
  );
}
