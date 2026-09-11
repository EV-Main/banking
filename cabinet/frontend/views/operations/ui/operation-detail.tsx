"use client";

import { Link } from "@/shared/ui/cabinet-link";

import type { ReactNode } from "react";

import { Badge, Button, Separator } from "@evinvest/uikit";

import type { Operation } from "@/shared/contracts";
import { cn } from "@/shared/lib/cn";
import { amountTone, dayLabel, dayLabelInline, formatUnits, formatUsdt, kindBadge, kindMeta, networkLabel, seconds, STATE_ICONS, stateLabel, stateTone, timeLabel } from "@/views/operations/lib/format";
import { NetworkMark } from "@/shared/ui/icons/networks";
import type { Locale, Translate } from "@evinvest/i18n";
import { useLocale, useT } from "@evinvest/i18n/react";

// The body of the operation detail panel (Figma `Cabinet · Operations` →
// `popover-operation-detail`). Rendered inside a Popover on the desktop timeline and a
// Drawer on a phone, so it owns no positioning of its own — only the content.
//
// Everything here is derived from the `Operation` row the timeline already holds; the
// panel makes no second request. That constrains what it may claim: the wire carries a
// lifecycle `state` and a `created_at`, so the progress list stops at what those two can
// prove. A per-step timestamp, a "signed by vault" leg, or a confirmation count would all
// have to be invented, and an invented number on a money surface is worse than an absent
// one.
export function OperationDetail({ operation, title, onManage }: { operation: Operation; title: string; onManage?: `/${string}` | null }) {
  const t = useT();
  const locale = useLocale();
  const meta = kindMeta(operation.kind);
  const StateIcon = STATE_ICONS[operation.state ?? ""];
  const at = seconds(operation.created_at);
  const steps = progressFor(operation, t, locale);
  const sub = subheadline(operation, t);

  return (
    <div className="flex flex-col">
      <header className="flex flex-col gap-1 px-4 pb-3 pt-4">
        <div className="flex items-center gap-2">
          {/* Decorative — the panel title beside it names the kind. i18n-max: 4 on the
              text fallback, reached only for a kind with no mark. */}
          <Badge className={cn("font-semibold", meta.tone)}>{meta.icon ? <meta.icon aria-hidden /> : kindBadge(operation.kind)}</Badge>
          <p className="min-w-0 flex-1 truncate text-base font-semibold text-ink">{title}</p>
          {/* No `capitalize`, and the mark doubles the tint — see the notes on the same
              badges in `operations-view`. */}
          <Badge className={stateTone(operation.state)}>
            {StateIcon && <StateIcon aria-hidden />}
            {stateLabel(operation.state, t)}
          </Badge>
        </div>
        <p className="text-xs text-ink-soft">{context(operation, at, t, locale)}</p>
        <p className={cn("pt-1 text-2xl font-semibold tabular-nums", amountTone(meta.direction))}>{headline(operation, t)}</p>
        {sub && <p className="text-xs text-ink-soft">{sub}</p>}
      </header>

      {steps.length > 0 && (
        <>
          <Separator />
          <section className="flex flex-col gap-3 px-4 py-4">
            <h3 className="text-xs font-medium uppercase tracking-wide text-ink-soft">{t("ui.progress")}</h3>
            {steps.map((step, i) => (
              <div key={i} className="flex items-center gap-2.5">
                <span
                  aria-hidden
                  className={cn("size-2.5 shrink-0 rounded-full", step.state === "done" ? "bg-positive" : step.state === "active" ? "bg-accent-warn" : "bg-ink-soft/40")}
                />
                <span className="flex min-w-0 flex-col">
                  <span className={cn("truncate text-sm", step.state === "todo" ? "text-ink-soft" : "font-medium text-ink")}>{step.label}</span>
                  <span className="truncate text-xs text-ink-soft">{step.meta}</span>
                </span>
              </div>
            ))}
          </section>
        </>
      )}

      <Separator />
      <section className="flex flex-col gap-2 px-4 py-4">
        <h3 className="text-xs font-medium uppercase tracking-wide text-ink-soft">{t("ui.details")}</h3>
        {/* The label is fixed and the value wraps, not the other way round. A deposit
            reference is ~50 characters and a TON address 48; letting the value size the
            row pushed it straight through the panel's right edge and over the row above. */}
        {/* Keyed by position, not by the label: the labels are translated now, and two
            locales are free to render two rows with the same word. */}
        {detailsFor(operation, t).map(([label, value], i) => (
          <div key={i} className="flex items-start gap-3">
            <span className="shrink-0 text-xs text-ink-soft">{label}</span>
            <span className="min-w-0 flex-1 break-all text-right text-xs font-medium tabular-nums text-ink">{value}</span>
          </div>
        ))}
      </section>

      {/* Only the two kinds with something still cancellable get a footer; for everything
          else there is no action left, and a footer would be chrome for nothing. */}
      {onManage && (
        <>
          <Separator />
          <div className="flex justify-end px-2 py-2">
            <Button asChild variant="ghost" size="sm">
              <Link href={onManage}>{t("ui.manage")}</Link>
            </Button>
          </div>
        </>
      )}
    </div>
  );
}

