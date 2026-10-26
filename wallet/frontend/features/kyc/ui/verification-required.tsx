"use client";

import { useT } from "@evinvest/i18n/react";

import { ShieldCheck } from "lucide-react";
import { useState } from "react";

import { Button, Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle, Skeleton } from "@evinvest/uikit";

import { useKycStatus } from "@/features/kyc/model/use-kyc-status";
import { VerificationDialog } from "@/features/kyc/ui/verification-dialog";
import { cn } from "@/shared/lib/cn";
import { SupportLink } from "@/shared/ui/support-link";

/**
 * What stands where a money surface would be for a caller the hub has not cleared yet.
 *
 * Not a failure box. The hub refuses a deposit address and a withdrawal below tier 1 on
 * purpose, and it says so distinctly — but the deposit screen flattened that into "deposit
 * address unavailable" over "this rail is still being provisioned", on every rail at once.
 * That reads as an outage the reader can only wait out, for a state they can leave in one
 * click. Withdraw had the mirror of it: a form that filled in completely and refused on
 * submit.
 *
 * Title and description are the caller's because what is closed differs between the screens;
 * the way out is identical, so it lives here — and it is the SAME dialog the profile card and
 * Home's checklist open (#213/#215/#382), so the account of what verification involves is
 * written once rather than three times.
 *
 * Parity with those two is not only the dialog, though: it is also the question of whether a
 * start may be offered AT ALL. A caller with a running case that cannot be resumed holds the
 * start gate on the plane, so the button here could only fail — the wrong-cause refusal #215
 * exists to remove, arrived at from a different direction. `useKycStatus` answers that for
 * all three surfaces; this block is the one that serves the money screens, which is where the
 * doomed click costs a paid vendor session.
 */
export function VerificationRequired({ title, description, className }: { title: string; description: string; className?: string }) {
  const t = useT();
  const { runningCase, canStart, loading } = useKycStatus();
  const [open, setOpen] = useState(false);

  return (
    // uikit's Empty draws a dashed frame but leaves the border width to the caller, and
    // doubles its padding at `md` — the call the dashboard already makes.
    <Empty className={cn("border md:p-6", className)}>
      <EmptyHeader>
        <EmptyMedia variant="icon">
          <ShieldCheck />
        </EmptyMedia>
        <EmptyTitle>{title}</EmptyTitle>
        <EmptyDescription>{loading || canStart || runningCase === null ? description : t("profile.kyc.caseReviewSub", "Your documents are with our reviewers — we'll email you the decision")}</EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        {loading ? (
          <Skeleton className="h-9 w-32 rounded-md" />
        ) : canStart ? (
          <>
            {/* i18n-max: 14 — the uikit Button is shrink-0. The budget is the one the old
                checklist row set, the tightest frame this key rendered in. */}
            <Button type="button" onClick={() => setOpen(true)}>
              {t("kyc.verifyNow", "Verify now")}
            </Button>
            <VerificationDialog open={open} onOpenChange={setOpen} />
          </>
        ) : (
          // Waiting on a verdict, with no session left to re-enter. A button here could only
          // fail, so what the screen owes this reader is a person to ask — the same mailbox
          // the profile row and the dialog's `unavailable` outcome offer.
          <SupportLink className="text-sm" />
        )}
      </EmptyContent>
    </Empty>
  );
}
