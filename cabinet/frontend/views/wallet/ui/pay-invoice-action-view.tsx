"use client";

import { useLocale, useT } from "@evinvest/i18n/react";

import { useEffect, useState } from "react";

import { Button, Skeleton, Spinner } from "@evinvest/uikit";

import { fetchPayableInvoice, payInvoice } from "@/entities/invoice/api/invoice-client";
import { invoicePhase, POLL_EVERY_MS, shouldPoll } from "@/entities/invoice/lib/state";
import { isStepUpRequired } from "@/entities/mandate/lib/mandate";
import { ActionFrame, useWalletAction } from "@/features/wallet-action";
import type { PayableInvoice } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { formatUsdt, toBaseUnits } from "@/views/wallet/lib/format";
import { WALLET_CARD } from "@/views/wallet/ui/wallet-chrome";

/**
 * Paying a scope's invoice as a money action on evinvest.ltd. The URL names only the
 * invoice; the amount, the payee and the memo are the server's. `paid` is reported to the
 * opener only once the ledger has posted the payment — until then the invoice is
 * `processing`, this page says so and re-reads it until it settles.
 */
export function PayInvoiceActionView() {
  const { params, finished, finish } = useWalletAction();
  return (
    <ActionFrame params={params} finished={finished} finish={finish}>
      <PayInvoice invoiceId={params.invoice} onPaid={(invoice) => finish("done", invoice.invoice_id ?? null)} />
    </ActionFrame>
  );
}

function PayInvoice({ invoiceId, onPaid }: { invoiceId: string | null; onPaid: (invoice: PayableInvoice) => void }) {
  const t = useT();
  const locale = useLocale();
  const [invoice, setInvoice] = useState<PayableInvoice | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [source, setSource] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // One key per page load: a second click, or a retry after a dropped answer, repeats the
  // same payment instead of making another.
  const [idempotencyKey] = useState(() => crypto.randomUUID());
  const [gaveUp, setGaveUp] = useState(false);
  // The ledger refused a payment that had been processing: the invoice is open again.
  const [reopened, setReopened] = useState(false);

  const phase = invoicePhase(invoice?.state);

  useEffect(() => {
    if (invoiceId === null) return;
    let live = true;
    fetchPayableInvoice(invoiceId)
      .then((read) => live && setInvoice(read))
      .catch((e: unknown) => live && setError(e));
    return () => {
      live = false;
    };
  }, [invoiceId]);

  // Re-read a processing invoice until the ledger answers: `paid` once it posts, `open`
  // again if it refuses. Every state change happens in the timer's callback, and the loop
  // ends with the effect — as soon as the invoice leaves `processing`.
  useEffect(() => {
    if (phase !== "processing" || invoiceId === null) return;
    const started = Date.now();
    let live = true;
    let timer: ReturnType<typeof setTimeout>;
    const next = () => {
      timer = setTimeout(async () => {
        try {
          const read = await fetchPayableInvoice(invoiceId);
          if (!live) return;
          const now = invoicePhase(read.state);
          setInvoice(read);
          if (now === "paid") onPaid(read);
          if (now === "payable") setReopened(true);
          if (now !== "processing") return;
        } catch {
          // A failed re-read is not a verdict on the payment: the next tick asks again.
          if (!live) return;
        }
        if (shouldPoll("processing", Date.now() - started)) next();
        else setGaveUp(true);
      }, POLL_EVERY_MS);
    };
    next();
    return () => {
      live = false;
      clearTimeout(timer);
    };
    // `onPaid` is the frame's once-only `finish`; the phase is the only trigger.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [phase, invoiceId]);

  if (invoiceId === null) {
    return <Message text={t("walletAction.invoiceMissing", "This link does not name an invoice.")} />;
  }
  if (invoice === null) {
    return error !== null ? <Message text={errorMessage(error, t)} /> : <Skeleton className="mx-4 h-48 sm:mx-8 lg:max-w-140" />;
  }

  const amount = toBaseUnits(invoice.amount);
  const sources = [
    { key: "main", label: t("walletAction.fromMain", "Main balance"), available: invoice.available_main },
    { key: invoice.scope ?? "", label: t("walletAction.fromScope", "{scope} balance", { scope: invoice.display_name ?? invoice.scope ?? "" }), available: invoice.available_scope },
  ].filter((s) => s.key !== "");
  const chosen = source ?? sources.find((s) => toBaseUnits(s.available) >= amount)?.key ?? "main";

  const pay = async () => {
    if (submitting) return;
    setSubmitting(true);
    setError(null);
    setReopened(false);
    setGaveUp(false);
    try {
      const paid = await payInvoice(invoiceId, { pay_from: chosen, idempotency_key: idempotencyKey });
      setInvoice(paid);
      if (invoicePhase(paid.state) === "paid") onPaid(paid);
    } catch (e) {
      setError(e);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className={cn(WALLET_CARD, "mx-4 flex flex-col gap-4 p-4.5 sm:mx-8 lg:max-w-140 lg:p-6")}>
      <div className="flex flex-col gap-1">
        <span className="text-xs font-medium text-ink-soft">{invoice.display_name}</span>
        <span className="text-2xl font-semibold text-ink">{formatUsdt(invoice.amount, locale)} USDT</span>
        {invoice.memo ? <p className="text-sm text-ink-soft">{invoice.memo}</p> : null}
      </div>

      {phase === "payable" ? (
        <>
          <div role="group" aria-label={t("walletAction.payFrom", "Pay from")} className="flex flex-col gap-2">
            {sources.map((s) => (
              <Button key={s.key} type="button" variant="outline" aria-pressed={chosen === s.key} disabled={toBaseUnits(s.available) < amount || submitting} onClick={() => setSource(s.key)} className={cn("justify-between", chosen === s.key && "border-ring")}>
                <span>{s.label}</span>
                <span>{formatUsdt(s.available, locale)}</span>
              </Button>
            ))}
          </div>
          {reopened ? (
            <p role="alert" className="text-sm text-accent-error">
              {t("walletAction.payRefused", "The payment did not go through. Nothing was charged — you can try again.")}
            </p>
          ) : null}
          {error !== null ? (
            <p role="alert" className="text-sm text-accent-error">
              {isStepUpRequired(error) ? t("walletAction.stepUp", "For your safety, sign in again to continue.") : errorMessage(error, t)}
            </p>
          ) : null}
          <Button type="button" onClick={pay} disabled={submitting || toBaseUnits(sources.find((s) => s.key === chosen)?.available) < amount}>
            {submitting ? <Spinner aria-hidden /> : t("walletAction.payNow", "Pay")}
          </Button>
        </>
      ) : phase === "processing" ? (
        <p role="status" className="flex items-center gap-2 text-sm text-ink">
          {gaveUp ? null : <Spinner aria-hidden />}
          {gaveUp
            ? t("walletAction.processingSlow", "Still processing. You can close this window — the payment is not lost, and the service is told once it completes.")
            : t("walletAction.processing", "Processing your payment…")}
        </p>
      ) : phase === "paid" ? (
        <p role="status" className="text-sm text-ink">
          {t("walletAction.invoicePaid", "This invoice is paid.")}
        </p>
      ) : (
        <p className="text-sm text-ink-soft">{t("walletAction.invoiceClosed", "This invoice can no longer be paid. Nothing has been charged.")}</p>
      )}
    </div>
  );
}

function Message({ text }: { text: string }) {
  return <p className="mx-4 mt-8 text-sm text-ink-soft sm:mx-8">{text}</p>;
}
