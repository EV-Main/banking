"use client";

import type { Locale, Translate } from "@evinvest/i18n";
import { useLocale, useT } from "@evinvest/i18n/react";
import { ArrowLeftRight, ChevronRight, ListChecks } from "lucide-react";
import { Link } from "@/shared/ui/cabinet-link";
import { Fragment, useMemo, useState } from "react";

import {
  ButtonGroup,
  Badge,
  Button,
  Card,
  CardContent,
  CardHeader,
  CardTitle,
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
  Item,
  ItemActions,
  ItemContent,
  ItemDescription,
  ItemGroup,
  ItemMedia,
  ItemSeparator,
  ItemTitle,
  Drawer,
  DrawerContent,
  DrawerTitle,
  DrawerTrigger,
  Popover,
  PopoverContent,
  PopoverTrigger,
  Skeleton,
} from "@evinvest/uikit";

import { allocationsResource } from "@/entities/fund/model/fund-resource";
import { operationsResource } from "@/entities/operation/model/operation-resource";
import { useIsCompact } from "@/views/operations/lib/use-is-compact";
import { OperationDetail } from "@/views/operations/ui/operation-detail";
import type { Operation } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { SECTION_STAGGER, Settled, Stagger, StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import {
  amountTone,
  dayLabel,
  formatUnits,
  formatUsdt,
  isPending,
  KIND_FILTERS,
  kindBadge,
  kindLabel,
  kindMeta,
  networkLabel,
  seconds,
  shortAddress,
  STATE_ICONS,
  stateLabel,
  stateTone,
  timeLabel,
} from "@/views/operations/lib/format";

// uikit's Empty draws a dashed frame but leaves the border width to the caller, and
// doubles its padding at `md`; these sit inside cards, not on a page of their own.
const EMPTY_BOX = "border md:p-6";

type Filter = "all" | (typeof KIND_FILTERS)[number];

const FILTERS: readonly Filter[] = ["all", ...KIND_FILTERS];

// The bar's labels are the *plural* of each kind, and used to be built as `${label}s` —
// which is English pluralisation hard-coded into the layout. Every locale gets its own
// entry instead; the singular forms in `ops.kind.*` are a different word in most of them.
const FILTER_LABEL_KEYS: Readonly<Record<Filter, string>> = {
  all: "ui.all",
  deposit: "ops.filter.deposits",
  withdrawal: "ops.filter.withdrawals",
  subscription: "ops.filter.subscriptions",
  redemption: "ops.filter.redemptions",
  fee: "ops.filter.fees",
};

// "No deposits yet" — again a whole sentence per kind rather than `No ${label.toLowerCase()}s`.
// Lower-casing a translated noun is wrong in German, and the genitive a Russian sentence
// needs here is not the nominative the filter button shows.
const FILTER_EMPTY_KEYS: Readonly<Record<Filter, string>> = {
  all: "ui.noOperations",
  deposit: "ops.empty.deposits",
  withdrawal: "ops.empty.withdrawals",
  subscription: "ops.empty.subscriptions",
  redemption: "ops.empty.redemptions",
  fee: "ops.empty.fees",
};

// The activity timeline (Figma `ios/operations` · `android/operations`, and the desktop
// "Recent operations" card's `View all`). Every money movement the user made, in one
// time-ordered feed, served pre-merged by the hub's `ListOperations` read model — the
// four sources have their own list endpoints, but only the hub can interleave them by
// time and still apply one page limit across the result.
//
// Every row opens a detail panel (a Popover here, a Drawer below `lg`) built from the row
// the timeline already holds — no second request. The panel still performs no mutation of
// its own: cancelling a queued withdrawal or redemption belongs to the surfaces that own
// those aggregates, so the panel links to them rather than growing a third copy.
export function OperationsView() {
  const t = useT();
  const locale = useLocale();
  const [filter, setFilter] = useState<Filter>("all");

  // Both reads are cached and both are shared with Home's activity card, so arriving from
  // "View all" shows the timeline already merged rather than re-fetching it.
  const timeline = useResource(operationsResource, undefined);
  // Fund slugs are keys, not names. The catalog turns them into the product the investor
  // actually bought; a failed lookup degrades to the slug rather than blanking the row.
  const catalog = useResource(allocationsResource).data?.allocations;

  const titleOf = useMemo(() => {
    const byService = new Map((catalog ?? []).map((a) => [a.service, a.title]));
    return (service: string | undefined) => (service ? (byService.get(service) ?? service) : t("dash.fundFallback"));
  }, [catalog, t]);

  const loading = timeline.isLoading;
  const truncated = timeline.data?.truncated ?? false;
  const error = timeline.data ? null : timeline.error ? errorMessage(timeline.error, t) : null;
  const all = timeline.data?.operations ?? [];
  // The filter applies to both bands. Lifting a pending row into "In progress" while the
  // filter still let it through below would leave a "Deposits" view showing a withdrawal.
  const visible = filter === "all" ? all : all.filter((o) => o.kind === filter);
  // Each operation appears in exactly one of the two bands. Showing a queued withdrawal
  // in "In progress" *and* again under today's date reads as two withdrawals — which on
  // a money screen is not a cosmetic problem. The lifted rows are the newest anyway, so
  // the timeline loses no chronology by starting below them.
  const pending = visible.filter(isPending);
  const settled = visible.filter((o) => !isPending(o));
  const groups = useMemo(() => groupByDay(settled, t, locale), [settled, t, locale]);

  return (
    <Stagger step={SECTION_STAGGER} className="px-4 pb-8 pt-6 lg:px-8">
      <StaggerItem as="header" className="mb-6 space-y-1">
        <p className="font-mono-tech text-xs uppercase tracking-widest text-accent-debug">{t("ui.operations")}</p>
        <h1 className="text-2xl font-semibold text-ink">{t("ui.operations")}</h1>
        <p className="text-sm text-ink-soft">{t("ops.subtitle")}</p>
      </StaggerItem>

      {error && <ResourceError variant="alert" title={t("err.opsLoad")} message={error} className="mb-6" />}

      {/* The timeline is one section — the filter bar and the table it filters arrive
          together, because a bar that lands before the rows invites a click that has
          nothing to act on yet. */}
      <StaggerItem>
        <Settled
        loading={loading}
        skeleton={
          <div className="space-y-4">
            <Skeleton className="h-10 w-72 rounded-lg" />
            <Skeleton className="h-80 w-full rounded-xl" />
          </div>
        }
      >
        {loading ? null : all.length === 0 ? (
          <Card>
            <CardContent>
              <Empty className={EMPTY_BOX}>
                <EmptyHeader>
                  <EmptyMedia variant="icon">
                    <ListChecks />
                  </EmptyMedia>
                  <EmptyTitle>{t("ui.noOperations")}</EmptyTitle>
                  <EmptyDescription>{t("ops.emptyHint")}</EmptyDescription>
                </EmptyHeader>
                <EmptyContent>
                  <Button asChild>
                    <Link href="/wallet/deposit">{t("ui.addFunds")}</Link>
                  </Button>
                  <Button asChild variant="outline">
                    <Link href="/invest">{t("ui.browseFunds")}</Link>
                  </Button>
                </EmptyContent>
              </Empty>
            </CardContent>
          </Card>
        ) : (
          <div className="space-y-6">
            {/* The kit's own segmented bar: ButtonGroup collapses the inner radii and
                the doubled borders between neighbours, so five buttons read as one
                control rather than five that happen to touch.

                Still a filter, not a set of panels — the group is a `radiogroup` of
                buttons carrying `aria-pressed`, not Tabs, because a TabsList whose
                triggers control no TabPanel leaves every `aria-controls` pointing at
                nothing.

                `w-fit` + `overflow-x-auto`: past the viewport the bar scrolls rather
                than squeezing, because five labels do not fit across 390px at any
                legible size. `shrink-0` on each button is what makes that true — the
                default would compress them to fit and clip the text instead. */}
            <ButtonGroup
              role="radiogroup"
              aria-label={t("ops.filterByKind")}
              className="max-w-full overflow-x-auto"
            >
              {FILTERS.map((value) => {
                const on = filter === value;
                return (
                  <Button
                    key={value}
                    type="button"
                    role="radio"
                    aria-checked={on}
                    // Every segment stays `outline`, including the active one: the
                    // group collapses neighbouring borders, and a variant without a
                    // border (secondary, ghost) punches a visible gap in the bar
                    // wherever the selection happens to be. The active state is a
                    // tint, using the same accent the sidebar and table rows use.
                    variant="outline"
                    size="sm"
                    onClick={() => setFilter(value)}
                    className={cn(
                      "shrink-0 px-3",
                      on && "bg-accent-debug/10 text-accent-debug hover:bg-accent-debug/15 hover:text-accent-debug",
                    )}
                  >
                    {t(FILTER_LABEL_KEYS[value])}
                  </Button>
                );
              })}
            </ButtonGroup>

            {visible.length === 0 ? (
              <Card>
                <CardContent>
                  <Empty className={EMPTY_BOX}>
                    <EmptyHeader>
                      <EmptyMedia variant="icon">
                        <ArrowLeftRight />
                      </EmptyMedia>
                      <EmptyTitle>{t(FILTER_EMPTY_KEYS[filter])}</EmptyTitle>
                      <EmptyDescription>{t("ops.filterEmptyHint")}</EmptyDescription>
                    </EmptyHeader>
                  </Empty>
                </CardContent>
              </Card>
            ) : (
              <div className="space-y-6">
                {pending.length > 0 && <InProgress operations={pending} titleOf={titleOf} />}
                {groups.map((group) => (
                  <section key={group.label} className="space-y-2">
                    <h2 className="px-1 text-xs font-medium uppercase tracking-wide text-ink-soft">{group.label}</h2>
                    <Card>
                      {/* The rows carry the inset instead of the card, so a hover (and the separator
                          between rows) reaches the card's edges rather than stopping 24px short. */}
                      <CardContent className="px-0">
                        <ItemGroup>
                          {group.operations.map((operation, i) => (
                            <Fragment key={rowKey(operation, i)}>
                              {i > 0 && <ItemSeparator />}
                              <Row operation={operation} titleOf={titleOf} />
                            </Fragment>
                          ))}
                        </ItemGroup>
                      </CardContent>
                    </Card>
                  </section>
                ))}
              </div>
            )}

            {/* Says only what is true: the page is capped. There is no statements export to
                point at yet, and promising one here would be inventing a feature. */}
            {truncated && <p className="text-xs text-ink-soft">{t("ops.truncatedNote")}</p>}
          </div>
        )}
        </Settled>
      </StaggerItem>
    </Stagger>
  );
}

// The "In progress" card (Figma `ios/operations`): everything still moving, lifted to the
// top so a queued withdrawal is not buried under a day of settled rows. These rows do not
// repeat in the timeline below — see the split in `OperationsView`.
function InProgress({ operations, titleOf }: { operations: Operation[]; titleOf: (service: string | undefined) => string }) {
  const t = useT();
  return (
    <Card>
      <CardHeader>
        <CardTitle>{t("ui.inProgress")}</CardTitle>
      </CardHeader>
      {/* The rows carry the inset instead of the card, so a hover (and the separator
          between rows) reaches the card's edges rather than stopping 24px short. */}
      <CardContent className="px-0">
        <ItemGroup>
          {operations.map((operation, i) => (
            <Fragment key={rowKey(operation, i)}>
              {i > 0 && <ItemSeparator />}
              <Row operation={operation} titleOf={titleOf} />
            </Fragment>
          ))}
        </ItemGroup>
      </CardContent>
    </Card>
  );
}

function Row({ operation, titleOf }: { operation: Operation; titleOf: (service: string | undefined) => string }) {
  const t = useT();
  const locale = useLocale();
  const [open, setOpen] = useState(false);
  const compact = useIsCompact();
  const meta = kindMeta(operation.kind);
  const StateIcon = STATE_ICONS[operation.state ?? ""];
  const at = seconds(operation.created_at);
  const title = rowTitle(operation, titleOf, t);

  const trigger = (
    <Item asChild size="sm" className="rounded-none px-6 py-3 lg:py-4">
      <button
        type="button"
        aria-label={t("ops.a11y.rowDetails", { title })}
        className="w-full cursor-pointer text-left outline-none transition-colors hover:bg-muted/30 focus-visible:ring-2 focus-visible:ring-ring"
      >
        <ItemMedia>
          {/* The mark is decorative: the row title already names the kind in words, so
              announcing it again would read "Deposit" twice. The text fallback is only
              reached for a kind this build has no mark for — i18n-max: 4 there, since it
              sits in a `shrink-0` badge beside a truncating title. */}
          <Badge className={cn("font-semibold", meta.tone)}>{meta.icon ? <meta.icon aria-hidden /> : kindBadge(operation.kind)}</Badge>
        </ItemMedia>
        <ItemContent className="min-w-0 gap-0.5">
          <ItemTitle className="block w-auto truncate font-semibold">{title}</ItemTitle>
          <ItemDescription className="line-clamp-1 text-xs">
            {rowSub(operation, t)}
            {at > 0 && ` · ${timeLabel(at, locale)}`}
          </ItemDescription>
        </ItemContent>
        <ItemActions className="shrink-0 flex-col items-end gap-1">
          <span className={cn("text-sm font-semibold tabular-nums", amountTone(meta.direction))}>{rowAmount(operation)}</span>
          {/* No `capitalize`: the label is now a translated word, not a lowercase wire
              identifier — and `capitalize` title-cases every word ("Partly Deferred").
              i18n-max: 12. The mark doubles the tint so the state does not reach the
              reader by colour alone; the word beside it is what actually names it. */}
          <Badge className={stateTone(operation.state)}>
            {StateIcon && <StateIcon aria-hidden />}
            {stateLabel(operation.state, t)}
          </Badge>
        </ItemActions>
        {/* Every row opens a panel now, so every row carries the same disclosure — not
            only the in-flight ones that used to link out to their managing surface. */}
        <ChevronRight className="size-4 shrink-0 text-ink-soft" aria-hidden />
      </button>
    </Item>
  );

  const detail = <OperationDetail operation={operation} title={title} onManage={manageHref(operation.kind, operation.state)} />;

  // A 380px popover on a 390px phone is a modal wearing a popover's clothes; below `lg`
  // the same panel is presented as a bottom sheet instead.
  if (compact) {
    return (
      <Drawer open={open} onOpenChange={setOpen}>
        <DrawerTrigger asChild>{trigger}</DrawerTrigger>
        {/* UIKIT-MIRROR: drawer-animation — TEMPORARY, mirrors EV-invest/lib#96.
            The published uikit's Drawer ships with no transition at all, so the sheet
            appears in a single frame. These are the kit's own classes, passed here until
            the npm bump carries them; `shared/config/uikit-mirror.test.ts` fails the
            moment the installed uikit animates, which is the signal to delete this. */}
        <DrawerContent
          className={cn(
            "data-[state=open]:animate-in data-[state=open]:slide-in-from-bottom",
            "transition duration-500 ease-in-out",
            "max-h-[85vh] overflow-y-auto",
          )}
        >
          <DrawerTitle className="sr-only">{title}</DrawerTitle>
          {detail}
        </DrawerContent>
      </Drawer>
    );
  }

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>{trigger}</PopoverTrigger>
      {/* `useFloating` flips and clamps the panel's position but never its height, so an
          in-flight withdrawal (the tall case) has to cap and scroll itself. */}
      <PopoverContent align="end" side="bottom" className="max-h-[70vh] w-95 overflow-y-auto p-0">
        {detail}
      </PopoverContent>
    </Popover>
  );
}

