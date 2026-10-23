"use client";

// "Someone has proposed that you stop being an owner." Reached from an email, by the person
// it is about.
//
// This page has the same machinery as the payout approval and a different job. Nobody
// arrives here pleased, and the writing is the design: plain sentences, no exclamation, no
// reassurance nobody asked for, and no urgency the deadline does not actually create. It
// says who proposed it, what reason they gave, how long there is, and what each answer
// does. It does not editorialise about the proposal, because this page is not the fund's
// argument — it is the notice.
//
// The asymmetry between the two answers is structural rather than decorative. Accepting
// ends an ownership stake and cannot be undone from here or anywhere else, so it is the
// smaller, colder control AND it goes through a second, explicit confirmation. Refusing is
// the reversible answer and takes one press. Neither is preselected, and refusing is not
// dressed up as the "safe" choice — the copy says plainly that refusing does not end the
// matter, because the peers can still decide it without them (docs/CONSILIUM.md, path (b)).

import { CheckCircle2, ShieldCheck } from "lucide-react";
import { useState } from "react";

import { useLocale, useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, CardDescription, CardHeader, CardTitle, Separator, Spinner } from "@evinvest/uikit";

import { ApprovalUnavailableError } from "@/entities/approval/api/approval-client";
import { decideRemoval, removalApprovalResource } from "@/entities/approval/model/approval-resource";
import type { RemovalDecision } from "@/shared/contracts/governance";
import { errorMessage } from "@/shared/lib/api-client";
import { expiresIn, formatMoment, hasExpired } from "@/shared/lib/datetime";
import { settledRemoval } from "@/shared/lib/decision";
import { stripTransportPrefix, wrongCodeAttempts } from "@/shared/lib/hub-refusal";
import { useResource } from "@/shared/lib/resource";
import { ResourceError } from "@/shared/ui/resource-error";
import {
  ApprovalBurned,
  ApprovalExpired,
  ApprovalOutcome,
  ApprovalPage,
  ApprovalSkeleton,
  ApprovalTitle,
  ApprovalUnavailable,
  ApprovalUnreachable,
  CodeField,
  DetailRow,
  FieldCaption,
} from "@/views/approval/ui/approval-chrome";

