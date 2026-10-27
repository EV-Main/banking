// Loads components under `node --test`, which strips types but cannot run TSX or
// resolve the `@/` alias. esbuild — the same build `mfe/build.mjs` runs — compiles the given
// entry into one module, imported from memory; nothing is written to disk.
//
// React stays outside the bundle, imported by absolute file URL from node_modules: bundled,
// its `act` cannot reach Node's `timers` and queues every task on a fresh MessageChannel,
// whose ports keep the test process alive. Outside, the component, `act` and the renderer
// share the one React instance Node's own loader holds.
//
// `stubs` replace modules by import path (e.g. `./stylesheet` for the element entry): a
// module with side effects on a real page gets a test-controlled stand-in instead.
import { build, stop, type Plugin } from "esbuild";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";

const FRONTEND = fileURLToPath(new URL("../../", import.meta.url));
const resolveFromApp = createRequire(`${FRONTEND}package.json`).resolve;

const external: Plugin = {
  name: "react-external",
  setup(b) {
    b.onResolve({ filter: /^(react|react-dom|scheduler)(\/.*)?$/ }, (args) => ({
      path: pathToFileURL(resolveFromApp(args.path)).href,
      external: true,
    }));
  },
};

/** Re-exported from every bundle: the renderer and `act` of the bundled React. */
const REACT = `
export { act, createElement } from "react";
export { flushSync } from "react-dom";
export { createRoot } from "react-dom/client";
`;

export async function bundle(entry: string, stubs: Record<string, string> = {}): Promise<unknown> {
  const stubbed: Plugin = {
    name: "stubs",
    setup(b) {
      for (const [path, contents] of Object.entries(stubs)) {
        const filter = new RegExp(`^${path.replace(/[.*+?^${}()|[\]\\/]/g, "\\$&")}$`);
        b.onResolve({ filter }, () => ({ path, namespace: "stub" }));
        b.onLoad({ filter, namespace: "stub" }, () => ({ contents, loader: "ts" }));
      }
    },
  };
  const out = await build({
    stdin: { contents: entry + REACT, resolveDir: FRONTEND, loader: "tsx", sourcefile: "test-entry.tsx" },
    bundle: true,
    write: false,
    format: "esm",
    platform: "node",
    // ESM builds first: a bundled CJS package would `require` the external React, which an
    // ES module cannot do.
    mainFields: ["module", "main"],
    jsx: "automatic",
    tsconfig: `${FRONTEND}tsconfig.json`,
    plugins: [stubbed, external],
    logLevel: "silent",
  });
  // esbuild's service child keeps the event loop alive; the test process must be able to exit.
  await stop();
  const code = out.outputFiles[0]?.text;
  if (code === undefined) throw new Error("esbuild produced no output");
  return import(`data:text/javascript;base64,${Buffer.from(code).toString("base64")}`);
}
