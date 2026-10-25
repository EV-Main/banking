// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { closedAction, dispatchWalletResult, walletResultDetail, WALLET_RESULT_EVENT } from "./wallet-result.ts";

test("a form closed after the confirm went through is done with the hub reference", () => {
  assert.deepEqual(closedAction({ phase: "done", confirmed: { reference: "mv_1" } }), { outcome: "done", reference: "mv_1" });
});

test("a form closed after a confirm without a reference is done with none", () => {
  assert.deepEqual(closedAction({ phase: "done", confirmed: { reference: null } }), { outcome: "done", reference: null });
});

test("a clean form the person leaves is cancelled", () => {
  assert.deepEqual(closedAction({ phase: "editing", error: null }), { outcome: "cancelled", reference: null });
  assert.deepEqual(closedAction({ phase: "creating" }), { outcome: "cancelled", reference: null });
});

test("a form the person leaves on an error is failed", () => {
  assert.deepEqual(closedAction({ phase: "editing", error: { code: "below_minimum" } }), { outcome: "failed", reference: null });
  assert.deepEqual(closedAction({ phase: "review", error: { code: "limit_reached" } }), { outcome: "failed", reference: null });
});

test("a form closed on expired terms or an unclear confirm is failed", () => {
  assert.deepEqual(closedAction({ phase: "expired" }), { outcome: "failed", reference: null });
  assert.deepEqual(closedAction({ phase: "unclear" }), { outcome: "failed", reference: null });
});

test("the detail carries the kind, the outcome and the reference", () => {
  assert.deepEqual(walletResultDetail("pay", { outcome: "done", reference: "inv_1" }), { kind: "pay", outcome: "done", reference: "inv_1" });
});

test("the detail has no reference key when there is none", () => {
  const detail = walletResultDetail("mandate", { outcome: "cancelled", reference: null });
  assert.deepEqual(detail, { kind: "mandate", outcome: "cancelled" });
  assert.equal("reference" in detail, false);
});

test("an empty reference is left out", () => {
  assert.deepEqual(walletResultDetail("move", { outcome: "done", reference: "" }), { kind: "move", outcome: "done" });
});

test("a reference of 128 characters is kept whole", () => {
  const reference = "r".repeat(128);
  assert.deepEqual(walletResultDetail("withdraw", { outcome: "done", reference }), { kind: "withdraw", outcome: "done", reference });
});

test("a reference over 128 characters is left out, not cut", () => {
  assert.deepEqual(walletResultDetail("withdraw", { outcome: "done", reference: "r".repeat(129) }), { kind: "withdraw", outcome: "done" });
});

test("the result event bubbles, is composed, is not cancelable, and carries the detail", () => {
  const target = new EventTarget();
  let heard: Event | undefined;
  target.addEventListener(WALLET_RESULT_EVENT, (event) => {
    heard = event;
  });

  dispatchWalletResult(target, { kind: "pay", outcome: "done", reference: "inv_1" });

  assert.ok(heard instanceof CustomEvent);
  assert.equal(heard.type, "ev:wallet-result");
  assert.equal(heard.bubbles, true);
  assert.equal(heard.composed, true);
  assert.equal(heard.cancelable, false);
  assert.deepEqual(heard.detail, { kind: "pay", outcome: "done", reference: "inv_1" });
});
