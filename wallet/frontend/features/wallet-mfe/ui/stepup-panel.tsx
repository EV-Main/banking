import { useT } from "@evinvest/i18n/react";

import { Alert, AlertDescription, AlertTitle, Button, Spinner } from "@evinvest/uikit";

import { passkeysSupported } from "@/shared/lib/webauthn";

import type { IntentKind } from "../lib/intent";
import { needsFreshIntent, pageMethods, type StepupError } from "../lib/stepup";
import type { StepupClient } from "../model/stepup-client";
import type { IntentFlowControls, Review } from "../model/use-intent-flow";
import { usePasskeyStepup } from "../model/use-passkey-stepup";
import { stepupErrorText, stepupMethodsText } from "./intent-words";

// The emailed code stays on EV Invest for now: it needs a Turnstile widget, so a site key in
// this bundle and each host's CSP allowing Cloudflare's script and frame. Neither exists yet,
// so an intent this page can only confirm by code goes to the popup.
const EMAIL_CODE_IN_PAGE = false;

/** What the person can do next after a failed step-up. */
function nextStep(error: StepupError): "start_again" | "try_again" | "elsewhere" {
  if (needsFreshIntent(error) || error.code === "captcha") return "start_again";
  if (error.code === "unavailable" || error.code === "authenticator_failed" || error.code === "signed_out") return "elsewhere";
  return "try_again";
}

function Elsewhere({ methods, onFallback }: { methods: Review["intent"]["stepup"]["methods"]; onFallback: () => void }) {
  const t = useT();
  return (
    <>
      <Alert variant="info">
        <AlertTitle>{t("walletMfe.stepupTitle", "One more check is needed")}</AlertTitle>
        <AlertDescription>
          {stepupMethodsText(methods, t)} {t("walletMfe.stepupElsewhere", "This page cannot ask for it yet, so finish on EV Invest.")}
        </AlertDescription>
      </Alert>
      <Button type="button" className="self-start" onClick={onFallback}>
        {t("walletMfe.continueOnEv", "Continue on EV Invest")}
      </Button>
    </>
  );
}

function PasskeyStep({ review, controls, client, onFallback }: { review: Review; controls: IntentFlowControls; client: StepupClient; onFallback: () => void }) {
  const t = useT();
  const { state, run } = usePasskeyStepup(client, review.intent, controls.confirmWith);
  const busy = state.status === "working" || review.confirming;
  const failed = state.status === "failed" ? state.error : null;
  const step = failed ? nextStep(failed) : "try_again";
  return (
    <>
      <Alert variant="info">
        <AlertTitle>{t("walletMfe.stepupTitle", "One more check is needed")}</AlertTitle>
        <AlertDescription>{t("walletMfe.stepupWithPasskey", "Confirm it with your passkey.")}</AlertDescription>
      </Alert>
      {failed && (
        <Alert variant="destructive" role="alert">
          <AlertDescription>{stepupErrorText(failed, t)}</AlertDescription>
        </Alert>
      )}
      <div className="flex flex-wrap gap-2">
        {step === "start_again" ? (
          <Button type="button" onClick={controls.renew}>
            {t("walletMfe.startAgain", "Start again")}
          </Button>
        ) : (
          <Button type="button" onClick={() => void run()} disabled={busy} aria-busy={busy}>
            {busy ? <Spinner aria-hidden /> : null}
            {failed ? t("walletMfe.tryAgain", "Try again") : t("walletMfe.confirmPasskey", "Confirm with passkey")}
          </Button>
        )}
        {(step === "elsewhere" || failed?.code === "no_passkey") && (
          <Button type="button" variant="outline" onClick={onFallback}>
            {t("walletMfe.continueOnEv", "Continue on EV Invest")}
          </Button>
        )}
      </div>
    </>
  );
}

/**
 * The step-up an intent asks for, by the methods the BFF listed for it and nothing else: no
 * threshold is known here, and no password is ever a method. What this page cannot do goes
 * to the popup on EV Invest.
 */
export function StepupPanel({ kind, review, controls, client, onFallback }: { kind: IntentKind; review: Review; controls: IntentFlowControls; client: StepupClient; onFallback: () => void }) {
  const methods = pageMethods(kind, review.intent.stepup.methods, { passkey: passkeysSupported(), emailCode: EMAIL_CODE_IN_PAGE });
  if (!methods.includes("passkey")) return <Elsewhere methods={review.intent.stepup.methods} onFallback={onFallback} />;
  return <PasskeyStep key={review.intent.id} review={review} controls={controls} client={client} onFallback={onFallback} />;
}
