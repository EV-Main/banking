// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The reason these need pinning down: this module is the only place the cabinet and the
// public site have to agree on a cookie *name*, and they compute it independently in two
// repositories (site_conductor `scripts/locale-cookie.ts`). If the two disagree the
// symptom is not an error — the conductor writes one cookie, the proxy reads another and
// finds nothing, and every reader silently falls back to Accept-Language. Which is
// exactly the bug this module was written to fix, so it would fix nothing while looking
// like it had.
import assert from "node:assert/strict";
import test, { afterEach } from "node:test";

import { confirmLocaleCookie, documentLocale, readLocaleCookie, writeLocaleCookie } from "./locale-cookie.ts";

// The module reaches for the two browser globals directly (it only ever runs in a
// browser), so a test has to supply them. Assigned onto globalThis rather than mocked
// through a seam, because the seam would be the thing under test.
function browser(protocol: "https:" | "http:", cookie = "") {
  (globalThis as Record<string, unknown>).location = { protocol };
  (globalThis as Record<string, unknown>).document = {
    cookie,
    documentElement: { lang: "" },
  };
  return globalThis.document as unknown as { cookie: string; documentElement: { lang: string } };
}

afterEach(() => {
  delete (globalThis as Record<string, unknown>).location;
  delete (globalThis as Record<string, unknown>).document;
});

test("over https the name carries the __Host- prefix the server also computes", () => {
  const doc = browser("https:");
  writeLocaleCookie("ru");
  assert.match(doc.cookie, /^__Host-ev_locale=ru;/);
  // `__Host-` is only accepted by browsers alongside Secure and Path=/, and the server
  // sets exactly these. A cookie that drops either is rejected outright — silently.
  assert.match(doc.cookie, /;path=\//);
  assert.match(doc.cookie, /;secure$/);
});

test("over plain http the prefix is dropped, matching the dev server", () => {
  const doc = browser("http:");
  writeLocaleCookie("de");
  assert.match(doc.cookie, /^ev_locale=de;/);
  // Secure over http would make the browser reject the cookie, so it must be absent.
  assert.doesNotMatch(doc.cookie, /secure/);
});

test("the remembered locale is read back out of a crowded cookie jar", () => {
  browser("https:", "__Host-ev_session=abc; __Host-ev_locale=vi; ab_hero=b");
  assert.equal(readLocaleCookie(), "vi");
});

test("a cookie whose name merely ends in the same characters is not mistaken for it", () => {
  // Substring matching here would read the session cookie of a differently-prefixed
  // deployment as a locale.
  browser("https:", "not-__Host-ev_locale=ru");
  assert.equal(readLocaleCookie(), null);
});

test("an unset or junk locale reads as null rather than throwing", () => {
  browser("https:", "");
  assert.equal(readLocaleCookie(), null);
  // `vn` is the country code for Vietnam and a plausible hand-edit; it is not a locale.
  browser("https:", "__Host-ev_locale=vn");
  assert.equal(readLocaleCookie(), null);
});

test("the rendered document wins over the cookie", () => {
  // The whole point: a reader on /ru who still has an English cookie from last week is
  // reading Russian *now*, and that is the language the account chip must link into.
  const doc = browser("https:", "__Host-ev_locale=en");
  doc.documentElement.lang = "ru";
  assert.equal(documentLocale(), "ru");
});

test("with no usable lang the cookie is the fallback, then English", () => {
  const doc = browser("https:", "__Host-ev_locale=fr");
  doc.documentElement.lang = "";
  assert.equal(documentLocale(), "fr");

  const bare = browser("https:", "");
  bare.documentElement.lang = "en-US";
  // A regional tag is not one of the five routable locales and there is no cookie, so
  // the floor applies rather than a half-match.
  assert.equal(documentLocale(), "en");
});

test("confirming writes both the locale and its twin under the same prefix rule", () => {
  // The proxy reads `ev_locale_chosen` by the server-computed name; a prefix mismatch
  // would silently leave every Settings choice looking like a guess.
  const writes: string[] = [];
  const doc = browser("https:");
  Object.defineProperty(doc, "cookie", { set: (v: string) => writes.push(v), get: () => "" });
  confirmLocaleCookie("vi");
  assert.match(writes[0], /^__Host-ev_locale=vi;/);
  assert.match(writes[1], /^__Host-ev_locale_chosen=vi;.*;secure$/);
});
