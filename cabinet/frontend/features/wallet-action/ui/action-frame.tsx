"use client";

import { useT } from "@evinvest/i18n/react";

import { createContext, type ReactNode, useContext } from "react";

import { Button, Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyTitle } from "@evinvest/uikit";

import type { ActionOutcome, ActionParams } from "../lib/action-params";
import type { FinishedAction } from "../model/use-wallet-action";

export interface ActionFrameProps {
  params: ActionParams;
  finished: FinishedAction | null;
  finish: (outcome: ActionOutcome, reference?: string | null) => void;
  /**
   * The action draws `<ActionCancel>` itself, in the row of its own buttons inside its card.
   * Without it the frame adds one under the content, for actions that are several cards.
   */
  inlineCancel?: boolean;
  children: ReactNode;
}

const CancelContext = createContext<(() => void) | null>(null);

// A popup opened by script may close itself; a tab the reader navigated to may not, and the
// call is then a no-op — the outcome screen stays, with its way back.
const close = () => window.close();

function Outcome({ finished, returnTo }: { finished: FinishedAction; returnTo: string | null }) {
  const t = useT();
  const title =
    finished.outcome === "done"
      ? t("walletAction.done", "Done")
      : finished.outcome === "failed"
        ? t("walletAction.failed", "That did not go through")
        : t("walletAction.cancelled", "Cancelled");
  return (
    <Empty>
      <EmptyHeader>
        <EmptyTitle>{title}</EmptyTitle>
        <EmptyDescription>
          {finished.reported
            ? t("walletAction.closeHint", "You can close this window — the page that opened it has been told.")
            : t("walletAction.closeHintLocal", "You can close this window now.")}
        </EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        {returnTo !== null && !finished.reported ? (
          <Button href={returnTo}>{t("walletAction.back", "Go back")}</Button>
        ) : (
          <Button type="button" variant="outline" onClick={close}>
            {t("walletAction.close", "Close window")}
          </Button>
        )}
      </EmptyContent>
    </Empty>
  );
}

/**
 * "Cancel" for an action card's button row. Only valid inside `<ActionFrame inlineCancel>`:
 * the frame owns the outcome, so the button cannot exist without it.
 */
export function ActionCancel({ disabled }: { disabled?: boolean }) {
  const t = useT();
  const cancel = useContext(CancelContext);
  if (cancel === null) throw new Error("ActionCancel needs an ActionFrame around it");
  return (
    <Button type="button" variant="outline" disabled={disabled} onClick={cancel}>
      {t("walletAction.cancel", "Cancel")}
    </Button>
  );
}

/**
 * The popup shell of a money action on evinvest.ltd: one centred column, no app chrome, the
 * action until it ends, then its outcome. Cancel reports `cancelled` so a waiting host stops
 * waiting, then closes the window; a tab that cannot close itself goes back to `return_to`.
 */
export function ActionFrame({ params, finished, finish, inlineCancel = false, children }: ActionFrameProps) {
  const t = useT();
  const cancel = () => {
    finish("cancelled");
    close();
    // Still here: not a script-opened window. Without an opener there is no host to tell,
    // only a page to return to; with one, the outcome screen offers "Close window".
    if (params.returnTo !== null && window.opener === null) window.location.assign(params.returnTo);
  };
  return (
    <div className="mx-auto flex w-full max-w-140 flex-col gap-4 px-4 py-6 sm:py-10">
      {finished !== null ? (
        <Outcome finished={finished} returnTo={params.returnTo} />
      ) : (
        <CancelContext value={cancel}>
          {children}
          {inlineCancel ? null : (
            <Button type="button" variant="outline" onClick={cancel}>
              {t("walletAction.cancel", "Cancel")}
            </Button>
          )}
        </CancelContext>
      )}
    </div>
  );
}
