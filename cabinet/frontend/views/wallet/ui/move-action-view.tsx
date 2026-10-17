"use client";

import { useLocale, useT } from "@evinvest/i18n/react";

import { useEffect, useId, useState } from "react";

import { Button, Field, FieldLabel, Input, Skeleton, Spinner } from "@evinvest/uikit";

import { fetchCompartments, moveFunds } from "@/entities/compartment/api/compartment-client";
import { amountProblem, type Direction, MAIN, movableScopes, moveEnds, preselectedScope } from "@/entities/compartment/lib/move";
import { ActionFrame, useWalletAction } from "@/features/wallet-action";
import type { Compartment } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { formatUsdt } from "@/views/wallet/lib/format";
import { WALLET_CARD } from "@/views/wallet/ui/wallet-chrome";

/**
 * Moving money between `main` and one scope's compartment, as a money action on
 * evinvest.ltd. The amount is typed here; the URL may only preselect a scope, and only one
 * the server's own list offers. The move's id goes back as the reference once the hub has
 * recorded it — recorded, not necessarily posted: the host re-reads its own backend.
 */
export function MoveActionView() {
  const { params, finished, finish } = useWalletAction();
  return (
    <ActionFrame params={params} finished={finished} finish={finish}>
      <MoveFunds urlScope={params.scope} onMoved={(id) => finish("done", id)} />
    </ActionFrame>
  );
}

function MoveFunds({ urlScope, onMoved }: { urlScope: string | null; onMoved: (id: string | null) => void }) {
  const t = useT();
  const locale = useLocale();
  const amountId = useId();
  const [compartments, setCompartments] = useState<Compartment[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [chosen, setChosen] = useState<string | null>(null);
  const [direction, setDirection] = useState<Direction>("in");
  const [amount, setAmount] = useState("");
  const [failed, setFailed] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  // One key per set of terms: a double click, or a retry after a dropped answer, repeats the
  // same move. Any edit — and a move the ledger refused, which is final — takes a fresh key,
  // because the hub answers the same key with other terms as a conflict.
  const [idempotencyKey, setIdempotencyKey] = useState(() => crypto.randomUUID());
  const edit = () => {
    setIdempotencyKey(crypto.randomUUID());
    setFailed(false);
    setError(null);
  };

  useEffect(() => {
    let live = true;
    fetchCompartments()
      .then((read) => live && setCompartments(read.compartments ?? []))
      .catch((e: unknown) => live && setError(e));
    return () => {
      live = false;
    };
  }, []);

  if (compartments === null) {
    return error !== null ? <Message text={errorMessage(error, t)} /> : <Skeleton className="mx-4 h-56 sm:mx-8 lg:max-w-140" />;
  }
  const scopes = movableScopes(compartments);
  if (scopes.length === 0) return <Message text={t("walletAction.moveNoScopes", "There is no balance to move to or from yet.")} />;

  const scope = chosen ?? preselectedScope(urlScope, scopes) ?? scopes[0]?.scope ?? "";
  const main = compartments.find((c) => c.scope === MAIN);
  const picked = scopes.find((c) => c.scope === scope);
  const source = direction === "in" ? main : picked;
  const problem = amountProblem(amount, source?.available);

  const submit = async () => {
    if (submitting || problem !== null) return;
    setSubmitting(true);
    setError(null);
    setFailed(false);
    try {
      const moved = await moveFunds({ ...moveEnds(scope, direction), amount: amount.trim(), idempotency_key: idempotencyKey });
      if (moved.state === "failed") {
        setIdempotencyKey(crypto.randomUUID());
        setFailed(true);
      } else onMoved(moved.id ?? null);
    } catch (e) {
      setError(e);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className={cn(WALLET_CARD, "mx-4 flex flex-col gap-4 p-4.5 sm:mx-8 lg:max-w-140 lg:p-6")}>
      <h1 className="text-lg font-semibold text-ink">{t("walletAction.moveTitle", "Move funds")}</h1>
      <div role="group" aria-label={t("walletAction.moveScope", "Balance")} className="flex flex-col gap-2">
        {scopes.map((c) => (
          <Button key={c.scope} type="button" variant="outline" aria-pressed={scope === c.scope} disabled={submitting} onClick={() => { setChosen(c.scope ?? null); edit(); }} className={cn("justify-between", scope === c.scope && "border-ring")}>
            <span>{c.display_name || c.scope}</span>
            <span>{formatUsdt(c.available, locale)}</span>
          </Button>
        ))}
      </div>
      <div role="group" aria-label={t("walletAction.moveDirection", "Direction")} className="flex gap-2">
        {(["in", "out"] as const).map((d) => (
          <Button key={d} type="button" variant="outline" aria-pressed={direction === d} disabled={submitting} onClick={() => { setDirection(d); edit(); }} className={cn("flex-1", direction === d && "border-ring")}>
            {d === "in" ? t("walletAction.moveIn", "Main → {scope}", { scope: picked?.display_name || scope }) : t("walletAction.moveOut", "{scope} → Main", { scope: picked?.display_name || scope })}
          </Button>
        ))}
      </div>
      <Field>
        <FieldLabel htmlFor={amountId}>{t("walletAction.moveAmount", "Amount, USDT")}</FieldLabel>
        <Input id={amountId} inputMode="decimal" placeholder="0.00" value={amount} disabled={submitting} aria-invalid={amount !== "" && problem !== null} onChange={(e) => { setAmount(e.target.value); edit(); }} />
        <p className="text-xs text-ink-soft">{t("walletAction.moveAvailable", "Available: {amount}", { amount: formatUsdt(source?.available, locale) })}</p>
      </Field>
      {amount !== "" && problem === "exceeds" ? <p role="alert" className="text-sm text-accent-error">{t("walletAction.moveExceeds", "That is more than is available.")}</p> : null}
      {failed ? <p role="alert" className="text-sm text-accent-error">{t("walletAction.moveFailed", "The move did not go through. Nothing was moved — you can try again.")}</p> : null}
      {error !== null ? <p role="alert" className="text-sm text-accent-error">{errorMessage(error, t)}</p> : null}
      <Button type="button" onClick={submit} disabled={submitting || problem !== null}>
        {submitting ? <Spinner aria-hidden /> : t("walletAction.moveNow", "Move")}
      </Button>
    </div>
  );
}

function Message({ text }: { text: string }) {
  return <p className="mx-4 mt-8 text-sm text-ink-soft sm:mx-8">{text}</p>;
}
