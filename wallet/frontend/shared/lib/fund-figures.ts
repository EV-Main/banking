// The fund figures as the locale writes them — the cabinet's twin of the site's
// `shared/lib/fund-figures.ts`, so the sign-in panel renders the same shape the hero does:
// "16.4% +" / "16,4 % +" and "$100M". The digits localise; the `$` and `M` do not, on
// purpose — `Intl`'s compact currency would restyle it to "$100m" under en-GB.
//
// Nothing renders a raw value from `FUND_FIGURES`: a surface that wants a figure asks here.

import type { Locale } from "@evinvest/i18n";

// Relative with extensions, like `views/invest/lib/subscribe-check.ts`: the node test
// runner resolves no `@/` alias, and this is one of the modules it runs.
import { FUND_FIGURES, type FundFigures } from "../config/fund-figures.ts";
import { formatCalendarDate } from "./datetime.ts";
import { formatPlainPct, formatWhole } from "./money.ts";

export interface FormattedFundFigures {
  /** "16.4% +" / "16,4 % +" — the locale's percent, then the floor marker. */
  targetIrr: string;
  /** "$100M" — the digits localised, the currency and unit not. */
  closingTarget: string;
  /** The as-of date in prose ("18 September 2026"), or absent while the figures are unconfirmed. */
  asOf?: string;
}

// `figures` is a parameter only so the test can exercise the unconfirmed branch; every
// surface passes nothing and reads the one real object.
export function formatFundFigures(locale: Locale, figures: FundFigures = FUND_FIGURES): FormattedFundFigures {
  const { targetIrrPct, closingTargetUsdM, asOf } = figures;
  return {
    targetIrr: `${formatPlainPct(targetIrrPct, locale)} +`,
    closingTarget: `$${formatWhole(closingTargetUsdM, locale)}M`,
    ...(asOf !== undefined && { asOf: formatCalendarDate(asOf, locale) }),
  };
}
