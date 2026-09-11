"use client";

import { ArrowLeftRight, LineChart, type LucideIcon, PieChart, TrendingDown, TrendingUp } from "lucide-react";
import type { Translate } from "@evinvest/i18n";
import { useT } from "@evinvest/i18n/react";
import { Link } from "@/shared/ui/cabinet-link";
import { type CSSProperties, Fragment, useState } from "react";

import { Badge, Button, Card, CardAction, CardContent, CardHeader, CardTitle, Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle, Item, ItemActions, ItemContent, ItemDescription, ItemGroup, ItemMedia, ItemSeparator, ItemTitle, Progress, Separator, Skeleton, Switch } from "@evinvest/uikit";

import { allocationsResource, positionsResource } from "@/entities/fund/model/fund-resource";
import { RECENT_OPS, operationsResource } from "@/entities/operation/model/operation-resource";
import { walletResource } from "@/entities/wallet/model/wallet-resource";
import type { Operation } from "@/shared/contracts";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { AnimatedNumber, SECTION_STAGGER, Settled, Stagger, StaggerItem } from "@/shared/ui/motion";
import { TipAnchor, type TipKey } from "@/shared/tips";
import { DASH_ADDRESS, formatPct, formatSignedUsd, formatUsd, num, shortAddress } from "@/views/dashboard/lib/format";
import { amountTone, kindBadge, kindLabel, kindMeta, networkLabel, stateLabel } from "@/views/operations/lib/format";

// The card is a preview, not the record — `/operations` holds the full timeline. Asked
// of the hub rather than sliced client-side, so the six shown are the six most recent
// across all four kinds, not the newest six of whatever happened to be fetched. The count
// lives with the resource because the shell's warm-up has to ask for the same one.

// The four ranges. `1M`/`6M`/`1Y` are near-universal abbreviations but still travel as
// keys — a locale that spells its months differently should be able to say so.
// i18n-max: 4 — four equal columns of a grid segmented control on mobile.
const RANGES = ["1m", "6m", "1y", "all"] as const;
const RANGE_LABEL_KEYS: Readonly<Record<(typeof RANGES)[number], string>> = {
  "1m": "dash.range.1m",
  "6m": "dash.range.6m",
  "1y": "dash.range.1y",
  all: "dash.range.all",
};

// Allocation slices cycle the brand accent tiers. The bar names its tier twice because
// Progress paints track and indicator from `--primary`, and the child selector is the only
// way to reach the indicator without forking the component.
const ACCENTS = [
  { dot: "bg-accent-debug", bar: "bg-accent-debug/20 *:bg-accent-debug" },
  { dot: "bg-positive", bar: "bg-positive/20 *:bg-positive" },
  { dot: "bg-accent-warn", bar: "bg-accent-warn/20 *:bg-accent-warn" },
  { dot: "bg-accent-info", bar: "bg-accent-info/20 *:bg-accent-info" },
] as const;

type Accent = (typeof ACCENTS)[number];

// Cards inset 16px on mobile (Figma `cabinet/mobile/home`), the uikit 24px from `lg`.
const CARD_PAD = "px-4 lg:px-6";

// Module scope on purpose: an inline `(n) => String(n)` would be a new function
// every render, and AnimatedNumber restarts its count when `format` changes.
const formatCount = (n: number) => String(Math.round(n));

// Mobile reads the hero and the stat strip as page content rather than as cards: those two
// surfaces sit flat on the background and only take their Card chrome from `lg`.
const CARD_FROM_LG = "rounded-none border-0 bg-transparent py-0 shadow-none lg:rounded-xl lg:border lg:bg-card lg:py-5 lg:shadow-sm";

// uikit's Empty draws a dashed frame but leaves the border width to the caller, and doubles
// its padding at `md`; these sit inside cards, not on a page of their own.
const EMPTY_BOX = "border md:p-6";

