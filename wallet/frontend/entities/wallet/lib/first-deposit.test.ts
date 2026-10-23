// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { soleDeposit } from "./first-deposit.ts";

const credit = (tx_ref: string) => ({ tx_ref, network: "trc20", amount: "100", created_at: 1 });

test("a list of exactly one credit names the first deposit", () => {
  assert.deepEqual(soleDeposit({ deposits: [credit("tx-1")] }), credit("tx-1"));
});

test("an unread or empty list names nothing", () => {
  assert.equal(soleDeposit(undefined), null);
  assert.equal(soleDeposit({}), null);
  assert.equal(soleDeposit({ deposits: [] }), null);
});

test("a longer history is not this tab's first deposit to announce", () => {
  assert.equal(soleDeposit({ deposits: [credit("tx-2"), credit("tx-1")] }), null);
});

test("a credit without a reference cannot be marked, so it is not announced", () => {
  assert.equal(soleDeposit({ deposits: [{ network: "ton", amount: "5" }] }), null);
});
