"use client";

// `/invest/[service]` — one product, in full.
//
// The list answers "how am I doing?"; this answers "what is this, and what do I want to
// do about it?". Dealing lives here rather than on the list because a subscription is a
// decision about one fund, and a form that appears under a row you happened to expand
// makes it look like a detail of the row rather than the point of the page.
//
// A slug that is not a registered allocation is a 404 from the hub — deliberately, since
// a page that rendered an unregistered service would be the same phantom-fund surface the
// registry exists to close.

import { useT } from "@evinvest/i18n/react";
import { ArrowDownToLine, ArrowLeft, Loader2, Sparkles, TrendingUp, TriangleAlert } from "lucide-react";
import { Link } from "@/shared/ui/cabinet-link";
import { useState } from "react";

import { Alert, AlertDescription, AlertTitle, Button, Card, CardContent, Skeleton } from "@evinvest/uikit";

import { accruedFeesResource, allocationsResource, feePolicyResource, fundNavResource, positionsResource, redemptionsResource } from "@/entities/fund/model/fund-resource";
import type { AccruedFees, FeePolicy, FundNav, Position } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { pct } from "@/shared/lib/rate";
import { useResource } from "@/shared/lib/resource";
import { TipAnchor } from "@/shared/tips";
import { ProductIcon, productTone } from "@/shared/ui/icons/products";
import { SECTION_STAGGER, Stagger, StaggerItem } from "@/shared/ui/motion";
import { compactUnits, formatSignedUsdt, formatUnits, formatUsdt, isNegative, isZero } from "@/views/invest/lib/format";
import { blockedReasonKey, buildProducts, type Product } from "@/views/invest/lib/product";
import { Note, ProductBadges, Stat, SupplyBar, TEAL_CTA } from "@/views/invest/ui/atoms";
import { QueuedList, RedeemPanel, SubscribePanel } from "@/views/invest/ui/deal-panels";

type Panel = "subscribe" | "redeem" | null;