// The portfolio dashboard (Figma `cabinet/home`). Bound to live wallet + fund-position
// data; figures with no backing series yet (the performance chart) are honest empty states
// rather than fabricated numbers.
export function DashboardView() {
  const t = useT();
  // All four reads are shared with other screens and cached, so a return to Home paints the
  // balance, the holdings and the timeline on the first frame — the skeletons below are for
  // the cold first load only. The catalog is the same registry the rail lists products from:
  // a fund row should name the product, not the slug that keys it.
  const wallet = useResource(walletResource);
  const positions = useResource(positionsResource);
  const operations = useResource(operationsResource, RECENT_OPS);
  const catalog = useResource(allocationsResource).data?.allocations ?? [];

  const balance = wallet.data?.balance;
  const pos = positions.data?.positions ?? [];
  const pnlSum = pos.reduce((s, p) => s + num(p.pnl), 0);
  const netContributed = pos.reduce((s, p) => s + num(p.cost_basis), 0);
  const allTimePct = netContributed > 0 ? (pnlSum / netContributed) * 100 : null;
  const walletLoading = wallet.isLoading;
  const posLoading = positions.isLoading;

  const allocations = pos.map((p, i) => ({ name: p.service ?? t("dash.fundFallback"), value: num(p.value), accent: ACCENTS[i % ACCENTS.length]! }));
  const allocTotal = allocations.reduce((s, a) => s + a.value, 0) || 1;

  const titleOf = (service: string | undefined) => (service ? (catalog.find((a) => a.service === service)?.title ?? service) : t("dash.fundFallback"));
  // The hub honours `limit`, so the slice is only a shape guarantee for the card.
  const ops = (operations.data?.operations ?? []).slice(0, RECENT_OPS).map((operation, i) => toOp(operation, i, titleOf, t));

  return (
    // One DOM order, two layouts. Mobile stacks in reading order (hero → figures →
    // what I own → move money → activity); from `xl` the same children are placed
    // explicitly on a two-column grid so the desktop composition is unchanged. The
    // sidebar track is a fixed 360px with no matching step on the spacing scale, so it
    // rides in as a custom property instead of an arbitrary class.
    //
    // The grid is also the entrance: `Stagger` renders this same element, and each
    // section below is a `StaggerItem` rendered as the element it already was, so the
    // placement classes stay on the grid items that carry them. The sequence follows
    // DOM order — which on mobile is reading order, and on desktop is close enough
    // that no section arrives before the one above it.
    <Stagger
      step={SECTION_STAGGER}
      className="grid grid-cols-1 gap-4 px-4 pb-6 pt-5 lg:gap-6 lg:px-8 lg:pb-7 lg:pt-6 xl:grid-cols-(--dash-columns) xl:items-start"
      style={{ "--dash-columns": "minmax(0, 1fr) 360px" } as CSSProperties}
    >
      {/* topbar — desktop only; on mobile the shell app bar plus the hero label carry the page */}
      <StaggerItem className="hidden items-center justify-between gap-4 lg:flex xl:col-span-2 xl:row-start-1">
        <div className="flex min-w-0 flex-col gap-1">
          <h1 className="text-2xl font-semibold leading-tight text-ink">{t("dash.portfolio")}</h1>
          <p className="text-sm text-ink-soft">{t("dash.portfolioSub")}</p>
        </div>
        {/* Shortcuts to the same two actions the Move money card offers, so they stay
            outline: one solid accent per screen, and that one belongs to the card that
            explains what it does. Two filled teal CTAs for the same destination read as
            loud rather than emphatic. */}
        <div className="flex shrink-0 gap-2.5">
          <Button asChild variant="outline">
            <Link href="/wallet/withdraw">{t("ui.withdraw")}</Link>
          </Button>
          <Button asChild variant="outline">
            <Link href="/wallet/deposit">{t("ui.deposit")}</Link>
          </Button>
        </div>
      </StaggerItem>

      <PerfCard value={balance?.total} loading={walletLoading} allTimePct={allTimePct} className="lg:order-1 xl:col-start-1 xl:row-span-2 xl:row-start-2" />

      {/* stat strip — a 2×2 card grid on mobile, one divided strip from `lg` */}
      <StaggerItem as={Card} className={cn("grid grid-cols-2 gap-3 lg:flex lg:flex-row lg:flex-wrap lg:items-stretch lg:gap-x-7 lg:gap-y-4 lg:px-6", CARD_FROM_LG, "lg:order-4 xl:col-span-2 xl:col-start-1 xl:row-start-4")}>
        <Stat label={t("dash.unrealizedPnl")} value={walletLoading || posLoading ? null : pnlSum} format={formatSignedUsd} tone={pnlSum < 0 ? "loss" : "gain"} hint={t("dash.hintAcrossPositions")} tip="dashboard.stats.unrealized-pnl" />
        <Separator orientation="vertical" className="hidden self-stretch lg:block" />
        <Stat label={t("dash.available")} value={walletLoading ? null : num(balance?.available)} format={formatUsd} hint={t("dash.hintAutoDeploysEod")} tip="dashboard.stats.available" />
        <Separator orientation="vertical" className="hidden self-stretch lg:block" />
        <Stat label={t("dash.activeStrategies")} value={posLoading ? null : pos.length} format={formatCount} hint={t("dash.hintFundPositions")} />
        <Separator orientation="vertical" className="hidden self-stretch lg:block" />
        <Stat label={t("dash.netContributed")} value={posLoading ? null : netContributed} format={formatUsd} hint={t("dash.hintAtCostBasis")} tip="dashboard.stats.net-invested" />
      </StaggerItem>

      {/* Below `xl` the DOM order is the mobile order; `lg:order-*` restores the desktop
          sequence for the single-column band between `lg` and `xl`. */}
      <WhatIOwn allocations={allocations} total={allocTotal} loading={posLoading} className="lg:order-3 xl:col-start-2 xl:row-start-3" />
      <MoveMoney className="lg:order-2 xl:col-start-2 xl:row-start-2" />

      {/* operations */}
      <StaggerItem as={Card} className="gap-3 py-4 lg:order-5 lg:gap-4 lg:py-5 xl:col-span-2 xl:col-start-1 xl:row-start-5">
        <CardHeader className={CARD_PAD}>
          <CardTitle>{t("dash.recentOperations")}</CardTitle>
          <CardAction>
            <Button asChild variant="link" size="sm" className="px-0">
              <Link href="/operations">{t("ui.viewAll")}</Link>
            </Button>
          </CardAction>
        </CardHeader>
        <CardContent className={CARD_PAD}>
          {ops.length === 0 ? (
            <Empty className={EMPTY_BOX}>
              <EmptyHeader>
                <EmptyMedia variant="icon">
                  <ArrowLeftRight />
                </EmptyMedia>
                <EmptyTitle>{t("ui.noOperations")}</EmptyTitle>
                <EmptyDescription>{t("dash.noOperationsHint")}</EmptyDescription>
              </EmptyHeader>
              <EmptyContent>
                {/* Same destination as the Move money card's filled Deposit, which is
                    already on screen — so this one stays outline. */}
                <Button asChild variant="outline">
                  <Link href="/wallet/deposit">{t("ui.addFunds")}</Link>
                </Button>
              </EmptyContent>
            </Empty>
          ) : (
            <ItemGroup>
              {ops.map((op, i) => (
                <Fragment key={op.id}>
                  {i > 0 && <ItemSeparator />}
                  <Item size="sm" className="px-0 py-3 lg:py-4">
                    <ItemMedia>
                      {/* Decorative: the row title names the kind in words right beside it. */}
                      <Badge className={cn("font-semibold", op.tagClass)}>{op.icon ? <op.icon aria-hidden /> : op.tag}</Badge>
                    </ItemMedia>
                    <ItemContent className="min-w-0 gap-0.5">
                      <ItemTitle className="block w-auto truncate font-semibold">{op.title}</ItemTitle>
                      <ItemDescription className="line-clamp-1 text-xs">{op.sub}</ItemDescription>
                    </ItemContent>
                    <ItemActions className={cn("shrink-0 text-sm font-semibold tabular-nums", op.amountClass)}>{op.amount}</ItemActions>
                  </Item>
                </Fragment>
              ))}
            </ItemGroup>
          )}
        </CardContent>
      </StaggerItem>
    </Stagger>
  );
}

