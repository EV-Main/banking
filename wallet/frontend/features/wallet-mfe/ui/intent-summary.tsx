import { useLocale, useT } from "@evinvest/i18n/react";

import { formatMoment } from "@/shared/lib/datetime";
import { formatExactUsdt } from "@/shared/lib/money";
import { networkLabel } from "@/shared/lib/rail";

import { mandateCapRows, summaryRows, type CapTerm, type IntentKind, type IntentSummary, type MandateCaps, type SummaryTerm } from "../lib/intent";

// Light DOM rules do not reach the shadow root, but the browser's own list and margins do.
const TERMS = "m-0 flex flex-col gap-2 text-sm";
const ROW = "flex min-w-0 justify-between gap-4";

/**
 * What the BFF says will happen, term by term, before anyone confirms it. Every figure is the
 * server's and is shown exactly: an approval that rounds is an approval of something else.
 * The recipient is shown in full — a shortened address is the one a swap hides behind.
 */
export function IntentSummaryView({ kind, summary }: { kind: IntentKind; summary: IntentSummary }) {
  const t = useT();
  const locale = useLocale();
  const labels: Record<SummaryTerm, string> = {
    amount: t("walletMfe.term.amount", "Amount"),
    fee: t("walletMfe.term.fee", "Fee"),
    net: t("walletMfe.term.net", "Recipient gets"),
    recipient: kind === "mandate" ? t("walletMfe.term.service", "Service") : t("walletMfe.term.recipient", "To"),
    network: t("wallet.withdrawalNetwork", "Withdrawal network"),
    scope: t("walletMfe.term.scope", "Account"),
    host: t("walletMfe.term.host", "Requested by"),
  };
  return (
    <div className="flex flex-col gap-4">
      <dl className={TERMS}>
        {summaryRows(summary).map((row) => (
          <div key={row.term} className={ROW}>
            <dt className="shrink-0 text-ink-soft">{labels[row.term]}</dt>
            <dd className="m-0 min-w-0 break-all text-right text-ink tabular-nums">
              {row.money ? `${formatExactUsdt(row.value, locale)} USDT` : row.term === "network" ? networkLabel(row.value) : row.value}
            </dd>
          </div>
        ))}
      </dl>
      {summary.mandate && <MandateCapsView caps={summary.mandate} />}
    </div>
  );
}

function MandateCapsView({ caps }: { caps: MandateCaps }) {
  const t = useT();
  const locale = useLocale();
  const operations: Readonly<Record<string, string>> = {
    reserve: t("walletAction.opReserve", "Hold funds for orders and subscriptions"),
    charge: t("walletAction.opCharge", "Charge directly"),
    book_fill: t("walletAction.opBookFill", "Settle trades with other users"),
  };
  const labels: Record<CapTerm, string> = {
    perOp: t("walletAction.capPerOp", "Per operation"),
    per24h: t("walletAction.cap24h", "Per 24 hours"),
    outstanding: t("walletAction.capOutstanding", "Held at once"),
    bookFill24h: t("walletAction.capBookFill24h", "Trades with other users, per 24 hours"),
  };
  const ends = caps.expiresAt === 0 ? t("walletAction.untilRevoked", "Until you end it") : formatMoment(String(caps.expiresAt), locale);
  return (
    <>
      <ul className="m-0 flex list-disc flex-col gap-1 pl-5 text-sm text-ink">
        {caps.operations.map((op) => (
          <li key={op}>{Object.hasOwn(operations, op) ? operations[op] : op}</li>
        ))}
      </ul>
      <dl className={TERMS}>
        {mandateCapRows(caps).map((row) => (
          <div key={row.term} className={ROW}>
            <dt className="text-ink-soft">{labels[row.term]}</dt>
            <dd className="m-0 text-ink tabular-nums">{formatExactUsdt(row.value, locale)} USDT</dd>
          </div>
        ))}
        <div className={ROW}>
          <dt className="text-ink-soft">{t("walletAction.expires", "Valid until")}</dt>
          <dd className="m-0 text-ink tabular-nums">{ends}</dd>
        </div>
      </dl>
    </>
  );
}
