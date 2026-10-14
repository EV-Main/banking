"use client";

// Who the claims on custody belong to, through units (#245): every allocation the hub
// knows — the products and the hidden `fee` / `fund` — with its cash claim, supply, price
// and holders, then banking's own treasury (D17) as a last row: a claim of the same shape
// but no allocation, so it has no units, price or holders. Read, never derived: each
// figure is its own ledger account, and Σ claims + `held_by_users` + `held_in_compartments`
// = custody is the hub's invariant, not this table's sum.

import { Layers } from "lucide-react";

import { useLocale, useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle, Skeleton } from "@evinvest/uikit";

import type { AllocationClaim, AllocationTreasury } from "@/shared/contracts/admin";
import { cn } from "@/shared/lib/cn";
import { formatNav, formatUnits, formatUsd } from "@/shared/lib/money";
import { hasUnixStamp } from "@/shared/lib/unix-stamp";
import { Link } from "@/shared/ui/cabinet-link";
import { Settled } from "@/shared/ui/motion";
import { accessLabel, accessTone } from "@/views/admin/lib/access";
import { ago } from "@/views/admin/lib/format";
import { HoldersCell } from "@/views/admin/treasury/ui/holders-cell";

export function TreasuryAllocations({ allocations, bankingTreasury }: { allocations: AllocationTreasury[] | null; bankingTreasury: AllocationClaim | null }) {
  const t = useT();
  return (
    <Card>
      <CardContent className="p-0">
        <Settled loading={!allocations} skeleton={<Skeleton className="m-6 h-24" />}>
          {!allocations ? null : allocations.length === 0 ? (
            <div className="p-8">
              <Empty className="border md:p-6">
                <EmptyHeader>
                  <EmptyMedia variant="icon">
                    <Layers />
                  </EmptyMedia>
                  <EmptyTitle>{t("admin.treasury.allocations.empty", "No allocations yet")}</EmptyTitle>
                  <EmptyDescription>{t("admin.treasury.allocations.emptyHint", "Every allocation the hub knows appears here with its claim, supply and holders — the products, and the hidden fee and fund allocations once they exist.")}</EmptyDescription>
                </EmptyHeader>
                <EmptyContent>
                  {/* The action that produces a row: a product exists once it is registered. */}
                  <Button asChild variant="outline">
                    <Link href="/admin/allocations">{t("nav.allocations", "Allocations")}</Link>
                  </Button>
                </EmptyContent>
              </Empty>
            </div>
          ) : (
            // Six columns do not fit a phone: the table scrolls inside its own box.
            <div className="overflow-x-auto">
              <table className="w-full min-w-160 text-sm">
                <thead>
                  {/* i18n-max: 14 per header — a long header widens the scroll, not a cell. */}
                  <tr className="border-b border-border text-left text-xs uppercase tracking-wide text-ink-soft">
                    <th className="px-5 py-3 font-medium">{t("admin.treasury.allocations.col.allocation", "Allocation")}</th>
                    <th className="px-5 py-3 font-medium">{t("admin.alloc.col.access", "Access")}</th>
                    <th className="px-5 py-3 text-right font-medium">{t("admin.treasury.allocations.col.claim", "Cash claim")}</th>
                    <th className="px-5 py-3 text-right font-medium">{t("admin.alloc.holders.outstanding", "Outstanding")}</th>
                    <th className="px-5 py-3 text-right font-medium">NAV</th>
                    <th className="px-5 py-3 font-medium">{t("admin.alloc.holders.title", "Holders")}</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-border">
                  {allocations.map((a) => (
                    <AllocationRow key={a.service} allocation={a} />
                  ))}
                  {bankingTreasury && <BankingTreasuryRow claim={bankingTreasury} />}
                </tbody>
              </table>
            </div>
          )}
        </Settled>
      </CardContent>
    </Card>
  );
}

function AllocationRow({ allocation: a }: { allocation: AllocationTreasury }) {
  const t = useT();
  const locale = useLocale();
  return (
    <tr>
      <td className="px-5 py-3">
        <p className="font-medium">{a.title || a.service}</p>
        <p className="font-mono-tech text-xs text-ink-soft">{a.service}</p>
      </td>
      <td className="px-5 py-3">
        <span className={cn("inline-flex whitespace-nowrap rounded-full border px-2 py-0.5 text-xs font-medium", accessTone(a.access))}>{accessLabel(a.access, t)}</span>
      </td>
      <ClaimCell claim={a.claim} />
      <td className="px-5 py-3 text-right tabular-nums">{formatUnits(a.units_outstanding, locale)}</td>
      <td className="px-5 py-3 text-right tabular-nums">
        <p>{formatNav(a.nav, locale)}</p>
        {/* `"0"` is "never marked" — the hub prices it at seed — not a mark at the epoch. */}
        <p className="text-xs text-ink-soft">{hasUnixStamp(a.nav_posted_at) ? ago(a.nav_posted_at, t) : t("admin.treasury.allocations.unmarked", "not yet marked")}</p>
      </td>
      <td className="px-5 py-3">
        <HoldersCell holders={a.holders} />
      </td>
    </tr>
  );
}

function BankingTreasuryRow({ claim }: { claim: AllocationClaim }) {
  const t = useT();
  return (
    <tr>
      <td className="px-5 py-3">
        <p className="font-medium">{t("admin.treasury.bankingTreasury", "Banking treasury")}</p>
        <p className="font-mono-tech text-xs text-ink-soft">main:treasury</p>
      </td>
      <td className="px-5 py-3 text-xs text-ink-soft">{t("admin.treasury.bankingTreasuryAccess", "banking's own")}</td>
      <ClaimCell claim={claim} />
      <td className="px-5 py-3 text-right text-ink-soft">—</td>
      <td className="px-5 py-3 text-right text-ink-soft">—</td>
      <td className="px-5 py-3 text-xs text-ink-soft">{t("admin.treasury.bankingTreasuryHolders", "withdrawal fees · spending closed for now")}</td>
    </tr>
  );
}

function ClaimCell({ claim }: { claim: AllocationClaim }) {
  const t = useT();
  const locale = useLocale();
  return (
    <td className="px-5 py-3 text-right tabular-nums">
      <p>{formatUsd(claim.available, locale)}</p>
      {/* Posted and reserved beside the spendable figure: an operator judging a payment
          out of the claim needs the reserved part, not only what is left. */}
      <p className="text-xs text-ink-soft">{t("admin.treasury.allocations.claimDetail", "posted {posted} · reserved {reserved}", { posted: formatUsd(claim.posted, locale), reserved: formatUsd(claim.reserved, locale) })}</p>
    </td>
  );
}