// Mobile (Figma `cabinet/mobile/home`) reads the hero as page content, not as a card: the
// value sits flat on the background, the range switch spans the width below it, and only the
// plot is boxed — with its legend above. From `lg` the whole block is the desktop card again.
function PerfCard({ value, loading, allTimePct, className }: { value: string | undefined; loading: boolean; allTimePct: number | null; className?: string }) {
  const t = useT();
  const [range, setRange] = useState<(typeof RANGES)[number]>("all");
  const down = (allTimePct ?? 0) < 0;
  return (
    // From `xl` the hero spans both rows of the side column, so it has to fill that
    // area — otherwise the plot area keeps its natural height and leaves a gap under
    // the card whenever the side column is the taller of the two.
    //
    // A `StaggerItem` rather than a wrapped `Card` because that `xl:h-full` — and the
    // row-span it fills — are the parent's business, and a wrapper would take them
    // from the card and keep them for itself.
    <StaggerItem as={Card} className={cn("flex-1 gap-4 lg:gap-5 xl:h-full", CARD_FROM_LG, className)}>
      <div className="flex flex-col gap-3.5 lg:flex-row lg:items-start lg:justify-between lg:gap-4 lg:px-6">
        <div className="flex min-w-0 flex-col gap-2">
          <p className="flex items-center gap-1.5 text-xs font-medium uppercase tracking-wider text-accent-debug">
            {t("dash.portfolioValue")}
            <TipAnchor anchor="dashboard.performance.portfolio-value" />
          </p>
          <div className="flex flex-col items-start gap-2.5 lg:flex-row lg:items-center lg:gap-3.5">
            {loading ? <Skeleton className="h-10 w-40 lg:h-12 lg:w-48" /> : <p className="text-4xl font-semibold leading-none tabular-nums lg:text-5xl"><AnimatedNumber value={num(value)} format={formatUsd} /></p>}
            {allTimePct !== null && (
              <Badge variant="outline" className={cn("gap-1 rounded-full tabular-nums", down ? "border-accent-error/40 text-accent-error" : "border-accent-warn/40 text-accent-warn")}>
                {down ? <TrendingDown /> : <TrendingUp />}
                {t("dash.allTimeSuffix", { pct: formatPct(allTimePct) })}
                <TipAnchor anchor="dashboard.performance.all-time-return" />
              </Badge>
            )}
          </div>
        </div>
        {/* Hand-written segmented control — uikit has no equivalent, so it carries its own focus ring. */}
        <div className="grid shrink-0 grid-cols-4 gap-0.5 rounded-lg border border-border bg-secondary p-1 lg:flex">
          {RANGES.map((r) => (
            <button
              key={r}
              type="button"
              aria-pressed={r === range}
              onClick={() => setRange(r)}
              className={cn(
                "rounded-md py-2 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring lg:px-3 lg:py-1.5 lg:text-xs",
                r === range ? "bg-accent-debug/15 font-semibold text-accent-debug" : "font-medium text-ink-soft hover:text-ink",
              )}
            >
              {t(RANGE_LABEL_KEYS[r])}
            </button>
          ))}
        </div>
      </div>
      <CardContent className="flex flex-col gap-3 px-0 lg:gap-5 lg:px-6 xl:flex-1">
        <div className="flex flex-wrap gap-x-4 gap-y-1.5 lg:order-2">
          <Legend dot="bg-accent-warn" label={t("dash.fundPerformance")} />
          <Legend dot="bg-positive" label={t("dash.yourParticipation")} />
        </div>
        {/* No performance series exists in `shared/contracts` yet, so the plot area says so
            rather than drawing a line that traces back to nothing. */}
        <Empty className={cn(EMPTY_BOX, "min-h-40 lg:order-1 lg:min-h-56")}>
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <LineChart />
            </EmptyMedia>
            <EmptyTitle>{t("dash.noHistory")}</EmptyTitle>
            <EmptyDescription>{t("dash.noHistoryHint")}</EmptyDescription>
          </EmptyHeader>
        </Empty>
      </CardContent>
    </StaggerItem>
  );
}

