import { isLocale, type Locale } from "@evinvest/i18n";

// What a URL proves about the language a signed-in reader wants — the decision the
// `LocaleSync` component (application/layout) carries out on every mount. Kept free of
// React and the DOM so the rule can be pinned down by tests; the component only
// gathers the inputs and acts on the verdict.

export type LocaleSyncInput = {
  /** The locale of the page the reader is on. */
  locale: Locale;
  /** `profile.language` as the API reported it — `""` when the account has none. */
  stored: string;
  /**
   * The proxy's "this locale came from Accept-Language" mark, raw: the locale it was
   * minted for, or null when there is none.
   */
  marker: string | null;
};

export type LocaleSyncDecision =
  | { kind: "none" }
  /** The URL was a guess and the account knows better: navigate to the stored one. */
  | { kind: "adopt-stored"; to: Locale };

/**
 * The rule: nothing about a URL ever changes the account, and the account only ever
 * changes the URL when the URL was a guess.
 *
 * It used to be the other way round — any of the four prefixed locales was read as
 * "the most recent thing the reader did" and written to the account — and a shared
 * `/de/cabinet/profile` link then silently rewrote an account set to Vietnamese on
 * every device at once (#347). Settings promises "saved to your account, so every
 * device follows"; a preference that follows the reader cannot also follow the link
 * they clicked. Nor can an account with no language be seeded from a URL: the proxy
 * remembers a guessed locale in the `ev_locale` cookie, and the next unprefixed entry
 * redirects to it — so a URL that looks deliberate may be last week's guess laundered
 * through a cookie. The proxy therefore marks that entry too unless Settings confirmed
 * the cookie (`ev_locale_chosen`, #367). Settings is the only writer.
 *
 * What remains is a read. The mark is matched on its value, not its presence: it
 * describes the entry that minted it, and one left behind by an earlier redirect the
 * reader never completed cannot speak for a URL it never saw. When it does name this
 * page and the account has a routable language of its own, that wins — the reader who
 * chose Deutsch on a laptop opens the cabinet in German on a phone, not in the phone's
 * OS language. A guess that matches the stored language, or meets an account with no
 * language, changes nothing.
 */
export function decideLocaleSync(input: LocaleSyncInput): LocaleSyncDecision {
  const { locale, stored, marker } = input;
  const guessed = marker === locale;
  if (guessed && isLocale(stored) && stored !== locale) return { kind: "adopt-stored", to: stored };
  return { kind: "none" };
}
