"use client";

// "This is your money — do you agree to it moving?" — reached from an email, with no
// session and no way back.
//
// The same two facts from docs/CONSILIUM.md shape it as shape the payout approval: the
// GET is inert (policy 5), and the reader agrees to what they can see (policy 12–13). One
// thing is different and every word carries it — the money is the READER's, not the
// fund's. Nobody else is asked, nobody else can answer, and the initiator is named as the
// person asking, in the reader's own terms: who wants it, from where to where, and why.
//
// Everything counted — attempts, the settled answer — is read back from the server
// (`useConsentDecision`); this page never decrements, increments or predicts any of it.

import { CheckCircle2, XCircle } from "lucide-react";

import { useLocale, useT } from "@evinvest/i18n/react";
import { Card, CardContent, CardDescription, CardHeader } from "@evinvest/uikit";

import { ApprovalUnavailableError } from "@/entities/approval/api/approval-client";
import { consentApprovalResource } from "@/entities/approval/model/approval-resource";
import { expiresIn, formatMoment, hasExpired } from "@/shared/lib/datetime";
import { settledConsent } from "@/shared/lib/decision";
import { useResource } from "@/shared/lib/resource";
import { useConsentDecision } from "@/views/approval/model/use-consent-decision";
import {
  ApprovalBurned,
  ApprovalExpired,
  ApprovalOutcome,
  ApprovalPage,
  ApprovalSkeleton,
  ApprovalTitle,
  ApprovalUnavailable,
  ApprovalUnreachable,
  ApprovalUnrenderable,
  DetailRow,
} from "@/views/approval/ui/approval-chrome";
import { ConsentDecisionCard } from "@/views/approval/ui/consent-decision";
import { PaymentTermsBlock, renderablePayment } from "@/views/approval/ui/payment-terms";

export function ConsentApprovalView({ token }: { token: string }) {
  const t = useT();
  const locale = useLocale();
  const summary = useResource(consentApprovalResource, token);
  const invitation = summary.data ?? null;

  const { pending, error, rejectedAttempts, justDecided, spent, decide } = useConsentDecision(token, invitation, summary.refresh);

  // NOT `?? null`: an unanswered seat arrives as the truthy string "pending" (`shared/lib/decision.ts`).
  const settled = settledConsent(invitation?.decision);
  // Once answered, the outcome stands: a background revalidation 404s the moment the token
  // is spent, and flipping "you agreed" into "this link is dead" would be alarming and wrong.
  const gone = spent || (!settled && summary.error instanceof ApprovalUnavailableError);

  if (gone) return <ApprovalPage><ApprovalUnavailable /></ApprovalPage>;
  if (!invitation) {
    return (
      <ApprovalPage>
        {summary.isLoading ? <ApprovalSkeleton /> : <ApprovalUnreachable onRetry={() => void summary.refresh()} retrying={summary.isValidating} />}
      </ApprovalPage>
    );
  }
  if (!renderablePayment(invitation)) {
    return (
      <ApprovalPage>
        <ApprovalUnrenderable description={t("consent.unavailableBody", "The terms of this payment didn't arrive, so there is nothing here to agree to and no decision is offered. This is a problem on our side, not with your link. Try again in a moment.")} onRetry={() => void summary.refresh()} retrying={summary.isValidating} />
      </ApprovalPage>
    );
  }

  const burned = !settled && (invitation.attempts_remaining ?? 0) <= 0;
  const expired = !settled && hasExpired(invitation.expires_at);

  return (
    <ApprovalPage>
      <Card>
        <CardHeader>
          <ApprovalTitle>{t("consent.title", "Your money is asked to move")}</ApprovalTitle>
          <CardDescription className="text-balance">
            {/* Two keys, one sentence each: the first is the whole point of the page, and it
                carries its weight in markup rather than in capitals a screen reader spells
                out letter by letter. */}
            <strong className="font-semibold text-ink">{t("consent.leadOwn", "This is your money.")}</strong> {t("consent.lead", "{initiator} is asking for it to be moved as set out below. Nothing happens unless you agree — nobody else is asked, and nobody else can answer for you.", { initiator: invitation.initiator_email })}
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-5">
          <PaymentTermsBlock terms={invitation} payloadHash={invitation.payload_hash} reasonLabel={t("consent.reasonLabel", "Reason given by {initiator}", { initiator: invitation.initiator_email })} tierAs="hint" />
          <div className="flex flex-col gap-2.5">
            <DetailRow label={t("consent.askedBy", "Asked by")} value={invitation.initiator_email} />
            <DetailRow label={t("consent.yourMoney", "Whose money")} value={invitation.subject_email} />
            <DetailRow
              label={t("approval.expires", "Expires")}
              value={t("approval.expiresValue", "{at} · {left}", { at: formatMoment(invitation.expires_at, locale), left: expiresIn(invitation.expires_at, t) })}
              tone={expired ? "text-accent-error" : undefined}
            />
          </div>
        </CardContent>
      </Card>

      {burned ? (
        <ApprovalBurned description={t("consent.burned.body", "The code was entered incorrectly too many times, so the link was closed. Nothing was approved and your money has not moved. Whoever asked can open a new request.")} />
      ) : expired ? (
        <ApprovalExpired />
      ) : settled ? (
        <ApprovalOutcome
          icon={settled === "approve" ? <CheckCircle2 /> : <XCircle />}
          tone={settled === "approve" ? "text-positive" : "text-ink-soft"}
          title={(settled === "approve" ? t("consent.decided.approvedTitle", "You approved this payment") : t("consent.decided.rejectedTitle", "You rejected this payment"))}
          description={(justDecided ? settled === "approve" ? t("consent.decided.approvedFresh", "Your answer has been recorded and the payment goes ahead. There is nothing further to do here.") : t("consent.decided.rejectedFresh", "Your answer has been recorded. Your money stays exactly where it is, and whoever asked has been told.") : t("approval.decided.body", "You answered this earlier, and that answer stands. An answer cannot be changed once it is given."))}
        />
      ) : (
        <ConsentDecisionCard email={invitation.subject_email} pending={pending} rejectedAttempts={rejectedAttempts} error={error} onDecide={decide} />
      )}

      <p className="text-center text-xs text-ink-soft">{t("approval.footnote", "This link was made for you alone. It works once, expires 72 hours after it was sent, and should not be forwarded.")}</p>
    </ApprovalPage>
  );
}