function Legend({ dot, label }: { dot: string; label: string }) {
  return (
    <span className="flex items-center gap-2 text-xs font-medium text-ink-soft">
      <span className={cn("size-2 rounded-full", dot)} />
      {label}
    </span>
  );
}

function MoveMoney({ className }: { className?: string }) {
  const t = useT();
  const [auto, setAuto] = useState(true);
  return (
    <StaggerItem as={Card} className={cn("gap-3.5 py-4 lg:gap-4 lg:py-5", className)}>
      <CardHeader className={CARD_PAD}>
        <CardTitle>{t("dash.moveMoney")}</CardTitle>
      </CardHeader>
      <CardContent className={cn("flex flex-col gap-3.5 lg:gap-4", CARD_PAD)}>
        <div className="flex gap-2.5">
          <Button asChild className="flex-1">
            <Link href="/wallet/deposit">{t("ui.deposit")}</Link>
          </Button>
          <Button asChild variant="outline" className="flex-1">
            <Link href="/wallet/withdraw">{t("ui.withdraw")}</Link>
          </Button>
        </div>
        <Item variant="outline" size="sm" className="rounded-lg bg-secondary">
          <ItemContent className="gap-0.5">
            <ItemTitle id="auto-deploy-label" className="gap-1.5">
              {t("dash.autoDeploy")}
              <TipAnchor anchor="dashboard.move-money.auto-deploy" />
            </ItemTitle>
            <ItemDescription className="text-xs">{t("dash.autoDeployHint")}</ItemDescription>
          </ItemContent>
          <ItemActions>
            <Switch checked={auto} onCheckedChange={setAuto} aria-labelledby="auto-deploy-label" />
          </ItemActions>
        </Item>
      </CardContent>
    </StaggerItem>
  );
}