// The surface that owns this kind's mutations. Only a *queued* operation can still be
// cancelled — offering "Manage" on a settled one sends the user somewhere with nothing to
// do. Deposits and subscriptions have nothing to act on once made.
function manageHref(kind: string | undefined, state: string | undefined): `/${string}` | null {
  if (state !== "queued") return null;
  if (kind === "withdrawal") return "/wallet/activity";
  if (kind === "redemption") return "/invest";
  return null;
}

function rowTitle(operation: Operation, titleOf: (service: string | undefined) => string, t: Translate): string {
  switch (operation.kind) {
    case "deposit":
      // The noun, not the button verb — this titles a timeline row. See the note on
      // `KINDS.deposit` in `views/operations/lib/format.ts`.
      return t("ops.kind.deposit");
    case "withdrawal":
      return t("ops.kind.withdrawal");
    case "subscription":
      return t("dash.op.subscribed", { fund: titleOf(operation.service) });
    case "redemption":
      return t("dash.op.redeemed", { fund: titleOf(operation.service) });
    case "fee":
      return t("ops.op.feeCharged", { fund: titleOf(operation.service) });
    default:
      return kindLabel(operation.kind, t);
  }
}

/// A decimal-string amount that is present and not zero. The wire sends every money field
/// as a string, and proto3 omits an empty one entirely, so both "" and "0" mean "no leg".
function nonZero(amount: string | undefined): boolean {
  return !!amount && Number(amount) > 0;
}