interface Step {
  label: string;
  meta: string;
  state: "done" | "active" | "todo";
}

// A deposit row exists only once the watcher confirmed it, and a subscription is an
// immutable mint — neither has a lifecycle to draw, which is also why their panel is a
// third the height of a withdrawal's.
function progressFor(operation: Operation, t: Translate, locale: Locale): Step[] {
  const state = operation.state ?? "";
  const at = seconds(operation.created_at);
  const requested: Step = { label: t("ops.step.requested"), meta: at > 0 ? `${dayLabel(at, t, locale)} ${timeLabel(at, locale)}` : "—", state: "done" };

  if (operation.kind === "withdrawal") {
    if (state === "cancelled" || state === "failed") {
      return [requested, { label: t(state === "cancelled" ? "ops.step.wdCancelled" : "ops.step.wdFailed"), meta: t("ops.step.reservationVoided"), state: "done" }];
    }
    return [
      requested,
      state === "queued"
        ? { label: t("ops.step.awaitingRail"), meta: t("ops.step.awaitingRailMeta"), state: "active" }
        : { label: t("ops.step.broadcast"), meta: t("ops.step.inFlight"), state: "done" },
      state === "completed"
        ? { label: t("ops.step.settledOnChain"), meta: t("ops.step.leftCustody"), state: "done" }
        : { label: t("ops.step.settledOnChain"), meta: t("ops.step.waiting"), state: "todo" },
    ];
  }

  if (operation.kind === "redemption") {
    if (state === "cancelled" || state === "failed") {
      return [requested, { label: t(state === "cancelled" ? "ops.step.redCancelled" : "ops.step.redFailed"), meta: t("ops.step.burnVoided"), state: "done" }];
    }
    return [
      requested,
      state === "queued"
        ? { label: t("ops.step.awaitingFund"), meta: t("ops.step.awaitingFundMeta"), state: "active" }
        : { label: t("ops.step.pricedAtSettle"), meta: operation.nav ? t("ops.step.perUnit", { nav: formatUsdt(operation.nav) }) : "—", state: "done" },
      state === "completed"
        ? { label: t("ops.step.cashPaidOut"), meta: t("ops.step.creditedToBalance"), state: "done" }
        : { label: t("ops.step.cashPaidOut"), meta: t("ops.step.waiting"), state: "todo" },
    ];
  }

  return [];
}

function context(operation: Operation, at: number, t: Translate, locale: Locale): string {
  // `dayLabelInline` rather than `dayLabel(...).toLowerCase()` — see the note on it.
  const when = at > 0 ? `${dayLabelInline(at, t, locale)} ${timeLabel(at, locale)}` : "—";
  if (operation.kind === "deposit" || operation.kind === "withdrawal") return t("ops.context.network", { network: networkLabel(operation.network), when });
  return t("ops.context.fund", { when });
}

function headline(operation: Operation, t: Translate): string {
  // An unsettled redemption has no cash figure at all, so the units it reserved are the
  // only true headline available.
  if (!operation.amount) return t("dash.unitsAmount", { n: Number(operation.units ?? 0), units: formatUnits(operation.units) });
  const { direction } = kindMeta(operation.kind);
  const sign = direction === "in" ? "+" : direction === "out" ? "−" : "";
  return `${sign}${formatUsdt(operation.amount)} USDT`;
}

