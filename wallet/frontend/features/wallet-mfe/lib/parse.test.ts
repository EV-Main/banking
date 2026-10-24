// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The element reads bodies from a BFF that ships on another release train, so these pin
// what a malformed field turns into rather than the happy path alone.
import assert from "node:assert/strict";
import test from "node:test";

import { parseWalletBalance, parseWalletHistory, parseWithdrawNetworks } from "./parse.ts";

test("balance fields that are not wire decimals read as zero", () => {
  const balance = parseWalletBalance({
    balance: { available: "12.50", in_orders: "1e3", invested: "abc", pending_withdrawal: 7, in_compartments: "1.", total: "-3.25" },
  });
  assert.deepEqual(balance, {
    available: "12.50",
    inOrders: "0",
    invested: "0",
    pendingWithdrawal: "0",
    inCompartments: "0",
    total: "-3.25",
  });
});

test("a body without a balance object reads as no balance", () => {
  assert.equal(parseWalletBalance({ balance: "100" }), null);
  assert.equal(parseWalletBalance(null), null);
});

test("an operation amount that is not a wire decimal reads as absent", () => {
  const { operations } = parseWalletHistory({
    operations: [
      { id: "a", amount: "abc" },
      { id: "b", amount: 5 },
      { id: "c", amount: "1,5" },
      { id: "d", amount: "2.5" },
    ],
  });
  assert.deepEqual(
    operations.map((operation) => operation.amount),
    ["", "", "", "2.5"],
  );
});

test("rows without a usable id are dropped", () => {
  const { operations } = parseWalletHistory({
    operations: [{ amount: "1" }, { id: "" }, { id: 42 }, { id: "x".repeat(257) }, null, { id: "kept" }],
  });
  assert.deepEqual(
    operations.map((operation) => operation.id),
    ["kept"],
  );
});

test("created_at is read from the BFF's string seconds and from a plain number", () => {
  const { operations } = parseWalletHistory({
    operations: [
      { id: "s", created_at: "1700000000" },
      { id: "n", created_at: 1700000000.9 },
      { id: "bad", created_at: "yesterday" },
      { id: "neg", created_at: -5 },
    ],
  });
  assert.deepEqual(
    operations.map((operation) => operation.createdAt),
    [1700000000, 1700000000, 0, 0],
  );
});

test("truncated is true only for a literal true", () => {
  assert.equal(parseWalletHistory({ operations: [], truncated: true }).truncated, true);
  assert.equal(parseWalletHistory({ operations: [], truncated: "true" }).truncated, false);
  assert.equal(parseWalletHistory({}).truncated, false);
});

test("withdraw networks are deduplicated and bad ids dropped, in the hub's order", () => {
  const networks = parseWithdrawNetworks({
    withdrawable: [{ network: "tron" }, { network: "eth" }, { network: "tron" }, { network: "Bad Id" }, { network: 7 }, {}, null, { network: "" }],
  });
  assert.deepEqual(networks, ["tron", "eth"]);
});

test("a body without a withdrawable list has no withdraw networks", () => {
  assert.deepEqual(parseWithdrawNetworks({ withdrawable: "tron" }), []);
  assert.deepEqual(parseWithdrawNetworks(null), []);
});