function WhatIOwn({ allocations, total, loading, className }: { allocations: { name: string; value: number; accent: Accent }[]; total: number; loading: boolean; className?: string }) {
  const t = useT();
  return (
    <StaggerItem as={Card} className={cn("gap-3.5 py-4 lg:gap-4 lg:py-5", className)}>
      <CardHeader className={CARD_PAD}>
        <CardTitle className="flex items-center gap-1.5">
          {t("dash.investedWhatIOwn")}
          <TipAnchor anchor="dashboard.invested.allocation" />
        </CardTitle>
        <CardAction className="text-xs font-medium tabular-nums text-ink-soft">{t("dash.strategyCount", { n: allocations.length })}</CardAction>
      </CardHeader>
      <CardContent className={CARD_PAD}>
        <Settled loading={loading} skeleton={<Skeleton className="h-24 w-full" />}>
          {loading ? null : allocations.length === 0 ? (
            <Empty className={EMPTY_BOX}>
              <EmptyHeader>
                <EmptyMedia variant="icon">
                  <PieChart />
                </EmptyMedia>
                <EmptyTitle>{t("dash.nothingInvested")}</EmptyTitle>
                <EmptyDescription>{t("dash.nothingInvestedHint")}</EmptyDescription>
              </EmptyHeader>
              <EmptyContent>
                <Button asChild variant="outline">
                  <Link href="/invest">{t("dash.browseStrategies")}</Link>
                </Button>
              </EmptyContent>
            </Empty>
          ) : (
            <div className="flex flex-col gap-4">
              {allocations.map((a, i) => {
                const pct = Math.round((a.value / total) * 100);
                return (
                  <div key={`${a.name}-${i}`} className="flex flex-col gap-2">
                    <div className="flex items-center">
                      <span className="flex flex-1 items-center gap-2">
                        <span className={cn("size-2.5 rounded-full", a.accent.dot)} />
                        <span className="truncate text-sm font-medium text-ink-soft">{a.name}</span>
                      </span>
                      <span className="text-sm font-semibold tabular-nums text-ink">{pct}%</span>
                    </div>
                    <Progress value={pct} className={cn("h-1.5", a.accent.bar)} />
                  </div>
                );
              })}
            </div>
          )}
        </Settled>
      </CardContent>
    </StaggerItem>
  );
}

