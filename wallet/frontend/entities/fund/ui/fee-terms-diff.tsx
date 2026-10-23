"use client";

// A change of fee terms as a reader compares it: five rows, what the fund charges NOW
// beside what is PROPOSED, the rows that move set in the accent so the eye lands on them.
//
// One component for the owners' room and the emailed approval page, because an owner who
// checks the numbers in one place and approves them in the other must be looking at the
// same rendering of the same five fields (docs/CONSILIUM.md, policy 13). `from` absent
// means the fund charged nothing — a different fact from a policy of zeros, and the column
// says so rather than printing 0%.

import { useT } from "@evinvest/i18n/react";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@evinvest/uikit";

import { cn } from "@/shared/lib/cn";
import { basisLabel, crystallizationLabel, type FeeTermsLike } from "@/shared/lib/fee-terms";
import { pct } from "@/shared/lib/rate";

// Plain, not tracked-uppercase: this table is read on the emailed approval page as well
// as in the admin console, and there it sits among prose.
const HEAD = "h-8 px-3 text-xs font-medium text-ink-soft";

export function FeeTermsDiff({ from, to }: { from: FeeTermsLike | null | undefined; to: FeeTermsLike }) {
  const t = useT();
  const rows: { key: string; label: string; now: string | null; next: string }[] = [
    { key: "admin.fees.field.management", label: t("admin.fees.field.management", "Management"), now: from ? t("invest.perAnnum", "{pct} p.a.", { pct: pct(from.management_bps) }) : null, next: t("invest.perAnnum", "{pct} p.a.", { pct: pct(to.management_bps) }) },
    { key: "admin.fees.field.performance", label: t("admin.fees.field.performance", "Performance"), now: from ? t("invest.ofTheGain", "{pct} of the gain", { pct: pct(from.performance_bps) }) : null, next: t("invest.ofTheGain", "{pct} of the gain", { pct: pct(to.performance_bps) }) },
    { key: "admin.fees.field.hurdle", label: t("admin.fees.field.hurdle", "Hurdle"), now: from ? pct(from.hurdle_bps) : null, next: pct(to.hurdle_bps) },
    { key: "admin.fees.chargedOn", label: t("admin.fees.chargedOn", "Charged on"), now: from ? basisLabel(from.basis, t) : null, next: basisLabel(to.basis, t) },
    { key: "invest.lockedIn", label: t("invest.lockedIn", "Locked in"), now: from ? crystallizationLabel(from.crystallization, t) : null, next: crystallizationLabel(to.crystallization, t) },
  ];
  return (
    // The kit's wrapper already scrolls; the border is the frame the approval page draws
    // around the five rows, and it goes on the outside of that wrapper.
    <div className="rounded-lg border border-border">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead className={HEAD} />
            <TableHead className={HEAD}>{t("consilium.feePolicy.now", "Now")}</TableHead>
            <TableHead className={HEAD}>{t("consilium.feePolicy.proposed", "Proposed")}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {/* The kit's cells are nowrap; on the emailed approval page a French or Russian
              label has to wrap inside a phone-width frame, not push it into a scroll. */}
          {rows.map((row) => {
            const moved = row.now !== row.next;
            return (
              <TableRow key={row.key}>
                <TableCell className="whitespace-normal px-3 text-ink-soft">{row.label}</TableCell>
                <TableCell className="whitespace-normal px-3 tabular-nums text-ink-soft">{row.now ?? t("consilium.feePolicy.nothingCharged", "Nothing charged")}</TableCell>
                <TableCell className={cn("whitespace-normal px-3 tabular-nums", moved ? "font-semibold text-accent-warn" : "text-ink")}>{row.next}</TableCell>
              </TableRow>
            );
          })}
        </TableBody>
      </Table>
    </div>
  );
}
