// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// Pins the two halves of the rule in `cabinet-link.tsx`: cabinet links do not prefetch
// automatically BECAUSE no route has a loading boundary for a prefetch to carry. The
// halves are checked together so they cannot drift apart silently — a `loading.tsx`
// added without revisiting the default would leave real skeletons unprefetched, and a
// prefetch re-enabled without one would bring back two RSC documents per link on every
// page load (banking#349).
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join, relative } from "node:path";
import test from "node:test";

const FRONTEND_ROOT = fileURLToPath(new URL("../../", import.meta.url));
const APP_ROOT = join(FRONTEND_ROOT, "app");
const LINK_SOURCE = fileURLToPath(new URL("./cabinet-link.tsx", import.meta.url));
const NEXT_CONFIG = fileURLToPath(new URL("../../next.config.ts", import.meta.url));

function walk(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walk(path, out);
    else out.push(path);
  }
  return out;
}

test("no cabinet route has a loading boundary — the premise behind prefetch={false}", () => {
  const boundaries = walk(APP_ROOT)
    .filter((path) => /^loading\.(tsx|ts|jsx|js)$/.test(path.slice(path.lastIndexOf("/") + 1)))
    .map((path) => relative(FRONTEND_ROOT, path));
  assert.deepEqual(
    boundaries,
    [],
    `A loading boundary changes what a prefetch is worth: with one, the viewport prefetch ` +
      `carries the layouts and the skeleton, so the click paints before the page arrives. ` +
      `Revisit the prefetch default in shared/ui/cabinet-link.tsx before keeping: ${boundaries.join(", ")}`,
  );
});

test("cabinet links opt out of automatic prefetching unless the caller says otherwise", () => {
  const source = readFileSync(LINK_SOURCE, "utf8");
  // The default sits immediately before the caller's spread, so an explicit `prefetch`
  // from a caller with a reason still wins. Lazy up to the props, not `[^>]*`: an inline
  // arrow in the JSX would contain a `>` and make a stricter match lie.
  assert.match(
    source,
    /<NextLink\b[\s\S]*?\bprefetch=\{false\}\s+\{\.\.\.props\}/,
    "cabinet-link.tsx must render <NextLink … prefetch={false} {...props} /> (banking#349)",
  );
});

test("no PPR or Cache Components in next.config — the other premise behind prefetch={false}", () => {
  const config = readFileSync(NEXT_CONFIG, "utf8");
  assert.doesNotMatch(
    config,
    /\bcacheComponents\s*:\s*true|\bppr\s*:/,
    "With PPR or Cache Components a prefetch carries the static shell of the page itself, " +
      "so the viewport prefetch stops being two empty documents. Revisit the prefetch " +
      "default in shared/ui/cabinet-link.tsx before enabling either.",
  );
});

// The proxy renews a lapsing session before rendering, and it cannot tell a router prefetch
// from a click: Next strips `next-router-prefetch` before the proxy runs (see the note on
// `renewIfLapsing` in proxy.ts). A prefetch would then slide the session's expiry for a link
// the reader only scrolled past. Nothing prefetches today; these keep it that way.

const SOURCE_DIRS = ["app", "application", "views", "features", "entities", "shared", "mfe"].map((dir) => join(FRONTEND_ROOT, dir));

function sources(): string[] {
  return SOURCE_DIRS.flatMap((dir) => walk(dir)).filter((path) => /\.(tsx?|jsx?|mjs)$/.test(path) && !/\.test\.tsx?$/.test(path));
}

test("only cabinet-link.tsx imports next/link — a bare one prefetches by default", () => {
  const importers = sources()
    .filter((path) => /from\s+["']next\/link["']/.test(readFileSync(path, "utf8")))
    .map((path) => relative(FRONTEND_ROOT, path));

  assert.deepEqual(importers, ["shared/ui/cabinet-link.tsx"], `Route these through shared/ui/cabinet-link.tsx: ${importers.join(", ")}`);
});

test("no link opts back into prefetching", () => {
  const optedIn = sources()
    .filter((path) => /\bprefetch=\{(?!false\})/.test(readFileSync(path, "utf8")))
    .map((path) => relative(FRONTEND_ROOT, path));

  assert.deepEqual(optedIn, [], `A prefetching link renews the session through the proxy on scroll: ${optedIn.join(", ")}`);
});

test("no code calls the router's prefetch", () => {
  const callers = sources()
    .filter((path) => {
      const source = readFileSync(path, "utf8");
      return /\buseRouter\b/.test(source) && /\.prefetch\(/.test(source);
    })
    .map((path) => relative(FRONTEND_ROOT, path));

  assert.deepEqual(callers, [], `router.prefetch renews the session through the proxy without a click: ${callers.join(", ")}`);
});
