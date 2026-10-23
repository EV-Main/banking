"use client";

import { ArrowUpRight, X } from "lucide-react";
import { Link } from "@/shared/ui/cabinet-link";
import { type CSSProperties, useState } from "react";

import { Button, Skeleton, Spinner } from "@evinvest/uikit";

import { cancelWithdrawal, depositsResource, withdrawalsResource } from "@/entities/wallet/model/wallet-resource";
import type { Deposit, Withdrawal } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { NetworkMark } from "@/shared/ui/icons/networks";
import { StaggerItem } from "@/shared/ui/motion";
import { STATE_ICONS, stateLabel } from "@/views/operations/lib/format";
import { formatUsdt, networkLabel, railMeta, shortAddress } from "@/views/wallet/lib/format";
import { useFirstDepositSignal } from "@/views/wallet/model/use-first-deposit-signal";
import { WALLET_CARD, WalletScreen } from "@/views/wallet/ui/wallet-chrome";
import type { Locale, Translate } from "@evinvest/i18n";
import { useLocale, useT } from "@evinvest/i18n/react";

// The desktop table track — network, destination, amount, status. Fixed px columns with no
// matching step on the spacing scale, so they ride in as a custom property rather than an
// arbitrary class, and the header row and the body rows stay locked to one definition.
const COLUMNS = { "--activity-columns": "110px 1fr 150px 130px" } as CSSProperties;

const STATUS_STYLES: Record<string, string> = {
  queued: "bg-accent-warn/15 text-accent-warn",
  processing: "bg-accent-debug/15 text-accent-debug",
  completed: "bg-positive/15 text-positive",
  credited: "bg-positive/15 text-positive",
  failed: "bg-accent-error/15 text-accent-error",
  cancelled: "bg-muted text-ink-soft",
};

// One row of the merged feed. Withdrawals carry no timestamp on the wire, so a strict time
// interleave isn't possible: withdrawals keep the hub's newest-first order on top (they hold
// the actionable queued rows), then deposits newest-first.
interface Entry {
  key: string;
  id: string;
  network: string;
  title: string;
  sub: string;
  amount: string;
  /** The wire state — keys the tint, never rendered. */
  state: string;
  /** The same state as the reader sees it, from the operations vocabulary. */
  stateText: string;
  cancellable: boolean;
}

export function ActivityView() {
  const t = useT();
  useFirstDepositSignal();
  const locale = useLocale();
  const [cancelError, setCancelError] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const withdrawals = useResource(withdrawalsResource);
  const deposits = useResource(depositsResource);

  const cancel = async (id: string) => {
    setBusy(id);
    try {
      // Cancelling names the wallet, withdrawals and operations tags, so this list
      // refreshes itself — and so does the balance on any other open surface.
      await cancelWithdrawal(id);
    } catch (e) {
      // The error object, not its English text — `errorMessage` localises it below.
      setCancelError(e);
    } finally {
      setBusy(null);
    }
  };

  const loading = withdrawals.isLoading || deposits.isLoading;
  const rows = withdrawals.data?.withdrawals ?? [];
  const credits = deposits.data?.deposits ?? [];
  // A failed refresh keeps the list it last showed; only a read that never succeeded is
  // worth reporting in its place.
  const failure = cancelError ?? (withdrawals.data ? null : withdrawals.error) ?? (deposits.data ? null : deposits.error) ?? null;
  const error = failure ? errorMessage(failure, t) : null;
  const entries: Entry[] = loading ? [] : buildEntries(rows, credits, t, locale);

  return (
    <WalletScreen title={t("ui.walletHistory", "History")} subtitle={t("wallet.activitySub", "Deposits and withdrawals — queued, processing, completed")} back="/wallet">
      {error && (
        <StaggerItem as="p" className="text-sm text-accent-error">
          {error}
        </StaggerItem>
      )}

      <StaggerItem>
        {loading ? (
          <Skeleton className="h-64 w-full rounded-xl lg:max-w-190" />
        ) : entries.length === 0 ? (
          <EmptyState />
        ) : (
          <div className={cn(WALLET_CARD, "overflow-hidden lg:max-w-190")}>
            <div
              style={COLUMNS}
              className="hidden grid-cols-(--activity-columns) items-center gap-3 border-b border-border px-5 py-3.5 text-xs font-medium text-ink-soft lg:grid"
            >
              {/* Hard pixel columns (110 / 1fr / 150 / 130). i18n-max: 12 on all four. */}
              <span>{t("wallet.networkCaps", "NETWORK")}</span>
              <span>{t("wallet.destinationCaps", "DESTINATION")}</span>
              <span className="text-right">{t("wallet.amountCaps", "AMOUNT")}</span>
              <span className="text-right">{t("wallet.statusCaps", "STATUS")}</span>
            </div>
            {entries.map((entry, i) => (
              <Row key={entry.key} entry={entry} first={i === 0} busy={busy === entry.id} onCancel={() => cancel(entry.id)} />
            ))}
          </div>
        )}
      </StaggerItem>
    </WalletScreen>
  );
}

