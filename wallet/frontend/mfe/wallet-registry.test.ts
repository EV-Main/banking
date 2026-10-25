// Run with `npm run test`.
//
// build.mjs writes this text to public/mfe/wallet/registry.json, which `@evinvest/wallet`
// reads to find the elements. A host that asks for a name missing here gets no element.
import assert from "node:assert/strict";
import test from "node:test";

import { walletRegistryJson } from "./wallet-registry.mjs";

test("the registry lists the three wallet elements, all from the one wallet bundle", () => {
  assert.deepEqual(JSON.parse(walletRegistryJson()), [
    { name: "banking.wallet", tag: "mfe-banking-wallet", kind: "component", scriptUrl: "/wallet/mfe/wallet.js" },
    { name: "banking.pay-invoice", tag: "mfe-banking-pay-invoice", kind: "component", scriptUrl: "/wallet/mfe/wallet.js" },
    { name: "banking.mandate", tag: "mfe-banking-mandate", kind: "component", scriptUrl: "/wallet/mfe/wallet.js" },
  ]);
});

test("the registry file is two-space JSON ending in one newline", () => {
  const text = walletRegistryJson();

  assert.ok(text.startsWith('[\n  {\n    "name": "banking.wallet",\n'));
  assert.ok(text.endsWith("\n]\n"));
});
