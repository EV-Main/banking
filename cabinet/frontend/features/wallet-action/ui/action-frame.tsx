"use client";

import { useT } from "@evinvest/i18n/react";

import type { ReactNode } from "react";

import { Button, Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyTitle } from "@evinvest/uikit";

import type { ActionOutcome, ActionParams } from "../lib/action-params";
import type { FinishedAction } from "../model/use-wallet-action";

export interface ActionFrameProps {
  params: ActionParams;
  finished: FinishedAction | null;
  finish: (outcome: ActionOutcome, reference?: string | null) => void;
  children: ReactNode;
}

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
 * The chrome of a money action on evinvest.ltd: the action itself until it ends, then its
 * outcome. Cancel reports `cancelled` so a waiting host stops waiting.
 */
export function ActionFrame({ params, finished, finish, children }: ActionFrameProps) {
  const t = useT();
  if (finished !== null) return <Outcome finished={finished} returnTo={params.returnTo} />;
  return (
    <div className="flex flex-col gap-4">
      {children}
      <div className="flex justify-end px-4 pb-4 sm:px-8">
        <Button type="button" variant="ghost" onClick={() => finish("cancelled")}>
          {t("walletAction.cancel", "Cancel")}
        </Button>
      </div>
    </div>
  );
}
