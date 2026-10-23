// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { amountProblem, moveEnds, movableScopes, preselectedScope } from "./move.ts";

const list = [{ scope: "main", available: "10" }, { scope: "invest", available: "5" }, { scope: "sa", available: "0" }];

test("main is not a scope a move can name", () => {
  assert.deepEqual(movableScopes(list).map((c) => c.scope), ["invest", "sa"]);
});

test("a scope from the URL is preselected only when the server offers it", () => {
  const scopes = movableScopes(list);
  assert.equal(preselectedScope("invest", scopes), "invest");
  assert.equal(preselectedScope("evil", scopes), null);
  assert.equal(preselectedScope("main", scopes), null);
  assert.equal(preselectedScope(null, scopes), null);
});

test("moving in goes main to the scope, moving out goes back", () => {
  assert.deepEqual(moveEnds("invest", "in"), { from_scope: "main", to_scope: "invest" });
  assert.deepEqual(moveEnds("invest", "out"), { from_scope: "invest", to_scope: "main" });
});

test("an amount must be a positive decimal within what the source holds", () => {
  assert.equal(amountProblem("", "10"), "empty");
  assert.equal(amountProblem("0", "10"), "invalid");
  assert.equal(amountProblem("-1", "10"), "invalid");
  assert.equal(amountProblem("1e3", "10"), "invalid");
  assert.equal(amountProblem("10.000001", "10"), "exceeds");
  assert.equal(amountProblem("10", "10"), null);
  assert.equal(amountProblem("0.5", "10"), null);
});
