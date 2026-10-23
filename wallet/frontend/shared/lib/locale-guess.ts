import { isLocale } from "@evinvest/i18n";

/**
 * Whether the locale an unprefixed entry resolves to is only a guess, which the account's
 * stored language may overrule (`LocaleSync`).
 *
 * `remembered` is the `ev_locale` cookie, `confirmed` the `ev_locale_chosen` one. The
 * first is written from anywhere — the proxy from a URL or from Accept-Language, the
 * public site from whatever page the reader was on — so on its own it cannot say whether
 * the reader ever picked it. The second is written by Settings alone, so a remembered
 * locale is a choice exactly when the two agree. Anything else (no cookie, a junk one,
 * one seeded from a shared link, one the public site rewrote after the choice) is
 * weaker than the account's language. Nothing here writes to the account.
 */
export function localeIsGuessed(remembered: string | undefined, confirmed: string | undefined): boolean {
  return !(isLocale(remembered) && remembered === confirmed);
}
