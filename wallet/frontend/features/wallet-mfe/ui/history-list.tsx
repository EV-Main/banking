import { useLocale, useT } from "@evinvest/i18n/react";

import { Badge, Button, Empty, EmptyDescription, EmptyHeader, EmptyTitle, ListRow, ListRows } from "@evinvest/uikit";

import { intlLocale } from "@/shared/lib/intl-locale";
import { formatUsdt } from "@/shared/lib/money";

import { operationDirection, operationKindLabel, operationStateLabel, isSettled } from "../lib/operation-words";
import type { WalletHistory, WalletOperation } from "../lib/parse";

// Light DOM: a host without a CSS reset would indent the list and push values past the card.
const UNPADDED = "m-0 list-none p-0";

function OperationRow({ operation }: { operation: WalletOperation }) {
  const t = useT();
  const locale = useLocale();
  const direction = operationDirection(operation.kind);
  // The Unicode minus mirrors the plus; a hyphen is narrower and breaks the column.
  const sign = operation.amount === "" ? "" : direction === "in" ? "+" : direction === "out" ? "−" : "";
  const when =
    operation.createdAt > 0
      ? new Intl.DateTimeFormat(intlLocale(locale), { dateStyle: "medium", timeStyle: "short" }).format(new Date(operation.createdAt * 1000))
      : undefined;
  return (
    <ListRow
      label={operationKindLabel(operation.kind, t)}
      description={when}
      value={<span className="tabular-nums">{operation.amount === "" ? "—" : `${sign}${formatUsdt(operation.amount, locale)}`}</span>}
    >
      {isSettled(operation.state) ? null : <Badge variant="outline">{operationStateLabel(operation.state, t)}</Badge>}
    </ListRow>
  );
}

/** `allHref`: the full timeline on the wallet origin, offered when this is only its newest page. */
export function HistoryList({ history, allHref }: { history: WalletHistory; allHref: string }) {
  const t = useT();
  if (history.operations.length === 0) {
    return (
      <Empty>
        <EmptyHeader>
          <EmptyTitle>{t("walletMfe.historyEmpty", "No activity yet")}</EmptyTitle>
          <EmptyDescription>{t("walletMfe.historyEmptyDesc", "Deposits and withdrawals will show up here.")}</EmptyDescription>
        </EmptyHeader>
      </Empty>
    );
  }
  return (
    <>
      <ListRows className={UNPADDED}>
        {history.operations.map((operation) => (
          <OperationRow key={operation.id} operation={operation} />
        ))}
      </ListRows>
      {history.truncated && (
        <Button asChild variant="link" size="sm" className="self-start px-0">
          <a href={allHref}>{t("walletMfe.allActivity", "See all activity")}</a>
        </Button>
      )}
    </>
  );
}
