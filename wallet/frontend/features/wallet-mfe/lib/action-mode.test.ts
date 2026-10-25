// Run with `npm run test`.
import assert from "node:assert/strict";
import test from "node:test";

import { actionMode, inlineOptIn, inPageMode } from "./action-mode.ts";

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

test("only the exact value inline opts the host in", () => {
  assert.equal(inlineOptIn("inline"), true);
  assert.equal(inlineOptIn(null), false);
  assert.equal(inlineOptIn(""), false);
  assert.equal(inlineOptIn("Inline"), false);
  assert.equal(inlineOptIn(" inline"), false);
  assert.equal(inlineOptIn("popup"), false);
});

test("without the opt-in a listed kind stays an offer", () => {
  assert.equal(actionMode(null, ["move", "pay", "withdraw", "mandate"], "move", "main"), "offer");
  assert.equal(actionMode("popup", ["withdraw"], "withdraw", null), "offer");
});

test("with the opt-in a listed kind runs in the page", () => {
  assert.equal(actionMode("inline", ["withdraw"], "withdraw", null), "in_page");
  assert.equal(actionMode("inline", ["mandate"], "mandate", null), "in_page");
  assert.equal(actionMode("inline", ["pay"], "pay", "sa"), "in_page");
});

test("with the opt-in a kind the BFF did not list stays an offer", () => {
  assert.equal(actionMode("inline", ["withdraw"], "mandate", null), "offer");
  assert.equal(actionMode("inline", null, "withdraw", null), "offer");
});

test("with the opt-in a listed move without a scope stays an offer", () => {
  assert.equal(actionMode("inline", ["move"], "move", null), "offer");
});
