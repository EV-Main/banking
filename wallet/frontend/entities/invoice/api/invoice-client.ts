// Browser → BFF client for the invoices a scope asks the caller to pay. The browser only
// names the invoice; the amount and the payee are the server's. No tokens are seen here.

import { getJson, postJson } from "@/shared/lib/api-client";
import type { PayableInvoice } from "@/shared/contracts";

export function fetchPayableInvoice(id: string): Promise<PayableInvoice> {
  return getJson<PayableInvoice>(`/api/wallet/invoices/${encodeURIComponent(id)}`);
}

export function payInvoice(id: string, body: { pay_from: string; idempotency_key: string }): Promise<PayableInvoice> {
  return postJson<PayableInvoice>(`/api/wallet/invoices/${encodeURIComponent(id)}/pay`, body);
}
