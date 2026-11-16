// Run with `npm run test`.
//
// build.mjs writes this text to public/mfe/wallet/registry.json (and the alias's copy), which `@evinvest/wallet`
// reads to find the elements. A host that asks for a name missing here gets no element.
import assert from "node:assert/strict";
import test from "node:test";

import { WALLET_ALIAS_DIR, WALLET_DIR, walletRegistryJson } from "./wallet-registry.mjs";

test("the registry lists the three wallet elements, all from the one wallet bundle in its directory", () => {
  for (const dir of [WALLET_DIR, WALLET_ALIAS_DIR]) {
    assert.deepEqual(JSON.parse(walletRegistryJson(dir)), [
      { name: "banking.wallet", tag: "mfe-banking-wallet", kind: "component", scriptUrl: `${dir}wallet.js` },
      { name: "banking.pay-invoice", tag: "mfe-banking-pay-invoice", kind: "component", scriptUrl: `${dir}wallet.js` },
      { name: "banking.mandate", tag: "mfe-banking-mandate", kind: "component", scriptUrl: `${dir}wallet.js` },
    ], dir);
  }
});

test("the bundles' directory is the one @evinvest/wallet holds them to", () => {
  assert.equal(WALLET_DIR, "/api/wallet/mfe/");
});

test("the registry file is two-space JSON ending in one newline", () => {
  const text = walletRegistryJson(WALLET_DIR);

  assert.ok(text.startsWith('[\n  {\n    "name": "banking.wallet",\n'));
  assert.ok(text.endsWith("\n]\n"));
});
