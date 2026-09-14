"use client";

// `/invest` — the portfolio view of the fund surface: what the whole holding is worth,
// what is free to deploy, and one card per product.
//
// Two rounds of layout have been thrown away here. First the dealing forms, which made
// the list read as a stack of forms. Then the wide table-ish rows that replaced them:
// they left the left half of every row empty while the figures crowded the right edge,
// and two summary cards stretched to a shared height they had no content for. The page
// is now a single dense band plus a grid — nothing is sized by anything other than what
// is in it.

import { useT } from "@evinvest/i18n/react";
import { ArrowRight, Sparkles, TrendingUp, Wallet } from "lucide-react";
import { Link } from "@/shared/ui/cabinet-link";
import { useMemo } from "react";

import { Badge, Button, Card, CardContent, Skeleton } from "@evinvest/uikit";

import { allocationsResource, fundNavResource, positionsResource, redemptionsResource } from "@/entities/fund/model/fund-resource";
import { walletResource } from "@/entities/wallet/model/wallet-resource";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { TipAnchor } from "@/shared/tips";
import { ProductIcon, productTone } from "@/shared/ui/icons/products";
import { SECTION_STAGGER, Stagger, StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { formatSignedUsdt, formatUnits, formatUsdt, fromBaseUnits, isNegative, isZero, toBaseUnits } from "@/views/invest/lib/format";
import { buildProducts, type Product } from "@/views/invest/lib/product";
import { SupplyBar, TEAL_CTA } from "@/views/invest/ui/atoms";

export function InvestView() {
  const t = useT();
  const positionList = useResource(positionsResource);
  const catalogList = useResource(allocationsResource);
  const redemptionList = useResource(redemptionsResource);
  const wallet = useResource(walletResource);

  const positions = positionList.data?.positions ?? null;
  const catalog = catalogList.data?.allocations ?? null;
  const redemptions = redemptionList.data?.redemptions ?? [];
  // The free balance is context, not the subject of this screen — a failure to read it
  // hides the figure rather than failing the page.
  const available = wallet.data?.balance?.available ?? null;
  // Only a positions read that has never succeeded is worth an alert; a failed refresh over
  // holdings already on screen is not something to interrupt the page for.
  const error = positionList.data ? null : positionList.error ? errorMessage(positionList.error, t) : null;

  const products = useMemo<Product[] | null>(() => (catalog && positions ? buildProducts(catalog, positions) : null), [catalog, positions]);

  const held = products?.filter((p) => p.position && !isZero(p.position.units)) ?? [];
  const totals = held.reduce(
    (acc, p) => ({ value: acc.value + toBaseUnits(p.position?.value), cost: acc.cost + toBaseUnits(p.position?.cost_basis) }),
    { value: 0n, cost: 0n },
  );
  const queued = redemptions.filter((r) => r.state === "queued");

  return (
    <Stagger step={SECTION_STAGGER} className="container max-w-5xl space-y-6 py-10">
      <StaggerItem as="header" className="space-y-3">
        <h1 className="text-3xl font-semibold leading-tight">{t("invest.title", "Your fund shares")}</h1>
        {/* `invest.overview` is a SECTION tip — a descriptor block, not an inline ⓘ — so
            it cannot live inside the heading row: it laid a full-width bordered box across
            the title. It belongs under the header, which is also the one place this
            explanation should live (the hand-written subtitle that used to sit here said
            the same thing in slightly different words). */}
        <TipAnchor anchor="invest.overview" />
      </StaggerItem>

      {error && <ResourceError variant="alert" title={t("err.positionsLoad", "Couldn't load your positions")} message={error} />}

      {!products ? (
        <StaggerItem className="space-y-4">
          <Skeleton className="h-28 w-full" />
          <div className="grid gap-4 lg:grid-cols-2">
            <Skeleton className="h-56 w-full" />
            <Skeleton className="h-56 w-full" />
          </div>
        </StaggerItem>
      ) : (
        <>
          <PortfolioBand invested={totals.value} cost={totals.cost} funds={held.length} available={available} queued={queued.length} />

          <StaggerItem as="section" className="space-y-3">
            <p className="flex items-center gap-2 text-xs font-semibold uppercase tracking-widest text-ink-soft">
              {t("invest.products", "Products")}
              {products.length > 0 &&<span className="rounded-full bg-accent-debug/15 px-2 py-0.5 text-xs font-semibold text-accent-debug">{products.length}</span>}
            </p>
            {products.length === 0 ? (
              <Card>
                <CardContent className="flex flex-col items-center gap-2 py-16 text-center text-ink-soft">
                  <Sparkles className="size-6" />
                  <p className="text-sm">{t("invest.noFunds", "No funds are open for subscription right now.")}</p>
                  <p className="max-w-sm text-xs">{t("invest.noFundsHint", "A fund appears here once an operator registers and opens it. Nothing you hold is affected.")}</p>
                </CardContent>
              </Card>
            ) : (
              <div className="grid gap-4 lg:grid-cols-2">
                {products.map((product) => (
                  <ProductCard key={product.service} product={product} />
                ))}
              </div>
            )}
          </StaggerItem>
        </>
      )}
    </Stagger>
  );
}

/**
 * The whole holding in one band.
 *
 * Deliberately a single row rather than the two cards it replaced: those were laid out
 * side by side with `h-full`, so the shorter one stretched to the taller one's height and
 * spent the difference on nothing. Here each block is only as tall as its content and the
 * rules between them carry the grouping.
 */
function PortfolioBand({ invested, cost, funds, available, queued }: { invested: bigint; cost: bigint; funds: number; available: string | null; queued: number }) {
  const t = useT();
  const pnl = invested - cost;
  const loss = pnl < 0n;
  const flat = pnl === 0n;
  // A percentage off a zero cost basis is not "0%", it is undefined — so it is omitted.
  const pct = cost > 0n ? Number((pnl * 10_000n) / cost) / 100 : null;

  return (
    <StaggerItem as={Card}>
      {/* The stacking lives on `max-md:`, not on a `md:` override of a base utility.
          `.flex-col` and `.md\:flex-row` have identical specificity — a media query adds
          none — so whichever Tailwind emits last wins at every width, and it emits the
          base utility last. `md:flex-row` therefore silently lost to `flex-col` and this
          band rendered as a centred column on the desktop it was designed for. Phrased
          this way the desktop state is the unclassed default and nothing competes. */}
      <CardContent className="flex py-5 max-md:flex-col max-md:gap-6 md:items-center">
        <div className="space-y-1.5 md:flex-1">
          <p className="text-xs font-semibold uppercase tracking-widest text-accent-debug">{t("invest.investedValue", "Invested value")}</p>
          <div className="flex flex-wrap items-baseline gap-2.5">
            <span className="text-3xl font-semibold leading-none tabular-nums">{formatUsdt(fromBaseUnits(invested))}</span>
            <span className="text-sm text-ink-soft">USDT</span>
            {!flat && (
              <span className={cn("rounded-full px-2 py-0.5 text-xs font-semibold tabular-nums", loss ? "bg-accent-info/15 text-accent-info" : "bg-positive/15 text-positive")}>
                {formatSignedUsdt(fromBaseUnits(pnl))}
                {pct !== null && ` · ${pct > 0 ? "+" : ""}${pct.toFixed(2)}%`}
              </span>
            )}
          </div>
          <p className="text-xs text-ink-soft">{funds === 0 ? t("invest.noUnitsHeld", "You hold no fund units yet.") : t("invest.acrossFunds", "{n, plural, one {Across # fund} other {Across # funds}}", { n: funds })}</p>
        </div>

        <div className="flex flex-wrap gap-8 md:border-l md:border-border md:px-7">
          <BandStat label={t("invest.costBasis", "Cost basis")} value={`${formatUsdt(fromBaseUnits(cost))} USDT`} />
          <BandStat label={t("invest.fundsHeld", "Funds held")} value={String(funds)} />
          <BandStat
            label={t("invest.awaitingSettlement", "Awaiting settlement")}
            value={queued === 0 ? t("ui.none", "None") : t("admin.valuation.queuedCount", "{n, plural, one {# queued} other {# queued}}", { n: queued })}
            tone={queued > 0 ? "text-accent-warn" : undefined}
          />
        </div>

        <div className="space-y-2 md:border-l md:border-border md:pl-7">
          <p className="flex items-center gap-1.5 text-xs text-ink-soft">
            <Wallet className="size-3.5" /> {t("invest.availableToInvest", "Available to invest")}
          </p>
          <p className="text-xl font-semibold tabular-nums">{available === null ? "—" : `${formatUsdt(available)} USDT`}</p>
          <Button asChild type="button" variant="outline" size="sm">
            <Link href="/wallet">{t("invest.topUp", "Top up")}</Link>
          </Button>
        </div>
      </CardContent>
    </StaggerItem>
  );
}

function BandStat({ label, value, tone }: { label: string; value: string; tone?: string }) {
  return (
    <div className="space-y-1">
      <p className="text-xs text-ink-soft">{label}</p>
      <p className={cn("text-sm font-semibold tabular-nums", tone)}>{value}</p>
    </div>
  );
}

/**
 * One product, as a card that links to its own page.
 *
 * A card rather than a row because a row at this width left the product's name alone on
 * the left with the figures pushed against the right edge — a lot of horizontal travel
 * for the eye and a lot of empty space in between. The card fills the width it is given
 * and the two columns of figures sit under the name they belong to.
 *
 * It carries its own NAV read: the catalog RPC is presentation-only, and a price is the
 * thing a holder is scanning this list for. Cached per fund and shared with that fund's own
 * page, so opening a product from this list opens on the price the card was already showing.
 */
function ProductCard({ product }: { product: Product }) {
  const t = useT();
  const nav = useResource(fundNavResource, product.service).data ?? null;

  const held = product.position && !isZero(product.position.units) ? product.position : null;
  const closed = product.allocation === null;
  const loss = held ? isNegative(held.pnl) : false;
  const flat = held ? isZero(held.pnl) : true;

  return (
    <Card className="transition-colors hover:border-accent-debug/40">
      <CardContent className="flex h-full flex-col gap-4 py-5">
        <div className="flex items-center gap-3">
          {/* Tinted by service id, not fixed to t1: the card and the rail's row are the
              same fund seen twice, and a single accent for every card made a catalog of
              them read as one block. */}
          <span className={cn("flex size-9 shrink-0 items-center justify-center rounded-lg", productTone(product.service))}>
            <ProductIcon icon={product.icon} className="size-4.5" />
          </span>
          <div className="min-w-0 flex-1">
            <p className="truncate text-base font-semibold">{product.title}</p>
            <p className="truncate font-mono-tech text-xs text-ink-soft">{product.service}</p>
          </div>
          {/* A non-shrinking sibling of the `min-w-0 flex-1` title column, so a long badge
              is taken straight out of the fund's name. i18n-max: 12. */}
          <Badge variant="outline" className={cn(closed ? "border-accent-warn/40 text-accent-warn" : "border-positive/40 text-positive")}>
            {closed ? t("invest.badge.redeemOnly", "Redeem only") : t("invest.badge.open", "Open")}
          </Badge>
        </div>

        <div className="flex flex-wrap gap-x-6 gap-y-3 border-y border-border py-3.5">
          <CardStat label={t("invest.navPerUnit", "NAV / unit")} value={nav ? formatUsdt(nav.nav) : "—"} large />
          {held ? (
            <>
              <CardStat label={t("invest.yourUnits", "Your units")} value={formatUnits(held.units)} />
              <CardStat label={t("invest.value", "Value")} value={formatUsdt(held.value)} />
              <CardStat
                label={t("invest.pnl", "P&L")}
                value={flat ? "0.00" : formatSignedUsdt(held.pnl)}
                tone={flat ? undefined : loss ? "text-accent-info" : "text-positive"}
                icon={flat ? undefined : <TrendingUp className={cn("size-3.5", loss && "rotate-180")} />}
              />
            </>
          ) : (
            <div className="ml-auto space-y-1 text-right">
              <p className="text-xs text-ink-soft">{t("invest.yourPosition", "Your position")}</p>
              <p className="text-sm text-ink-soft">{t("invest.notInvestedYet", "Not invested yet")}</p>
            </div>
          )}
        </div>

        {nav && <SupplyBar issued={nav.units_outstanding} cap={nav.unit_cap} />}

        {/* One CTA treatment per card, full width. The list used to mix a filled button
            with a bare text link, which read as two different kinds of thing. */}
        <Button asChild className={cn("mt-auto w-full", held ? "" : TEAL_CTA)} variant={held ? "outline" : "default"}>
          <Link href={`/invest/${encodeURIComponent(product.service)}`}>
            {held ? t("ui.manage", "Manage") : t("nav.invest", "Invest")}
            <ArrowRight className="size-4" />
          </Link>
        </Button>
      </CardContent>
    </Card>
  );
}

function CardStat({ label, value, large, tone, icon }: { label: string; value: string; large?: boolean; tone?: string; icon?: React.ReactNode }) {
  return (
    <div className="space-y-1">
      <p className="text-xs text-ink-soft">{label}</p>
      <p className={cn("flex items-center gap-1 font-semibold tabular-nums", large ? "text-xl" : "text-sm", tone)}>
        {icon}
        {value}
      </p>
    </div>
  );
}