function subheadline(operation: Operation, t: Translate): string | null {
  if (operation.kind === "withdrawal" && operation.net_amount) return t("ops.detail.netArrives", { amount: formatUsdt(operation.net_amount) });
  if (operation.kind === "redemption" && !operation.amount) return t("ops.detail.pricedAtSettle");
  // A fee moves units between holders on the share ledger — no cash leaves the account and
  // NAV per unit does not move, so nobody else in the fund pays for it either.
  if (operation.kind === "fee") {
    return t(operation.state === "partly_deferred" ? "ops.detail.feeDeferred" : "ops.detail.feeTaken");
  }
  return null;
}

// Inline and badge-free, so the row keeps the shape of every other detail row — the mark
// sits in the value column, not in a tinted square of its own.
//
// Call sites pass a `key`: the element goes into the `rows` array, and `react/jsx-key`
// reads a `push` into an array as a list. The renderer keys the row by position anyway.
function NetworkRow({ network }: { network: string | undefined }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <NetworkMark network={network} className="size-3.5 shrink-0" />
      {networkLabel(network)}
    </span>
  );
}

// A value is a node, not a string, because one of them is not text: the network row
// carries the chain's mark beside its name, the same pairing the wallet uses.
function detailsFor(operation: Operation, t: Translate): [string, ReactNode][] {
  const rows: [string, ReactNode][] = [];
  switch (operation.kind) {
    case "deposit":
      rows.push([t("ops.detail.amountCredited"), `${formatUsdt(operation.amount)} USDT`]);
      rows.push([t("ui.network"), <NetworkRow key="network" network={operation.network} />]);
      if (operation.tx_ref) rows.push([t("ops.detail.reference"), operation.tx_ref]);
      break;
    case "withdrawal":
      rows.push([t("ops.detail.amountDebited"), `${formatUsdt(operation.amount)} USDT`]);
      if (operation.fee) rows.push([t("wallet.networkFee"), `${formatUsdt(operation.fee)} USDT`]);
      if (operation.net_amount) rows.push([t("ops.detail.netSent"), `${formatUsdt(operation.net_amount)} USDT`]);
      rows.push([t("ui.network"), <NetworkRow key="network" network={operation.network} />]);
      if (operation.address) rows.push([t("ops.detail.toAddress"), operation.address]);
      rows.push([t("ops.detail.reference"), operation.tx_ref || t("ops.detail.notYetBroadcast")]);
      break;
    case "subscription":
      rows.push([t("ops.detail.cashIn"), `${formatUsdt(operation.amount)} USDT`]);
      rows.push([t("ops.detail.unitsMinted"), formatUnits(operation.units)]);
      if (operation.nav) rows.push([t("ops.detail.pricePerUnit"), `${formatUsdt(operation.nav)} USDT`]);
      break;
    case "redemption":
      rows.push([t("ops.detail.unitsRedeemed"), formatUnits(operation.units)]);
      rows.push([t("ops.detail.pricePerUnit"), operation.nav ? `${formatUsdt(operation.nav)} USDT` : t("ops.detail.setAtSettle")]);
      rows.push([t("ops.detail.cashOut"), operation.amount ? `${formatUsdt(operation.amount)} USDT` : t("ops.detail.setAtSettle")]);
      break;
    case "fee":
      // The legs first, because they are what the charge WAS; the units are how it was
      // taken. Both legs are listed even at zero here (unlike the timeline row, where
      // space is short) — on a detail panel an explicit "0.00 performance" is the answer
      // to "was I charged for the gain?", not noise.
      rows.push([t("ops.detail.managementFee"), `${formatUsdt(operation.management)} USDT`]);
      rows.push([t("ops.detail.performanceFee"), `${formatUsdt(operation.performance)} USDT`]);
      rows.push([t("admin.fees.col.unitsTaken"), formatUnits(operation.units)]);
      if (operation.nav) rows.push([t("ops.detail.pricePerUnit"), `${formatUsdt(operation.nav)} USDT`]);
      rows.push([t("ops.detail.valueTaken"), `${formatUsdt(operation.amount)} USDT`]);
      break;
    default:
      break;
  }
  return rows;
}
