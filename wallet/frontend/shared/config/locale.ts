import { locale as rootLocale } from "next/root-params";
import { isLocale, type Locale } from "@evinvest/i18n";

/**
 * The reader's locale, from the URL.
 *
 * It used to come from a cookie the proxy minted, on the reasoning that the
 * cabinet sits behind auth and a prefix would buy nothing. That is true of SEO
 * and false of the reader: the cabinet is mounted inside the conductor's shell at
 * `/{locale}/cabinet/…`, and someone reading evinvest.ltd in Russian who clicks
 * through to their cabinet should not have the language silently decided by a
 * cookie that may say something else. One URL shape across both, one answer to
 * "what language is this page".
 *
 * `next/root-params` rather than `params`: this is called from server components
 * that are handed no props (the status pages, and anything deep in a view), and
 * threading `params` through nineteen routes to reach them is exactly the cost
 * the old cookie was avoiding. Root params give the same value without it —
 * `[locale]` is the root layout's own segment, which is why that layout has to
 * stay the root (see `app/[locale]/layout.tsx`).
 *
 * Falls back to English rather than throwing: this renders pages, and a bad
 * segment is a 404 the layout raises, not a 500 from here.
 */
export async function currentLocale(): Promise<Locale> {
  const value = await rootLocale();
  return isLocale(value) ? value : "en";
}
