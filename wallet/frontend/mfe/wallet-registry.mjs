// The registry `@evinvest/wallet` reads (`WALLET_REGISTRY_URL`), written by build.mjs to
// public/mfe/wallet/registry.json. Every element lives in one bundle; the host loads it once
// and each tag is defined. Names and tags are the SDK's. Kept apart from the build so a test
// reads it without running esbuild.

const WALLET_SCRIPT = "/wallet/mfe/wallet.js";

export const WALLET_REGISTRY = [
  { name: "banking.wallet", tag: "mfe-banking-wallet", kind: "component", scriptUrl: WALLET_SCRIPT },
  { name: "banking.pay-invoice", tag: "mfe-banking-pay-invoice", kind: "component", scriptUrl: WALLET_SCRIPT },
  { name: "banking.mandate", tag: "mfe-banking-mandate", kind: "component", scriptUrl: WALLET_SCRIPT },
];

/** The file's exact text. */
export function walletRegistryJson() {
  return `${JSON.stringify(WALLET_REGISTRY, null, 2)}\n`;
}
