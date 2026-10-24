import { useT } from "@evinvest/i18n/react";

import { useEffect, useRef, useState } from "react";

import { Alert, AlertDescription, AlertTitle, Button, Spinner } from "@evinvest/uikit";

import { clockText, secondsLeft, type IntentKind } from "../lib/intent";
import type { IntentFlowControls, Review } from "../model/use-intent-flow";
import { IntentSummaryView } from "./intent-summary";
import { intentErrorText, stepupMethodsText } from "./intent-words";

const TICK_MS = 1000;

/** Seconds left on the intent; calls `onExpire` once when they run out. */
function Countdown({ deadlineMs, onExpire }: { deadlineMs: number; onExpire: () => void }) {
  const t = useT();
  const [now, setNow] = useState(() => Date.now());
  const expire = useRef(onExpire);
  useEffect(() => {
    expire.current = onExpire;
  }, [onExpire]);
  useEffect(() => {
    let fired = false;
    const tick = () => {
      const at = Date.now();
      setNow(at);
      if (!fired && secondsLeft(deadlineMs, at) === 0) {
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

function ReviewStep({ kind, review, controls, onFallback }: { kind: IntentKind; review: Review; controls: IntentFlowControls; onFallback: () => void }) {
  const t = useT();
  const { intent, confirming, error } = review;
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
      {!confirming && <Countdown deadlineMs={review.deadlineMs} onExpire={controls.expire} />}
      {error && (
        <Alert variant="destructive" role="alert">
          <AlertDescription>{intentErrorText(error, t)}</AlertDescription>
        </Alert>
      )}
      {intent.stepup.required ? (
        <Alert variant="info">
          <AlertTitle>{t("walletMfe.stepupTitle", "One more check is needed")}</AlertTitle>
          <AlertDescription>
            {stepupMethodsText(intent.stepup.methods, t)} {t("walletMfe.stepupElsewhere", "This page cannot ask for it yet, so finish on EV Invest.")}
          </AlertDescription>
        </Alert>
      ) : null}
      <div className="flex flex-wrap gap-2">
        {intent.stepup.required ? (
          <Button type="button" onClick={onFallback}>
            {t("walletMfe.continueOnEv", "Continue on EV Invest")}
          </Button>
        ) : (
          <Button type="button" onClick={controls.confirm} disabled={confirming} aria-busy={confirming}>
            {confirming ? <Spinner aria-hidden /> : null}
            {t("walletMfe.confirm", "Confirm")}
          </Button>
        )}
        <Button type="button" variant="outline" onClick={controls.edit} disabled={confirming}>
          {t("walletMfe.change", "Change")}
        </Button>
      </div>
    </div>
  );
}

/** Everything after the form: the BFF's terms, the confirm, and what came of it. */
export function ConfirmStep({ kind, controls, onClose, onFallback }: { kind: IntentKind; controls: IntentFlowControls; onClose: () => void; onFallback: () => void }) {
  const t = useT();
  const { flow } = controls;
  if (flow.phase === "review") return <ReviewStep kind={kind} review={flow} controls={controls} onFallback={onFallback} />;
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
