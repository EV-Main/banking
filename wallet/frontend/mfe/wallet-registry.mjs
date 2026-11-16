// The registry `@evinvest/banking` reads (`WALLET_REGISTRY_URL`), written by build.mjs to
// public/mfe/wallet/registry.json. Every element lives in one bundle; the host loads it once
// and each tag is defined. Names and tags are the SDK's. Kept apart from the build so a test
// reads it without running esbuild.

/** The bundles' public directory on evinvest.ltd (`@evinvest/banking` `WALLET_MFE_PREFIX`). */
export const WALLET_DIR = "/api/wallet/mfe/";
/** Its alias for the hosts built before the move (#609). A registry reader refuses a
 *  path-relative `scriptUrl`, so each directory gets a registry naming itself. */
export const WALLET_ALIAS_DIR = "/wallet/mfe/";

/** The registry file's exact text, for the bundles served from `dir`. */
export function walletRegistryJson(dir) {
  const scriptUrl = `${dir}wallet.js`;
  const registry = [
    { name: "banking.wallet", tag: "mfe-banking-wallet", kind: "component", scriptUrl },
    { name: "banking.pay-invoice", tag: "mfe-banking-pay-invoice", kind: "component", scriptUrl },
    { name: "banking.mandate", tag: "mfe-banking-mandate", kind: "component", scriptUrl },
  ];
  return `${JSON.stringify(registry, null, 2)}\n`;
}
