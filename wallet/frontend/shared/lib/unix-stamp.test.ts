// The regression these cover shipped to production: the owners' room rendered "Owner
// since —" for every seat, and the same dash stood in for every deadline and decision
// time in the consilium, both approval pages and the removal/admission cards.
//
// The first assertion is the one that matters. It fails against the old RFC 3339 parser
// (`new Date("1757000000")` is an Invalid Date) and passes against this one, which is the
// only reason to have written it. Asserting merely that the result is a string would have
// passed on the bug, since the bug's output — "—" — is a perfectly good string.

import assert from "node:assert/strict";
import test from "node:test";

import { hasUnixStamp, unixStampToDate } from "./unix-stamp.ts";

test("a wire stamp is unix seconds, not RFC 3339", () => {
  // 2025-09-04T18:13:20Z. The old parser returned null here and the UI showed a dash.
  const at = unixStampToDate("1757009600");
  assert.notEqual(at, null, "a decimal seconds string must parse");
  assert.equal(at?.toISOString(), "2025-09-04T18:13:20.000Z", "seconds, so scaled by 1000");
});

test('"0" is absence, never 1 Jan 1970', () => {
  // The contract writes 0 for "not decided yet" (banking/v1/consilium.proto). Rendering
  // that as a 1970 date would put a confident wrong answer on a card about money.
  assert.equal(unixStampToDate("0"), null);
  assert.equal(hasUnixStamp("0"), false);
});

test("absence in every other form it arrives in", () => {
  assert.equal(unixStampToDate(undefined), null);
  assert.equal(unixStampToDate(""), null);
  // Not a number at all: the RFC 3339 shape this module used to expect must not sneak
  // back in as a half-working input.
  assert.equal(unixStampToDate("2026-03-12T14:03:00Z"), null);
  assert.equal(unixStampToDate("nonsense"), null);
});

test("a real stamp is present", () => {
  assert.equal(hasUnixStamp("1757009600"), true);
});

test("negative and non-finite stamps are absent, not pre-epoch dates", () => {
  assert.equal(unixStampToDate("-1"), null);
  assert.equal(unixStampToDate("Infinity"), null);
  assert.equal(unixStampToDate("NaN"), null);
});