function rowSub(operation: Operation, t: Translate): string {
  switch (operation.kind) {
    case "deposit":
      // `USDT` is the ticker, not a word — it is the same mark in every locale.
      return t("ops.sub.deposit", { network: networkLabel(operation.network), ref: operation.tx_ref ? shortAddress(operation.tx_ref) : "USDT" });
    case "withdrawal": {
      // `shortAddress` already renders an absent address as an em dash — falling back to
      // the network label instead would print the rail twice on the same line.
      const destination = shortAddress(operation.address);
      const network = networkLabel(operation.network);
      // What actually ships is the net; the row's figure is the gross debited, so the
      // fee is stated rather than left as an unexplained gap between the two.
      return operation.net_amount
        ? t("ops.sub.withdrawalNet", { network, destination, amount: formatUsdt(operation.net_amount) })
        : t("ops.sub.withdrawal", { network, destination });
    }
    case "subscription":
    case "redemption":
      return t("dash.unitsAmount", { n: Number(operation.units ?? 0), units: formatUnits(operation.units) });
    case "fee": {
      // The two legs answer the question the amount alone cannot: whether this was rent
      // on the capital or a share of the gain. A zero leg is omitted rather than printed,
      // because "0 performance" reads as a fee that was somehow waived.
      const legs = [
        nonZero(operation.management) ? t("ops.sub.managementLeg", { amount: formatUsdt(operation.management) }) : null,
        nonZero(operation.performance) ? t("ops.sub.performanceLeg", { amount: formatUsdt(operation.performance) }) : null,
      ].filter(Boolean);
      const taken = t("dash.unitsAmount", { n: Number(operation.units ?? 0), units: formatUnits(operation.units) });
      return legs.length ? `${taken} · ${legs.join(" + ")}` : taken;
    }
    default:
      return "";
  }
}

