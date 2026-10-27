import { useT } from "@evinvest/i18n/react";

import { useEffect, useRef, useState } from "react";

import { Alert, AlertDescription, Button, Spinner } from "@evinvest/uikit";

import { clockText, intentCancelled, isPastDeadline, nowMs, secondsLeft, type IntentKind } from "../lib/intent";
import type { IntentFlowControls, Review } from "../model/use-intent-flow";
import { IntentSummaryView } from "./intent-summary";
import { intentErrorText } from "./intent-words";

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
      {t("walletIntent.validFor", "These terms are valid for {time}", { time: clockText(secondsLeft(deadlineMs, now)) })}
    </p>
  );
}

interface StepProps<K extends IntentKind> {
  kind: K;
  controls: IntentFlowControls<K>;
}

function ReviewStep<K extends IntentKind>({ kind, review, controls }: StepProps<K> & { review: Review }) {
  const t = useT();
  const { intent, confirming, error } = review;
  const cancelled = error !== null && intentCancelled(error);
  const busy = confirming || controls.stepping;
  return (
    <div className="flex flex-col gap-4">
      {review.termsChanged && (
        <Alert variant="info">
          <AlertDescription>{t("walletIntent.termsChanged", "The terms changed since you started. Check them again before you confirm.")}</AlertDescription>
        </Alert>
      )}
      <IntentSummaryView kind={kind} summary={intent.summary} />
      {kind === "withdraw" && (
        <p className="m-0 text-xs text-ink-soft">
          {t("walletIntent.withdrawHold", "A withdrawal to an address you have not used before is held for 6 hours. You can cancel it from the email we send you.")}
        </p>
      )}
      {/* No countdown under an open step-up: the intent must not renew beneath it. */}
      {!busy && <Countdown deadlineMs={review.deadlineMs} onExpire={controls.expire} />}
      {error && (
        <Alert variant="destructive" role="alert">
          <AlertDescription>{intentErrorText(error, t)}</AlertDescription>
        </Alert>
      )}
      <div className="flex flex-wrap gap-2">
        {cancelled ? (
          <Button type="button" onClick={controls.renew}>
            {t("walletIntent.startAgain", "Start again")}
          </Button>
        ) : (
          <Button type="button" onClick={controls.confirm} disabled={busy} aria-busy={busy}>
            {busy ? <Spinner aria-hidden /> : null}
            {t("walletIntent.confirm", "Confirm")}
          </Button>
        )}
        <Button type="button" variant="outline" onClick={controls.edit} disabled={busy}>
          {t("ui.back", "Back")}
        </Button>
      </div>
    </div>
  );
}

/** Everything between the form and the result: the BFF's terms, the confirm, an unclear outcome. */
export function ConfirmStep<K extends IntentKind>({ kind, controls }: StepProps<K>) {
  const t = useT();
  const { flow } = controls;
  if (flow.phase === "review") return <ReviewStep kind={kind} review={flow} controls={controls} />;
  if (flow.phase === "expired") {
    return (
      <div className="flex flex-col items-start gap-4">
        <Alert variant="info">
          <AlertDescription>{t("walletIntent.expired", "These terms ran out. Get new ones to continue.")}</AlertDescription>
        </Alert>
        <Button type="button" onClick={controls.renew}>
          {t("walletIntent.renew", "Get new terms")}
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
              ? t("walletIntent.unclear", "Still processing. Nothing is lost — check again in a moment.")
              : t("walletIntent.unclearFinal", "We could not get the result. Check your recent activity before you try again.")}
          </AlertDescription>
        </Alert>
        {flow.askAgain && (
          <Button type="button" onClick={controls.confirm}>
            {t("walletIntent.checkAgain", "Check again")}
          </Button>
        )}
      </div>
    );
  }
  return null;
}
