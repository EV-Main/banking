"use client";

import { useLocale, useT } from "@evinvest/i18n/react";

import { Button, Spinner } from "@evinvest/uikit";

import type { Mandate } from "@/shared/contracts";
import { cn } from "@/shared/lib/cn";
import { formatDay } from "@/shared/lib/datetime";
import { formatUsdt } from "@/views/wallet/lib/format";
import { WALLET_CARD } from "@/views/wallet/ui/wallet-chrome";

/**
 * One live mandate, every figure the server's: who may draw, how much, until when. The
 * reader's only act on it here is to end it.
 */
export function MandateCard({ mandate, revoking, onRevoke }: { mandate: Mandate; revoking: boolean; onRevoke: () => void }) {
  const t = useT();
  const locale = useLocale();
  const operations = {
    reserve: t("walletAction.opReserve", "Hold funds for orders and subscriptions"),
    charge: t("walletAction.opCharge", "Charge directly"),
    book_fill: t("walletAction.opBookFill", "Settle trades with other users"),
  };
  const rows: [string, string][] = [
    [t("walletAction.capPerOp", "Per operation"), `${formatUsdt(mandate.cap_per_op, locale)} USDT`],
    [t("walletAction.cap24h", "Per 24 hours"), `${formatUsdt(mandate.cap_24h, locale)} USDT`],
    [t("walletAction.capOutstanding", "Held at once"), `${formatUsdt(mandate.cap_outstanding, locale)} USDT`],
    [t("walletAction.expires", "Valid until"), mandate.expires_at === "0" ? t("walletAction.untilRevoked", "Until you end it") : formatDay(mandate.expires_at, locale)],
  ];
  return (
    <section className={cn(WALLET_CARD, "flex flex-col gap-3 p-4.5 lg:p-6")}>
      <div className="flex flex-col gap-1">
        <span className="text-xs font-medium text-ink-soft">{t("walletAction.mandateService", "Service in {scope}", { scope: mandate.scope })}</span>
        <h2 className="text-lg font-semibold text-ink">{mandate.client_display_name || mandate.client_id}</h2>
      </div>
      <ul className="flex list-disc flex-col gap-1 pl-5 text-sm text-ink">
        {mandate.operations.map((op) => (
          <li key={op}>{operations[op]}</li>
        ))}
      </ul>
      <dl className="flex flex-col gap-1.5 text-sm">
        {rows.map(([label, value]) => (
          <div key={label} className="flex justify-between gap-4">
            <dt className="text-ink-soft">{label}</dt>
            <dd className="text-ink">{value}</dd>
          </div>
        ))}
      </dl>
      <Button type="button" variant="outline" onClick={onRevoke} disabled={revoking}>
        {revoking ? <Spinner aria-hidden /> : t("walletAction.revoke", "End this permission")}
      </Button>
    </section>
  );
}
