// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// These pin the bug that made the whole feature unusable: the wire never sends null for an
// unanswered seat. It sends the string "pending" (the plane's own enum member) or an empty
// string, both truthy — so `decision ?? null` read every fresh invitation as already
// settled and hid the answer form.
import assert from "node:assert/strict";
import test from "node:test";

import { settledConsent, settledRemoval } from "./decision.ts";

test("an unanswered seat is not a settled answer, however the wire spells it", () => {
  for (const raw of ["pending", "PENDING", "", "   ", null, undefined, "unknown_future_state"]) {
    assert.equal(settledRemoval(raw), null, `removal: ${String(raw)}`);
    assert.equal(settledConsent(raw), null, `consent: ${String(raw)}`);
  }
});

test("a payment answer is read in either tense", () => {
  assert.equal(settledConsent("approve"), "approve");
  assert.equal(settledConsent("APPROVED"), "approve");
  assert.equal(settledConsent("reject"), "reject");
  assert.equal(settledConsent(" rejected "), "reject");
});

test("a removal answer is read in both vocabularies", () => {
  assert.equal(settledRemoval("remove"), "remove");
  assert.equal(settledRemoval("approve"), "remove");
  assert.equal(settledRemoval("removed"), "remove");
  assert.equal(settledRemoval("keep"), "keep");
  assert.equal(settledRemoval("reject"), "keep");
  assert.equal(settledRemoval("kept"), "keep");
});

test("a non-string is never mistaken for an answer", () => {
  for (const raw of [0, 1, {}, [], true]) {
    assert.equal(settledRemoval(raw), null, String(raw));
    assert.equal(settledConsent(raw), null, String(raw));
  }
});