// Takes the figure and its formatter rather than a finished string: a string can
// only be swapped, and swapping is the thing AnimatedNumber exists to replace.
// `format` has to be a stable reference (all of these are module functions from
// shared/lib/money) or the count restarts on every parent render.
function Stat({ label, value, format, tone, hint, tip }: { label: string; value: number | null; format: (n: number) => string; tone?: "gain" | "loss"; hint: string; tip?: TipKey }) {
  const valueClass = tone === "gain" ? "text-positive" : tone === "loss" ? "text-accent-error" : "text-ink";
  const hintClass = tone === "gain" ? "text-positive/80" : tone === "loss" ? "text-accent-error/80" : "text-ink-soft";
  return (
    // Its own tile on mobile, a cell of the shared strip from `lg`.
    <Card className="min-w-0 flex-1 gap-1 px-3.5 py-3 lg:min-w-30 lg:gap-1.5 lg:rounded-none lg:border-0 lg:bg-transparent lg:p-0 lg:shadow-none">
      <div className="flex items-center gap-1.5">
        <p className="truncate text-xs font-medium text-ink-soft">{label}</p>
        {tip && <TipAnchor anchor={tip} />}
      </div>
      {value === null ? <Skeleton className="h-6 w-20" /> : <p className={cn("truncate text-xl font-semibold tabular-nums lg:text-2xl", valueClass)}><AnimatedNumber value={value} format={format} /></p>}
      <p className={cn("truncate text-xs font-medium", hintClass)}>{hint}</p>
    </Card>
  );
}

interface Op {
  id: string;
  /** The kind's mark; `null` for a kind this build has no mark for, which falls back to
   *  {@link tag}. Same rule as the operations timeline — the vocabulary is shared. */
  icon: LucideIcon | null;
  /** The text a mark-less kind wears instead. */
  tag: string;
  tagClass: string;
  title: string;
  sub: string;
  amount: string;
  amountClass: string;
}

// One timeline row rendered in the dashboard's summary-money policy (`formatUsd`, to the
// cent with a currency symbol) rather than the ledger policy the Operations page uses —
// same data, different unit of measure for the surface it sits on. The badge, tone and
// sign vocabulary is shared with `/operations` so a row reads identically in both places.
function toOp(operation: Operation, index: number, titleOf: (service: string | undefined) => string, t: Translate): Op {
  const meta = kindMeta(operation.kind);
  const sign = meta.direction === "in" ? "+" : meta.direction === "out" ? "\u2212" : "";
  return {
    id: `${operation.kind ?? ""}-${operation.id ?? ""}-${index}`,
    icon: meta.icon,
    tag: kindBadge(operation.kind),
    tagClass: meta.tone,
    title: opTitle(operation, titleOf, t),
    sub: opSub(operation, t),
    // A queued redemption is not yet priced, so it shows the units it reserved — a
    // formatted zero would claim the user was paid nothing.
    amount: operation.amount ? `${sign}${formatUsd(operation.amount)}` : t("dash.unitsAmount", { n: Number(operation.units ?? 0), units: operation.units ?? "0" }),
    amountClass: operation.amount ? amountTone(meta.direction) : "text-ink-soft",
  };
}

function opTitle(operation: Operation, titleOf: (service: string | undefined) => string, t: Translate): string {
  if (operation.kind === "subscription") return t("dash.op.subscribed", { fund: titleOf(operation.service) });
  if (operation.kind === "redemption") return t("dash.op.redeemed", { fund: titleOf(operation.service) });
  if (operation.kind === "withdrawal") return t("dash.op.withdrawal", { network: networkLabel(operation.network) });
  if (operation.kind === "deposit") return t("dash.op.deposit", { network: networkLabel(operation.network) });
  return kindLabel(operation.kind, t);
}

function opSub(operation: Operation, t: Translate): string {
  // The lifecycle state reaches the reader through the same vocabulary the operations
  // timeline uses \u2014 it used to be the bare wire identifier, English by construction.
  const state = stateLabel(operation.state, t);
  if (operation.kind === "withdrawal") return t("dash.op.sub", { ref: shortAddress(operation.address, DASH_ADDRESS), state });
  if (operation.kind === "deposit") return t("dash.op.sub", { ref: shortAddress(operation.tx_ref, DASH_ADDRESS), state });
  return state;
}
