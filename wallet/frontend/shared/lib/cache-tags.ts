// The invalidation vocabulary shared by every cached read and every mutation that moves it.
//
// A tag names a fact about the account, not an endpoint: `wallet` is "what this account
// holds", and a withdrawal moves it. Naming facts is what lets a mutation say what it
// changed without knowing which screens are mounted — `cancelWithdrawal` names wallet ·
// withdrawals · operations and every open surface reading any of the three refreshes itself.

export const TAG = {
  /** Balance and holdings. */
  wallet: "wallet",
  /** The caller's own withdrawal requests. */
  withdrawals: "withdrawals",
  /** The caller's own deposits. */
  deposits: "deposits",
  /** The merged activity timeline. Every money movement lands here. */
  operations: "operations",
  /** The caller's own profile. */
  profile: "profile",
} as const;
