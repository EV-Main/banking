"use client";

// A payment out of the caller's own claim, answered by the caller signed in — the session
// replaces the token-and-code of `entities/approval`. An answer names only the order: the
// terms are the stored order's, so a page cannot approve more than it showed.

import type { PaymentConfirmation } from "@/shared/contracts/payments";
import { getJson, postJson } from "@/shared/lib/api-client";

/** 404 for an order that is not the caller's, is not answered here, or does not exist — alike. */
export function fetchPaymentConfirmation(id: string): Promise<PaymentConfirmation> {
  return getJson<PaymentConfirmation>(`/api/wallet/payments/${encodeURIComponent(id)}`);
}

/** Declining moves nothing, so it asks for no step-up; approving is the `approve_payment` intent. */
export function rejectPayment(id: string): Promise<PaymentConfirmation> {
  return postJson<PaymentConfirmation>(`/api/wallet/payments/${encodeURIComponent(id)}/reject`, {});
}
