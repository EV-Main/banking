// The scope hosts a wallet action page may answer. An action page on evinvest.ltd posts its
// result to the window that opened it, and sends a navigated user back — both only to an
// address named here, compared byte for byte: a page that took its opener or return
// address from the URL alone would hand the outcome of a money action to whoever built the
// link.
//
// In code rather than env on purpose: a list that is wrong in one environment's secret is a
// hole nobody sees, a list in the repo is reviewed. It mirrors the `origins` concierge keeps
// per scope (plan §2); once that registry is readable from the BFF this table goes.

export interface WalletScopeHost {
  /** Exact `https://host` origins of the scope's apps — the only openers a page posts to. */
  origins: readonly string[];
  /** Paths on those origins a navigated action may return to. Empty: none, the page stays put. */
  returnPaths: readonly string[];
}

export const WALLET_SCOPE_HOSTS: Readonly<Record<string, WalletScopeHost>> = {
  invest: { origins: ["https://invest.evinvest.ltd"], returnPaths: [] },
  // The panel's static export serves every screen with a trailing slash.
  sa: { origins: ["https://sa.evinvest.ltd"], returnPaths: ["/invoices/"] },
};
