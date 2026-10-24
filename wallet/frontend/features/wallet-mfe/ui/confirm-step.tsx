import { useT } from "@evinvest/i18n/react";

import { useEffect, useRef, useState } from "react";

import { Alert, AlertDescription, AlertTitle, Button, Spinner } from "@evinvest/uikit";

import { clockText, intentCancelled, isPastDeadline, nowMs, secondsLeft, type IntentKind } from "../lib/intent";
import type { StepupClient } from "../model/stepup-client";
import type { IntentFlowControls, Review } from "../model/use-intent-flow";
import { IntentSummaryView } from "./intent-summary";
import { intentErrorText } from "./intent-words";
import { StepupPanel } from "./stepup-panel";

const TICK_MS = 1000;

/** Seconds left on the intent; calls `onExpire` once when they run out. */
function Countdown({ deadlineMs, onExpire }: { deadlineMs: number; onExpire: () => void }) {
  const t = useT();
  const [now, setNow] = useState(nowMs);
  const expire = useRef(onExpire);
  useEffect(() => {
    expire.current = onExpire;
  }, [onExpire]);
  useEffect(() => {
    let fired = false;
    const tick = () => {
      const at = nowMs();
      setNow(at);
      if (!fired && isPastDeadline(deadlineMs, at)) {
        fired = true;
        expire.current();
      }
    };
    const timer = setInterval(tick, TICK_MS);
    return () => clearInterval(timer);
  }, [deadlineMs]);
  return (
    <p role="timer" className="m-0 text-xs text-ink-soft tabular-nums">
      {t("walletMfe.validFor", "These terms are valid for {time}", { time: clockText(secondsLeft(deadlineMs, now)) })}
    </p>
  );
}

interface StepProps {
  kind: IntentKind;
  controls: IntentFlowControls;
  stepup: StepupClient;
  onFallback: () => void;
}

function ReviewStep({ kind, review, controls, stepup, onFallback }: StepProps & { review: Review }) {
  const t = useT();
  const { intent, confirming, error } = review;
  const cancelled = error !== null && intentCancelled(error);
  return (
    <div className="flex flex-col gap-4">
      {review.termsChanged && (
        <Alert variant="info">
          <AlertDescription>{t("walletMfe.termsChanged", "The terms changed since you started. Check them again before you confirm.")}</AlertDescription>
        </Alert>
      )}
      <IntentSummaryView kind={kind} summary={intent.summary} />
      {kind === "withdraw" && (
        <p className="m-0 text-xs text-ink-soft">
          {t("walletMfe.withdrawHold", "A withdrawal to an address you have not used before is held for 6 hours. You can cancel it from the email we send you.")}
        </p>
      )}
      {/* No countdown under an open passkey prompt: the intent must not renew beneath it. */}
      {!confirming && !controls.stepping && <Countdown deadlineMs={review.deadlineMs} onExpire={controls.expire} />}
      {error && (
        <Alert variant="destructive" role="alert">
          <AlertDescription>{intentErrorText(error, t)}</AlertDescription>
        </Alert>
      )}
      {intent.stepup.required && !cancelled && <StepupPanel kind={kind} review={review} controls={controls} client={stepup} onFallback={onFallback} />}
      <div className="flex flex-wrap gap-2">
        {cancelled ? (
          <Button type="button" onClick={controls.renew}>
            {t("walletMfe.startAgain", "Start again")}
          </Button>
        ) : intent.stepup.required ? null : (
          <Button type="button" onClick={controls.confirm} disabled={confirming} aria-busy={confirming}>
            {confirming ? <Spinner aria-hidden /> : null}
            {t("walletMfe.confirm", "Confirm")}
          </Button>
        )}
        <Button type="button" variant="outline" onClick={controls.edit} disabled={confirming || controls.stepping}>
          {t("walletMfe.change", "Change")}
        </Button>
      </div>
    </div>
  );
}

/** Everything after the form: the BFF's terms, the confirm, and what came of it. */
export function ConfirmStep({ kind, controls, stepup, onClose, onFallback }: StepProps & { onClose: () => void }) {
  const t = useT();
  const { flow } = controls;
  if (flow.phase === "review") return <ReviewStep kind={kind} review={flow} controls={controls} stepup={stepup} onFallback={onFallback} />;
  if (flow.phase === "expired") {
    return (
      <div className="flex flex-col items-start gap-4">
        <Alert variant="info">
          <AlertDescription>{t("walletMfe.expired", "These terms ran out. Get new ones to continue.")}</AlertDescription>
        </Alert>
        <Button type="button" onClick={controls.renew}>
          {t("walletMfe.renew", "Get new terms")}
        </Button>
      </div>
    );
  }
  if (flow.phase === "unclear") {
    return (
      <div className="flex flex-col items-start gap-4">
        <Alert variant="info" role="status">
          <AlertDescription>
            {flow.askAgain
              ? t("walletMfe.unclear", "Still processing. Nothing is lost — check again in a moment.")
              : t("walletMfe.unclearFinal", "We could not get the result. Check your recent activity before you try again.")}
          </AlertDescription>
        </Alert>
        {flow.askAgain && (
          <Button type="button" onClick={controls.confirm}>
            {t("walletMfe.checkAgain", "Check again")}
          </Button>
        )}
      </div>
    );
  }
  if (flow.phase === "done") {
    return (
      <div className="flex flex-col items-start gap-4">
        <Alert variant="success" role="status">
          <AlertTitle>{t("walletAction.done", "Done")}</AlertTitle>
        </Alert>
        <Button type="button" onClick={onClose}>
          {t("walletMfe.close", "Close")}
        </Button>
      </div>
    );
  }
  return null;
}
