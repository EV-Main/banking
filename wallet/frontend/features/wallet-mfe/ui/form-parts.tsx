import { useT } from "@evinvest/i18n/react";

import type { ReactNode } from "react";

import { Alert, AlertDescription, Button, Spinner } from "@evinvest/uikit";

import type { IntentError, IntentRequest } from "../lib/intent";
import { intentErrorText } from "./intent-words";

/** What every action form takes: its last inputs, the create's state, and where to send. */
export interface ActionFormProps {
  draft: IntentRequest | null;
  busy: boolean;
  error: IntentError | null;
  onSubmit: (request: IntentRequest) => void;
  onCancel: () => void;
}

export function FormError({ error }: { error: IntentError | null }) {
  const t = useT();
  if (error === null) return null;
  return (
    <Alert variant="destructive" role="alert">
      <AlertDescription>{intentErrorText(error, t)}</AlertDescription>
    </Alert>
  );
}

/** A choice of a few values as pressed buttons: no overlay, so it works inside a closed root. */
export function Choice<T extends string>({ label, options, value, disabled, onChange }: { label: string; options: readonly { value: T; label: ReactNode }[]; value: T; disabled: boolean; onChange: (value: T) => void }) {
  return (
    <div role="group" aria-label={label} className="flex flex-wrap gap-2">
      {options.map((option) => (
        <Button
          key={option.value}
          type="button"
          variant={option.value === value ? "primary" : "outline"}
          aria-pressed={option.value === value}
          disabled={disabled}
          onClick={() => onChange(option.value)}
          className="flex-1"
        >
          {option.label}
        </Button>
      ))}
    </div>
  );
}

/** Review goes to the BFF for terms; nothing moves until the confirm after it. */
export function FormActions({ busy, disabled, onCancel }: { busy: boolean; disabled: boolean; onCancel: () => void }) {
  const t = useT();
  return (
    <div className="flex flex-wrap gap-2">
      <Button type="submit" disabled={busy || disabled} aria-busy={busy}>
        {busy ? <Spinner aria-hidden /> : null}
        {t("walletMfe.review", "Review")}
      </Button>
      <Button type="button" variant="outline" onClick={onCancel} disabled={busy}>
        {t("walletAction.cancel", "Cancel")}
      </Button>
    </div>
  );
}
