// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { offerWalletAction, readScope, walletActionHref, WALLET_ACTION_EVENT } from "./wallet-action.ts";

test("a scope id is read as itself, trimmed", () => {
  assert.equal(readScope(" sa "), "sa");
});

test("main reads as no scope", () => {
  assert.equal(readScope("main"), null);
});

test("a malformed scope id reads as no scope", () => {
  assert.equal(readScope("s a"), null);
  assert.equal(readScope("../sa"), null);
  assert.equal(readScope(""), null);
  assert.equal(readScope(null), null);
});

test("the action href carries the path plus scope and invoice only", () => {
  assert.equal(
    walletActionHref("https://evinvest.ltd/en/cabinet?leak=1#x", "pay", { scope: "sa", invoice: "inv_1" }),
    "https://evinvest.ltd/wallet/pay?invoice=inv_1&scope=sa",
  );
});

test("malformed ids are left out of the action href", () => {
  assert.equal(walletActionHref("https://evinvest.ltd", "withdraw", { scope: "s a", invoice: "x&amount=9" }), "https://evinvest.ltd/wallet/withdraw");
});

test("offering an action reports true when the host prevents the default", () => {
  const target = new EventTarget();
  let href: string | undefined;
  target.addEventListener(WALLET_ACTION_EVENT, (event) => {
    assert.ok(event instanceof CustomEvent);
    href = event.detail.href;
    event.preventDefault();
  });
  assert.equal(offerWalletAction(target, "https://evinvest.ltd/wallet/pay"), true);
  assert.equal(href, "https://evinvest.ltd/wallet/pay");
});

test("offering an action reports false when nobody takes it", () => {
  assert.equal(offerWalletAction(new EventTarget(), "https://evinvest.ltd/wallet/pay"), false);
});
