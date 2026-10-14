"use client";

// Admin console — the platform's OWN money: the reserved `fee` allocation (#245), and
// beside it banking's own treasury (D17).
//
// What used to be three figures off one claim is an allocation like any other: a cash
// claim credited by the book's taker fee and settled 2-and-20, a supply of units, a NAV,
// and holders — the people the owners have seated on it. The retained withdrawal fee no
// longer lands there: since D17 it is banking's, on `scope_account:main:treasury`, which
// has a cash claim and nothing else — no units, so no holders. The screen's whole job is
// still to make one distinction unmistakable: this is never client money and never the
// `fund` allocation, which are separate claims this surface cannot reach.
//
// Nothing pays revenue out from here. A holder redeems, or the owners approve a payment
// out of `service:fee` or banking's treasury on the Payments screen; the one write here is
// a PROPOSAL — seating a person.

import { useT } from "@evinvest/i18n/react";
import { Skeleton } from "@evinvest/uikit";

import { fundRevenueResource } from "@/entities/admin/model/admin-resource";
import { useResource } from "@/shared/lib/resource";
import { Settled, StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { BankingTreasuryCards, FeeAllocationCards } from "@/views/admin/revenue/ui/fee-allocation-cards";
import { HolderGrantForm } from "@/views/admin/revenue/ui/holder-grant-form";
import { WhereNext } from "@/views/admin/revenue/ui/where-next";
import { HoldersTable } from "@/views/admin/ui/holders-table";
import { AdminHeader, AdminScreen } from "@/views/admin/ui/shell";

export function RevenueView() {
  const t = useT();
  const revenue = useResource(fundRevenueResource);
  const fee = revenue.data ?? null;
  const failed = !fee && Boolean(revenue.error);

  return (
    <AdminScreen className="space-y-8">
      <AdminHeader eyebrow={t("admin.eyebrow.administer", "Administer")} title={t("nav.revenue", "Revenue stats")} subtitle={t("admin.revenue.subtitle", "What the platform earned — the fee allocation: its cash, its units and who holds them")} />

      {failed && <ResourceError error={revenue.error} onRetry={() => void revenue.refresh()} retrying={revenue.isValidating} />}

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.revenue.earned", "The fee allocation")}</p>
        <FeeAllocationCards fee={fee} loading={!fee && !failed} unavailable={failed} />
        <p className="max-w-3xl text-xs text-ink-soft">{t("admin.revenue.ownMoneyNote", "This is the platform's own money. Client balances and the fund allocation are separate ledger claims and are not included here — and cannot be reached from this screen.")}</p>
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.treasury.bankingTreasury", "Banking treasury")}</p>
        {/* A hub older than D17 sends no treasury beside `fee`: that reads as unavailable,
            not as a zero nobody reported. */}
        <BankingTreasuryCards claim={fee?.banking_treasury ?? null} loading={!fee && !failed} unavailable={failed || (fee !== null && !fee.banking_treasury)} />
        <p className="max-w-3xl text-xs text-ink-soft">{t("admin.revenue.bankingTreasuryNote", "Withdrawal fees land here, not in the fee allocation: banking's own cash, held by no one through units. The owners approve every payment out of it.")}</p>
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.alloc.holders.title", "Holders")}</p>
        <div className="grid gap-4 lg:grid-cols-2">
          {failed ? (
            // A read that failed is not an empty cap table: the column says so, with the retry.
            <ResourceError error={revenue.error} onRetry={() => void revenue.refresh()} retrying={revenue.isValidating} />
          ) : (
            <Settled loading={!fee} skeleton={<Skeleton className="h-24 w-full" />}>
              {fee && <HoldersTable holders={fee.holders} outstanding={fee.units_outstanding} />}
            </Settled>
          )}
          <HolderGrantForm allocation="fee" />
        </div>
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.revenue.whereNext", "Move or inspect it")}</p>
        <WhereNext />
      </StaggerItem>
    </AdminScreen>
  );
}
