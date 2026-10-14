"use client";

// The treasury in two layers: what the ledger owes and to whom (Layer 1 — people directly
// or in their scope compartments, through the units of an allocation, or banking's own
// treasury), and where the backing physically sits (Layer 2 — custody per rail). Since #245
// nothing here is a remainder: `held_by_users`, `held_in_compartments`, banking's treasury
// and each allocation's claim are read off their own accounts, so a figure with no holder
// cannot hide inside a derived one.

import { RefreshCw } from "lucide-react";

import { useT } from "@evinvest/i18n/react";
import { Button } from "@evinvest/uikit";

import { treasuryResource } from "@/entities/admin/model/admin-resource";
import { errorMessage } from "@/shared/lib/api-client";
import { useResource } from "@/shared/lib/resource";
import { TipAnchor } from "@/shared/tips";
import { StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { RichMessage } from "@/shared/ui/rich-message";
import { railLabel } from "@/views/admin/lib/format";
import { TreasuryAllocations } from "@/views/admin/treasury/ui/allocations-table";
import { MoneyCard } from "@/views/admin/treasury/ui/money-card";
import { RailFunding } from "@/views/admin/treasury/ui/rail-funding";
import { RecordArrival } from "@/views/admin/treasury/ui/record-arrival";
import { SeedCapitalForm } from "@/views/admin/treasury/ui/seed-capital-form";
import { AdminHeader, AdminScreen } from "@/views/admin/ui/shell";

export function TreasuryView() {
  const t = useT();
  // Cached, so returning from another console screen paints the figures immediately and
  // refreshes them behind. A failed read must STOP the skeletons — pulsing placeholders
  // beside an error read as "still coming", so the retry never gets clicked — which is
  // exactly what `isLoading` reports: false once an attempt has settled either way.
  const read = useResource(treasuryResource);
  const treasury = read.data ?? null;
  const error = read.error ? errorMessage(read.error, t) : null;
  const loading = read.isLoading || read.isValidating;
  const retry = () => void read.refresh();
  const cardState = { loading: loading && !treasury, unavailable: !loading && !treasury };

  return (
    <AdminScreen className="space-y-8">
      <AdminHeader
        eyebrow={t("admin.eyebrow.administer", "Administer")}
        title={t("nav.treasury", "Treasury")}
        subtitle={t("admin.treasury.subtitle", "Two layers — ledger claims (USDT) by holder vs on-chain liquidity by rail")}
        action={
          <Button type="button" variant="outline" size="sm" disabled={loading} onClick={retry}>
            <RefreshCw className={loading ? "size-4 animate-spin" : "size-4"} /> {t("ui.refresh", "Refresh")}
          </Button>
        }
      />

      {error && <ResourceError message={error} onRetry={retry} retrying={loading} />}

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.treasury.layer1", "Layer 1 · Ledger — claims by holder (USDT)")}</p>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <MoneyCard label={t("admin.treasury.claimsTotal", "Claims · total (USDT)")} value={treasury?.total_custody} hint={t("admin.treasury.claimsTotalHint", "= on-chain custody · backed")} tip="admin.treasury.layer1.claims-total" {...cardState} />
          <MoneyCard label={t("admin.treasury.heldByUsers", "Held by users")} value={treasury?.held_by_users} hint={t("admin.treasury.heldByUsersHint", "user claims · held directly")} tip="admin.treasury.layer1.held-by-users" {...cardState} />
          <MoneyCard label={t("admin.treasury.heldInCompartments", "Held in compartments")} value={treasury?.held_in_compartments} hint={t("admin.treasury.heldInCompartmentsHint", "users' scope compartments · still theirs")} {...cardState} />
          <MoneyCard label={t("admin.treasury.reservedWithdrawals", "Reserved · withdrawals")} value={treasury?.reserved_for_withdrawals} hint={t("admin.treasury.reservedWithdrawalsHint", "queued + in-flight (clearing)")} tip="admin.treasury.layer1.reserved-withdrawals" {...cardState} />
        </div>
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.treasury.allocations.title", "Held through allocations")}</p>
        {/* A section-type tip — a descriptor block, not an inline ⓘ — so it sits under the
            heading rather than in it, the way the settings sections place theirs. */}
        <TipAnchor anchor="admin.treasury.layer1.allocations" />
        <TreasuryAllocations allocations={treasury?.allocations ?? null} bankingTreasury={treasury?.banking_treasury ?? null} />
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.treasury.layer2", "Layer 2 · Treasury — liquidity by rail")}</p>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          {treasury ? (
            <>
              {treasury.rails.map((rail) => (
                <MoneyCard key={rail.network} network={rail.network} label={railLabel(rail.network, t)} value={rail.custody} loading={false} footer={<RailFunding rail={rail} />} />
              ))}
              <MoneyCard label={t("admin.treasury.bank", "Bank · USD")} value={treasury.bank} unit="USD" hint={t("admin.treasury.bankHint", "off-ramp · FX")} loading={false} tip="admin.treasury.bank" />
            </>
          ) : (
            Array.from({ length: 4 }).map((_, i) => <MoneyCard key={i} label="" value={undefined} loading={loading} unavailable={!loading} />)
          )}
        </div>
      </StaggerItem>

      <RecordArrival rails={treasury?.rails} onRecorded={retry} />
      <SeedCapitalForm rails={treasury?.rails} />

      {/* The expression is code, so it is an ICU argument rather than a key of its own —
          and the sentence around it stays whole, which is what a translator needs to put
          it where their grammar wants it. `RichMessage` is what lets that argument render
          as `<code>` rather than as prose: an invariant set in the body face reads as
          something someone wrote, not as something the system enforces. */}
      <StaggerItem as="p" className="max-w-3xl text-xs text-ink-soft">
        <RichMessage format={(t, values) => t("admin.treasury.invariantNote", "Per-rail backing is the treasury's job, not the ledger's: a shortfall on one rail is accept-and-queue, then rebalanced via CEX / alt-rail / top-up. The global invariant is {invariant}.", values)} values={{ invariant: <code className="font-mono-tech">sum(custody) == sum(claims)</code> }} />
      </StaggerItem>
    </AdminScreen>
  );
}
