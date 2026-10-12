// Run with `npm run test`. The proxy cannot be unit-tested, so the rule it applies to
// decide whether an unprefixed entry is a guess lives here.
import assert from "node:assert/strict";
import test from "node:test";

import { localeIsGuessed } from "./locale-guess.ts";

test("no cookie at all is a guess", () => {
  assert.equal(localeIsGuessed(undefined, undefined), true);
});

test("a cookie seeded from a URL, never confirmed in Settings, is a guess (#367)", () => {
  assert.equal(localeIsGuessed("de", undefined), true);
});

test("a cookie that matches the Settings confirmation is a choice", () => {
  assert.equal(localeIsGuessed("vi", "vi"), false);
});

test("a cookie rewritten after the confirmation is a guess again", () => {
  assert.equal(localeIsGuessed("de", "vi"), true);
});

test("a junk cookie is a guess even when the confirmation echoes it", () => {
  assert.equal(localeIsGuessed("zz", "zz"), true);
});
