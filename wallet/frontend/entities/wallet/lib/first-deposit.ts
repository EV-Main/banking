import type { Deposit, DepositList } from "@/shared/contracts";

/**
 * The account's first credited deposit, if the list proves it is the first: `/api/wallet/
 * deposits` is the whole history (the hub lists everything credited, newest first, under a
 * defensive cap of 200 that no first deposit is near), so a list of exactly one entry names
 * the first deposit with certainty, and a longer one means the
 * first happened before — on some earlier visit, or some other device — and is not this
 * tab's to announce. `null` on an empty or unread list, and on a credit with no reference.
 */
export function soleDeposit(list: DepositList | undefined): (Deposit & { tx_ref: string }) | null {
  const deposits = list?.deposits ?? [];
  if (deposits.length !== 1) return null;
  const [only] = deposits;
  return only?.tx_ref ? { ...only, tx_ref: only.tx_ref } : null;
}
