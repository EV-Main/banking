// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The wrong-code sentence is copied from the money plane verbatim
// (`piggybank/core/src/infrastructure/consilium.rs`); the prefixes are `DomainError`'s
// Display forms (`domain/src/error.rs`). A backend rewording that breaks one of these is
// the test doing its job: the screen falls back to the raw prose, safe but untranslated.
import assert from "node:assert/strict";
import test from "node:test";

import { stripTransportPrefix, wrongCodeAttempts } from "./hub-refusal.ts";

test("the variant name the hub prefixes its sentence with is dropped, once, and only in front", () => {
  assert.equal(stripTransportPrefix("validation failed: incorrect code — 4 attempts remaining"), "incorrect code — 4 attempts remaining");
  assert.equal(stripTransportPrefix("conflict: the change is active"), "the change is active");
  assert.equal(stripTransportPrefix("precondition failed: outflows are paused"), "outflows are paused");
  assert.equal(stripTransportPrefix("forbidden: not yours to cancel"), "not yours to cancel");
  assert.equal(stripTransportPrefix("Validation failed: twice: validation failed: x"), "twice: validation failed: x");
  assert.equal(stripTransportPrefix("the conflict: is mid-sentence"), "the conflict: is mid-sentence");
  assert.equal(stripTransportPrefix(""), "");
});

test("a refused code is recognised with the server's own count of attempts left", () => {
  assert.equal(wrongCodeAttempts(new Error("validation failed: incorrect code — 4 attempts remaining")), 4);
  assert.equal(wrongCodeAttempts("incorrect code — 1 attempts remaining"), 1);
  assert.equal(wrongCodeAttempts("Incorrect code — 1 attempt remaining"), 1);
  assert.equal(wrongCodeAttempts("incorrect code — 0 attempts remaining"), 0);
});

test("a wrong-code sentence without a readable count is not guessed at", () => {
  assert.equal(wrongCodeAttempts("validation failed: incorrect code"), null);
});

test("anything else is left to the caller's existing error handling", () => {
  for (const other of [new Error("Can't reach the server. Check your connection and try again."), "insufficient revenue", "", null, undefined, 42]) {
    assert.equal(wrongCodeAttempts(other), null, String(other));
  }
});
