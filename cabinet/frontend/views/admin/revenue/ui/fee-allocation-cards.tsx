"use client";

// The `fee` allocation's four figures: cash available (the one an owner acts on, so it
// carries the colour), cash posted with what is reserved, the supply, and the price. All
// read off the allocation's own accounts — none is a difference of the others.

import { useLocale, useT } from "@evinvest/i18n/react";
import { Card, CardContent, Skeleton } from "@evinvest/uikit";

import type { AllocationClaim, AllocationTreasury } from "@/shared/contracts/admin";
import { formatNav, formatUnits } from "@/shared/lib/money";
import { hasUnixStamp } from "@/shared/lib/unix-stamp";
import { ago } from "@/views/admin/lib/format";
import { MoneyCard } from "@/views/admin/revenue/ui/money-card";

export function FeeAllocationCards({ fee, loading, unavailable }: { fee: AllocationTreasury | null; loading: boolean; unavailable: boolean }) {
  const t = useT();
  const locale = useLocale();
  return (
    <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
      <MoneyCard label={t("admin.revenue.cashAvailable", "Cash · available")} value={fee?.claim.available} hint={t("admin.revenue.cashAvailableHint", "free of approved payments")} loading={loading} unavailable={unavailable} emphasis />
      <MoneyCard label={t("admin.revenue.cashPosted", "Cash · posted")} value={fee?.claim.posted} hint={t("admin.revenue.cashPostedHint", "settled fees + settled 2-and-20, reserved included")} loading={loading} unavailable={unavailable} />
      <FigureCard label={t("admin.alloc.holders.outstanding", "Outstanding")} value={fee ? formatUnits(fee.units_outstanding, locale) : undefined} hint={t("admin.revenue.unitsHint", "units of the fee allocation, held by people")} loading={loading} unavailable={unavailable} />
      <FigureCard
        label="NAV"
        value={fee ? formatNav(fee.nav, locale) : undefined}
        // `"0"` is "never marked" — priced at seed — not a mark at the epoch.
        hint={fee && hasUnixStamp(fee.nav_posted_at) ? t("admin.revenue.navPosted", "marked {ago}", { ago: ago(fee.nav_posted_at, t) }) : t("admin.treasury.allocations.unmarked", "not yet marked")}
        loading={loading}
        unavailable={unavailable}
      />
    </div>
  );
}

/** banking's own treasury (D17): a cash claim with no units behind it, so only the two
 *  cash figures the `fee` row leads with. */
export function BankingTreasuryCards({ claim, loading, unavailable }: { claim: AllocationClaim | null; loading: boolean; unavailable: boolean }) {
  const t = useT();
  return (
    <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
      <MoneyCard label={t("admin.revenue.cashAvailable", "Cash · available")} value={claim?.available} hint={t("admin.revenue.cashAvailableHint", "free of approved payments")} loading={loading} unavailable={unavailable} emphasis />
      <MoneyCard label={t("admin.revenue.cashPosted", "Cash · posted")} value={claim?.posted} hint={t("admin.revenue.treasuryPostedHint", "retained withdrawal fees, reserved included")} loading={loading} unavailable={unavailable} />
    </div>
  );
}

/** A non-money figure in the money cards' frame: units and a price, already formatted by
 *  their own policy, so the card does not run them through `formatUsd`. */
function FigureCard({ label, value, hint, loading, unavailable }: { label: string; value: string | undefined; hint: string; loading: boolean; unavailable: boolean }) {
  return (
    <Card>
      <CardContent className="space-y-1 py-5">
        <p className="text-xs text-ink-soft">{label}</p>
        {loading ? <Skeleton className="mt-1 h-8 w-28" /> : <p className={unavailable ? "text-3xl font-semibold tabular-nums text-ink-soft" : "text-3xl font-semibold tabular-nums"}>{unavailable ? "—" : value}</p>}
        {!loading && !unavailable && <p className="text-xs text-ink-soft">{hint}</p>}
      </CardContent>
    </Card>
  );
}
