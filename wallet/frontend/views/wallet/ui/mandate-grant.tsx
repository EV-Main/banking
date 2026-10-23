"use client";

import { useT } from "@evinvest/i18n/react";

import { useEffect, useState } from "react";

import { Button, Skeleton, Spinner } from "@evinvest/uikit";

import { fetchMandateRequest, fetchMandates, grantMandate } from "@/entities/mandate/api/mandate-client";
import { isStepUpRequired } from "@/entities/mandate/lib/mandate";
import { grantStillHolds, isRequestAlive, isShowable, isUnavailable, keepsKeyAfter, replacedBy } from "@/entities/mandate/lib/request";
import { ActionCancel } from "@/features/wallet-action";
import type { Mandate, MandateRequest } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { MandateTerms } from "@/views/wallet/ui/mandate-terms";
import { WALLET_CARD } from "@/views/wallet/ui/wallet-chrome";


/**
 * Granting a service the permission it asked for. The URL names only the request; the
 * service, operations, caps and term are read back from the server and shown as they are.
 * The caller keys this by request id, so a different request starts with a fresh key.
 */
export function GrantMandate({ requestId, urlScope, onGranted }: { requestId: string; urlScope: string | null; onGranted: (mandate: Mandate) => void }) {
  const t = useT();
  const [request, setRequest] = useState<MandateRequest | null>(null);
  // The live mandate of this service that a grant would replace; read once, with the request.
  const [replaced, setReplaced] = useState<Mandate | null>(null);
  const [ended, setEnded] = useState(false);
  const [unavailable, setUnavailable] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [submitting, setSubmitting] = useState(false);
  // Made at page load: a double click, or a retry after a dropped answer, repeats the same
  // grant. It is replaced once the server has refused this attempt (see `keepsKeyAfter`).
  const [idempotencyKey, setIdempotencyKey] = useState(() => crypto.randomUUID());

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

  const grant = async () => {
    if (submitting || request === null) return;
    if (!isRequestAlive(request, Math.floor(Date.now() / 1000))) return setUnavailable(true);
    setSubmitting(true);
    setError(null);
    setEnded(false);
    try {
      const mandate = await grantMandate({ request_id: request.request_id, idempotency_key: idempotencyKey });
      // Never report "done" for a permission that is already gone: the host would unlock on it.
      if (grantStillHolds(mandate)) onGranted(mandate);
      else {
        setEnded(true);
        setIdempotencyKey(crypto.randomUUID());
      }
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else setError(e);
      if (!keepsKeyAfter(e)) setIdempotencyKey(crypto.randomUUID());
    } finally {
      setSubmitting(false);
    }
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
      <MandateTerms terms={{ ...request, ends_at: request.mandate_expires_at }} />
      {replaced !== null ? (
        <div className="flex flex-col gap-3 border-t border-border pt-4">
          <p className="text-sm text-ink-soft">{t("walletAction.grantReplaces", "{service} already has permission here. Allowing this replaces it.", { service })}</p>
          <span className="text-xs font-medium text-ink-soft">{t("walletAction.grantCurrent", "Allowed now")}</span>
          <MandateTerms terms={{ ...replaced, ends_at: replaced.expires_at }} />
        </div>
      ) : null}
      {ended ? <p role="alert" className="text-sm text-accent-error">{t("walletAction.grantEnded", "This permission has already ended. Ask the service to request it again.")}</p> : null}
      {error !== null ? (
        <p role="alert" className="text-sm text-accent-error">
          {/* Sign-in link deliberately absent: a repeat sign-in may not refresh the auth time the
              gate reads until EV-Main/concierge#166 lands, so the button would loop. */}
          {isStepUpRequired(error) ? t("walletAction.stepUp", "For your safety, sign in again to continue.") : errorMessage(error, t)}
        </p>
      ) : null}
      <div className="flex gap-2">
        <Button type="button" onClick={() => void grant()} disabled={submitting} className="flex-1">
          {submitting ? <Spinner aria-hidden /> : t("walletAction.grantNow", "Allow")}
        </Button>
        <ActionCancel disabled={submitting} />
      </div>
    </section>
  );
}
