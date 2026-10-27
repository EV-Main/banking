// Browser → BFF wallet client. Thin typed fetchers over the BFF route handlers; the
// shapes are the proto-derived types from `@/shared/contracts`. Transport, CSRF and
// session handling belong to `@/shared/lib/api-client`. No tokens are ever seen here —
// the BFF holds them.

import { getJson, postJson } from "@/shared/lib/api-client";
import type { DepositAddress, DepositList, Wallet, Withdrawal, WithdrawalList } from "@/shared/contracts";

export function fetchWallet(): Promise<Wallet> {
  return getJson<Wallet>("/api/wallet");
}

export function fetchDepositAddress(network: string): Promise<DepositAddress> {
  return getJson<DepositAddress>(`/api/wallet/deposit-address?network=${encodeURIComponent(network)}`);
}

export function fetchWithdrawals(): Promise<WithdrawalList> {
  return getJson<WithdrawalList>("/api/wallet/withdrawals");
}

export function fetchDeposits(): Promise<DepositList> {
  return getJson<DepositList>("/api/wallet/deposits");
}

export function cancelWithdrawal(withdrawalId: string): Promise<Withdrawal> {
  return postJson<Withdrawal>("/api/wallet/withdrawals/cancel", { withdrawal_id: withdrawalId });
}