export function ProductView({ service }: { service: string }) {
  const t = useT();
  const [panel, setPanel] = useState<Panel>(null);

  // The catalog and the positions are both needed to decide what this product *is* to this
  // caller: open-and-unheld, open-and-held, or closed-but-still-held. Both are cached and
  // both were already read by the list this page is usually entered from, so the product
  // resolves on the first frame. `undefined` is still "loading" and `null` is "no such
  // product" — collapsing the two would show a not-found flash on every cold load.
  const catalogList = useResource(allocationsResource);
  const positionList = useResource(positionsResource);
  const navRead = useResource(fundNavResource, service);
  const redemptionList = useResource(redemptionsResource);
  // Two separate reads on purpose: the terms are the fund's and cache for minutes, while
  // the accrued figure is the caller's own and moves every second the clock runs.
  const feeRead = useResource(feePolicyResource, service);
  const accruedRead = useResource(accruedFeesResource, service);

  const resolving = catalogList.isLoading || positionList.isLoading;
  const readFailed = (!catalogList.data && catalogList.error) || (!positionList.data && positionList.error);
  const error = readFailed ? errorMessage(readFailed, t) : null;
  const product: Product | null | undefined = resolving
    ? undefined
    : (buildProducts(catalogList.data?.allocations ?? [], positionList.data?.positions ?? []).find((p) => p.service === service) ?? null);

  const nav = navRead.data ?? null;
  const feePolicy = feeRead.data ?? null;
  const accruedFees = accruedRead.data ?? null;
  const redemptions = (redemptionList.data?.redemptions ?? []).filter((r) => r.service === service);

  if (product === undefined) {
    return (
      <div className="container max-w-4xl space-y-6 py-12">
        <Skeleton className="h-10 w-64" />
        <Skeleton className="h-40 w-full" />
        <Skeleton className="h-32 w-full" />
      </div>
    );
  }

  if (product === null) {
    return (
      <div className="container max-w-4xl space-y-6 py-12">
        <BackLink />
        <Card>
          <CardContent className="flex flex-col items-center gap-2 py-16 text-center text-ink-soft">
            <TriangleAlert className="size-6" />
            <p className="text-sm">{error ?? t("invest.notRegistered", { service })}</p>
            <p className="max-w-sm text-xs">{t("invest.notRegisteredHint")}</p>
          </CardContent>
        </Card>
      </div>
    );
  }

  const held = product.position && !isZero(product.position.units) ? product.position : null;
  const closed = product.allocation === null;
  const stale = nav?.stale ?? false;
  // `posted_at` is 0 until an operator marks the fund, which is exactly when the hub is
  // still pricing at the bootstrap NAV of 1.0.
  const unmarked = nav !== null && Number(nav.posted_at ?? 0) === 0;
  const blocked = blockedReasonKey(product, nav);
  const queued = redemptions.filter((r) => r.state === "queued");

  return (
    <Stagger step={SECTION_STAGGER} className="container max-w-4xl space-y-7 py-12">
      <StaggerItem>
        <BackLink />
      </StaggerItem>

      <StaggerItem as="header" className="flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0 space-y-1">
          <div className="flex flex-wrap items-center gap-3">
            {/* The same mark and tint the rail and the invest card drew, so arriving here
                from either one lands on a header that is recognisably the row just clicked. */}
            <span className={cn("flex size-10 shrink-0 items-center justify-center rounded-xl", productTone(product.service))}>
              <ProductIcon icon={product.icon} className="size-5" />
            </span>
            <h1 className="text-3xl font-semibold">{product.title}</h1>
            <ProductBadges closed={closed} stale={stale} />
          </div>
          <p className="font-mono-tech text-xs text-ink-soft">{product.service}</p>
          {product.summary && <p className="max-w-xl pt-1 text-sm text-ink-soft">{product.summary}</p>}
        </div>
        <div className="flex flex-wrap gap-2">
          {!closed && (
            <Button type="button" className={cn(TEAL_CTA)} disabled={blocked !== null} onClick={() => setPanel((p) => (p === "subscribe" ? null : "subscribe"))}>
              <Sparkles className="size-4" />
              {panel === "subscribe" ? t("ui.close") : t("invest.subscribe")}
            </Button>
          )}
          {held && (
            <Button type="button" variant="outline" disabled={stale} onClick={() => setPanel((p) => (p === "redeem" ? null : "redeem"))}>
              <ArrowDownToLine className="size-4" />
              {panel === "redeem" ? t("ui.close") : t("invest.redeem")}
            </Button>
          )}
        </div>
      </StaggerItem>

      {error && (
        <StaggerItem>
          <Alert variant="destructive">
            <TriangleAlert className="size-4" />
            <AlertTitle>{t("err.fundRefresh")}</AlertTitle>
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        </StaggerItem>
      )}

      {/* The two columns are one section. The dealing panels inside open and close under
          `Panel`, and a stagger that also owned them would be animating the same elements
          from two directions the first time one is opened. */}
      <StaggerItem className="grid gap-5 lg:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
        <div className="space-y-5">
          {held ? <HoldingStats position={held} /> : <PriceOnly nav={nav} unmarked={unmarked} />}

          {/* The gates, stated before the action rather than after a failed submit. */}
          {blocked && <Note tone="amber">{t(blocked)}</Note>}
          {unmarked && !closed && <Note tone="muted">{t("invest.unmarkedNote")}</Note>}

          {panel === "subscribe" && !blocked && <SubscribePanel service={product.service} nav={nav} />}
          {panel === "redeem" && held && <RedeemPanel service={product.service} position={held} nav={nav} />}

          {queued.length > 0 && <QueuedList items={queued} />}
        </div>

        <div className="space-y-5">
          <SupplyCard nav={nav} />
          <FeeCard policy={feePolicy} accrued={held ? accruedFees : null} />
        </div>
      </StaggerItem>
    </Stagger>
  );
}

