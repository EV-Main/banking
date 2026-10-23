// Node module hooks for the React hook tests (registered by `react-hook-harness.ts`).
//
// Three redirects and one loader:
// - a specifier the test file named in `stubs` resolves to the URL it gave — for a module
//   whose real body Node cannot load (type stripping refuses parameter properties, which
//   `shared/lib/api-client.ts` uses) and which the module under test only touches lightly,
//   or for a stand-in the test controls. The importer's `?case=` is passed on to it. Inside
//   the stub itself the specifier still means the real module;
// - `@/x` resolves the way tsconfig `paths` does, to `<frontend>/x.ts` or `x.tsx`, so a
//   module under test can be loaded as written instead of only through relative imports;
// - `lightweight-charts` resolves to `fake-lightweight-charts.mjs`, under a fresh query on
//   EVERY import, so each `import()` the hook makes is a download of its own and a test can
//   see whether the HOOK caches a failure. (An errored module record would otherwise replay
//   its error forever.) This models the hook's contract, not the bundler: Next 16's
//   Turbopack runtime keeps a failed chunk's rejected promise for the life of the page, so
//   in the built app a second `import()` after a failure does not reach the network.
//   The importer's `?case=` rides along, so a download a finished test left in flight
//   reports to that test's controller, not to whichever test is running when it lands.
// - a `.tsx` file is compiled with esbuild (the frontend's own dev dependency, the one
//   `mfe/build.mjs` builds with): Node's type stripping does not do JSX.

import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

import { transform } from "esbuild";

const ROOT = new URL("../../", import.meta.url);
const FAKE_ENGINE = new URL("./fake-lightweight-charts.mjs", import.meta.url);
let engineImports = 0;
/** @type {Record<string, string>} */
let stubs = {};

export function initialize(data) {
  stubs = data?.stubs ?? {};
}

/** The `?case=` of the module doing the importing, if it was loaded with one. */
const caseOf = (parentURL) => (parentURL ? (new URL(parentURL).searchParams.get("case") ?? "") : "");

export async function resolve(specifier, context, next) {
  const stub = stubs[specifier];
  // A stub importing the very specifier it replaces gets the real module, so a stand-in can
  // re-export the real thing and override only the parts a test needs to control.
  if (stub && !(context.parentURL ?? "").startsWith(stub)) {
    const forCase = caseOf(context.parentURL);
    return { url: forCase ? `${stub}?case=${encodeURIComponent(forCase)}` : stub, shortCircuit: true };
  }
  if (specifier.startsWith("@/")) {
    for (const suffix of [".ts", ".tsx", "/index.ts"]) {
      const url = new URL(specifier.slice(2) + suffix, ROOT);
      if (existsSync(fileURLToPath(url))) return { url: url.href, shortCircuit: true };
    }
  }
  if (specifier === "lightweight-charts") {
    engineImports += 1;
    const forCase = caseOf(context.parentURL);
    return { url: `${FAKE_ENGINE.href}?import=${engineImports}&case=${encodeURIComponent(forCase)}`, format: "module", shortCircuit: true };
  }
  return next(specifier, context);
}

export async function load(url, context, next) {
  if (!url.startsWith("file:") || !new URL(url).pathname.endsWith(".tsx")) return next(url, context);
  const path = fileURLToPath(url);
  const { code } = await transform(await readFile(path, "utf8"), { loader: "tsx", format: "esm", jsx: "automatic", sourcefile: path, sourcemap: "inline" });
  return { format: "module", source: code, shortCircuit: true };
}
