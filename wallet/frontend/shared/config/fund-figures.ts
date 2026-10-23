// The fund's headline figures — the ONLY place in the cabinet a number about the fund is
// typed.
//
// MIRROR of the public site's `frontend/shared/config/fund-figures.ts` (site_conductor),
// field for field — less its `kycTiming`, which is an English sentence and lives in the
// catalogue here as `kyc.dialog.timeBody`. The landing's hero and the cabinet's sign-in panel quote the same fund,
// and a visitor who clicks through from one to the other must not watch it contradict
// itself (site_conductor #204, banking #385). The two repos share no code yet; until this
// object moves into an `@evinvest/*` package, a change on either side is a change on both.
//
// Confirmed by the owner on 2026-09-18 (#204, #385): the values are the ones the site
// showed before they were centralised, and `asOf` is the confirmation date. Set `asOf`
// back to `undefined` to take the "as of" line off the page while a new figure is pending
// — a placeholder date must never render as a fact.

export interface FundFigures {
  /** Target IRR, percent per annum. Rendered as a floor: "16.4% +". */
  readonly targetIrrPct: number;
  /** Hard cap the fund closes at, in USD millions. Rendered "$100M". */
  readonly closingTargetUsdM: number;
  /**
   * ISO 8601 calendar date the figures were last confirmed on. `undefined` means nobody
   * has confirmed them yet and the "as of" line MUST NOT render — a placeholder date next
   * to a return figure reads as a fact.
   */
  readonly asOf: string | undefined;
  /**
   * Minimum subscription, USD. `undefined` means the owner has not set one and copy MUST
   * omit the sentence — never render a placeholder amount.
   */
  readonly minSubscriptionUsd: number | undefined;
}

export const FUND_FIGURES: FundFigures = {
  targetIrrPct: 16.4,
  closingTargetUsdM: 100,
  asOf: "2026-09-18",
  minSubscriptionUsd: undefined,
};
