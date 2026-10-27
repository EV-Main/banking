"use client";

// "This payment is out of your account — do you agree?" — reached signed in, from the notice
// that asks or as a money action a scope host opens in a popup (`@evinvest/wallet`
// `walletPaymentUrl`). The money is the reader's: nobody else is asked, and nobody else can
// answer.
//
// The terms are the server's, shown whole before either button is offered. What an answer
// does is read back from the server; this page predicts nothing about the order's state.

import { CheckCircle2, Clock, XCircle } from "lucide-react";
import { useEffect, useState } from "react";

import { useLocale, useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, CardDescription, CardHeader, Spinner } from "@evinvest/uikit";

import { fetchPaymentConfirmation, rejectPayment } from "@/entities/payment/api/confirmation-client";
import { ConfirmStep, firstPartyIntentClient, IntentErrorAlert, StepupSurface, useIntentFlow } from "@/features/wallet-intent";
import type { ConsentDecision, PaymentConfirmation } from "@/shared/contracts/payments";
import { errorMessage, RequestError } from "@/shared/lib/api-client";
import { expiresIn, formatMoment, hasExpired } from "@/shared/lib/datetime";
import { settledConsent } from "@/shared/lib/decision";
import { ActionCancel, ActionFrame, useWalletAction } from "@/features/wallet-action";
import { ApprovalOutcome, ApprovalSkeleton, ApprovalTitle, ApprovalUnreachable, DetailRow } from "@/views/approval/ui/approval-chrome";
import { PaymentTermsBlock, renderablePayment } from "@/views/approval/ui/payment-terms";

const COLUMN = "flex flex-col gap-6";

/**
 * The payment answer as a wallet money action: an approval reports `done`, a refusal
 * `cancelled`, both with the payment id as the reference; the host re-reads the payment
 * from its own backend either way. Only the id comes from the URL — from the path, not the
 * query — and `scope`, `opener`, `state` and `return_to` follow the action contract.
 */
export function PaymentConfirmView({ id }: { id: string }) {
  const { params, finished, finish } = useWalletAction();
  // Reached from the notice, there is nobody to report to and nowhere to go back to: the
  // frame's bare "Done" would replace this page's own account of what the answer did.
  const hosted = params.opener !== null || params.returnTo !== null;
  const answered = (decision: ConsentDecision) => {
    if (hosted) finish(decision === "approve" ? "done" : "cancelled", id);
  };
  return (
    <ActionFrame inlineCancel params={params} finished={finished} finish={finish}>
      <PaymentAnswer id={id} hosted={hosted} onAnswered={answered} />
    </ActionFrame>
  );
}

