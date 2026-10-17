"use client";

import { useLocale, useT } from "@evinvest/i18n/react";
import { Badge, Card, CardContent } from "@evinvest/uikit";

import type { PayoutLinesPage } from "@/shared/contracts";
import { formatExactUsdt } from "@/shared/lib/money";
import { DetailRow } from "@/views/approval/ui/approval-chrome";

// What the approval itself carries (total, largest line, flags) next to what it cannot: how
// many recipients banking has never paid in this scope. Figures are the wire strings, digit
// for digit — the digest the approval binds is taken over exactly these amounts.
export function PayoutLinesSummary({ page }: { page: PayoutLinesPage }) {
  const t = useT();
  const locale = useLocale();
  const unseen = page.recipients_without_history ?? 0;
  return (
    <Card>
      <CardContent className="flex flex-col gap-2.5 py-5">
        <DetailRow label={t("approval.lines.from", "Paid from")} value={page.from_account ?? "—"} />
        <DetailRow label={t("approval.lines.total", "Total")} value={`${formatExactUsdt(page.total, locale)} USDT`} />
        <DetailRow label={t("approval.lines.maxLine", "Largest line")} value={`${formatExactUsdt(page.max_line, locale)} USDT`} />
        <DetailRow label={t("approval.lines.recipients", "Recipients")} value={String(page.recipients ?? 0)} />
        <DetailRow
          label={t("approval.lines.withoutHistory", "Recipients without history in this scope")}
          value={String(unseen)}
          tone={unseen > 0 ? "text-accent-error" : undefined}
        />
        <DetailRow label={t("approval.lines.digest", "Lines digest")} value={page.lines_digest ?? "—"} mono />
        <div className="flex flex-wrap gap-2">
          {page.distribution && <Badge variant="outline">{t("approval.lines.distribution", "Distribution")}</Badge>}
          {page.over_scope_budget && <Badge variant="outline">{t("approval.lines.overBudget", "Over the scope's daily budget")}</Badge>}
        </div>
      </CardContent>
    </Card>
  );
}
