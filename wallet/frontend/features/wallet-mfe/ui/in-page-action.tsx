import { useT } from "@evinvest/i18n/react";

import { useEffect, useId, useRef, type KeyboardEvent } from "react";

import { Alert, AlertTitle, Button, Card, CardContent, CardHeader, CardTitle } from "@evinvest/uikit";

import { ConfirmStep, StepupSurface, useIntentFlow, type IntentFlowControls } from "@/features/wallet-intent";

import { intentClientFor } from "../api/wallet-intents";
import { escapeCloses } from "../lib/escape-closes";
import { closedAction, type ClosedAction } from "../lib/wallet-result";
import type { WalletAction } from "../lib/wallet-action";
import { ActionRoot } from "./action-root";
import type { ActionFormProps } from "./form-parts";
import { MandateForm } from "./mandate-form";
import { MoveForm } from "./move-form";
import { PayForm } from "./pay-form";
import { WithdrawForm } from "./withdraw-form";

export interface InPageActionProps {
  /** The wallet origin: the BFF the intents go to. */
  origin: string;
  /** The wallet stylesheet next to the bundle, adopted into the closed root. */
  sheetHref: string;
  /**
   * The element's `scope`, which a move and a payment name in their params. The BFF takes the
   * host's scope from the page's origin and refuses params that name another.
   */
  scope: string | null;
  action: WalletAction;
  invoice: string | null;
  request: string | null;
  /** How the action ended, read from the flow at the moment the form closes. */
  onClose: (closed: ClosedAction) => void;
  /** The same action as an `ev:wallet-action` offer, for when the step-up cannot load here. */
  onFallback: () => void;
}

function ActionForm({ props, controls, onCancel }: { props: InPageActionProps; controls: IntentFlowControls<WalletAction>; onCancel: () => void }) {
  const { flow } = controls;
  const form: ActionFormProps = {
    draft: controls.draft,
    busy: flow.phase === "creating",
    error: flow.phase === "editing" ? flow.error : null,
    onSubmit: controls.submit,
    onCancel,
  };
  switch (props.action) {
    // `actionMode` opens neither without a scope; the popup handles that case.
    case "move":
      return props.scope === null ? null : <MoveForm {...form} scope={props.scope} />;
    case "pay":
      return props.scope === null ? null : <PayForm {...form} scope={props.scope} invoice={props.invoice} />;
    case "mandate":
      return <MandateForm {...form} request={props.request} />;
    case "withdraw":
      return <WithdrawForm {...form} origin={props.origin} />;
  }
}

/**
 * After a phase change, focus back on the form when the element that held it went away (the
 * Review or Confirm button is replaced by the next step) and nothing else took it: left on the
 * body, Escape and Tab would start from the top of the host page. Focus the person moved into
 * the host page, or that is still on an element inside the form, stays where it is.
 */
function useFocusAcrossPhases(phase: string) {
  const region = useRef<HTMLDivElement>(null);
  const shown = useRef(phase);
  useEffect(() => {
    if (shown.current === phase) return;
    shown.current = phase;
    const card = region.current;
    const root = card?.getRootNode();
    if (!card || !(root instanceof ShadowRoot) || root.activeElement !== null) return;
    const outside = document.activeElement;
    if (outside === null || outside === document.body) card.focus();
  }, [phase]);
  return region;
}

/** One money action inside the host page: the form, the BFF's terms, the confirm. */
export function InPageAction(props: InPageActionProps) {
  const t = useT();
  const titleId = useId();
  // Nothing to do on the answer itself: the form reports the flow as it stands when it closes.
  const controls = useIntentFlow<WalletAction>(intentClientFor(props.origin), () => undefined);
  const region = useFocusAcrossPhases(controls.flow.phase);
  const editing = controls.flow.phase === "editing" || controls.flow.phase === "creating";
  const close = () => props.onClose(closedAction(controls.flow));
  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key !== "Escape" || !escapeCloses(controls.flow.phase)) return;
    // The page around the form has its own Escape handlers; this one is the form's alone.
    event.stopPropagation();
    event.preventDefault();
    close();
  };
  const titles: Record<WalletAction, string> = {
    move: t("walletAction.moveTitle", "Move funds"),
    pay: t("walletMfe.payInvoice", "Pay invoice"),
    mandate: t("walletMfe.mandateTitle", "Allow a service to use your funds"),
    withdraw: t("ui.withdraw", "Withdraw"),
  };
  // The step-up element stays outside the closed root: its styles live in the document's head
  // and its focus trap works through `document`, neither of which reaches inside.
  return (
    <>
      <ActionRoot sheetHref={props.sheetHref}>
        <Card ref={region} role="region" aria-labelledby={titleId} tabIndex={-1} className="outline-none" onKeyDown={onKeyDown}>
          <CardHeader>
            <CardTitle id={titleId}>{titles[props.action]}</CardTitle>
          </CardHeader>
          <CardContent>
            {editing ? (
              <ActionForm props={props} controls={controls} onCancel={close} />
            ) : controls.flow.phase === "done" ? (
              <div className="flex flex-col items-start gap-4">
                <Alert variant="success" role="status">
                  <AlertTitle>{t("walletAction.done", "Done")}</AlertTitle>
                </Alert>
                <Button type="button" onClick={close}>
                  {t("walletMfe.close", "Close")}
                </Button>
              </div>
            ) : (
              <ConfirmStep kind={props.action} controls={controls} />
            )}
          </CardContent>
        </Card>
      </ActionRoot>
      <StepupSurface dialog={controls.stepupDialog} scopeHost={{ origin: props.origin, handoff: props.onFallback }} />
    </>
  );
}
