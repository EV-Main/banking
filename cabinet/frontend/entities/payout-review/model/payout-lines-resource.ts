"use client";

import { fetchPayoutLines } from "@/entities/payout-review/api/payout-lines-client";
import { defineResource } from "@/shared/lib/resource";

// Keyed by page. The lines of a stored operation never change; only its `state` does, which
// a minute-old page may lag behind — the approval itself is where the decision is taken.
export const payoutLinesResource = defineResource({
  name: "approval.payoutLines",
  fetch: fetchPayoutLines,
  key: (operationId: string, offset: number) => `${operationId}:${offset}`,
  revalidate: 60,
  enabled: (operationId: string) => operationId.length > 0,
});
