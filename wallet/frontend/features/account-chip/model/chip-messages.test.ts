import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { CHIP_KEYS, chipMessages } from "./chip-messages.ts";

// `chip-messages.ts` duplicates its strings out of the main catalogues so the
// chip bundle does not have to carry all five of them (see the note in that
// file). Duplication is only acceptable while something checks it, which is this.

const LOCALES = ["en", "ru", "vi", "fr", "de"] as const;

const load = (locale: string): Record<string, unknown> =>
  JSON.parse(readFileSync(new URL(`../../../messages/${locale}/common.json`, import.meta.url), "utf8"));

const en = load("en") as Record<string, string>;

test("every chip key exists in the English catalogue", () => {
  for (const key of CHIP_KEYS) {
    assert.equal(typeof en[key], "string", `${key} missing from messages/en/common.json`);
  }
});

test("chip copy matches the main catalogue in every locale", () => {
  for (const locale of LOCALES) {
    const messages = chipMessages(locale);
    const catalogue = load(locale);
    for (const key of CHIP_KEYS) {
      const expected =
        locale === "en"
          ? en[key]
          : // A translation whose stored English has drifted is served as English by
            // the policy, so that is what the chip must show too.
            (() => {
              const entry = catalogue[key] as { en: string; t: string } | undefined;
              assert.ok(entry, `${locale}/${key} missing`);
              return entry.en === en[key] ? entry.t : en[key];
            })();
      assert.equal(
        messages[key],
        expected,
        `${locale}/${key} drifted: chip has ${JSON.stringify(messages[key])}, catalogue says ${JSON.stringify(expected)}`,
      );
    }
  }
});

test("the chip carries only the keys it renders", () => {
  for (const locale of LOCALES) {
    assert.deepEqual(
      Object.keys(chipMessages(locale)).sort(),
      [...CHIP_KEYS].sort(),
      `${locale} carries keys the chip does not render — the point of this module is that it is small`,
    );
  }
});

test("an unknown locale falls back to English rather than throwing", () => {
  // The chip reads its locale from a cookie on a page it does not control.
  assert.equal(chipMessages("xx" as never)["ui.account"], en["ui.account"]);
});
