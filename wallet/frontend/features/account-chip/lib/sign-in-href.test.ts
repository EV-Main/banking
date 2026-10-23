// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The chip is the public site's entry into the cabinet, so the host may tell it where the
// visitor was going and why (#392). The properties pinned here: nothing from the host
// changes the destination page, only its query; a missing attribute is today's plain
// login link; and a `returnTo` that could leave the origin is neutralised by the same
// rule the login page applies, so the two never disagree.
import assert from "node:assert/strict";
import test from "node:test";

import { LOCALES } from "@evinvest/i18n";

import { cabinetPath } from "../../../shared/config/base-path.ts";
import { signInHref, signInIntent } from "./sign-in-href.ts";

test("without host attributes the link is the plain login page", () => {
  for (const locale of LOCALES) {
    assert.equal(signInHref(locale), cabinetPath(locale, "/login"), locale);
    assert.equal(signInHref(locale, { intent: null, returnTo: null }), cabinetPath(locale, "/login"), locale);
    assert.equal(signInHref(locale, { intent: "", returnTo: "" }), cabinetPath(locale, "/login"), locale);
  }
});

test("intent and a zone-relative returnTo are forwarded as the login page's query", () => {
  assert.equal(signInHref("en", { intent: "signup", returnTo: "/invest" }), "/en/cabinet/login?intent=signup&returnTo=%2Finvest");
  assert.equal(signInHref("ru", { intent: "login" }), "/ru/cabinet/login?intent=login");
  assert.equal(signInHref("de", { returnTo: "/wallet?tab=deposit" }), "/de/cabinet/login?returnTo=%2Fwallet%3Ftab%3Ddeposit");
});

test("an intent the cabinet does not know is dropped, not forwarded", () => {
  assert.equal(signInIntent("signup"), "signup");
  assert.equal(signInIntent("login"), "login");
  assert.equal(signInIntent("upgrade"), null);
  assert.equal(signInIntent(" signup"), null);
  assert.equal(signInIntent(null), null);
  assert.equal(signInHref("fr", { intent: "upgrade", returnTo: "/invest" }), "/fr/cabinet/login?returnTo=%2Finvest");
});

test("a returnTo that could escape the origin is neutralised before it is forwarded", () => {
  for (const hostile of ["//evil.example", "/\\evil.example", "https://evil.example/x", "wallet", "/a\\b"]) {
    assert.equal(signInHref("en", { returnTo: hostile }), "/en/cabinet/login?returnTo=%2F", hostile);
  }
});

test("a full cabinet path is passed through untouched; the login page reduces it", () => {
  // The login page's `loginReturnTo` accepts both shapes and prefixes exactly once (#390),
  // so the chip does not need a second copy of that rule.
  assert.equal(signInHref("en", { returnTo: "/en/cabinet/invest" }), "/en/cabinet/login?returnTo=%2Fen%2Fcabinet%2Finvest");
});
