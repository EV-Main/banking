import { useT } from "@evinvest/i18n/react";

import { useEffect, useId, useRef, type KeyboardEvent } from "react";

import { Card, CardContent, CardHeader, CardTitle } from "@evinvest/uikit";

import { intentClientFor } from "../api/wallet-intents";
import { escapeCloses } from "../lib/escape-closes";
import type { WalletAction } from "../lib/wallet-action";
import { useIntentFlow, type IntentFlowControls } from "../model/use-intent-flow";
import { ActionRoot } from "./action-root";
import { ConfirmStep } from "./confirm-step";
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
  onClose: () => void;
  /** The same action as an `ev:wallet-action` offer, for what this page cannot finish yet. */
  onFallback: () => void;
}

function ActionForm({ props, controls }: { props: InPageActionProps; controls: IntentFlowControls }) {
  const { flow } = controls;
  const form: ActionFormProps = {
    draft: controls.draft,
    busy: flow.phase === "creating",
    error: flow.phase === "editing" ? flow.error : null,
    onSubmit: controls.submit,
    onCancel: props.onClose,
  };
  switch (props.action) {
    // `inPageMode` opens neither without a scope; the popup handles that case.
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
  const controls = useIntentFlow(intentClientFor(props.origin));
  const region = useFocusAcrossPhases(controls.flow.phase);
  const editing = controls.flow.phase === "editing" || controls.flow.phase === "creating";
  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key !== "Escape" || !escapeCloses(controls.flow.phase)) return;
    // The page around the form has its own Escape handlers; this one is the form's alone.
    event.stopPropagation();
    event.preventDefault();
    props.onClose();
  };
  const titles: Record<WalletAction, string> = {
    move: t("walletAction.moveTitle", "Move funds"),
    pay: t("walletMfe.payInvoice", "Pay invoice"),
    mandate: t("walletMfe.mandateTitle", "Allow a service to use your funds"),
    withdraw: t("ui.withdraw", "Withdraw"),
  };
  return (
    <ActionRoot sheetHref={props.sheetHref}>
      <Card ref={region} role="region" aria-labelledby={titleId} tabIndex={-1} className="outline-none" onKeyDown={onKeyDown}>
        <CardHeader>
          <CardTitle id={titleId}>{titles[props.action]}</CardTitle>
        </CardHeader>
        <CardContent>
          {editing ? (
            <ActionForm props={props} controls={controls} />
          ) : (
            <ConfirmStep kind={props.action} controls={controls} onClose={props.onClose} onFallback={props.onFallback} />
          )}
        </CardContent>
      </Card>
    </ActionRoot>
  );
}
