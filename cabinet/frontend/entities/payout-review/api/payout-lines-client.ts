// Browser → BFF: one page of a pending scope payout's lines. Only a voter on the approval's
// roster gets one; everyone else, and every id that leads nowhere, gets the same 404.

import type { PayoutLinesPage } from "@/shared/contracts";
import { getJson } from "@/shared/lib/api-client";

/** Lines per page — the hub's default, so the page and the server agree on what "a page" is. */
export const PAYOUT_LINES_PAGE = 50;

export function fetchPayoutLines(operationId: string, offset: number): Promise<PayoutLinesPage> {
  const query = `?offset=${offset}&limit=${PAYOUT_LINES_PAGE}`;
  return getJson<PayoutLinesPage>(`/api/approvals/payouts/${encodeURIComponent(operationId)}/lines${query}`);
}
