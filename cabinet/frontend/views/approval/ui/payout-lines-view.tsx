"use client";

// "Who is paid what" for a pending scope payout (banking#529). The approval concierge shows
// carries totals only; an approver on its roster opens this to see every line before voting.
//
// The BFF answers one identical 404 for an unknown operation and for one the reader is not
// an approver of, so this page cannot — and does not try to — say which it was.

import { Link2Off } from "lucide-react";
import { useState } from "react";

import { useLocale, useT } from "@evinvest/i18n/react";
import { Badge, Button, Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle, Skeleton, Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@evinvest/uikit";

import { PAYOUT_LINES_PAGE } from "@/entities/payout-review/api/payout-lines-client";
import { payoutLinesResource } from "@/entities/payout-review/model/payout-lines-resource";
import { RequestError } from "@/shared/lib/api-client";
import { formatExactUsdt } from "@/shared/lib/money";
import { useResource } from "@/shared/lib/resource";
import { PageFrame } from "@/shared/ui/page-frame";
import { ResourceError } from "@/shared/ui/resource-error";
import { PayoutLinesSummary } from "@/views/approval/ui/payout-lines-summary";
import { TABLE_HEAD } from "@/views/admin/lib/table";

export function PayoutLinesView({ operationId }: { operationId: string }) {
  const t = useT();
  const locale = useLocale();
  const [offset, setOffset] = useState(0);
  const read = useResource(payoutLinesResource, operationId, offset);
  const page = read.data ?? null;
  const total = page?.recipients ?? 0;
  const lines = page?.lines ?? [];
  const title = t("approval.lines.title", "Payout lines");
  const description = t("approval.lines.description", "Every recipient and amount of the payout you are asked to approve.");

  if (!page && read.error instanceof RequestError && read.error.status === 404) {
    return (
      <PageFrame title={title}>
        <Empty className="border">
          <EmptyHeader>
            <EmptyMedia variant="icon"><Link2Off /></EmptyMedia>
            <EmptyTitle>{t("approval.lines.unavailable", "These lines are not available")}</EmptyTitle>
            <EmptyDescription>{t("approval.lines.unavailableHint", "Only the people asked to approve a payout can see its lines. Check that you opened the link from your own approval.")}</EmptyDescription>
          </EmptyHeader>
        </Empty>
      </PageFrame>
    );
  }

  return (
    <PageFrame title={title} description={description}>
      {!page && read.error ? (
        <ResourceError error={read.error} onRetry={() => void read.refresh()} retrying={read.isValidating} />
      ) : !page ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <>
          <PayoutLinesSummary page={page} />
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className={TABLE_HEAD}>#</TableHead>
                <TableHead className={TABLE_HEAD}>{t("approval.lines.col.recipient", "Recipient")}</TableHead>
                <TableHead className={`${TABLE_HEAD} text-right`}>{t("approval.lines.col.amount", "Amount (USDT)")}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {lines.map((line) => (
                <TableRow key={line.line_no}>
                  <TableCell className="tabular-nums text-ink-soft">{(line.line_no ?? 0) + 1}</TableCell>
                  <TableCell className="whitespace-normal break-all">
                    {line.recipient}
                    <span className="ml-2 inline-flex flex-wrap gap-1.5 align-middle">
                      {line.no_history && <Badge variant="outline" className="text-accent-error">{t("approval.lines.noHistory", "No history in this scope")}</Badge>}
                      {line.amount === page.max_line && <Badge variant="outline">{t("approval.lines.largest", "Largest")}</Badge>}
                    </span>
                  </TableCell>
                  <TableCell className="text-right tabular-nums">{formatExactUsdt(line.amount, locale)}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          {total > PAYOUT_LINES_PAGE && (
            <div className="flex items-center justify-between gap-3">
              <span className="text-xs text-ink-soft tabular-nums">
                {t("approval.lines.range", "{from}–{to} of {total}", { from: offset + 1, to: Math.min(offset + PAYOUT_LINES_PAGE, total), total })}
              </span>
              <div className="flex gap-2">
                <Button variant="outline" size="sm" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - PAYOUT_LINES_PAGE))}>{t("approval.lines.prev", "Previous")}</Button>
                <Button variant="outline" size="sm" disabled={offset + PAYOUT_LINES_PAGE >= total} onClick={() => setOffset(offset + PAYOUT_LINES_PAGE)}>{t("approval.lines.next", "Next")}</Button>
              </div>
            </div>
          )}
        </>
      )}
    </PageFrame>
  );
}