function Row({ entry, first, busy, onCancel }: { entry: Entry; first: boolean; busy: boolean; onCancel: () => void }) {
  const t = useT();
  const rail = railMeta(entry.network);
  const StateIcon = STATE_ICONS[entry.state];
  return (
    <div style={COLUMNS} className={cn("flex items-center gap-3 px-3.5 py-3 lg:grid lg:grid-cols-(--activity-columns) lg:px-5 lg:py-3.5", !first && "border-t border-border")}>
      <span className="flex items-center gap-2.5 lg:gap-2">
        <span className={cn("flex size-8 shrink-0 items-center justify-center rounded-md text-xs font-semibold lg:size-6", rail.tone)}>
          <NetworkMark network={entry.network} className="size-4 lg:size-3.5" />
        </span>
        <span className="hidden text-sm text-ink lg:inline">{rail.label}</span>
      </span>

      <span className="flex min-w-0 flex-1 flex-col">
        <span className="truncate text-sm font-medium text-ink">{entry.title}</span>
        <span className="truncate text-xs text-ink-soft">
          <span className="lg:hidden">
            {rail.label} · {entry.sub}
          </span>
          <span className="hidden lg:inline">{entry.sub}</span>
        </span>
      </span>

      <span className="flex shrink-0 flex-col items-end gap-1 lg:contents">
        <span className="text-sm font-medium tabular-nums text-ink lg:text-right">{entry.amount}</span>
        <span className="flex items-center justify-end gap-2">
          {/* No `capitalize`: the label is a translated word now, not the lowercase wire
              identifier the class existed to dress up. i18n-max: 12 (130px column).
              The mark doubles the tint — a status must not reach the reader by colour
              alone — and is `aria-hidden` because the word beside it already names it. */}
          <span className={cn("inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium", STATUS_STYLES[entry.state] ?? "bg-muted text-ink-soft")}>
            {StateIcon && <StateIcon className="size-3 shrink-0" aria-hidden />}
            {entry.stateText}
          </span>
          {entry.cancellable && (
            <button
              type="button"
              disabled={busy}
              onClick={onCancel}
              aria-label={t("wallet.cancelWithdrawal", "Cancel withdrawal")}
              className="rounded-md border border-border p-1 text-ink-soft outline-none transition-colors hover:text-ink focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-50"
            >
              {busy ? <Spinner className="size-3" aria-hidden /> : <X className="size-3" />}
            </button>
          )}
        </span>
      </span>
    </div>
  );
}

function EmptyState() {
  const t = useT();
  return (
    <div className={cn(WALLET_CARD, "flex flex-col items-center gap-3 px-6 py-12 text-center lg:max-w-100")}>
      <span className="flex size-11 items-center justify-center rounded-xl bg-primary-ink/15">
        <ArrowUpRight className="size-5 text-primary-ink" />
      </span>
      <p className="text-sm font-semibold text-ink">{t("wallet.noActivity", "No activity yet")}</p>
      <p className="max-w-65 text-xs text-ink-soft">{t("wallet.noActivityHint", "Your deposits and withdrawals will appear here once you make your first transfer.")}</p>
      <Button asChild className="mt-1">
        <Link href="/wallet/deposit">{t("wallet.newDeposit", "New deposit")}</Link>
      </Button>
    </div>
  );
}

function buildEntries(withdrawals: Withdrawal[], deposits: Deposit[], t: Translate, locale: Locale): Entry[] {
  const out: Entry[] = withdrawals.map((w, i) => ({
    key: `w-${w.id ?? i}`,
    id: w.id ?? "",
    network: w.network ?? "",
    title: shortAddress(w.address),
    sub: w.tx_ref ? t("wallet.txRef", "tx {ref}", { ref: shortAddress(w.tx_ref) }) : t("wallet.railUsdt", "{network} · USDT", { network: networkLabel(w.network) }),
    amount: `−${formatUsdt(w.amount, locale)} USDT`,
    state: w.state ?? "queued",
    stateText: stateLabel(w.state ?? "queued", t),
    cancellable: w.state === "queued",
  }));

  const sorted = [...deposits].sort((a, b) => Number(b.created_at ?? 0) - Number(a.created_at ?? 0));
  for (const [i, d] of sorted.entries()) {
    out.push({
      // tx_ref alone can be missing (or repeat for multi-output txs) — suffix with
      // created_at + position so React keys stay unique.
      key: `d-${d.tx_ref ?? ""}-${d.created_at ?? ""}-${i}`,
      id: d.tx_ref ?? "",
      network: d.network ?? "",
      // A row title is a noun ("a deposit"), not the wallet button's verb.
      title: d.tx_ref ? shortAddress(d.tx_ref) : t("ops.kind.deposit", "Deposit"),
      sub: t("wallet.railUsdt", "{network} · USDT", { network: networkLabel(d.network) }),
      amount: `+${formatUsdt(d.amount, locale)} USDT`,
      state: "credited",
      stateText: stateLabel("credited", t),
      cancellable: false,
    });
  }
  return out;
}
