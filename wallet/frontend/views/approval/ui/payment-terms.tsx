"use client";

// The terms of a payment order, as an approval page shows them — the amount, both ends,
// the tier, the reason, the fingerprint. Shared by the investor's consent page and the
// owners' approval page when the consilium carries a payment.
//
// Policy 12–13 shape it: everything the digest covers is on screen before the code field
// is. An external destination shows its address whole through `FullAddress`; an internal
// end shows the label the digest binds AND the detail a person recognises it by, because
// "investor 8f3e…" is exactly the thing a reader cannot check.

import { useLocale, useT } from "@evinvest/i18n/react";
import { Separator } from "@evinvest/uikit";

import { tierHint, tierLabel } from "@/entities/payment/lib/format";
import { PaymentEndSummary } from "@/entities/payment/ui/payment-end";
import type { ConsiliumPaymentTerms } from "@/shared/contracts/payments";
import { hashPrefix } from "@/shared/lib/hash";
import { formatExactUsdt } from "@/shared/lib/money";
import { DetailRow, FieldCaption, FullAddress } from "@/views/approval/ui/approval-chrome";

/** What the digest covers is exactly these; `payment_id` is not part of the terms. */
export type PaymentTerms = Omit<ConsiliumPaymentTerms, "payment_id">;

/**
 * The terms a reader is agreeing to must actually be on screen. The BFF fills a missing
 * order with empty strings, and an empty string is not nullish — so a page that cannot
 * show the amount and where it goes must not show buttons (policy 12–13).
 */
export function renderablePayment(terms: PaymentTerms | null | undefined): terms is PaymentTerms {
  if (!terms?.amount?.trim()) return false;
  const to = terms.destination;
  return Boolean(to.kind === "external" ? to.address?.trim() : to.label?.trim());
}

export function PaymentTermsBlock({
  terms,
  payloadHash,
  reasonLabel,
  tierAs = "label",
}: {
  terms: PaymentTerms;
  payloadHash: string;
  reasonLabel: string;
  /**
   * `label` names the tier as the console and the owners' room do; `hint` says what it
   * means for the money instead. The consent page takes the hint: its reader is an
   * investor, and "Internal" tells them nothing about whether their money leaves the ledger.
   */
  tierAs?: "label" | "hint";
}) {
  const t = useT();
  const locale = useLocale();
  const external = terms.destination.kind === "external";
  return (
    <>
      <div className="flex flex-col gap-1.5">
        <FieldCaption>{t("approval.amount", "Amount")}</FieldCaption>
        <p className="text-4xl font-semibold leading-none tabular-nums text-ink">
          {/* The wire string, digit for digit — `payload_hash` covers the exact decimal. */}
          {formatExactUsdt(terms.amount, locale)}
          <span className="ml-2 text-base font-medium text-ink-soft">USDT</span>
        </p>
      </div>

      <div className="grid gap-4 sm:grid-cols-2">
        <div className="flex flex-col gap-1.5">
          <FieldCaption>{t("approval.payment.from", "From")}</FieldCaption>
          <PaymentEndSummary end={terms.source} className="text-sm" />
        </div>
        {!external && (
          <div className="flex flex-col gap-1.5">
            <FieldCaption>{t("approval.payment.to", "To")}</FieldCaption>
            <PaymentEndSummary end={terms.destination} className="text-sm" />
          </div>
        )}
      </div>

      {external && <FullAddress label={t("approval.payment.to", "To")} address={terms.destination.address} />}

      <div className="flex flex-col gap-1.5">
        <FieldCaption>{reasonLabel}</FieldCaption>
        {/* The initiator's own words, whole and unsummarised, set apart from the page's. */}
        <blockquote className="whitespace-pre-line rounded-lg border-l-2 border-accent-warn/60 bg-secondary px-3.5 py-3 text-sm leading-relaxed text-ink">
          {terms.reason?.trim() || "—"}
        </blockquote>
      </div>

      <div className="flex flex-col gap-2.5">
        <DetailRow label={t("approval.payment.tier", "Tier")} value={tierAs === "hint" ? tierHint(terms.tier, t) : tierLabel(terms.tier, t)} />
        <DetailRow label={t("approval.payloadHash", "Request fingerprint")} value={hashPrefix(payloadHash)} mono />
      </div>
      <p className="text-xs text-ink-soft">{t("approval.payment.payloadHashHint", "The fingerprint is taken over the amount, both ends and the reason above. It is checked again before any money moves, so a request cannot be edited after you have agreed to it.")}</p>

      <Separator />
    </>
  );
}