export function RemovalApprovalView({ token }: { token: string }) {
  const t = useT();
  const locale = useLocale();

  const summary = useResource(removalApprovalResource, token);
  const invitation = summary.data ?? null;

  const [code, setCode] = useState("");
  const [pending, setPending] = useState<RemovalDecision | null>(null);
  const [actionError, setActionError] = useState<unknown>(null);
  const [rejectedAttempts, setRejectedAttempts] = useState<number | null>(null);
  const [justDecided, setJustDecided] = useState(false);
  const [spent, setSpent] = useState(false);
  /** The second, deliberate step in front of the irreversible answer. */
  const [confirming, setConfirming] = useState(false);

  // See the payout view: the wire says "pending", not null, for an unanswered notice.
  const settled = settledRemoval(invitation?.decision);
  // See the payout view: an answered notice keeps its outcome, whatever a later read says.
  const gone = spent || (!settled && summary.error instanceof ApprovalUnavailableError);

  const decide = async (decision: RemovalDecision) => {
    const secret = code.trim().toUpperCase();
    if (!secret || pending) return;
    const attemptsBefore = invitation?.attempts_remaining ?? null;
    setPending(decision);
    setActionError(null);
    // Cleared per attempt, not per success. Left standing it would keep showing a count
    // from the previous try AND suppress the error banner for this one, so a network
    // failure after a wrong code would report nothing at all.
    setRejectedAttempts(null);
    try {
      const result = await decideRemoval(token, secret, decision);
      setCode("");
      setConfirming(false);
      if (result.decided) {
        setJustDecided(true);
        setRejectedAttempts(null);
      } else {
        setRejectedAttempts(result.invitation.attempts_remaining);
      }
    } catch (cause) {
      if (cause instanceof ApprovalUnavailableError) {
        setSpent(true);
        return;
      }
      setActionError(cause);
      setCode("");
      setConfirming(false);
      // The same refusal the payout page meets (banking#324): the plane's count of attempts
      // left, behind the BFF's prefix, goes under the field rather than out as a sentence.
      const attemptsLeft = wrongCodeAttempts(cause);
      if (attemptsLeft !== null) setRejectedAttempts(attemptsLeft);
      // The server owns the attempt count; a failure here may or may not have spent one.
      // Read through `settledRemoval`: an open seat arrives as the truthy "pending".
      await summary.refresh();
      const fresh = removalApprovalResource.peek(token);
      if (fresh && attemptsBefore !== null && fresh.attempts_remaining < attemptsBefore && !settledRemoval(fresh.decision)) {
        setRejectedAttempts(fresh.attempts_remaining);
      }
    } finally {
      setPending(null);
    }
  };

  if (gone) {
    return (
      <ApprovalPage>
        <ApprovalUnavailable />
      </ApprovalPage>
    );
  }

  if (!invitation) {
    return (
      <ApprovalPage>
        {summary.isLoading ? (
          <ApprovalSkeleton />
        ) : (
          <ApprovalUnreachable onRetry={() => void summary.refresh()} retrying={summary.isValidating} />
        )}
      </ApprovalPage>
    );
  }

  // An omitted `attempts_remaining` is a zero on this wire, and a zero is a burned token.
  const burned = !settled && (invitation.attempts_remaining ?? 0) <= 0;
  const expired = !settled && hasExpired(invitation.expires_at);

  return (
    <ApprovalPage>
      <Card>
        <CardHeader>
          <ApprovalTitle>{t("approval.removal.title", "A request to end your ownership")}</ApprovalTitle>
          <CardDescription className="text-balance">
            {t("approval.removal.lead", "{initiator} has proposed that you stop being an owner of the fund. You have a say in it, and there is a deadline.", { initiator: invitation.initiator_email })}
          </CardDescription>
        </CardHeader>

        <CardContent className="flex flex-col gap-5">
          <div className="flex flex-col gap-1.5">
            <FieldCaption>{t("approval.removal.reasonLabel", "Reason given")}</FieldCaption>
            {/* The reason is someone else's words about the reader. It is shown whole, in
                their own phrasing, with no summarising and no quotation marks that would
                let the fund distance itself from it. */}
            <p className="whitespace-pre-line rounded-lg bg-secondary px-3.5 py-3 text-sm leading-relaxed text-ink">
              {invitation.reason?.trim() || t("approval.removal.noReason", "No reason was given.")}
            </p>
          </div>

          <div className="flex flex-col gap-2.5">
            <DetailRow label={t("approval.removal.proposedBy", "Proposed by")} value={invitation.initiator_email} />
            <DetailRow label={t("approval.removal.about", "About")} value={invitation.target_email} />
            <DetailRow label={t("approval.openedAt", "Opened")} value={formatMoment(invitation.created_at, locale)} />
            <DetailRow
              label={t("approval.expires", "Expires")}
              value={t("approval.expiresValue", "{at} · {left}", { at: formatMoment(invitation.expires_at, locale), left: expiresIn(invitation.expires_at, t) })}
              tone={expired ? "text-accent-error" : undefined}
            />
          </div>
        </CardContent>
      </Card>

      {burned ? (
        <ApprovalBurned />
      ) : expired ? (
        <ApprovalExpired />
      ) : settled ? (
        <ApprovalOutcome
          icon={settled === "remove" ? <CheckCircle2 /> : <ShieldCheck />}
          title={(settled === "remove" ? t("approval.removal.accepted.title", "You gave up your seat") : t("approval.removal.refused.title", "You refused this"))}
          description={(settled === "remove" ? justDecided ? t("approval.removal.accepted.freshBody", "Your ownership has ended and the other owners have been told. Nothing else about your account has changed.") : t("approval.removal.accepted.body", "You accepted this earlier. Your ownership has ended.") : t("approval.removal.refused.body", "Your answer has been recorded. You keep your seat unless every other eligible owner agrees to end it, and you will be told either way."))}
        />
      ) : (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t("approval.removal.decisionTitle", "Your answer")}</CardTitle>
            <CardDescription className="text-balance">{t("approval.removal.decisionLead", "You can accept this or refuse it, and either answer needs the code from your email. Refusing does not end the matter: the other owners can still decide it between them, and that takes all of them agreeing.")}</CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-4">
            <CodeField value={code} onChange={setCode} disabled={pending !== null} attemptsRemaining={rejectedAttempts} />

            {actionError !== null && rejectedAttempts === null && <ResourceError message={stripTransportPrefix(errorMessage(actionError, t))} />}

            {confirming ? (
              // The second step. It restates the consequence in full rather than asking
              // "are you sure?" — a reader who has already decided is not helped by being
              // asked again, only by being told exactly what is about to happen.
              <div className="flex flex-col gap-3.5 rounded-lg border border-accent-error/40 bg-accent-error/10 p-4">
                <div className="flex flex-col gap-1.5">
                  <p className="text-sm font-semibold text-ink">{t("approval.removal.confirmTitle", "This ends your ownership")}</p>
                  <p className="text-sm leading-relaxed text-ink">{t("approval.removal.confirmBody", "Your owner seat ends immediately and cannot be restored from here. You would need the remaining owners to make you an owner again. Nothing else about your account changes.")}</p>
                </div>
                <div className="flex flex-col gap-2.5 sm:flex-row">
                  <Button variant="destructive" className="sm:flex-1" disabled={pending !== null} onClick={() => void decide("remove")}>
                    {pending === "remove" && <Spinner aria-hidden />}
                    {t("approval.removal.confirmAction", "Give up my seat")}
                  </Button>
                  <Button variant="ghost" disabled={pending !== null} onClick={() => setConfirming(false)}>
                    {t("ui.cancel", "Cancel")}
                  </Button>
                </div>
              </div>
            ) : (
              <>
                <Separator />
                {/* Refuse first and larger: it is the reversible answer, and the reversible
                    answer is the one a mis-click should land on. Accepting is a distinct,
                    smaller, destructive control that opens the confirmation above rather
                    than submitting. */}
                <div className="flex flex-col gap-2.5 sm:flex-row">
                  <Button
                    size="lg"
                    variant="outline"
                    className="font-semibold sm:flex-1"
                    disabled={code.trim().length === 0 || pending !== null}
                    onClick={() => void decide("keep")}
                  >
                    {pending === "keep" && <Spinner aria-hidden />}
                    {t("approval.removal.refuse", "Refuse — keep my seat")}
                  </Button>
                  <Button
                    variant="destructive"
                    disabled={code.trim().length === 0 || pending !== null}
                    onClick={() => setConfirming(true)}
                  >
                    {t("approval.removal.accept", "Accept and give up my seat")}
                  </Button>
                </div>
                <p className="text-xs leading-relaxed text-ink-soft">{t("approval.removal.irreversible", "Accepting is final. Your ownership ends as soon as it is recorded, and it cannot be undone from this page or any other.")}</p>
              </>
            )}
          </CardContent>
        </Card>
      )}

      <p className="text-center text-xs text-ink-soft">{t("approval.footnote", "This link was made for you alone. It works once, expires 72 hours after it was sent, and should not be forwarded.")}</p>
    </ApprovalPage>
  );
}
