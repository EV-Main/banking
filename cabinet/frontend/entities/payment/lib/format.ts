// The words for a payment order's wire vocabulary — state, tier, requirement, consent —
// shared by every surface that shows an order: the owners' room and the emailed approvals. Each takes the caller's `t`, because this is a plain module with no
// translator of its own.
//
// Every lookup is guarded by its table and falls back to the bare wire word: the state
// machine lives in the money plane, and a state it grows tomorrow should render as
// `reconciling`, not as `payment.state.reconciling` (see `shared/contracts/payments.ts`).

import type { Translate } from "@evinvest/i18n";

import type { PaymentConsent } from "@/shared/contracts/payments";
import { settledConsent } from "@/shared/lib/decision";
import { wordFor } from "@/shared/lib/wire-words";

const stateWords = (t: Translate): Readonly<Record<string, string>> => ({
  pending: t("payment.state.pending", "Pending"),
  approved: t("payment.state.approved", "Approved"),
  executed: t("payment.state.executed", "Executed"),
  execution_failed: t("payment.state.execution_failed", "Execution failed"),
  rejected: t("payment.state.rejected", "Rejected"),
  expired: t("payment.state.expired", "Expired"),
  cancelled: t("payment.state.cancelled", "Cancelled"),
});

const tierWords = (t: Translate): Readonly<Record<string, string>> => ({
  internal: t("payment.tier.internal", "Internal"),
  service: t("payment.tier.service", "To a product"),
  external: t("payment.tier.external", "External"),
});

const tierHints = (t: Translate): Readonly<Record<string, string>> => ({
  internal: t("payment.tier.hint.internal", "Between two claims inside the fund. No money leaves the ledger."),
  service: t("payment.tier.hint.service", "Into one of the fund's products, as its pooled claim."),
  external: t("payment.tier.hint.external", "Out to an on-chain address. Ships as a withdrawal once approved."),
});

const requirementWords = (t: Translate): Readonly<Record<string, string>> => ({
  owner_consilium: t("payment.requirement.owner_consilium", "Owners' consilium"),
  subject_consent: t("payment.requirement.subject_consent", "The investor's consent"),
});

const requirementHints = (t: Translate): Readonly<Record<string, string>> => ({
  owner_consilium: t("payment.requirement.hint.owner_consilium", "Fund-owned money: more than half of the owners must confirm from their mailboxes."),
  subject_consent: t("payment.requirement.hint.subject_consent", "An investor's own money: that investor alone is emailed and must agree."),
});

/** Still waiting on an answer — the only state the initiator can still cancel. */
export function isPaymentOpen(state: string): boolean {
  return state === "pending";
}

export function paymentStateLabel(state: string, t: Translate): string {
  return wordFor(stateWords(t), state) ?? state;
}

/** Token classes for a state pill. Neutral unless the state carries real news. */
export function paymentStateTone(state: string): string {
  switch (state) {
    case "executed":
      return "text-positive";
    case "pending":
    case "approved":
      return "text-accent-debug";
    case "rejected":
    case "execution_failed":
      return "text-accent-error";
    default:
      return "text-ink-soft";
  }
}

export function tierLabel(tier: string, t: Translate): string {
  return wordFor(tierWords(t), tier) ?? tier;
}

/** What the tier means for the money, in a sentence — for a reader who is not an operator. */
export function tierHint(tier: string, t: Translate): string {
  return wordFor(tierHints(t), tier) ?? tier;
}

export function requirementLabel(requirement: string, t: Translate): string {
  return wordFor(requirementWords(t), requirement) ?? requirement;
}

/** Who has to agree, in a sentence — the requirement's hint beside its label. */
export function requirementHint(requirement: string, t: Translate): string {
  return wordFor(requirementHints(t), requirement) ?? requirement;
}

/**
 * Where the one emailed seat stands, in words.
 *
 * `invalidated` is read before `decision`: a seat whose pins moved cannot be answered
 * whatever the decision field says, and "approved" on a seat the plane no longer trusts
 * would be the wrong news. An unanswered seat is `settledConsent`'s null, never the
 * truthy "pending" string.
 */
export function consentLabel(consent: PaymentConsent, t: Translate): string {
  if (consent.invalidated) return t("payment.consent.invalidated", "Consent void");
  const settled = settledConsent(consent.decision);
  if (settled === "approve") return t("payment.consent.approve", "Consented");
  if (settled === "reject") return t("payment.consent.reject", "Declined");
  return consent.notified ? t("payment.consent.pending", "Awaiting the investor") : t("payment.consent.unsent", "Email not yet sent");
}

export function consentTone(consent: PaymentConsent): string {
  if (consent.invalidated) return "text-accent-error";
  const settled = settledConsent(consent.decision);
  if (settled === "approve") return "text-positive";
  if (settled === "reject") return "text-accent-error";
  return "text-accent-debug";
}
