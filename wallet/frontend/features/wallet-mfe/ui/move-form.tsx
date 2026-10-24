import { useT } from "@evinvest/i18n/react";

import { useState, type FormEvent } from "react";

import { Field, FieldDescription, FieldLabel, Input } from "@evinvest/uikit";

import { isPositiveWireDecimal } from "@/shared/lib/money";

import { Choice, FormActions, FormError, type ActionFormProps } from "./form-parts";

const MAIN = "main";
type Direction = "in" | "out";

/** A move between main and this host's own compartment; the BFF refuses any other pair. */
export function MoveForm({ scope, draft, busy, error, onSubmit, onCancel }: ActionFormProps & { scope: string }) {
  const t = useT();
  const last = draft?.kind === "move" ? draft.params : null;
  const [direction, setDirection] = useState<Direction>(last?.from_scope === scope ? "out" : "in");
  const [amount, setAmount] = useState(last?.amount ?? "");
  const valid = isPositiveWireDecimal(amount.trim());

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!valid || busy) return;
    const [from, to] = direction === "in" ? [MAIN, scope] : [scope, MAIN];
    onSubmit({ kind: "move", params: { from_scope: from, to_scope: to, amount: amount.trim() } });
  };

  return (
    <form className="flex flex-col gap-4" onSubmit={submit} noValidate>
      <Choice
        label={t("walletAction.moveDirection", "Direction")}
        value={direction}
        disabled={busy}
        onChange={setDirection}
        options={[
          { value: "in", label: t("walletAction.moveIn", "Main → {scope}", { scope }) },
          { value: "out", label: t("walletAction.moveOut", "{scope} → Main", { scope }) },
        ]}
      />
      <Field className="flex flex-col gap-2">
        <FieldLabel>{t("walletAction.moveAmount", "Amount, USDT")}</FieldLabel>
        <Input
          inputMode="decimal"
          autoComplete="off"
          autoFocus
          placeholder="0.00"
          value={amount}
          disabled={busy}
          aria-invalid={amount !== "" && !valid}
          onChange={(event) => setAmount(event.target.value)}
        />
        <FieldDescription>{t("walletMfe.reviewHint", "You see the exact terms before anything moves.")}</FieldDescription>
      </Field>
      <FormError error={error} />
      <FormActions busy={busy} disabled={!valid} onCancel={onCancel} />
    </form>
  );
}
