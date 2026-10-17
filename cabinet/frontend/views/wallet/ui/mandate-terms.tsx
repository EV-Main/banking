"use client";

import { useLocale, useT } from "@evinvest/i18n/react";

import { capRows, endsWhenRevoked, type CapKind } from "@/entities/mandate/lib/request";
import type { MandateOperation } from "@/shared/contracts";
import { formatDay } from "@/shared/lib/datetime";
import { formatExactUsdt } from "@/shared/lib/money";

export interface Terms {
  operations: readonly MandateOperation[];
  cap_per_op: string;
  cap_24h: string;
  cap_outstanding: string;
  cap_book_fill_24h: string;
  /** Unix seconds; "0" = until revoked. */
  ends_at: string;
}

/**
 * What a service may do, up to how much, until when — every figure the server's. Caps are
 * shown through the exact formatter: this is what the reader authorises, so it must not be
 * rounded on its way to the screen.
 */
export function MandateTerms({ terms }: { terms: Terms }) {
  const t = useT();
  const locale = useLocale();
  const operations: Record<MandateOperation, string> = {
    reserve: t("walletAction.opReserve", "Hold funds for orders and subscriptions"),
    charge: t("walletAction.opCharge", "Charge directly"),
    book_fill: t("walletAction.opBookFill", "Settle trades with other users"),
  };
  const labels: Record<CapKind, string> = {
    perOp: t("walletAction.capPerOp", "Per operation"),
    per24h: t("walletAction.cap24h", "Per 24 hours"),
    outstanding: t("walletAction.capOutstanding", "Held at once"),
    bookFill24h: t("walletAction.capBookFill24h", "Trades with other users, per 24 hours"),
  };
  const rows: (readonly [string, string])[] = capRows(terms).map(({ kind, value }) => [labels[kind], `${formatExactUsdt(value, locale)} USDT`]);
  rows.push([t("walletAction.expires", "Valid until"), endsWhenRevoked(terms.ends_at) ? t("walletAction.untilRevoked", "Until you end it") : formatDay(terms.ends_at, locale)]);
  return (
    <>
      <ul className="flex list-disc flex-col gap-1 pl-5 text-sm text-ink">
        {terms.operations.map((op) => (
          <li key={op}>{operations[op]}</li>
        ))}
      </ul>
      <dl className="flex flex-col gap-1.5 text-sm">
        {rows.map(([label, value]) => (
          <div key={label} className="flex justify-between gap-4">
            <dt className="text-ink-soft">{label}</dt>
            <dd className="text-ink tabular-nums">{value}</dd>
          </div>
        ))}
      </dl>
    </>
  );
}