function BackLink() {
  const t = useT();
  return (
    <Link href="/invest" className="inline-flex items-center gap-1.5 text-sm text-ink-soft transition-colors hover:text-ink">
      <ArrowLeft className="size-4" />
      {t("invest.allProducts")}
    </Link>
  );
}

/**
 * The product's supply, as a fact about the fund rather than about the holder.
 *
 * This is the visible half of the unit cap: a fund is sized by an operator, and once the
 * authorised units are issued it stops minting. Saying so here means "subscription
 * refused" is never the first time a holder hears about it.
 */
function SupplyCard({ nav }: { nav: FundNav | null }) {
  const t = useT();
  if (!nav) {
    return (
      <Card>
        <CardContent className="py-6">
          <Loader2 className="size-4 animate-spin text-ink-soft" />
        </CardContent>
      </Card>
    );
  }
  return (
    <Card className="h-fit">
      <CardContent className="space-y-4 py-6">
        <p className="text-sm font-semibold">{t("admin.valuation.unitSupply")}</p>
        <SupplyBar issued={nav.units_outstanding} cap={nav.unit_cap} />
        <dl className="space-y-2.5 border-t border-border pt-4 text-sm">
          <Row
            label={t("invest.remainingCapacity")}
            value={t("dash.unitsAmount", { n: Number(nav.remaining_capacity ?? 0), units: compactUnits(nav.remaining_capacity) })}
          />
          <Row label={t("invest.navPerUnit")} value={`${formatUsdt(nav.nav)} USDT`} />
          <Row label={t("invest.fundAum")} value={nav.aum ? `${formatUsdt(nav.aum)} USDT` : t("invest.notYetValued")} />
        </dl>
        <p className="text-xs text-ink-soft">{t("invest.supplyNote")}</p>
      </CardContent>
    </Card>
  );
}

/**
 * What this fund charges, and what the caller's own holding has run up against it.
 *
 * It sits beside the supply card rather than inside the holding block because the terms
 * apply to a product whether or not the caller is in it — somebody deciding whether to
 * subscribe needs to read the fee before they act, not discover it on the first charge.
 *
 * A product with no policy renders nothing at all. That is deliberate: an absent policy
 * and a policy of zeros are different facts, and a card of zeros reads like a fee that was
 * generously waived rather than a fund that never had one.
 *
 * The accrued figures are shown only to a holder, because they are a statement about a
 * position. `total` is what would be taken if the fee were charged this instant — the
 * number the `Value` stat opposite has NOT been reduced by — so the card says so plainly
 * instead of leaving the two to be reconciled by the reader.
 */
function FeeCard({ policy, accrued }: { policy: FeePolicy | null; accrued: AccruedFees | null }) {
  const t = useT();
  if (!policy?.configured) return null;
  const owed = accrued?.configured ? accrued : null;
  const basisKey = BASIS_LABEL_KEYS[policy.basis ?? ""];
  const periodKey = PERIOD_LABEL_KEYS[policy.crystallization ?? ""];
  return (
    <Card className="h-fit">
      <CardContent className="space-y-4 py-6">
        <p className="text-sm font-semibold">{t("nav.fees")}</p>
        <dl className="space-y-2.5 text-sm">
          <Row label={t("admin.fees.field.management")} value={t("invest.perAnnum", { pct: pct(policy.management_bps) })} />
          <Row label={t("admin.fees.field.performance")} value={t("invest.ofTheGain", { pct: pct(policy.performance_bps) })} />
          {policy.hurdle_bps ? <Row label={t("admin.fees.field.hurdle")} value={t("invest.hurdleFirst", { pct: pct(policy.hurdle_bps) })} /> : null}
          {/* An unmapped basis/period falls back to the wire identifier — a value the hub
              added that this build has no word for, shown rather than swallowed. */}
          <Row label={t("admin.fees.chargedOn")} value={basisKey ? t(basisKey) : (policy.basis ?? "—")} />
          <Row label={t("invest.lockedIn")} value={periodKey ? t(periodKey) : (policy.crystallization ?? "—")} />
        </dl>

        {owed && (
          <div className="space-y-2.5 border-t border-border pt-4">
            {/* Its own heading, so the rows can be labelled `Management` and `Performance`
                without colliding with the identically-named terms above. Long enough labels
                to disambiguate inline would wrap onto two lines in this column. */}
            <p className="text-xs uppercase tracking-wide text-ink-soft">{t("invest.accruedOnHolding")}</p>
            <dl className="space-y-2.5 text-sm">
              <Row label={t("admin.fees.field.management")} value={`${formatUsdt(owed.management)} USDT`} />
              <Row label={t("admin.fees.field.performance")} value={`${formatUsdt(owed.performance)} USDT`} />
              {isZero(owed.debt) ? null : <Row label={t("invest.carriedOver")} value={`${formatUsdt(owed.debt)} USDT`} />}
              <Row label={t("ui.total")} value={`${formatUsdt(owed.total)} USDT`} />
              <Row label={t("invest.yourMark")} value={`${formatUsdt(owed.high_water_mark)} USDT`} />
            </dl>
          </div>
        )}

        <p className="text-xs text-ink-soft">{t(owed ? "invest.feeNoteHolder" : "invest.feeNoteProspect")}</p>
      </CardContent>
    </Card>
  );
}

