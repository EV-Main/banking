"use client";

// The wallet reads, cached once for the five screens that show them (Home, Wallet, Deposit,
// Withdraw, Invest) — and the two mutations that move them, re-exported so that naming what
// a withdrawal invalidates can't be forgotten at a call site. Views import from here, not
// from `../api/wallet-client`.

import { cancelWithdrawal as cancelWithdrawalRequest, fetchDepositAddress, fetchDeposits, fetchWallet, fetchWithdrawals, submitWithdrawal as submitWithdrawalRequest } from "@/entities/wallet/api/wallet-client";
import type { Withdrawal } from "@/shared/contracts";
import { TAG } from "@/shared/lib/cache-tags";
import { defineResource, revalidateTag } from "@/shared/lib/resource";

export const walletResource = defineResource({
  name: "wallet",
  fetch: fetchWallet,
  tags: [TAG.wallet],
});

export const withdrawalsResource = defineResource({
  name: "wallet.withdrawals",
  fetch: fetchWithdrawals,
  tags: [TAG.withdrawals],
});

export const depositsResource = defineResource({
  name: "wallet.deposits",
  fetch: fetchDeposits,
  tags: [TAG.deposits],
});

// A custody address is assigned per account per rail and does not rotate, so the only
// reason to ask twice is a new network — hence the long window and the per-network key.
// Not persisted: it is the user's own funding address, and memory-only is the honest scope.
export const depositAddressResource = defineResource({
  name: "wallet.depositAddress",
  fetch: fetchDepositAddress,
  key: (network) => network,
  revalidate: 600,
  enabled: (network) => network.trim().length > 0,
});

/** Request a withdrawal. Moves the balance, the queue, and the timeline. */
export async function submitWithdrawal(body: { network: string; address: string; amount: string; idempotency_key: string }): Promise<Withdrawal> {
  const created = await submitWithdrawalRequest(body);
  revalidateTag(TAG.wallet, TAG.withdrawals, TAG.operations);
  return created;
}

/** Withdraw a pending request. Releases the held funds, so the balance moves back. */
export async function cancelWithdrawal(withdrawalId: string): Promise<Withdrawal> {
  const cancelled = await cancelWithdrawalRequest(withdrawalId);
  revalidateTag(TAG.wallet, TAG.withdrawals, TAG.operations);
  return cancelled;
}
