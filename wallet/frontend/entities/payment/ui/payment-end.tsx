"use client";

// One end of a payment order, as a reader recognises it.
//
// The label is what the digest binds and every approval surface shares; the detail is
// what a PERSON tells the end by — the receiving investor's masked mailbox, the product's
// title. Both are always shown together so "investor 8f3e…" is never approved for the
// wrong person. An external end shows its rail and a shortened address here, with the
// whole one a tap away in a toggletip — never only in a `title`, which a keyboard or a
// touch screen cannot open; the surfaces where the address is the thing being approved
// render it whole through `FullAddress` instead (docs/CONSILIUM.md, policy 13).

import { useT } from "@evinvest/i18n/react";
import { InfoTip, InfoTipContent, InfoTipTrigger } from "@evinvest/uikit";

import type { PaymentEnd } from "@/shared/contracts/payments";
import { cn } from "@/shared/lib/cn";
import { shortAddress } from "@/shared/lib/money";
import { networkLabel } from "@/shared/lib/rail";
import { NetworkMark } from "@/shared/ui/icons/networks";

export function PaymentEndSummary({ end, className }: { end: PaymentEnd; className?: string }) {
  const t = useT();
  const external = end.kind === "external";
  return (
    <span className={cn("flex min-w-0 flex-col gap-0.5", className)}>
      <span className="truncate font-medium text-ink">{end.label || "—"}</span>
      {external ? (
        <span className="flex min-w-0 items-center gap-1.5 text-xs text-ink-soft">
          <NetworkMark network={end.network} className="size-3.5 shrink-0" />
          <span className="shrink-0">{networkLabel(end.network)}</span>
          <span className="min-w-0 truncate font-mono-tech">{shortAddress(end.address)}</span>
          <InfoTip>
            <InfoTipTrigger label={t("payment.end.fullAddress", "Show the full address")} />
            <InfoTipContent className="w-auto max-w-80 break-all font-mono-tech text-xs">{end.address}</InfoTipContent>
          </InfoTip>
        </span>
      ) : end.detail ? (
        <span className="truncate text-xs text-ink-soft">{end.detail}</span>
      ) : null}
    </span>
  );
}

/** The two ends in one line, for a sentence-shaped summary: "Fund revenue → Investor …". */
export function endWords(end: PaymentEnd): string {
  if (end.kind === "external") return `${end.label || networkLabel(end.network)} · ${end.address}`;
  return end.detail ? `${end.label} · ${end.detail}` : end.label;
}
