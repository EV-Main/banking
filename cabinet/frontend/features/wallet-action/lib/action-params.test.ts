// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// Every field here gates where the outcome of a money action is posted or where the reader
// is sent back, so each rejection is pinned on its own: a check that quietly widened would
// hand the result to whoever built the link.
import assert from "node:assert/strict";
import test from "node:test";

import { readActionParams, resultMessage, type ScopeHost } from "./action-params.ts";

const PAGE = "https://evinvest.ltd";
const HOSTS: Record<string, ScopeHost> = {
  sa: { origins: ["https://sa.evinvest.ltd"], returnPaths: ["/billing"] },
};
const STATE = "0123456789abcdef0123456789abcdef";

const read = (query: Record<string, string>) => readActionParams(new URLSearchParams(query).toString(), PAGE, HOSTS);

test("a bare registered origin is accepted as the opener", () => {
  assert.equal(read({ scope: "sa", opener: "https://sa.evinvest.ltd" }).opener, "https://sa.evinvest.ltd");
});

test("an opener with a path is rejected", () => {
  assert.equal(read({ scope: "sa", opener: "https://sa.evinvest.ltd/billing" }).opener, null);
});

test("an opener with a query is rejected", () => {
  assert.equal(read({ scope: "sa", opener: "https://sa.evinvest.ltd/?x=1" }).opener, null);
});

test("an opener from an origin the scope did not register is rejected", () => {
  assert.equal(read({ scope: "sa", opener: "https://evil.example" }).opener, null);
});

test("an opener is rejected when no scope names who may answer", () => {
  assert.equal(read({ opener: "https://sa.evinvest.ltd" }).opener, null);
});

test("a state of 32 lowercase hex digits is kept", () => {
  assert.equal(read({ state: STATE }).state, STATE);
});

test("a state that is not 32 lowercase hex digits is dropped", () => {
  assert.equal(read({ state: STATE.slice(1) }).state, null);
  assert.equal(read({ state: STATE.toUpperCase() }).state, null);
  assert.equal(read({ state: `${STATE}0` }).state, null);
});

test("a return address on the page's own origin comes back as origin plus path", () => {
  assert.equal(read({ return_to: "https://evinvest.ltd/en/cabinet/wallet" }).returnTo, "https://evinvest.ltd/en/cabinet/wallet");
});

test("a return address with a query is rejected even on the page's own origin", () => {
  assert.equal(read({ return_to: "https://evinvest.ltd/en/cabinet/wallet?token=1" }).returnTo, null);
});

test("a return address on a scope origin needs one of its registered paths", () => {
  assert.equal(read({ scope: "sa", return_to: "https://sa.evinvest.ltd/billing" }).returnTo, "https://sa.evinvest.ltd/billing");
  assert.equal(read({ scope: "sa", return_to: "https://sa.evinvest.ltd/other" }).returnTo, null);
});

test("no result message is built without an opener", () => {
  const params = read({ scope: "sa", state: STATE });
  assert.equal(resultMessage(params, "done", "ref-1"), null);
});

test("the result message echoes the state and carries a reference of up to 128 characters", () => {
  const params = read({ scope: "sa", opener: "https://sa.evinvest.ltd", state: STATE });
  assert.deepEqual(resultMessage(params, "done", "r".repeat(128)), {
    type: "ev:wallet-result",
    state: STATE,
    outcome: "done",
    reference: "r".repeat(128),
  });
});

test("a reference longer than 128 characters is left out of the result message", () => {
  const params = read({ scope: "sa", opener: "https://sa.evinvest.ltd", state: STATE });
  assert.deepEqual(resultMessage(params, "done", "r".repeat(129)), {
    type: "ev:wallet-result",
    state: STATE,
    outcome: "done",
  });
});