// The same two vocabularies the admin fee console writes, read here — one wire value has
// one name across the cabinet, so both surfaces point at the same catalogue entries.
const BASIS_LABEL_KEYS: Record<string, string> = {
  invested_capital: "admin.fees.basis.investedCapital",
  market_value: "admin.fees.basis.marketValue",
};

const PERIOD_LABEL_KEYS: Record<string, string> = {
  monthly: "admin.fees.period.monthly",
  quarterly: "admin.fees.period.quarterly",
  semi_annual: "admin.fees.period.semiAnnual",
  annual: "admin.fees.period.annual",
};

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <dt className="text-ink-soft">{label}</dt>
      <dd className="font-medium tabular-nums">{value}</dd>
    </div>
  );
}

function HoldingStats({ position }: { position: Position }) {
  const t = useT();
  const loss = isNegative(position.pnl);
  const flat = isZero(position.pnl);
  return (
    <div className="grid gap-3 sm:grid-cols-2">
      <Stat label={t("invest.units")} value={formatUnits(position.units)} tip="invest.position.units" />
      <Stat label={t("invest.nav")} value={formatUsdt(position.nav)} tip="invest.position.nav" />
      <Stat label={t("invest.value")} value={`${formatUsdt(position.value)} USDT`} emphasis tip="invest.position.value" />
      <Stat
        label={t("invest.pnl")}
        value={`${formatSignedUsdt(position.pnl)} USDT`}
        tip="invest.position.pnl"
        emphasis
        tone={loss && !flat ? "text-accent-info" : "text-positive"}
        icon={<TrendingUp className={cn("size-3.5", loss && !flat && "rotate-180")} />}
      />
    </div>
  );
}

/** A product the caller holds nothing in: the price is all there is to show. */
function PriceOnly({ nav, unmarked }: { nav: FundNav | null; unmarked: boolean }) {
  const t = useT();
  return (
    <Card>
      <CardContent className="flex flex-wrap items-center justify-between gap-4 py-6">
        <div className="space-y-1">
          <p className="flex items-center gap-1.5 text-xs uppercase tracking-wide text-ink-soft">
            {t("invest.navPerUnit")}
            <TipAnchor anchor="invest.position.nav" />
          </p>
          <p className="text-2xl font-semibold tabular-nums">{nav ? `${formatUsdt(nav.nav)} USDT` : "—"}</p>
        </div>
        <p className="max-w-sm text-sm text-ink-soft">{t(unmarked ? "invest.notYetValuedHint" : "invest.noUnitsInFund")}</p>
      </CardContent>
    </Card>
  );
}
