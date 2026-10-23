// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { completesVerification } from "./kyc-completion.ts";

test("a tier watched rising from 0 is a completion", () => {
  assert.equal(completesVerification(0, 1, false), true);
});

test("coming back from the vendor with the pending mark is a completion", () => {
  assert.equal(completesVerification(null, 1, true), true);
});

test("a tab that first opens on a verified account watched nothing", () => {
  assert.equal(completesVerification(null, 1, false), false);
  assert.equal(completesVerification(1, 1, false), false);
});

test("still on tier 0 is not a completion, pending or not", () => {
  assert.equal(completesVerification(0, 0, true), false);
  assert.equal(completesVerification(null, 0, true), false);
});
