import { useT } from "@evinvest/i18n/react";

import { STEPUP_TAG } from "@evinvest/concierge";
import { StepupDialog, type Stepup } from "@evinvest/concierge/react";
import { API_VERSION } from "@evinvest/mfe";
import { useElementBundle } from "@evinvest/mfe/react";
import type { ReactNode } from "react";

import { Alert, AlertDescription, Button, Spinner } from "@evinvest/uikit";

import { useStepupSource } from "../model/stepup-source";

interface StepupSurfaceProps {
  dialog: Stepup["dialogProps"];
  /**
   * The page is a scope host's: the wallet origin to read concierge's registry from, and the
   * popup on it that finishes the act when the element cannot load here. Null on the wallet's
   * own pages, where the page's origin is the wallet's.
   */
  scopeHost: { origin: string; handoff: () => void } | null;
}

function Opening() {
  const t = useT();
  return (
    <p role="status" className="m-0 flex items-center gap-2 text-sm text-ink-soft">
      <Spinner aria-hidden />
      {t("walletIntent.stepupOpening", "Opening the confirmation…")}
    </p>
  );
}

function Unavailable({ handoff, onDismiss }: { handoff: (() => void) | null; onDismiss: () => void }) {
  const t = useT();
  return (
    <div className="flex flex-col gap-3">
      <Alert variant="destructive" role="alert">
        <AlertDescription>
          {handoff
            ? t("walletIntent.stepupElsewhere", "The confirmation cannot open on this page. Finish on EV Invest.")
            : t("walletIntent.stepupFailed", "The confirmation could not load. Try again in a moment.")}
        </AlertDescription>
      </Alert>
      <div className="flex flex-wrap gap-2">
        {handoff && (
          <Button type="button" onClick={handoff}>
            {t("walletIntent.continueOnEv", "Continue on EV Invest")}
          </Button>
        )}
        <Button type="button" variant="outline" onClick={onDismiss}>
          {t("ui.back", "Back")}
        </Button>
      </div>
    </div>
  );
}

// The dialog's `fallback` shows both while the bundle loads and after it failed; the shared
// load (one per tag) tells the two apart.
function BundleFallback({ scriptUrl, unavailable }: { scriptUrl: string; unavailable: ReactNode }) {
  const state = useElementBundle(STEPUP_TAG, scriptUrl, { minApiVersion: API_VERSION, layer: false });
  return state === "failed" ? unavailable : <Opening />;
}

/**
 * Concierge's step-up dialog for the flow's `requestProof`. Which factor answers, and enrolling
 * one, is the element's; this only places it and covers the element not loading.
 */
export function StepupSurface({ dialog, scopeHost }: StepupSurfaceProps) {
  const source = useStepupSource(scopeHost?.origin ?? null);
  const unavailable = <Unavailable handoff={scopeHost?.handoff ?? null} onDismiss={() => dialog.onOpenChange(false)} />;
  if (source.status === "ready") return <StepupDialog scriptUrl={source.scriptUrl} {...dialog} fallback={<BundleFallback scriptUrl={source.scriptUrl} unavailable={unavailable} />} />;
  if (!dialog.open) return null;
  return source.status === "failed" ? unavailable : <Opening />;
}
