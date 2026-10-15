"use client";

import { useT } from "@evinvest/i18n/react";

import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from "@evinvest/uikit";

import { useWalletAction } from "../model/use-wallet-action";
import { ActionFrame } from "./action-frame";

export type PendingAction = "move" | "pay" | "mandate";

/**
 * An action whose page exists — so the host contract and its four paths are whole — but
 * whose money route does not yet: moving between compartments, paying an invoice and
 * granting a mandate have no BFF route today. It says so, and Cancel tells the host.
 */
export function UnavailableAction({ action }: { action: PendingAction }) {
  const t = useT();
  const { params, finished, finish } = useWalletAction();
  const title =
    action === "move"
      ? t("walletAction.moveTitle", "Move funds")
      : action === "pay"
        ? t("walletAction.payTitle", "Pay invoice")
        : t("walletAction.mandateTitle", "Allow a service to charge you");
  return (
    <ActionFrame params={params} finished={finished} finish={finish}>
      <Empty className="mx-4 mt-8 sm:mx-8">
        <EmptyHeader>
          <EmptyTitle>{title}</EmptyTitle>
          <EmptyDescription>{t("walletAction.unavailable", "This is not available yet. Nothing has been charged or moved.")}</EmptyDescription>
        </EmptyHeader>
      </Empty>
    </ActionFrame>
  );
}
