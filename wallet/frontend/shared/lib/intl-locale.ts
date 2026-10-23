import type { Locale } from "@evinvest/i18n";

/**
 * The BCP 47 tag to hand `Intl` for one of our locales.
 *
 * Four of the five map to themselves. `en` does not: passing `"en"` to `Intl`
 * gets you `en-US`, which renders "Mar 12", while the cabinet has always shown
 * "12 Mar 2026". That is not a preference to re-litigate per screen — it is the
 * existing output, and an English reader seeing "12 Mar" in their timeline and
 * "Mar 12" in their notifications is a bug whichever convention you prefer.
 *
 * So this lives in `shared/` rather than beside one view's formatter: it is the
 * cabinet's answer to "what does `en` mean to `Intl`", and there is only one.
 *
 * Money is deliberately NOT routed through here — `shared/lib/money.ts` owns one
 * policy per unit of measure and maps `en` to `en-US` itself, because that is the
 * tag its English output was always produced with (and `en-GB` would spell the
 * dollar "US$").
 */
export function intlLocale(locale: Locale): string {
  return locale === "en" ? "en-GB" : locale;
}
