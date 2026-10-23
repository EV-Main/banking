"use client";

// A payment out of the caller's own claim, answered by the caller signed in — the session
// replaces the token-and-code of `entities/approval`. The body of an answer names only the
// decision: the terms are the stored order's, so a page cannot approve more than it showed.

import type { ConsentDecision, PaymentConfirmation } from "@/shared/contracts/payments";
import { getJson, postJson } from "@/shared/lib/api-client";

/** 404 for an order that is not the caller's, is not answered here, or does not exist — alike. */
export function fetchPaymentConfirmation(id: string): Promise<PaymentConfirmation> {
  return getJson<PaymentConfirmation>(`/api/wallet/payments/${encodeURIComponent(id)}`);
}

/**
 * Approving answers 401 `step_up_required` when the last interactive sign-in is older than
 * five minutes; declining never does. The answer comes back as the order after any execution
 * it triggered.
 */
export function answerPaymentConfirmation(id: string, decision: ConsentDecision): Promise<PaymentConfirmation> {
  return postJson<PaymentConfirmation>(`/api/wallet/payments/${encodeURIComponent(id)}/answer`, { decision });
}
