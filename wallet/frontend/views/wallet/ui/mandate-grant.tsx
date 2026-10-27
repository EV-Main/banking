"use client";

import { useT } from "@evinvest/i18n/react";

import { useEffect, useState } from "react";

import { Button, Skeleton, Spinner } from "@evinvest/uikit";

import { fetchMandateRequest, fetchMandates } from "@/entities/mandate/api/mandate-client";
import { grantStillHolds, isRequestAlive, isShowable, isUnavailable, replacedBy } from "@/entities/mandate/lib/request";
import { ActionCancel } from "@/features/wallet-action";
import { ConfirmStep, firstPartyIntentClient, IntentErrorAlert, StepupSurface, useIntentFlow } from "@/features/wallet-intent";
import type { Mandate, MandateRequest } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { MandateTerms } from "@/views/wallet/ui/mandate-terms";
import { WALLET_CARD } from "@/views/wallet/ui/wallet-chrome";

/**
 * Granting a service the permission it asked for. The URL names only the request; the
 * service, operations, caps and term are read back from the server and shown as they are.
 * The caller keys this by request id, so a different request starts a fresh flow.
 */
export function GrantMandate({ requestId, urlScope, onGranted }: { requestId: string; urlScope: string | null; onGranted: (mandate: Mandate) => void }) {
  const t = useT();
  const [request, setRequest] = useState<MandateRequest | null>(null);
  // The live mandate of this service that a grant would replace; read once, with the request.
  const [replaced, setReplaced] = useState<Mandate | null>(null);
  const [ended, setEnded] = useState(false);
  const [unavailable, setUnavailable] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const controls = useIntentFlow<"mandate">(firstPartyIntentClient, ({ result }) => {
    // Never report "done" for a permission that is already gone: the host would unlock on it.
    if (grantStillHolds(result)) onGranted(result);
    else setEnded(true);
  });
  const { flow } = controls;

  useEffect(() => {
    let live = true;
    // The list only feeds the "replaces" notice, so its failure must not block the grant.
    const heldNow = fetchMandates().then((read) => read.mandates, () => []);
    Promise.all([fetchMandateRequest(requestId), heldNow])
      .then(([read, mandates]) => {
        if (!live) return;
        const now = Math.floor(Date.now() / 1000);
        if (isRequestAlive(read, now) && isShowable(read, urlScope)) {
          setRequest(read);
          setReplaced(replacedBy(read, mandates, now));
        } else setUnavailable(true);
      })
      .catch((e: unknown) => {
        if (!live) return;
        if (isUnavailable(e)) setUnavailable(true);
        else setError(e);
      });
    return () => {
      live = false;
    };
  }, [requestId, urlScope]);

  const grant = () => {
    if (request === null) return;
    if (!isRequestAlive(request, Math.floor(Date.now() / 1000))) return setUnavailable(true);
    setEnded(false);
    controls.submit({ kind: "mandate", params: { request_id: request.request_id } });
  };

  if (unavailable) {
    return (
      <div className="mt-8 flex flex-col items-start gap-4">
        <p className="text-sm text-ink-soft">{t("walletAction.requestUnavailable", "This request is not available. It may have expired — ask the service to send a new one.")}</p>
        <ActionCancel />
      </div>
    );
  }
  if (request === null) {
    return error !== null ? (
      <div className="mt-8 flex flex-col items-start gap-4">
        <p role="alert" className="text-sm text-accent-error">{errorMessage(error, t)}</p>
        <ActionCancel />
      </div>
    ) : (
      <Skeleton className="h-64" />
    );
  }

  const service = request.client_display_name || request.client_id;
  return (
    <section className={cn(WALLET_CARD, "flex flex-col gap-4 p-4.5 lg:p-6")}>
      <div className="flex flex-col gap-1">
        <span className="text-xs font-medium text-ink-soft">{t("walletAction.mandateService", "Service in {scope}", { scope: request.scope })}</span>
        <h1 className="text-lg font-semibold text-ink">{t("walletAction.grantTitle", "Allow {service} to use your funds?", { service })}</h1>
      </div>
      {flow.phase === "editing" || flow.phase === "creating" ? <MandateTerms terms={{ ...request, ends_at: request.mandate_expires_at }} /> : null}
      {replaced !== null ? (
        <div className="flex flex-col gap-3 border-t border-border pt-4">
          <p className="text-sm text-ink-soft">{t("walletAction.grantReplaces", "{service} already has permission here. Allowing this replaces it.", { service })}</p>
          <span className="text-xs font-medium text-ink-soft">{t("walletAction.grantCurrent", "Allowed now")}</span>
          <MandateTerms terms={{ ...replaced, ends_at: replaced.expires_at }} />
        </div>
      ) : null}
      {ended ? <p role="alert" className="text-sm text-accent-error">{t("walletAction.grantEnded", "This permission has already ended. Ask the service to request it again.")}</p> : null}
      {flow.phase === "editing" || flow.phase === "creating" ? (
        <>
          <IntentErrorAlert error={flow.phase === "editing" ? flow.error : null} />
          <div className="flex gap-2">
            <Button type="button" onClick={grant} disabled={flow.phase === "creating"} className="flex-1">
              {flow.phase === "creating" ? <Spinner aria-hidden /> : t("walletAction.grantNow", "Allow")}
            </Button>
            <ActionCancel disabled={flow.phase === "creating"} />
          </div>
        </>
      ) : (
        <ConfirmStep kind="mandate" controls={controls} />
      )}
      <StepupSurface dialog={controls.stepupDialog} scopeHost={null} />
    </section>
  );
}
