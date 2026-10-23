"use client";

import { useT } from "@evinvest/i18n/react";

import { useState } from "react";

import { Button, Skeleton } from "@evinvest/uikit";

import { useKycStatus } from "@/features/kyc/model/use-kyc-status";
import { VerificationDialog } from "@/features/kyc/ui/verification-dialog";
import { Hairline, Pill, RowLabel } from "@/shared/ui/list-card";
import { Settled } from "@/shared/ui/motion";
import { config } from "@/config";
import { platformAccountHref } from "@/shared/config/platform";
import { useSession } from "@/shared/lib/use-session";
import { SupportLink } from "@/shared/ui/support-link";

// Where a user begins — or picks up — identity verification from their profile. It sits inside
// `card-Verification`, below the rows that state what the hub currently holds, so the card
// reads as status-then-action rather than as a form.
//
// Four states, because the tier alone was never enough to tell them apart (#190):
//
//   · nothing read yet — a skeleton, like the two rows above it. `/kyc/status` is its own
//     resource with no persisted value, so it starts reading when THIS row mounts, which is
//     already after the profile landed: the window is real, and guessing through it renders
//     the live Start button at exactly the user this row exists to stop — the one who just
//     came back from the vendor;
//   · nothing running — the invitation;
//   · a running case the user can re-enter — the same action, worded as a continuation. A
//     repeat start returns the SAME vendor session (concierge#55), so this costs nothing;
//   · a running case with nowhere to return to — no button, because it could only fail. What
//     stands in its place is a pending Pill and a way to reach a person: a waiting state that
//     is a grey sentence and an empty slot tells the reader neither what is happening nor
//     what they may still do.
//
// The button opens the dialog rather than starting: what the user is about to hand over, and
// how long it takes, is said once and in one place (#213) — this row is the entry, not the
// explanation.

export function StartVerificationRow() {
  const t = useT();
  const { level, runningCase, canStart, loading } = useKycStatus();
  const [open, setOpen] = useState(false);
  const unverified = useSession()?.user?.emailVerified === false;

  // The plane can know the tier has moved before the profile's mirror of it does; when it
  // does, this row is already history and must not offer to spend another vendor session.
  if (level > 0) return null;

  // A level above 0 needs a verified email, so the check would be paid for nothing: the
  // reader proves the address first.
  if (unverified) {
    return (
      <>
        <Hairline />
        <div className="flex min-w-0 items-center justify-between gap-3 py-3.5">
          <RowLabel title={t("profile.kyc.verifyEmailTitle", "Verify your email first")} sub={t("profile.kyc.verifyEmailSub", "Identity checks open once your address is confirmed")} />
          {/* Concierge owns the account, so this leaves the cabinet: a plain anchor, no locale or base path. */}
          <a href={platformAccountHref(config.public.platformUrl, "security")} className="shrink-0 text-sm font-semibold text-primary-ink">
            {t("auth.email.verify", "Verify")}
          </a>
        </div>
      </>
    );
  }

  const running = runningCase !== null;
  // Alive, holding the start gate, and with no vendor session left to re-enter.
  const reviewing = running && !canStart;
  return (
    <>
      <Hairline />
      <div className="flex flex-col py-3.5">
        <Settled loading={loading} skeleton={<RowSkeleton />}>
          {loading ? null : (
            <>
              <div className="flex min-w-0 items-center justify-between gap-3">
                <RowLabel
                  title={running ? t("profile.kyc.caseTitle", "Verification in progress") : t("profile.kyc.startTitle", "Verify your identity")}
                  sub={running ? (canStart ? t("profile.kyc.caseResumeSub", "You've already started — pick up where you left off") : t("profile.kyc.caseReviewSub", "Your documents are with our reviewers — we'll email you the decision")) : t("profile.kyc.startSub", "A photo of your ID and a selfie — a few minutes")}
                />
                {/* i18n-max: 12 — a `shrink-0` Button or Pill beside the `min-w-0` row label. */}
                {canStart ? (
                  <Button type="button" size="sm" className="shrink-0 rounded-lg font-semibold" onClick={() => setOpen(true)}>
                    {running ? t("profile.kyc.continue", "Continue") : t("profile.kyc.start", "Start")}
                  </Button>
                ) : reviewing ? (
                  <Pill tone="pending">{t("profile.kyc.casePill", "In review")}</Pill>
                ) : null}
              </div>
              {reviewing && <ReviewContact />}
            </>
          )}
        </Settled>
      </div>
      <VerificationDialog open={open} onOpenChange={setOpen} />
    </>
  );
}

/** The same two lines and trailing chip the rows above this one show while they read. */
function RowSkeleton() {
  return (
    <div className="flex min-w-0 items-center justify-between gap-3">
      <div className="flex min-w-0 flex-col gap-1.5">
        <Skeleton className="h-4 w-36" />
        <Skeleton className="h-3 w-52 max-w-full" />
      </div>
      <Skeleton className="h-5 w-16 shrink-0 rounded-full" />
    </div>
  );
}

/**
 * Waiting on a verdict is not a dead end. The reviewers answer by email, but "we'll email
 * you" is a promise with no handle on it — this is the handle, and it is the same mailbox the
 * dialog's `unavailable` outcome offers, so a reader who is stuck reaches one place either way.
 */
function ReviewContact() {
  return (
    <p className="mt-2 text-xs leading-snug text-ink-soft">
      <SupportLink />
    </p>
  );
}
