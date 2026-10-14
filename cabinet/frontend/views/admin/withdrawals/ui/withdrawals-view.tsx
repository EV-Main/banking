"use client";

import { Inbox, TriangleAlert } from "lucide-react";
import { useState } from "react";

import { useLocale, useT } from "@evinvest/i18n/react";
import { Badge, Button, Card, CardContent, Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle, Input, Skeleton, Spinner, Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@evinvest/uikit";

import { dispatchWithdrawal, failWithdrawal, settleWithdrawal } from "@/entities/admin/api/admin-client";
import { withdrawalQueueResource } from "@/entities/admin/model/admin-resource";
import type { WithdrawalQueueItem } from "@/shared/contracts/admin";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { TipAnchor } from "@/shared/tips";
import { Settled, StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { networkLabel } from "@/shared/lib/rail";
import { NetworkMark } from "@/shared/ui/icons/networks";
import { ago, formatUsdt, stateLabel } from "@/views/admin/lib/format";
import { EDGE_CELL, TABLE_HEAD } from "@/views/admin/lib/table";
import { AdminHeader, AdminScreen } from "@/views/admin/ui/shell";

// Which confirm panel is open under a row: settle asks for the mined tx ref,
// fail asks for an (optional) audit reason and repeats the double-pay warning.
type Panel = { id: string; kind: "settle" | "fail" };

export function WithdrawalsView() {
  const t = useT();
  const [actionError, setActionError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [panel, setPanel] = useState<Panel | null>(null);
  const [txRef, setTxRef] = useState("");
  const [reason, setReason] = useState("");

  // Cached, so arriving from another console screen shows the queue as it was last read and
  // refreshes it behind. `refresh()` after an action replaces the manual re-fetch, and keeps
  // any other surface reading the same queue in step.
  const read = useResource(withdrawalQueueResource);
  const queue = read.data ? (read.data.items ?? []) : null;
  const error = actionError ?? (read.data || !read.error ? null : errorMessage(read.error, t));

  const run = async (id: string, fn: () => Promise<unknown>) => {
    setBusy(id);
    setActionError(null);
    try {
      await fn();
      setPanel(null);
      setTxRef("");
      setReason("");
      await read.refresh();
    } catch (e) {
      setActionError(errorMessage(e, t));
    } finally {
      setBusy(null);
    }
  };

  const openPanel = (next: Panel) => {
    setPanel((current) => (current && current.id === next.id && current.kind === next.kind ? null : next));
    setTxRef("");
    setReason("");
  };

  return (
    <AdminScreen className="space-y-8">
      <AdminHeader eyebrow={t("admin.eyebrow.administer", "Administer")} title={t("nav.withdrawals", "Withdrawals")} subtitle={t("admin.withdrawals.subtitle", "Dispatch, settle, or fail withdrawals — investors' and payouts from banking's treasury")} />

      {error && <ResourceError message={error} />}

      <StaggerItem as="section" className="space-y-3">
        <p className="flex items-center gap-2 text-xs font-semibold uppercase tracking-widest text-ink-soft">
          {t("admin.withdrawals.awaitingAction", "Awaiting action")}
          {/* The count pill lands on the same step as the label it trails, so its fill and
              accent colour — not a smaller size — are what set it apart. */}
          {queue && (
            <span className="whitespace-nowrap rounded-full bg-accent-warn/15 px-2 py-0.5 text-xs font-semibold text-accent-warn">
              {t("admin.withdrawals.openCount", "{n, plural, one {# open} other {# open}}", { n: queue.length })}
            </span>
          )}
        </p>
        <Card>
          <CardContent className="p-0">
            <Settled
              loading={!queue}
              skeleton={
                <div className="p-6">
                  <Skeleton className="h-32 w-full" />
                </div>
              }
            >
              {!queue ? null : queue.length === 0 ? (
                <div className="p-8">
                  <Empty className="border md:p-6">
                    <EmptyHeader>
                      <EmptyMedia variant="icon">
                        <Inbox />
                      </EmptyMedia>
                      <EmptyTitle>{t("admin.withdrawals.empty", "No withdrawals awaiting action")}</EmptyTitle>
                      <EmptyDescription>{t("admin.withdrawals.emptyHint", "A withdrawal appears here once an investor asks for one.")}</EmptyDescription>
                    </EmptyHeader>
                  </Empty>
                </div>
              ) : (
                <Table>
                  <TableHeader>
                    {/* i18n-max: 14 per header — auto-layout table; the address cell is
                        the one that gives width back before the kit's wrapper scrolls. */}
                    <TableRow>
                      <TableHead className={cn(TABLE_HEAD, EDGE_CELL)}>{t("admin.col.user", "User")}</TableHead>
                      <TableHead className={cn(TABLE_HEAD, EDGE_CELL)}>
                        <span className="flex items-center gap-1.5">
                          {t("ui.destination", "Destination")}
                          <TipAnchor anchor="admin.withdrawals.destination" />
                        </span>
                      </TableHead>
                      <TableHead className={cn(TABLE_HEAD, EDGE_CELL)}>
                        <span className="flex items-center gap-1.5">
                          {t("admin.withdrawals.col.grossNet", "Gross / net")}
                          <TipAnchor anchor="admin.withdrawals.gross-net" />
                        </span>
                      </TableHead>
                      <TableHead className={cn(TABLE_HEAD, EDGE_CELL)}>
                        <span className="flex items-center gap-1.5">
                          {t("admin.col.state", "State")}
                          <TipAnchor anchor="admin.withdrawals.state" />
                        </span>
                      </TableHead>
                      <TableHead className={cn(TABLE_HEAD, EDGE_CELL)}>{t("admin.col.age", "Age")}</TableHead>
                      <TableHead className={cn(TABLE_HEAD, EDGE_CELL, "text-right")}>{t("admin.col.actions", "Actions")}</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {queue.map((item) => {
                      const isBusy = busy === item.withdrawal_id;
                      const open = panel?.id === item.withdrawal_id ? panel : null;
                      return (
                        <WithdrawalRow
                          key={item.withdrawal_id}
                          item={item}
                          busy={isBusy}
                          panel={open}
                          txRef={txRef}
                          reason={reason}
                          onTxRef={setTxRef}
                          onReason={setReason}
                          onOpen={openPanel}
                          onDispatch={() => run(item.withdrawal_id, () => dispatchWithdrawal(item.withdrawal_id))}
                          onSettle={() => run(item.withdrawal_id, () => settleWithdrawal(item.withdrawal_id, txRef))}
                          onFail={() => run(item.withdrawal_id, () => failWithdrawal(item.withdrawal_id, reason))}
                        />
                      );
                    })}
                  </TableBody>
                </Table>
              )}
            </Settled>
          </CardContent>
        </Card>
        <p className="max-w-3xl text-xs text-ink-soft">{t("admin.withdrawals.footnote", "Dispatch broadcasts a queued withdrawal once its rail has liquidity. Settle records the mined transaction and releases the reservation. Fail voids and refunds — ONLY safe when nothing reached the chain; the hub refuses it while a broadcast record exists.")}</p>
      </StaggerItem>
    </AdminScreen>
  );
}

function WithdrawalRow({
  item,
  busy,
  panel,
  txRef,
  reason,
  onTxRef,
  onReason,
  onOpen,
  onDispatch,
  onSettle,
  onFail,
}: {
  item: WithdrawalQueueItem;
  busy: boolean;
  panel: Panel | null;
  txRef: string;
  reason: string;
  onTxRef: (v: string) => void;
  onReason: (v: string) => void;
  onOpen: (p: Panel) => void;
  onDispatch: () => void;
  onSettle: () => void;
  onFail: () => void;
}) {
  const t = useT();
  const locale = useLocale();
  const queued = item.state === "queued";
  return (
    <>
      <TableRow>
        <TableCell className={EDGE_CELL}>
          {/* A treasury payout has no investor behind it — banking is paying out of its
              own treasury (D17), on the owners' approval. Naming that beats rendering a
              blank User cell, and it tells the operator whose money the dispatch/settle
              below is about to move. */}
          {item.source === "treasury" ? (
            <Badge variant="success">{t("admin.withdrawals.source.treasury", "Banking treasury")}</Badge>
          ) : (
            <p className="font-medium">{item.email || item.user_id.slice(0, 8)}</p>
          )}
          <p className="font-mono-tech text-xs text-ink-soft">{item.withdrawal_id.slice(0, 8)}</p>
        </TableCell>
        <TableCell className={EDGE_CELL}>
          {/* Was the bare wire id under a `uppercase` class. The rail's short mark comes
              from `@/shared/lib/rail` — the same one the investor cabinet renders — so a
              chain is not named two different ways in two consoles, and `Polygon` keeps
              the casing it is actually written with. The console's long prose form
              (`railLabel`, "BEP20 · BNB Chain") is for cards, not this dense cell. */}
          <p className="flex items-center gap-1.5 text-xs text-ink-soft">
            <NetworkMark network={item.network} className="size-3.5 shrink-0" />
            {networkLabel(item.network)}
          </p>
          <p className="font-mono-tech text-xs" title={item.address}>
            {shortAddr(item.address)}
          </p>
        </TableCell>
        <TableCell className={cn(EDGE_CELL, "tabular-nums")}>
          <p>{formatUsdt(item.amount, locale)} USDT</p>
          <p className="text-xs text-ink-soft">{t("admin.withdrawals.netSuffix", "{amount} net", { amount: formatUsdt(item.net_amount, locale) })}</p>
        </TableCell>
        <TableCell className={EDGE_CELL}>
          <span className={queued ? "text-accent-warn" : "text-positive"}>{stateLabel(item.state, t)}</span>
        </TableCell>
        <TableCell className={cn(EDGE_CELL, "text-ink-soft")}>{ago(item.created_at, t)}</TableCell>
        <TableCell className={EDGE_CELL}>
          {/* i18n-max: 12 per verb — up to two `shrink-0` Buttons share this cell. */}
          <div className="flex justify-end gap-2">
            {queued ? (
              <Button type="button" variant="outline" size="sm" disabled={busy} onClick={onDispatch}>
                {busy ? <Spinner aria-hidden /> : null}
                {t("admin.dispatch", "Dispatch")}
              </Button>
            ) : (
              <>
                <Button type="button" variant="outline" size="sm" disabled={busy} onClick={() => onOpen({ id: item.withdrawal_id, kind: "settle" })}>
                  {t("admin.settle", "Settle")}
                </Button>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  className="border-accent-error/40 text-accent-error hover:bg-accent-error/10"
                  disabled={busy}
                  onClick={() => onOpen({ id: item.withdrawal_id, kind: "fail" })}
                >
                  {t("admin.fail", "Fail")}
                </Button>
              </>
            )}
          </div>
        </TableCell>
      </TableRow>
      {panel && (
        <TableRow data-state="selected">
          <TableCell colSpan={6} className={cn(EDGE_CELL, "whitespace-normal")}>
            {panel.kind === "settle" ? (
              <div className="flex items-center gap-3">
                <Input
                  value={txRef}
                  onChange={(e) => onTxRef(e.target.value)}
                  placeholder={t("admin.withdrawals.placeholder.txHash", "Mined transaction hash (0x…)")}
                  spellCheck={false}
                  className="max-w-xl font-mono-tech text-xs"
                />
                <TipAnchor anchor="admin.withdrawals.settle.tx-hash" />
                <Button type="button" size="sm" disabled={busy || !txRef.trim()} onClick={onSettle}>
                  {busy ? <Spinner aria-hidden /> : null}
                  {t("admin.withdrawals.confirmSettle", "Confirm settle")}
                </Button>
              </div>
            ) : (
              <div className="space-y-2">
                <p className="flex items-center gap-2 text-xs text-accent-error">
                  <TriangleAlert className="size-4" /> {t("admin.withdrawals.failWarning", "Failing refunds the user. If the broadcast reached the chain this would double-pay — the hub refuses while a broadcast record exists, but verify on-chain first.")}
                </p>
                <div className="flex items-center gap-3">
                  <Input value={reason} onChange={(e) => onReason(e.target.value)} placeholder={t("admin.withdrawals.placeholder.reason", "Reason (audit note, optional)")} className="max-w-xl text-xs" />
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    className="border-accent-error/40 text-accent-error hover:bg-accent-error/10"
                    disabled={busy}
                    onClick={onFail}
                  >
                    {busy ? <Spinner aria-hidden /> : null}
                    {t("admin.withdrawals.confirmFail", "Confirm fail")}
                  </Button>

                </div>
              </div>
            )}
          </TableCell>
        </TableRow>
      )}
    </>
  );
}

function shortAddr(address: string): string {
  return address.length > 18 ? `${address.slice(0, 8)}…${address.slice(-6)}` : address;
}
