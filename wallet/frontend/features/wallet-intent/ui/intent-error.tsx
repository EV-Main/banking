import { useT } from "@evinvest/i18n/react";

import { Alert, AlertDescription } from "@evinvest/uikit";

import type { IntentError } from "../lib/intent";
import { intentErrorText } from "./intent-words";

export function IntentErrorAlert({ error }: { error: IntentError | null }) {
  const t = useT();
  if (error === null) return null;
  return (
    <Alert variant="destructive" role="alert">
      <AlertDescription>{intentErrorText(error, t)}</AlertDescription>
    </Alert>
  );
}
