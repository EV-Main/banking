// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// These two functions are a pair, and the bug that prompted the tests was them being
// only half-implemented: `zonePagePath` existed, its inverse did not, and every caller
// comparing `usePathname()` against a plain `/wallet` silently stopped matching when
// `basePath` was removed. Nothing failed loudly — the nav just never highlighted. So
// the round-trip is the property worth pinning down.
import assert from "node:assert/strict";
import test from "node:test";

import { LOCALES } from "@evinvest/i18n";

import {
  zonePagePath,
  isZonePage,
  isNonPagePath,
  localeRepairedPath,
  relocalise,
  walletActionPage,
  zonePathname,
} from "./base-path.ts";

test("every locale round-trips back to the path the app reasons in", () => {
  for (const locale of LOCALES) {
    for (const path of ["/", "/wallet", "/admin/revenue", "/invest/alpha-fund"] as const) {
      assert.equal(zonePathname(zonePagePath(locale, path)), path, `${locale} ${path}`);
    }
  }
});

test("the zone prefix comes off, locale and all", () => {
  assert.equal(zonePathname("/en/cabinet/admin/revenue"), "/admin/revenue");
  assert.equal(zonePathname("/ru/cabinet/wallet"), "/wallet");
  // The wallet host root is "/" — not "" — so `active: (p) => p === "/"` still matches.
  assert.equal(zonePathname("/de/cabinet"), "/");
});

test("a path without the locale prefix still loses the zone prefix", () => {
  // `splitLocalePath` reads a missing prefix as the default locale rather than throwing.
  assert.equal(zonePathname("/cabinet/wallet"), "/wallet");
  assert.equal(zonePathname("/cabinet"), "/");
});

test("a path outside the zone is returned unchanged", () => {
  // The conductor's own routes share the origin; mangling them would be worse than
  // leaving a non-match to the caller.
  assert.equal(zonePathname("/en/about"), "/about");
  assert.equal(zonePathname("/"), "/");
});

test("a segment that merely starts with the zone name is not a prefix", () => {
  // "/cabinets" is a different route; slicing by length alone would have eaten it.
  assert.equal(zonePathname("/en/cabinets/list"), "/cabinets/list");
});

// The v0.2.57 outage: dropping `basePath` moved every asset from `/_next/…` to
// `/cabinet/_next/…`, the proxy's anchored exclusions stopped matching, and the
// zone served its own stylesheets and scripts as the login page — unstyled,
// never hydrating. These pin the real production paths, not the stripped ones.
test("real production asset paths are not pages", () => {
  for (const path of [
    "/cabinet/_next/static/chunks/3nl-j504onixu.css",
    "/cabinet/_next/static/chunks/turbopack-32gvlhco9th6x.js",
    "/cabinet/_next/static/media/Inter_Italic-s.p.3ub0m60m9w90g.ttf",
    "/cabinet/_next/image",
    "/cabinet/api/session",
    "/cabinet/mfe/account-chip.js",
    "/cabinet/favicon.ico",
  ]) {
    assert.equal(isNonPagePath(path), true, path);
  }
});

test("the bare forms still hold, for a direct run of the zone", () => {
  for (const path of [
    "/_next/static/chunks/x.css",
    "/_next/image",
    "/api/session",
    "/mfe/account-chip.js",
    "/favicon.ico",
  ]) {
    assert.equal(isNonPagePath(path), true, path);
  }
});

test("pages are pages", () => {
  // Every one of these must keep its locale redirect and its session gate.
  for (const path of [
    "/",
    "/cabinet",
    "/cabinet/wallet",
    "/en/cabinet",
    "/ru/cabinet/wallet",
    "/en/cabinet/login",
    "/de/cabinet/admin/revenue",
  ]) {
    assert.equal(isNonPagePath(path), false, path);
  }
});

test("a page that merely mentions an excluded name is still a page", () => {
  // Guards the lookahead against being loosened into a substring test: these are
  // reader-facing routes and ungating them would expose the (app) shell.
  for (const path of [
    "/en/cabinet/api-keys",
    "/en/cabinet/mfeature",
    "/cabinets/_next/static/x.css",
    "/en/cabinet/_next/static/x.css",
  ]) {
    assert.equal(isNonPagePath(path), false, path);
  }
});

test("relocalising a page keeps its query and hash", () => {
  // Two callers relocalise the current page — the settings language switcher and
  // LocaleSync — and a version that drops the query string is a bug that only shows up
  // the first time a zone page takes a parameter. Pinned so they cannot diverge.
  const url = { pathname: "/ru/cabinet/wallet", search: "?filter=deposits", hash: "#top" };
  assert.equal(relocalise("de", url), "/de/cabinet/wallet?filter=deposits#top");
});

test("relocalising to English keeps the locale segment the wallet host actually serves", () => {
  // Unlike the public site, the zone has no unprefixed form: /cabinet/* is the entry
  // the proxy redirects AWAY from, so English pages really do live under /en.
  assert.equal(
    relocalise("en", { pathname: "/de/cabinet/settings", search: "", hash: "" }),
    "/en/cabinet/settings",
  );
});

test("relocalising the wallet host root does not grow a trailing slash", () => {
  assert.equal(relocalise("vi", { pathname: "/en/cabinet", search: "", hash: "" }), "/vi/cabinet");
});

