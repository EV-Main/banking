// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { requiredPermission } from "./route-permissions.ts";

test("every section asks for a permission, and only whole segments match", () => {
  assert.equal(requiredPermission("/"), "bank:self:wallet");
  assert.equal(requiredPermission("/wallet/withdraw"), "bank:self:wallet");
  assert.equal(requiredPermission("/invest/trader_alpha/trade"), "bank:self:invest");
  assert.equal(requiredPermission("/admin/users"), "concierge:user:read");
  assert.equal(requiredPermission("/consilium"), "concierge:user:read");
  assert.equal(requiredPermission("/settings"), "concierge:self:sessions");
  assert.equal(requiredPermission("/walletfoo"), null, "a sibling path is not the section");
});