function PaymentAnswer({ id, hosted, onAnswered }: { id: string; hosted: boolean; onAnswered: (decision: ConsentDecision) => void }) {
  const t = useT();
  const locale = useLocale();
  const [payment, setPayment] = useState<PaymentConfirmation | null>(null);
  const [missing, setMissing] = useState(false);
  const [loadError, setLoadError] = useState(false);
  const [reload, setReload] = useState(0);
  const [rejecting, setRejecting] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [justAnswered, setJustAnswered] = useState(false);
  const answered = (read: PaymentConfirmation, decision: ConsentDecision) => {
    setPayment(read);
    setJustAnswered(true);
    onAnswered(decision);
  };
  const controls = useIntentFlow<"approve_payment">(firstPartyIntentClient, ({ result }) => answered(result, "approve"));
  const { flow } = controls;
  const answering = flow.phase !== "editing" || rejecting;

  useEffect(() => {
    let live = true;
    fetchPaymentConfirmation(id)
      .then((read) => live && setPayment(read))
      .catch((e: unknown) => {
        if (!live) return;
        if (e instanceof RequestError && e.status === 404) setMissing(true);
        else setLoadError(true);
      });
    return () => {
      live = false;
    };
  }, [id, reload]);

  const retry = () => {
    setLoadError(false);
    setReload((n) => n + 1);
  };

  const approve = () => {
    setError(null);
    controls.submit({ kind: "approve_payment", params: { payment_id: id } });
  };

  const reject = async () => {
    if (answering) return;
    setRejecting(true);
    setError(null);
    try {
      answered(await rejectPayment(id), "reject");
    } catch (e) {
      setError(e);
      // A refusal that closed the order (a pin moved) is read back, not guessed at.
      setReload((n) => n + 1);
    } finally {
      setRejecting(false);
    }
  };

  // Nothing left to answer here: a host that opened the popup still needs to hear that it ended.
  const leave = hosted ? <ActionCancel /> : null;

  if (missing) {
    return (
      <div className={COLUMN}>
        <ApprovalOutcome icon={<XCircle />} title={t("paymentConfirm.missing.title", "This payment is not available")} description={t("paymentConfirm.missing.body", "It may have been answered, withdrawn or have expired, or it was not made for this account. Nothing was approved.")} />
        {leave}
      </div>
    );
  }
  if (payment === null) {
    return (
      <div className={COLUMN}>
        {loadError ? <ApprovalUnreachable onRetry={retry} retrying={false} /> : <ApprovalSkeleton />}
        {loadError ? leave : null}
      </div>
    );
  }
  if (!renderablePayment(payment)) {
    return (
      <div className={COLUMN}>
        <ApprovalOutcome icon={<XCircle />} title={t("paymentConfirm.unrenderable.title", "The terms did not arrive")} description={t("paymentConfirm.unrenderable.body", "There is nothing here to agree to, so no answer is offered. This is a problem on our side. Try again in a moment.")}>
          <Button variant="outline" onClick={retry}>
            {t("status.tryAgain", "Try again")}
          </Button>
        </ApprovalOutcome>
        {leave}
      </div>
    );
  }

  const settled = settledConsent(payment.decision);
  const open = payment.state === "pending" && !settled && !payment.invalidated;
  const expired = payment.state === "pending" && !settled && hasExpired(payment.expires_at);

  return (
    <div className={COLUMN}>
      <Card>
        <CardHeader>
          <ApprovalTitle>{t("paymentConfirm.title", "A payment out of your account")}</ApprovalTitle>
          <CardDescription className="text-balance">
            <strong className="font-semibold text-ink">{t("paymentConfirm.leadOwn", "This is your money.")}</strong> {t("paymentConfirm.lead", "{initiator} has asked for it to be moved as set out below. Nothing happens unless you agree.", { initiator: payment.initiator_email })}
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-5">
          <PaymentTermsBlock terms={payment} payloadHash={payment.payload_hash} reasonLabel={t("paymentConfirm.reasonLabel", "Reason given by {initiator}", { initiator: payment.initiator_email })} />
          <DetailRow
            label={t("approval.expires", "Expires")}
            value={t("approval.expiresValue", "{at} · {left}", { at: formatMoment(payment.expires_at, locale), left: expiresIn(payment.expires_at, t) })}
            tone={expired ? "text-accent-error" : undefined}
          />
        </CardContent>
      </Card>

      {open && !expired && (flow.phase === "editing" || flow.phase === "creating") ? (
        <div className="flex flex-col gap-3">
          {error !== null ? (
            <p role="alert" className="text-sm text-accent-error">
              {errorMessage(error, t)}
            </p>
          ) : null}
          <IntentErrorAlert error={flow.phase === "editing" ? flow.error : null} />
          <Button type="button" onClick={approve} disabled={answering}>
            {flow.phase === "creating" ? <Spinner aria-hidden /> : t("paymentConfirm.approve", "Approve this payment")}
          </Button>
          <Button type="button" variant="outline" onClick={() => void reject()} disabled={answering}>
            {rejecting ? <Spinner aria-hidden /> : t("paymentConfirm.reject", "Decline")}
          </Button>
        </div>
      ) : open && !expired && flow.phase !== "done" ? (
        <ConfirmStep kind="approve_payment" controls={controls} />
      ) : (
        <>
          <Outcome payment={payment} settled={settled} expired={expired} fresh={justAnswered} />
          {leave}
        </>
      )}
      <StepupSurface dialog={controls.stepupDialog} scopeHost={null} />
    </div>
  );
}

function Outcome({ payment, settled, expired, fresh }: { payment: PaymentConfirmation; settled: "approve" | "reject" | null; expired: boolean; fresh: boolean }) {
  const t = useT();
  if (payment.invalidated) {
    return <ApprovalOutcome icon={<XCircle />} tone="text-accent-error" title={t("paymentConfirm.void.title", "This payment was stopped")} description={t("paymentConfirm.void.body", "Your sign-in sessions were revoked or your address changed after it was opened, so it can no longer be approved. Nothing moved.")} />;
  }
  if (payment.state === "execution_failed") {
    return <ApprovalOutcome icon={<XCircle />} tone="text-accent-error" title={t("paymentConfirm.failed.title", "The payment could not be carried out")} description={payment.failure_reason ?? t("paymentConfirm.failed.body", "Your answer stands, but the payment did not go through. Nothing was taken from your account.")} />;
  }
  if (expired || payment.state === "expired") {
    return <ApprovalOutcome icon={<Clock />} title={t("paymentConfirm.expired.title", "This payment ran out of time")} description={t("paymentConfirm.expired.body", "Nobody answered in time, so it lapsed. Nothing moved.")} />;
  }
  if (payment.state === "cancelled") {
    return <ApprovalOutcome icon={<XCircle />} title={t("paymentConfirm.cancelled.title", "This payment was withdrawn")} description={t("paymentConfirm.cancelled.body", "Whoever asked took the request back. Nothing moved.")} />;
  }
  if (settled === "reject" || payment.state === "rejected") {
    return <ApprovalOutcome icon={<XCircle />} tone="text-ink-soft" title={t("paymentConfirm.declined.title", "You declined this payment")} description={fresh ? t("paymentConfirm.declined.fresh", "Your answer has been recorded. Your money stays where it is.") : t("approval.decided.body", "You answered this earlier, and that answer stands. An answer cannot be changed once it is given.")} />;
  }
  return <ApprovalOutcome icon={<CheckCircle2 />} tone="text-positive" title={t("paymentConfirm.approved.title", "You approved this payment")} description={fresh ? t("paymentConfirm.approved.fresh", "Your answer has been recorded and the payment goes ahead.") : t("approval.decided.body", "You answered this earlier, and that answer stands. An answer cannot be changed once it is given.")} />;
}
