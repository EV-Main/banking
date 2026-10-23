"use client";

import { useT } from "@evinvest/i18n/react";

import { Clock, IdCard, ShieldCheck } from "lucide-react";

import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@evinvest/uikit";

import { useStartVerification } from "@/features/kyc/model/use-start-verification";
import { Step } from "@/features/kyc/ui/step";
import { StartVerificationButton, VerificationOutcome } from "@/features/kyc/ui/verification-controls";

// The one explanation of verification in the cabinet, and the one place its primary action
// lives. Every entry point — the profile's identity card, Home's checklist, and the wallet —
// opens THIS, rather than each writing its own account of what the user is about to do.
//
// It is centred (`Dialog`, not `Sheet` or `Drawer`, both of which are edge-anchored) because
// this is a thing the reader stops to read, not a panel they work alongside.
//
// It is controlled rather than carrying its own trigger: the checklist's trigger sits in a
// step row, the profile's is a row button, and the wallet's is a link inside a rail — one
// `DialogTrigger` could not be all three. Being controlled is also why it is mounted beside each trigger and
// stays mounted while closed, which is what `reset` below is for.

export function VerificationDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  const t = useT();
  const start = useStartVerification();

  function change(next: boolean) {
    // A start in flight cannot be called back: the vendor session is created by the request,
    // and the answer navigates this tab. Closing here would not cancel anything — it would
    // only hide the fact that the page is about to leave, so that a reader who pressed Esc
    // and moved on lands at the provider a second later. Esc, the overlay and the close
    // button all arrive here, so holding the dialog open is the one place to say no.
    if (!next && start.starting) return;
    // The outcome belongs to the attempt, not to the component: this instance outlives the
    // close, and `unavailable` is what nearly every reader gets today (verification is
    // deployed unconfigured). Re-opening must not replay it as though it had just happened.
    if (!next) start.reset();
    onOpenChange(next);
  }

  return (
    <Dialog open={open} onOpenChange={change}>
      {/* No `max-w-lg`: `DIALOG_CONTENT` already carries `sm:max-w-lg`, and passing the
          unprefixed class made twMerge drop the `max-w-[calc(100%-2rem)]` beside it — which
          is the only thing keeping a phone's dialog off both edges of the screen. */}
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t("kyc.dialog.title", "Verify your identity")}</DialogTitle>
          {/* The gate as the hub actually applies it (#179): a deposit ADDRESS is what is
              withheld, so this must not say "you cannot receive funds" — someone who was
              verified once already holds an address and is still credited. */}
          <DialogDescription>{t("kyc.dialog.intro", "A short identity check by our verification provider. Your deposit address and withdrawals open once it is done.")}</DialogDescription>
        </DialogHeader>

        <div className="flex flex-col gap-4">
          <Step icon={IdCard} title={t("kyc.dialog.needTitle", "What you'll need")} body={t("kyc.dialog.needBody", "A government-issued ID, and a phone or webcam for a selfie.")} />
          <Step icon={Clock} title={t("kyc.dialog.timeTitle", "How long it takes")} body={t("kyc.dialog.timeBody", "A few minutes to submit. Most checks are decided within the hour.")} />
          <Step icon={ShieldCheck} title={t("kyc.dialog.afterTitle", "What happens next")} body={t("kyc.dialog.afterBody", "We'll email you the result. You can close the tab — the check keeps running without it.")} />
        </div>

        <DialogFooter>
          {/* Inside the footer rather than above it. The live region is mounted empty and
              stays mounted — `role="status"` inserted in the same tick as its own text is
              unreliably announced, which also rules out `empty:hidden`, since a
              `display: none` region is out of the accessibility tree entirely. So it always
              occupies a slot, and the footer's `gap-2` is the cheapest slot in the dialog;
              as its own child of the `gap-4` grid it cost twice that, permanently, for a
              message that is absent on every normal run. */}
          <div className="sm:mr-auto">
            <VerificationOutcome start={start} />
          </div>
          <StartVerificationButton start={start} label={t("kyc.dialog.action", "Start verification")} className="w-full sm:w-auto" />
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
