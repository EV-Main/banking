// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The approval page is `/owner-removal/<token>` — a different path for every visitor — so it
// is matched by prefix, and a prefix rule is worth pinning from both sides: the token route
// is caught, and nothing that merely starts with its name is.
import assert from "node:assert/strict";
import test from "node:test";

import { LOCALES } from "@evinvest/i18n";

import { isTokenApprovalPath, mayObserve, zoneGatePath } from "./public-routes.ts";

test("the locale and zone prefix come off before the gate compares anything", () => {
  assert.equal(zoneGatePath("/en/cabinet/owner-removal/abc"), "/owner-removal/abc");
  assert.equal(zoneGatePath("/ru/cabinet/login"), "/login");
  assert.equal(zoneGatePath("/de/cabinet"), "/");
});

test("a path outside the zone is left alone rather than mangled", () => {
  // The conductor owns routes on this origin; half-eating one would make the gate answer
  // a question about a page that is not ours.
  assert.equal(zoneGatePath("/en/about"), "/en/about");
  assert.equal(zoneGatePath("/"), "/");
});

test("an approval link is a token page in every locale", () => {
  for (const locale of LOCALES) {
    assert.equal(isTokenApprovalPath(`/${locale}/cabinet/owner-removal/01J8XYZTOKEN`), true, locale);
  }
});

test("a page that merely starts with a token page's name is not one", () => {
  for (const path of ["/en/cabinet/owner-removals", "/en/cabinet/owner-removal-all"]) {
    assert.equal(isTokenApprovalPath(path), false, path);
  }
});

test("only the token pages ask for no-referrer", () => {
  // The URL is the credential on this page and nowhere else, so the stricter header is
  // scoped to it rather than weakening the referrer policy of the whole wallet host.
  assert.equal(isTokenApprovalPath("/fr/cabinet/owner-removal/tok"), true);
  assert.equal(isTokenApprovalPath("/en/cabinet/login"), false);
  assert.equal(isTokenApprovalPath("/en/cabinet/wallet"), false);
});

test("the approval pages are never observed by analytics or error monitoring", () => {
  // The URL is the credential here. PostHog puts `$current_url` on every event and
  // Sentry puts the same URL on `request.url` and on each fetch breadcrumb, so mounting
  // either provider here ships a live single-use token off-origin to a third party before
  // the reader has done anything. `Referrer-Policy: no-referrer` does not cover this — it
  // governs the `Referer` header, not a URL an SDK reads out of `location`.
  for (const locale of LOCALES) {
    assert.equal(mayObserve(`/${locale}/cabinet/owner-removal/01J8XYZTOKEN`), false, locale);
  }
});

test("every other page is still observed", () => {
  // The opt-out is scoped to the pages that hold a secret in their URL; turning telemetry
  // off more widely than that would be a silent regression in the wallet host's monitoring.
  for (const path of [
    "/en/cabinet",
    "/en/cabinet/login",
    "/ru/cabinet/wallet/withdraw",
    "/de/cabinet/admin/revenue",
    "/en/cabinet/owner-removals",
  ]) {
    assert.equal(mayObserve(path), true, path);
  }
});
