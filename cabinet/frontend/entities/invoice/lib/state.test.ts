// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { invoicePhase, POLL_GIVE_UP_MS, shouldPoll } from "./state.ts";

test("processing is its own phase, never paid", () => {
  assert.equal(invoicePhase("processing"), "processing");
  assert.equal(invoicePhase("paid"), "paid");
  assert.equal(invoicePhase("open"), "payable");
});

test("an unknown or missing state is closed, so nothing is offered", () => {
  assert.equal(invoicePhase("refunded"), "closed");
  assert.equal(invoicePhase(undefined), "closed");
  assert.equal(invoicePhase("expired"), "closed");
  assert.equal(invoicePhase("cancelled"), "closed");
});

test("only a processing invoice inside the window is polled", () => {
  assert.equal(shouldPoll("processing", 0), true);
  assert.equal(shouldPoll("processing", POLL_GIVE_UP_MS), false);
  assert.equal(shouldPoll("payable", 0), false);
  assert.equal(shouldPoll("paid", 0), false);
});
