// Bundles the element remotes into self-registering ESM files served from public/mfe/:
// the account chip, and banking's wallet elements (public/mfe/wallet/, published as
// https://evinvest.ltd/wallet/mfe/ through the next.config rewrite). React and each
// element are bundled in (the host never imports this module graph — it only injects the
// <script>). The sibling CSS is compiled separately by the Tailwind CLI (see the
// `build:mfe` npm script) so this stays a pure JS build.
import { mkdir, readdir, readFile, writeFile } from "node:fs/promises";

import { build } from "esbuild";

const shared = {
  bundle: true,
  format: "esm",
  minify: true,
  target: ["es2022"],
  jsx: "automatic",
  // Resolve the `@/*` path alias from the app tsconfig.
  tsconfig: "tsconfig.json",
  logLevel: "info",
};

// React reads process.env.NODE_ENV; there is no `process` in the browser, so inline it.
const NODE_ENV = { "process.env.NODE_ENV": '"production"' };

await build({
  ...shared,
  entryPoints: ["mfe/account-chip/entry.tsx"],
  outfile: "public/mfe/account-chip.js",
  define: {
    ...NODE_ENV,
    // The menu's Service-Arb entry; unset, it is left out.
    "process.env.NEXT_PUBLIC_SA_PANEL_URL":
      process.env.NEXT_PUBLIC_SA_PANEL_URL === undefined ? "undefined" : JSON.stringify(process.env.NEXT_PUBLIC_SA_PANEL_URL),
    // Where "Profile" and "Manage account" go: concierge's account page on the platform.
    "process.env.NEXT_PUBLIC_PLATFORM_URL":
      process.env.NEXT_PUBLIC_PLATFORM_URL === undefined ? "undefined" : JSON.stringify(process.env.NEXT_PUBLIC_PLATFORM_URL),
  },
});

// ── wallet ──────────────────────────────────────────────────────────────────

const LOCALES = ["en", "ru", "vi", "fr", "de"];
const WALLET_SOURCES = ["features/wallet-mfe", "mfe/wallet"];

/** Every `t("key", …)` the wallet's own sources call, so its catalogue holds those and nothing else. */
async function walletKeys() {
  const keys = new Set();
  for (const dir of WALLET_SOURCES) {
    for (const file of await readdir(dir, { recursive: true })) {
      if (!/\.tsx?$/.test(file) || /\.test\.tsx?$/.test(file)) continue;
      const source = await readFile(`${dir}/${file}`, "utf8");
      for (const match of source.matchAll(/\bt\(\s*"([^"]+)"/g)) keys.add(match[1]);
    }
  }
  return [...keys].sort();
}

/**
 * The wallet's catalogue per locale, resolved the way the wallet host's policy resolves it: a
 * translation whose stored English has drifted from today's is served as English. Cut from
 * the wallet host catalogues at build time so the copy is never a second hand-kept table.
 */
async function walletMessages() {
  const load = async (locale) => JSON.parse(await readFile(`messages/${locale}/common.json`, "utf8"));
  const en = await load("en");
  const keys = await walletKeys();
  const missing = keys.filter((key) => typeof en[key] !== "string");
  if (missing.length > 0) {
    throw new Error(`wallet keys missing from messages/en/common.json — run \`npm run i18n:extract\`: ${missing.join(", ")}`);
  }
  const table = {};
  for (const locale of LOCALES) {
    const catalogue = locale === "en" ? en : await load(locale);
    table[locale] = Object.fromEntries(
      keys.map((key) => {
        if (locale === "en") return [key, en[key]];
        const entry = catalogue[key];
        return [key, entry && entry.en === en[key] ? entry.t : en[key]];
      }),
    );
  }
  return table;
}

await build({
  ...shared,
  entryPoints: ["mfe/wallet/entry.tsx"],
  outfile: "public/mfe/wallet/wallet.js",
  define: {
    ...NODE_ENV,
    __WALLET_MESSAGES__: JSON.stringify(await walletMessages()),
    // In-page money actions (docs/WALLET-INPAGE-SPEC.md), off unless the build asks; off, the
    // branch folds away and every action stays the `ev:wallet-action` offer.
    __WALLET_IN_PAGE__: JSON.stringify(process.env.WALLET_IN_PAGE === "1"),
  },
});

// The registry `@evinvest/wallet` reads (`WALLET_REGISTRY_URL`). Both elements live in one
// bundle; the host loads it once and either tag is defined. Names and tags are the SDK's.
const WALLET_SCRIPT = "/wallet/mfe/wallet.js";
await mkdir("public/mfe/wallet", { recursive: true });
await writeFile(
  "public/mfe/wallet/registry.json",
  `${JSON.stringify(
    [
      { name: "banking.wallet", tag: "mfe-banking-wallet", kind: "component", scriptUrl: WALLET_SCRIPT },
      { name: "banking.pay-invoice", tag: "mfe-banking-pay-invoice", kind: "component", scriptUrl: WALLET_SCRIPT },
    ],
    null,
    2,
  )}\n`,
);
