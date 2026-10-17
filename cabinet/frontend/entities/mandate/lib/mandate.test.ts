// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { isLive, isStepUpRequired, visibleMandates } from "./mandate.ts";

const base = { scope: "invest", revoked_at: "0", replaced_at: "0", expires_at: "0" };

test("a revoked, replaced or expired mandate is not live", () => {
  assert.equal(isLive(base, 100), true);
  assert.equal(isLive({ ...base, revoked_at: "5" }, 100), false);
  assert.equal(isLive({ ...base, replaced_at: "5" }, 100), false);
  assert.equal(isLive({ ...base, expires_at: "100" }, 100), false);
  assert.equal(isLive({ ...base, expires_at: "101" }, 100), true);
});

test("the URL scope narrows the server's list and never adds to it", () => {
  const list = [base, { ...base, scope: "sa" }];
  assert.equal(visibleMandates(list, "sa", 1).length, 1);
  assert.equal(visibleMandates(list, "unknown", 1).length, 2);
  assert.equal(visibleMandates(list, null, 1).length, 2);
});

test("only a 401 step_up_required reads as a step-up demand", () => {
  assert.equal(isStepUpRequired({ status: 401, body: { error: "step_up_required" } }), true);
  assert.equal(isStepUpRequired({ status: 403, body: { error: "step_up_required" } }), false);
  assert.equal(isStepUpRequired({ status: 401, body: { error: "unauthenticated" } }), false);
  assert.equal(isStepUpRequired(null), false);
});
