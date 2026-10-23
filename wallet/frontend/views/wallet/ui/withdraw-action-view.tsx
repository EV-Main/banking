"use client";

import { ActionFrame, useWalletAction } from "@/features/wallet-action";
import { WithdrawView } from "@/views/wallet/ui/withdraw-view";

/**
 * The withdrawal as a money action on evinvest.ltd: the wallet host's own form, opened by a
 * scope host in a popup (or navigated to). The withdrawal id goes back as the reference
 * once the hub has accepted the request — accepted, not settled: the host re-reads its
 * backend for what actually moved.
 */
export function WithdrawActionView() {
  const { params, finished, finish } = useWalletAction();
  return (
    <ActionFrame params={params} finished={finished} finish={finish}>
      <WithdrawView onWithdrawn={(withdrawal) => finish("done", withdrawal.id ?? null)} />
    </ActionFrame>
  );
}
