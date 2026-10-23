// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The bug behind these (#390) was two modules disagreeing about what `returnTo` holds:
// the proxy wrote the real `/{locale}/cabinet/…` path and the login view prefixed it
// again, so every deep link for a signed-out visitor came back as
// `/cabinet/{locale}/cabinet/…`. The property pinned here is that whatever shape arrives,
// the shell is handed the page once-prefixed — and never a foreign origin.
import assert from "node:assert/strict";
import test from "node:test";

import { LOCALES } from "@evinvest/i18n";

import { cabinetPath } from "../../../shared/config/base-path.ts";
import { loginHref, loginReturnTo, safeReturnTo } from "./return-to.ts";

test("a zone-relative returnTo is prefixed exactly once", () => {
  assert.equal(loginReturnTo("ru", "/wallet"), "/ru/cabinet/wallet");
  assert.equal(loginReturnTo("en", "/invest/alpha-fund?tab=orders"), "/en/cabinet/invest/alpha-fund?tab=orders");
  assert.equal(loginReturnTo("de", "/admin/revenue#top"), "/de/cabinet/admin/revenue#top");
});

test("the zone root and a missing returnTo both land on the cabinet home", () => {
  for (const locale of LOCALES) {
    assert.equal(loginReturnTo(locale, "/"), cabinetPath(locale, "/"), locale);
    assert.equal(loginReturnTo(locale, null), cabinetPath(locale, "/"), locale);
    assert.equal(loginReturnTo(locale, undefined), cabinetPath(locale, "/"), locale);
    assert.equal(loginReturnTo(locale, ""), cabinetPath(locale, "/"), locale);
  }
});

test("a full cabinet path is accepted and not prefixed twice", () => {
  // The shapes production actually produced (issue #390) plus an old bookmark of the
  // unprefixed mount.
  assert.equal(loginReturnTo("en", "/en/cabinet"), "/en/cabinet");
  assert.equal(loginReturnTo("ru", "/ru/cabinet/wallet"), "/ru/cabinet/wallet");
  assert.equal(loginReturnTo("en", "/cabinet/wallet"), "/en/cabinet/wallet");
  assert.equal(loginReturnTo("en", "/en/cabinet?tab=x"), "/en/cabinet?tab=x");
  assert.equal(loginReturnTo("vi", "/vi/cabinet/invest/x?y=1"), "/vi/cabinet/invest/x?y=1");
});

test("the login page's locale wins over one carried in a full path", () => {
  // Both come from the same request in the proxy's bounce; a hand-written link is the
  // only way they differ, and the page the reader is looking at is the better guess.
  assert.equal(loginReturnTo("ru", "/en/cabinet/wallet"), "/ru/cabinet/wallet");
});

test("every locale round-trips its own pages through the login", () => {
  for (const locale of LOCALES) {
    for (const path of ["/", "/wallet", "/admin/revenue"] as const) {
      const full = cabinetPath(locale, path);
      assert.equal(loginReturnTo(locale, path), full, `${locale} ${path} (zone-relative)`);
      assert.equal(loginReturnTo(locale, full), full, `${locale} ${path} (full)`);
    }
  }
});

test("an open redirect is refused and falls back to the cabinet home", () => {
  for (const raw of ["//evil.example", "/\\evil.example", "/wallet\\..", "https://evil.example/", "wallet"]) {
    assert.equal(safeReturnTo(raw), "/", raw);
    assert.equal(loginReturnTo("en", raw), "/en/cabinet", raw);
  }
});

test("the provider href points at the shell's login, naming the provider, with the page URL-encoded", () => {
  assert.equal(loginHref("ru", "/wallet", "google"), "/api/auth/login?provider=google&returnTo=%2Fru%2Fcabinet%2Fwallet");
  assert.equal(loginHref("en", undefined, "github"), "/api/auth/login?provider=github&returnTo=%2Fen%2Fcabinet");
  assert.equal(loginHref("en", "/invest/x?y=1", "google"), "/api/auth/login?provider=google&returnTo=%2Fen%2Fcabinet%2Finvest%2Fx%3Fy%3D1");
});
