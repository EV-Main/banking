// The registry `@evinvest/banking` reads (`WALLET_REGISTRY_URL`), written by build.mjs to
// public/mfe/wallet/registry.json. Every element lives in one bundle; the host loads it once
// and each tag is defined. Names and tags are the SDK's. Kept apart from the build so a test
// reads it without running esbuild.

/** The bundles' public directory on evinvest.ltd (`@evinvest/banking` `WALLET_MFE_PREFIX`). */
const WALLET_DIR = "/api/wallet/mfe/";

/** The registry file's exact text. */
export function walletRegistryJson() {
  const scriptUrl = `${WALLET_DIR}wallet.js`;
  const registry = [
    { name: "banking.wallet", tag: "mfe-banking-wallet", kind: "component", scriptUrl },
    { name: "banking.pay-invoice", tag: "mfe-banking-pay-invoice", kind: "component", scriptUrl },
    { name: "banking.mandate", tag: "mfe-banking-mandate", kind: "component", scriptUrl },
  ];
  return `${JSON.stringify(registry, null, 2)}\n`;
}
