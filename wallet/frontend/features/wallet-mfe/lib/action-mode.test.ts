// Run with `npm run test`.
import assert from "node:assert/strict";
import test from "node:test";

import { inPageMode } from "./action-mode.ts";

test("an unknown kind list keeps every action an offer", () => {
  assert.equal(inPageMode(null, "withdraw", null), "offer");
  assert.equal(inPageMode(null, "move", "main"), "offer");
});

test("an empty kind list keeps every action an offer", () => {
  assert.equal(inPageMode([], "withdraw", "main"), "offer");
});

test("a kind missing from the list stays an offer while others are listed", () => {
  assert.equal(inPageMode(["withdraw"], "mandate", null), "offer");
  assert.equal(inPageMode(["withdraw"], "move", "main"), "offer");
});

test("a listed move or pay without a scope stays an offer", () => {
  assert.equal(inPageMode(["move", "pay"], "move", null), "offer");
  assert.equal(inPageMode(["move", "pay"], "pay", null), "offer");
});

test("a listed withdraw or mandate runs in the page without a scope", () => {
  assert.equal(inPageMode(["withdraw", "mandate"], "withdraw", null), "in_page");
  assert.equal(inPageMode(["withdraw", "mandate"], "mandate", null), "in_page");
});

test("a listed kind with a scope runs in the page", () => {
  assert.equal(inPageMode(["move", "pay", "withdraw", "mandate"], "move", "main"), "in_page");
  assert.equal(inPageMode(["pay"], "pay", "main"), "in_page");
});
