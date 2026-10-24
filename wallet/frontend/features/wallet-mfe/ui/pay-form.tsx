import { useT } from "@evinvest/i18n/react";

import { useState, type FormEvent } from "react";

import { Alert, AlertDescription, Button } from "@evinvest/uikit";

import { Choice, FormActions, FormError, type ActionFormProps } from "./form-parts";

const MAIN = "main";

/**
 * Paying one invoice of this host. The page knows only the invoice's id: the amount and the
 * payee come back in the BFF's terms, read from the invoice banking issued.
 */
export function PayForm({ scope, invoice, draft, busy, error, onSubmit, onCancel }: ActionFormProps & { scope: string; invoice: string | null }) {
  const t = useT();
  const last = draft?.kind === "pay" ? draft.params : null;
  const [from, setFrom] = useState(last?.pay_from ?? MAIN);

  if (invoice === null) {
    return (
      <div className="flex flex-col items-start gap-4">
        <Alert variant="info">
          <AlertDescription>{t("walletMfe.noInvoice", "There is no invoice to pay here.")}</AlertDescription>
        </Alert>
        <Button type="button" variant="outline" onClick={onCancel}>
          {t("walletAction.cancel", "Cancel")}
        </Button>
      </div>
    );
  }

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!busy) onSubmit({ kind: "pay", params: { invoice_id: invoice, pay_from: from } });
  };

  return (
    <form className="flex flex-col gap-4" onSubmit={submit}>
      <Choice
        label={t("walletAction.payFrom", "Pay from")}
        value={from}
        disabled={busy}
        onChange={setFrom}
        options={[
          { value: MAIN, label: t("walletAction.fromMain", "Main balance") },
          { value: scope, label: t("walletAction.fromScope", "{scope} balance", { scope }) },
        ]}
      />
      <FormError error={error} />
      <FormActions busy={busy} disabled={false} onCancel={onCancel} />
    </form>
  );
}
