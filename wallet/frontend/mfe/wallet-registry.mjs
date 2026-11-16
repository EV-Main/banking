// The registry `@evinvest/banking` reads (`WALLET_REGISTRY_URL`), written by build.mjs to
// public/mfe/banking/registry.json. Every element lives in one bundle; the host loads it once
// and each tag is defined. Names and tags are the SDK's. Kept apart from the build so a test
// reads it without running esbuild.

/** The bundles' public directory on evinvest.ltd (`@evinvest/banking` `WALLET_MFE_PREFIX`). */
export const WALLET_DIR = "/mfe/banking/";
/** The directories hosts built before the move ask for (#609), and the file each one's
 *  registry.json is. A registry reader refuses a path-relative `scriptUrl`, so each directory
 *  gets a registry naming itself. */
export const WALLET_OLD_DIRS = [
  { dir: "/api/wallet/mfe/", registry: "registry.api.json" },
  { dir: "/wallet/mfe/", registry: "registry.alias.json" },
];

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
