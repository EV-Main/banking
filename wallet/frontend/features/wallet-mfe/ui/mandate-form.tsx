import { useT } from "@evinvest/i18n/react";

import type { FormEvent } from "react";

import { Alert, AlertDescription, Button } from "@evinvest/uikit";

import { IntentErrorAlert } from "@/features/wallet-intent";

import { FormActions, type ActionFormProps } from "./form-parts";

/**
 * Granting a mandate a service asked for. Nothing is typed: the caps, the service and the
 * expiry are the stored request's, and the BFF shows them in its terms.
 */
export function MandateForm({ request, busy, error, onSubmit, onCancel }: ActionFormProps & { request: string | null }) {
  const t = useT();

  if (request === null) {
    return (
      <div className="flex flex-col items-start gap-4">
        <Alert variant="info">
          <AlertDescription>{t("walletAction.requestUnavailable", "This request is not available. It may have expired — ask the service to send a new one.")}</AlertDescription>
        </Alert>
        <Button type="button" variant="outline" onClick={onCancel}>
          {t("walletAction.cancel", "Cancel")}
        </Button>
      </div>
    );
  }

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!busy) onSubmit({ kind: "mandate", params: { request_id: request } });
  };

  return (
    <form className="flex flex-col gap-4" onSubmit={submit}>
      <p className="m-0 text-sm text-ink-soft">{t("walletMfe.mandateIntro", "A service asks to use your funds within set limits. Review the limits before you allow it.")}</p>
      <IntentErrorAlert error={error} />
      <FormActions busy={busy} disabled={false} onCancel={onCancel} />
    </form>
  );
}