function rowAmount(operation: Operation): string {
  // A redemption is priced at settle, so a queued one genuinely has no cash figure —
  // rendered as an em dash rather than a zero that would read as "you got nothing".
  if (!operation.amount) return "—";
  const { direction } = kindMeta(operation.kind);
  const sign = direction === "in" ? "+" : direction === "out" ? "−" : "";
  return `${sign}${formatUsdt(operation.amount)} USDT`;
}

// `id` is unique per kind (a UUID, or a deposit's tx_ref) but nothing guarantees it across
// kinds, and a multi-output deposit tx can repeat its ref — so the key carries the kind
// and the row's position too.
function rowKey(operation: Operation, index: number): string {
  return `${operation.kind ?? ""}-${operation.id ?? ""}-${index}`;
}

interface DayGroup {
  label: string;
  operations: Operation[];
}

// The feed arrives newest-first from the hub; this only inserts the day headings, so the
// runs stay in the order the hub sorted them.
function groupByDay(operations: Operation[], t: Translate, locale: Locale): DayGroup[] {
  const now = new Date();
  const groups: DayGroup[] = [];
  for (const operation of operations) {
    const label = dayLabel(seconds(operation.created_at), t, locale, now);
    const last = groups.at(-1);
    if (last?.label === label) last.operations.push(operation);
    else groups.push({ label, operations: [operation] });
  }
  return groups;
}
