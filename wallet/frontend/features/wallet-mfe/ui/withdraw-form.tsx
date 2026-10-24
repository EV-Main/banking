import { useT } from "@evinvest/i18n/react";

import { useEffect, useState, type FormEvent } from "react";

import { Field, FieldLabel, Input, ResourceError, Skeleton } from "@evinvest/uikit";

import { isPositiveWireDecimal } from "@/shared/lib/money";
import { networkLabel } from "@/shared/lib/rail";

import { fetchWithdrawNetworks } from "../api/wallet-remote";
import { Choice, FormActions, FormError, type ActionFormProps } from "./form-parts";

type Rails = { status: "loading" } | { status: "error" } | { status: "ready"; networks: string[] };

function useWithdrawNetworks(origin: string): { rails: Rails; reload: () => void } {
  const [rails, setRails] = useState<Rails>({ status: "loading" });
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let live = true;
    void fetchWithdrawNetworks(origin).then((read) => {
      if (live) setRails(read.ok ? { status: "ready", networks: read.value } : { status: "error" });
    });
    return () => {
      live = false;
    };
  }, [origin, attempt]);
  return { rails, reload: () => setAttempt((n) => n + 1) };
}

/**
 * A withdrawal from main to an external address. The page sends the inputs as typed; the fee,
 * the minimum and what arrives come back in the BFF's terms, from the hub's rail.
 */
export function WithdrawForm({ origin, draft, busy, error, onSubmit, onCancel }: ActionFormProps & { origin: string }) {
  const t = useT();
  const { rails, reload } = useWithdrawNetworks(origin);
  const last = draft?.kind === "withdraw" ? draft.params : null;
  const [picked, setPicked] = useState<string | null>(last?.network ?? null);
  const [address, setAddress] = useState(last?.address ?? "");
  const [amount, setAmount] = useState(last?.amount ?? "");

  if (rails.status === "loading") return <Skeleton className="h-40 w-full" />;
  if (rails.status === "error") return <ResourceError message={t("walletMfe.unavailable", "Your wallet could not be loaded.")} onRetry={reload} />;
  if (rails.networks.length === 0) return <FormError error={{ code: "network_unavailable" }} />;

  const network = picked !== null && rails.networks.includes(picked) ? picked : rails.networks[0];
  const valid = address.trim() !== "" && isPositiveWireDecimal(amount.trim());
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (valid && !busy) onSubmit({ kind: "withdraw", params: { network, address: address.trim(), amount: amount.trim() } });
  };

  return (
    <form className="flex flex-col gap-4" onSubmit={submit} noValidate>
      <Choice
        label={t("wallet.withdrawalNetwork", "Withdrawal network")}
        value={network}
        disabled={busy}
        onChange={setPicked}
        options={rails.networks.map((id) => ({ value: id, label: networkLabel(id) }))}
      />
      <Field className="flex flex-col gap-2">
        <FieldLabel>{t("walletMfe.address", "Address")}</FieldLabel>
        <Input autoComplete="off" spellCheck={false} autoFocus value={address} disabled={busy} onChange={(event) => setAddress(event.target.value)} />
      </Field>
      <Field className="flex flex-col gap-2">
        <FieldLabel>{t("walletAction.moveAmount", "Amount, USDT")}</FieldLabel>
        <Input inputMode="decimal" autoComplete="off" placeholder="0.00" value={amount} disabled={busy} aria-invalid={amount !== "" && !isPositiveWireDecimal(amount.trim())} onChange={(event) => setAmount(event.target.value)} />
      </Field>
      <FormError error={error} />
      <FormActions busy={busy} disabled={!valid} onCancel={onCancel} />
    </form>
  );
}