test("a zone path with no locale is repaired into one", () => {
  // The conductor's unprefixed mount, and every link written before the locale
  // moved into the URL.
  assert.equal(localeRepairedPath("/cabinet", "ru"), "/ru/cabinet");
  assert.equal(localeRepairedPath("/cabinet/wallet", "ru"), "/ru/cabinet/wallet");
  assert.equal(localeRepairedPath("/cabinet/admin/revenue", "de"), "/de/cabinet/admin/revenue");
});

test("a first segment that is not a locale is replaced, not kept", () => {
  // The case that used to reach the root layout's `notFound()` — thrown by the ROOT
  // layout, so no `not-found.tsx` boundary exists above it and Next served its own
  // black 404 instead of ours. Prefixing rather than replacing would produce
  // /ru/zz/cabinet/wallet, which routes nowhere.
  assert.equal(localeRepairedPath("/zz/cabinet/wallet", "ru"), "/ru/cabinet/wallet");
  assert.equal(localeRepairedPath("/pt/cabinet", "en"), "/en/cabinet");
  assert.equal(localeRepairedPath("/zz/cabinet/", "vi"), "/vi/cabinet");
});

test("a path that is not a zone page is left for someone else", () => {
  // Null, not a guess: the conductor owns these routes on the same origin, and
  // redirecting them into the zone would break the site around it.
  assert.equal(localeRepairedPath("/zz/anything", "ru"), null);
  assert.equal(localeRepairedPath("/", "ru"), null);
  assert.equal(localeRepairedPath("/cabinets/list", "ru"), null);
  assert.equal(localeRepairedPath("/zz/cabinets/list", "ru"), null);
});

// The account chip mounts on both origins and links to a different place from each: into
// the wallet host home from the public site, to the profile from inside the wallet host. The
// answer comes from `document.location.pathname`, so it has to be right for the real
// shapes both hosts produce — locale-prefixed pages, the unprefixed mount, and the
// conductor's own routes that merely share the origin.
test("a zone page is a zone page, with or without its locale", () => {
  for (const path of ["/cabinet", "/cabinet/", "/cabinet/wallet", "/en/cabinet", "/ru/cabinet/", "/de/cabinet/admin/revenue"]) {
    assert.equal(isZonePage(path), true, path);
  }
  for (const locale of LOCALES) {
    for (const path of ["/", "/wallet", "/profile"] as const) {
      assert.equal(isZonePage(zonePagePath(locale, path)), true, `${locale} ${path}`);
    }
  }
});

test("the conductor's own routes are not zone pages", () => {
  for (const path of ["/", "/en", "/en/about", "/ru/invest", "/zz/anything"]) {
    assert.equal(isZonePage(path), false, path);
  }
});

test("a sibling that merely starts with the zone name is not a zone page", () => {
  for (const path of ["/cabinets", "/cabinets/list", "/en/cabinets/list", "/cabinetry"]) {
    assert.equal(isZonePage(path), false, path);
  }
});

test("assets and BFF calls under the prefix are not zone pages", () => {
  // The chip never renders inside one of these, but the function is public and the
  // proxy's distinction between pages and non-pages should hold here too.
  for (const path of ["/cabinet/_next/static/chunks/x.css", "/cabinet/api/session", "/cabinet/mfe/account-chip.js", "/cabinet/favicon.ico"]) {
    assert.equal(isZonePage(path), false, path);
  }
});

test("repairing agrees with zonePagePath for every locale", () => {
  // One rule, two directions — the same property the round-trip test above pins.
  for (const locale of LOCALES) {
    for (const path of ["/", "/wallet", "/admin/revenue"] as const) {
      const bogus = `/zz${zonePagePath("en", path).slice("/en".length)}`;
      assert.equal(localeRepairedPath(bogus, locale), zonePagePath(locale, path), `${locale} ${path}`);
    }
  }
});

test("the four wallet action addresses map onto their action pages", () => {
  assert.equal(walletActionPage("/wallet/pay"), "/wallet/action/pay");
  assert.equal(walletActionPage("/wallet/withdraw"), "/wallet/action/withdraw");
  assert.equal(walletActionPage("/wallet/move"), "/wallet/action/move");
  assert.equal(walletActionPage("/wallet/mandate"), "/wallet/action/mandate");
});

test("a wallet action address with a trailing slash is not an action", () => {
  assert.equal(walletActionPage("/wallet/withdraw/"), null);
});

test("a wallet payment address keeps its path under the zone", () => {
  const id = "4f0c2d1e-0000-4000-8000-0123456789ab";
  assert.equal(walletActionPage(`/wallet/payment/${id}`), `/wallet/payment/${id}`);
  assert.equal(walletActionPage(`/wallet/payment/${"a".repeat(128)}`), `/wallet/payment/${"a".repeat(128)}`);
});

test("a wallet payment address with an unsafe or missing id is not an action", () => {
  const id = "4f0c2d1e-0000-4000-8000-0123456789ab";
  for (const path of [
    "/wallet/payment/",
    "/wallet/payment/a/b",
    "/wallet/payment/a%2Fb",
    "/wallet/payment/a.b",
    `/wallet/payment/${id}/`,
    `/wallet/payment/${"a".repeat(129)}`,
    "/wallet/payments/a",
    "/wallet/action/payment/a",
  ]) {
    assert.equal(walletActionPage(path), null, path);
  }
});

test("the wallet element bundles are not pages", () => {
  assert.equal(isNonPagePath("/wallet/mfe/x.js"), true);
});
