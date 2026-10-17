"use client";

import { useT } from "@evinvest/i18n/react";

import { useEffect, useState } from "react";

import { Skeleton } from "@evinvest/uikit";

import { fetchMandates, revokeMandate } from "@/entities/mandate/api/mandate-client";
import { isStepUpRequired, visibleMandates } from "@/entities/mandate/lib/mandate";
import { ActionFrame, useWalletAction } from "@/features/wallet-action";
import type { Mandate } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { MandateCard } from "@/views/wallet/ui/mandate-card";
import { GrantMandate } from "@/views/wallet/ui/mandate-grant";

/**
 * Two pages in one route. With a `request` in the URL it grants what that server-held request
 * asks; without one it lists what services may draw and ends them. The URL never carries the
 * service or its caps (checklist item 12): a grant names a request, the server holds the
 * terms, and the page shows them back before the reader accepts. The URL's `scope` merely
 * narrows the server's own list.
 */
export function MandateActionView() {
  const { params, finished, finish } = useWalletAction();
  const done = (mandate: Mandate) => finish("done", mandate.mandate_id);
  return (
    <ActionFrame params={params} finished={finished} finish={finish}>
      {params.request !== null ? <GrantMandate key={params.request} requestId={params.request} urlScope={params.scope} onGranted={done} /> : <Mandates urlScope={params.scope} onRevoked={done} />}
    </ActionFrame>
  );
}

function Mandates({ urlScope, onRevoked }: { urlScope: string | null; onRevoked: (mandate: Mandate) => void }) {
  const t = useT();
  const [mandates, setMandates] = useState<Mandate[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [revoking, setRevoking] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    fetchMandates()
      .then((read) => live && setMandates(visibleMandates(read.mandates, urlScope, Math.floor(Date.now() / 1000))))
      .catch((e: unknown) => live && setError(e));
    return () => {
      live = false;
    };
  }, [urlScope]);

  const revoke = async (id: string) => {
    if (revoking !== null) return;
    setRevoking(id);
    setError(null);
    try {
      onRevoked(await revokeMandate(id));
    } catch (e) {
      setError(e);
    } finally {
      setRevoking(null);
    }
  };

  const failure =
    error === null ? null : (
      <p role="alert" className="text-sm text-accent-error">
        {isStepUpRequired(error) ? t("walletAction.stepUp", "For your safety, sign in again to continue.") : errorMessage(error, t)}
      </p>
    );

  if (mandates === null) return failure ?? <Skeleton className="mx-4 h-48 sm:mx-8 lg:max-w-140" />;
  return (
    <div className="mx-4 flex flex-col gap-4 sm:mx-8 lg:max-w-140">
      <h1 className="text-lg font-semibold text-ink">{t("walletAction.mandateListTitle", "Services allowed to charge you")}</h1>
      {mandates.length === 0 ? <p className="text-sm text-ink-soft">{t("walletAction.mandateNone", "No service is allowed to charge you right now.")}</p> : null}
      {mandates.map((m) => (
        <MandateCard key={m.mandate_id} mandate={m} revoking={revoking === m.mandate_id} onRevoke={() => void revoke(m.mandate_id)} />
      ))}
      {failure}
    </div>
  );
}
