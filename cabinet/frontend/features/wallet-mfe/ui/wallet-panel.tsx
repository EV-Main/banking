import { useLocale, useT } from "@evinvest/i18n/react";

import { useId } from "react";

import { Card, CardContent, CardHeader, CardTitle, ResourceError, Skeleton } from "@evinvest/uikit";

import { cabinetPath } from "@/shared/config/base-path";
import { formatUsdt } from "@/shared/lib/money";

import type { WalletAction } from "../lib/wallet-action";
import { useWalletData } from "../model/use-wallet-data";
import { BalanceRows } from "./balance-rows";
import { HistoryList } from "./history-list";
import { SignedOutCard } from "./signed-out-card";
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

  if (data.status === "signed_out") return <SignedOutCard origin={origin} />;

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
        {/* Stale figures are flagged above them; a read that never arrived is reported in
            its own place below. */}
        {ready?.refreshFailed && ready.balance && ready.history && (
          <ResourceError message={t("walletMfe.stale", "Some figures may be out of date.")} onRetry={reload} />
        )}
        {data.status === "loading" && <Skeleton className="h-24 w-full" />}
        {ready && !ready.balance && <ResourceError message={t("walletMfe.balanceUnavailable", "Your balance could not be loaded.")} onRetry={reload} />}
        {ready?.balance && <BalanceRows balance={ready.balance} scope={scope} />}
        <WalletActions origin={origin} scope={scope} onAction={onAction} />
        {ready && (
          <section className="flex flex-col gap-2" aria-labelledby={historyId}>
            <h3 id={historyId} className="m-0 text-sm font-medium text-ink-soft">
              {t("walletMfe.history", "Recent activity")}
            </h3>
            {ready.history ? (
              <HistoryList history={ready.history} allHref={new URL(cabinetPath(locale, "/wallet/activity"), origin).href} />
            ) : (
              <ResourceError message={t("walletMfe.historyUnavailable", "Your activity could not be loaded.")} onRetry={reload} />
            )}
          </section>
        )}
      </CardContent>
    </Card>
  );
}
