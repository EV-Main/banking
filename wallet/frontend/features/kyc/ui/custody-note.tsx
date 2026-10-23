"use client";

import { useT } from "@evinvest/i18n/react";
import { ShieldCheck } from "lucide-react";

import { cn } from "@/shared/lib/cn";

/**
 * The one line under a money input that says how the money is protected: held in custody,
 * booked to a verified identity, and moved only under it. Facts the cabinet can stand
 * behind today — custody and KYC — and no more: the reconciliation claim ("every unit
 * has a holder") waits for #245's phase 1. The custodian (Turnkey) and the verifier
 * (Didit) are named: the owner allows naming client-facing services, and a named
 * counterparty is a checkable claim where "in custody" alone is not (#385).
 *
 * In the KYC slice because verification is what the sentence rests on: it is the same
 * promise `VerificationRequired` makes from the other side, to a reader not yet cleared.
 */
export function CustodyNote({ className }: { className?: string }) {
  const t = useT();
  return (
    <p className={cn("flex items-start gap-1.5 text-xs leading-relaxed text-ink-soft", className)}>
      <ShieldCheck className="mt-0.5 size-3.5 shrink-0 text-primary-ink" aria-hidden />
      {t("kyc.custodyNote", "Deposits are held in Turnkey custody under your identity verified by Didit; nothing moves without it.")}
    </p>
  );
}
