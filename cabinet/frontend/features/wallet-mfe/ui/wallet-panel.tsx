import { useLocale, useT } from "@evinvest/i18n/react";

import { useId } from "react";

import { Card, CardContent, CardHeader, CardTitle, Empty, EmptyDescription, EmptyHeader, EmptyTitle, ResourceError, Skeleton } from "@evinvest/uikit";

import { formatUsdt } from "@/shared/lib/money";

import type { WalletAction } from "../lib/wallet-action";
import { useWalletData } from "../model/use-wallet-data";
import { BalanceRows } from "./balance-rows";
import { HistoryList } from "./history-list";
import { WalletActions } from "./wallet-actions";

export interface WalletPanelProps {
  /** The wallet origin — where the bundle came from, and where its BFF and action pages are. */
  origin: string;
  /** A scope host's id, or null for the full wallet. */
  scope: string | null;
  onAction: (action: WalletAction) => void;
}

/** Balances and history, read from the wallet origin; the money actions leave the element. */
export function WalletPanel({ origin, scope, onAction }: WalletPanelProps) {
  const t = useT();
  const locale = useLocale();
  const historyId = useId();
  const { data, reload } = useWalletData(origin);

  if (data.status === "signed_out") {
    return (
      <Card>
        <Empty>
          <EmptyHeader>
            <EmptyTitle>{t("walletMfe.signedOut", "Sign in to see your wallet")}</EmptyTitle>
            <EmptyDescription>{t("walletMfe.signedOutDesc", "Your balance and activity appear here once you are signed in to EV Invest.")}</EmptyDescription>
          </EmptyHeader>
        </Empty>
      </Card>
    );
  }

  const ready = data.status === "ready" ? data : null;
  return (
    <Card aria-busy={data.status === "loading"}>
      <CardHeader className="flex flex-col gap-1">
        <CardTitle>{t("ui.wallet", "Wallet")}</CardTitle>
        {ready?.balance ? (
          <p className="m-0 text-2xl font-semibold tabular-nums text-ink">{formatUsdt(ready.balance.total, locale)} USDT</p>
        ) : data.status === "loading" ? (
          <Skeleton className="h-8 w-40" />
        ) : null}
      </CardHeader>
      <CardContent className="flex flex-col gap-6">
        {data.status === "error" && <ResourceError message={t("walletMfe.unavailable", "Your wallet could not be loaded.")} onRetry={reload} />}
        {ready?.refreshFailed && <ResourceError message={t("walletMfe.stale", "Some figures may be out of date.")} onRetry={reload} />}
        {data.status === "loading" && <Skeleton className="h-24 w-full" />}
        {ready?.balance && <BalanceRows balance={ready.balance} scope={scope} />}
        <WalletActions origin={origin} scope={scope} onAction={onAction} />
        {ready?.history && (
          <section className="flex flex-col gap-2" aria-labelledby={historyId}>
            <h3 id={historyId} className="m-0 text-sm font-medium text-ink-soft">
              {t("walletMfe.history", "Recent activity")}
            </h3>
            <HistoryList history={ready.history} />
          </section>
        )}
      </CardContent>
    </Card>
  );
}
