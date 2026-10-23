"use client";

// The one control on the consent page that can turn it into a decision: the code from
// the email, and the two answers it unlocks.
//
// Owns the code field so a spent attempt can clear it here; everything else — whether
// an attempt is in flight, how many remain, what the last one said — is the parent's,
// because those are read back from the server and this card never predicts them.

import { useState } from "react";

import { useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, CardDescription, CardHeader, CardTitle, Spinner } from "@evinvest/uikit";

import type { ConsentDecision } from "@/shared/contracts/payments";
import { errorMessage } from "@/shared/lib/api-client";
import { ResourceError } from "@/shared/ui/resource-error";
import { CodeField } from "@/views/approval/ui/approval-chrome";

export function ConsentDecisionCard({
  email,
  pending,
  rejectedAttempts,
  error,
  onDecide,
}: {
  email: string;
  pending: ConsentDecision | null;
  /** Set only after a code was rejected — the server's figure, never a countdown. */
  rejectedAttempts: number | null;
  error: unknown;
  onDecide: (decision: ConsentDecision, code: string) => Promise<void>;
}) {
  const t = useT();
  const [code, setCode] = useState("");
  const locked = code.trim().length === 0 || pending !== null;

  const decide = async (decision: ConsentDecision) => {
    const secret = code.trim().toUpperCase();
    if (!secret || pending) return;
    await onDecide(decision, secret);
    // Cleared whatever happened: a right code is spent, a wrong one is not worth
    // keeping, and a network failure is retyped from the email anyway.
    setCode("");
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t("consent.decisionTitle", "Your decision")}</CardTitle>
        <CardDescription className="text-balance">{t("consent.decisionLead", "Both answers need the code from your email. Enter it once, then choose. Rejecting keeps your money exactly where it is.")}</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-4">
        <p className="text-xs text-ink-soft">{t("approval.votingAs", "You are answering as {email}.", { email })}</p>

        <CodeField value={code} onChange={setCode} disabled={pending !== null} attemptsRemaining={rejectedAttempts} />

        {error !== null && rejectedAttempts === null && <ResourceError message={errorMessage(error, t)} />}

        {/* Two answers, unequal in weight as well as colour, as on the payout page — but
            here rejecting is the reader keeping their own money where it is, so it is
            never styled as the dangerous one. */}
        <div className="flex flex-col gap-2.5 sm:flex-row">
          <Button size="lg" className="font-semibold sm:flex-1" disabled={locked} aria-busy={pending === "approve"} onClick={() => void decide("approve")}>
            {pending === "approve" && <Spinner aria-hidden />}
            {t("consent.approve", "Approve — move my money")}
          </Button>
          <Button size="lg" variant="outline" className="sm:shrink-0" disabled={locked} aria-busy={pending === "reject"} onClick={() => void decide("reject")}>
            {pending === "reject" && <Spinner aria-hidden />}
            {t("consent.reject", "Reject — keep my money")}
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}
