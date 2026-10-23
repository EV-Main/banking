// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The document title is metadata, not body copy, so the only thing that ever checked it
// was `i18n:check` — and that gate had "EV Investment — Cabinet" on its loanword allowlist
// for de and vi, which is how the tab title shipped in English under both (#348). This
// pins the outcome from the reader's side: the same policy the layout's `generateMetadata`
// applies must yield a non-English title and description in every non-English locale.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { DEFAULT_LOCALE, LOCALES, translator, type Locale } from "@evinvest/i18n";
import { resolveCatalogue, type TranslatedCatalogue } from "@evinvest/i18n/policy";

// Read rather than import `./i18n.ts`: the runner strips types but resolves neither the
// `@/` alias nor JSON modules, so this rebuilds `messagesFor` from the same inputs and
// the same `resolveCatalogue` — a drifted entry falls back to English here too.
const catalogue = (locale: Locale) =>
  JSON.parse(readFileSync(new URL(`../../messages/${locale}/common.json`, import.meta.url), "utf8"));
const en = catalogue(DEFAULT_LOCALE) as Record<string, string>;
const translate = (locale: Locale) =>
  translator(
    locale === DEFAULT_LOCALE ? en : resolveCatalogue(locale, en, catalogue(locale) as TranslatedCatalogue).messages,
    locale,
  );

for (const key of ["meta.title", "meta.description"]) {
  test(`${key} is translated in every non-English locale`, () => {
    // `translator` answers a missing key with the English it was handed, never an empty
    // string — so presence is checked on the catalogue, not on the resolved text.
    assert.ok(key in en, `${key} is missing from messages/en/common.json`);
    const source = translate(DEFAULT_LOCALE)(key, en[key] ?? key);
    for (const locale of LOCALES) {
      if (locale === DEFAULT_LOCALE) continue;
      const text = translate(locale)(key, source);
      assert.notEqual(text, source, `${locale}/${key} is still the English "${source}"`);
    }
  });
}
