"use client";

import { useT } from "@evinvest/i18n/react";

import { Button, Spinner } from "@evinvest/uikit";

import type { Mandate } from "@/shared/contracts";
import { cn } from "@/shared/lib/cn";
import { MandateTerms } from "@/views/wallet/ui/mandate-terms";
import { WALLET_CARD } from "@/views/wallet/ui/wallet-chrome";

/**
 * One live mandate, every figure the server's: who may draw, how much, until when. The
 * reader's only act on it here is to end it.
 */
export function MandateCard({ mandate, revoking, onRevoke }: { mandate: Mandate; revoking: boolean; onRevoke: () => void }) {
  const t = useT();
  return (
    <section className={cn(WALLET_CARD, "flex flex-col gap-3 p-4.5 lg:p-6")}>
      <div className="flex flex-col gap-1">
        <span className="text-xs font-medium text-ink-soft">{t("walletAction.mandateService", "Service in {scope}", { scope: mandate.scope })}</span>
        <h2 className="text-lg font-semibold text-ink">{mandate.client_display_name || mandate.client_id}</h2>
      </div>
      <MandateTerms terms={{ ...mandate, ends_at: mandate.expires_at }} />
      <Button type="button" variant="outline" onClick={onRevoke} disabled={revoking}>
        {revoking ? <Spinner aria-hidden /> : t("walletAction.revoke", "End this permission")}
      </Button>
    </section>
  );
}
