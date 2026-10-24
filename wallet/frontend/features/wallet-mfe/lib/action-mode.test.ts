// Run with `npm run test`.
import assert from "node:assert/strict";
import test from "node:test";

import { actionMode } from "./action-mode.ts";

test("with the flag off the action stays an offer, scope or not", () => {
  assert.equal(actionMode(false, "main"), "offer");
  assert.equal(actionMode(false, null), "offer");
});

test("with the flag on and no scope the action stays an offer", () => {
  assert.equal(actionMode(true, null), "offer");
});

test("with the flag on and a scope the action runs in the page", () => {
  assert.equal(actionMode(true, "main"), "in_page");
});
