// Node module hooks for the React hook tests (registered by `react-hook-harness.ts`).
//
// Two redirects and one loader:
// - a specifier the test file named in `stubs` resolves to the URL it gave — for a module
//   whose real body Node cannot load (type stripping refuses parameter properties, which
//   `shared/lib/api-client.ts` uses) and which the module under test only touches lightly,
//   or for a stand-in the test controls. The importer's `?case=` is passed on to it. Inside
//   the stub itself the specifier still means the real module;
// - `@/x` resolves the way tsconfig `paths` does, to `<frontend>/x.ts` or `x.tsx`, so a
//   module under test can be loaded as written instead of only through relative imports;
// - a `.tsx` file is compiled with esbuild (the frontend's own dev dependency, the one
//   `mfe/build.mjs` builds with): Node's type stripping does not do JSX.

import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

import { transform } from "esbuild";

const ROOT = new URL("../../", import.meta.url);
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
  return next(specifier, context);
}

export async function load(url, context, next) {
  if (!url.startsWith("file:") || !new URL(url).pathname.endsWith(".tsx")) return next(url, context);
  const path = fileURLToPath(url);
  const { code } = await transform(await readFile(path, "utf8"), { loader: "tsx", format: "esm", jsx: "automatic", sourcefile: path, sourcemap: "inline" });
  return { format: "module", source: code, shortCircuit: true };
}
