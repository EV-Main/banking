// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The cabinet paints the uikit's palette, never a private copy of it. Two things undid
// that once and neither announced itself: markup written against a token vocabulary the
// kit had already retired (kept alive by a local alias sheet, PR #290), and kit tokens
// re-pointed in the cabinet's own CSS so half the screens drifted from the design system.
// These tests fail the build on either, so a fix has to land upstream in EV-invest/lib's
// `tokens.css` — or sit in the marked block below, which forces its own deletion.
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { join, relative } from "node:path";
import test from "node:test";

const FRONTEND_ROOT = fileURLToPath(new URL("../../", import.meta.url));
const KIT_TOKENS = createRequire(import.meta.url).resolve("@evinvest/uikit/styles/tokens.css");
const SELF = fileURLToPath(import.meta.url);

// The alias table of the retired bridge, `application/styles/tokens-legacy.css`. Each entry
// is a colour name that no longer exists in the kit; the value it once stood for is the
// name to write instead.
const LEGACY_NAMES: Record<string, string> = {
  "muted-foreground": "ink-soft",
  "card-foreground": "ink",
  "popover-foreground": "ink",
  "primary-foreground": "on-primary",
  "secondary-foreground": "on-secondary",
  "accent-foreground": "ink",
  "destructive-foreground": "on-accent-error",
  foreground: "ink",
  accent: "hover",
  destructive: "accent-error",
  "main-brand": "brand",
  "main-black": "background",
  "main-surface": "secondary",
  "main-card": "card",
  "main-mist": "ink",
  "main-accent-t1": "primary-ink",
  "main-accent-t2": "positive",
  "main-accent-t3": "accent-warn",
  "main-accent-t4": "chart-4",
};

// A temporary token fix mirrored from lib ahead of the npm bump lives between these two
// comments in a cabinet stylesheet (`application/styles/globals.css`). Nothing else may
// redeclare a kit token.
const BLOCK_BEGIN = "/* BEGIN temporary token override */";
const BLOCK_END = "/* END temporary token override */";

// Theme choices the cabinet makes on purpose, in the kit's own contract (a consumer
// re-themes by writing token values). Each needs a reason here, not just a name.
const THEME_CHOICES: Record<string, string> = {
  // Single-typeface surface: the kit's display serif is retired so uikit's own `font-serif`
  // classes (NotFound / Forbidden / ServerError) fall back to Inter too.
  "--font-serif": "the cabinet is Inter-only; see globals.css",
};

const SKIP_DIRS = new Set(["node_modules", ".next", ".turbo", "public", "dist"]);

function* sources(dir: string, ext: readonly string[]): Generator<string> {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (!SKIP_DIRS.has(entry.name)) yield* sources(path, ext);
    } else if (ext.some((e) => entry.name.endsWith(e)) && path !== SELF) {
      yield path;
    }
  }
}

// Only a colour name in utility position counts: `text-foreground`, `hover:bg-destructive/10`,
// `--color-main-card`. `variant="destructive"` is a component variant, not a token.
const UTILITY_PREFIX =
  "(?:text|bg|border|ring|fill|stroke|from|to|via|outline|shadow|decoration|divide|placeholder|caret|ring-offset|inset-ring|border-[trblxyse]|selection|marker|outline-offset|accent)-";
const LEGACY_UTILITY = new RegExp(
  `(?<![A-Za-z0-9_-])(?:${UTILITY_PREFIX}|--color-)(${Object.keys(LEGACY_NAMES).join("|")})(?![A-Za-z0-9_-])`,
  "g",
);

test("no source writes a token name the kit has retired", () => {
  const found: string[] = [];
  for (const file of sources(FRONTEND_ROOT, [".ts", ".tsx", ".css"])) {
    const text = readFileSync(file, "utf8");
    for (const match of text.matchAll(LEGACY_UTILITY)) {
      const line = text.slice(0, match.index).split("\n").length;
      found.push(`${relative(FRONTEND_ROOT, file)}:${line}: ${match[0]} → ${LEGACY_NAMES[match[1]!]}`);
    }
  }
  assert.deepEqual(found, [], `legacy token names in source (write the kit's name instead):\n${found.join("\n")}`);
});

interface Declaration {
  name: string;
  value: string;
  line: number;
  inBlock: boolean;
}

// Custom-property declarations of a stylesheet, with whether each sits inside the marked
// block. Comments are blanked (the markers first swapped for sentinels) so a declaration
// quoted in prose — the kit's sheet shows `--brand-mark: url(...)` inside one — does not
// count as written. A name only counts at the start of a declaration, so the `--ink` in
// `color-mix(in srgb, var(--ink) 8%, transparent)` is a read, not a write.
function declarations(css: string): Declaration[] {
  const prepared = css
    .replaceAll(BLOCK_BEGIN, "\u0001")
    .replaceAll(BLOCK_END, "\u0002")
    .replace(/\/\*[\s\S]*?\*\//g, (comment) => comment.replace(/[^\n]/g, " "));
  const out: Declaration[] = [];
  for (const match of prepared.matchAll(/(?<=(?:^|[{;])\s*)(--[a-z0-9-]+)\s*:\s*([^;{}]+)/gm)) {
    const before = prepared.slice(0, match.index);
    out.push({
      name: match[1]!,
      value: match[2]!.trim(),
      line: before.split("\n").length,
      inBlock: before.lastIndexOf("\u0001") > before.lastIndexOf("\u0002"),
    });
  }
  return out;
}

// `#E6E1D3` and `#e6e1d3`, or a re-spaced `color-mix(...)`, are the same value: compare
// canonical forms, or a dead override survives a bump that merely re-typed it.
const canonical = (value: string) => value.toLowerCase().replace(/\s+/g, " ").trim();

const kit = new Map<string, Set<string>>();
for (const { name, value } of declarations(readFileSync(KIT_TOKENS, "utf8"))) {
  kit.set(name, (kit.get(name) ?? new Set()).add(canonical(value)));
}

test("the kit's sheet is the one this test knows how to read", () => {
  // If the kit ever stops declaring these, the override check below would pass vacuously.
  for (const name of ["--ink", "--card", "--background", "--primary", "--color-ink"]) {
    assert.ok(kit.has(name), `${name} missing from ${KIT_TOKENS}`);
  }
});

test("no cabinet stylesheet redeclares a kit token outside the marked block", () => {
  const strays: string[] = [];
  for (const file of sources(FRONTEND_ROOT, [".css"])) {
    for (const { name, line, inBlock } of declarations(readFileSync(file, "utf8"))) {
      if (!kit.has(name) || inBlock || name in THEME_CHOICES) continue;
      strays.push(`${relative(FRONTEND_ROOT, file)}:${line}: ${name}`);
    }
  }
  assert.deepEqual(
    strays,
    [],
    `kit tokens redeclared in the cabinet — fix them in EV-invest/lib tokens.css, or mirror the fix between\n${BLOCK_BEGIN} … ${BLOCK_END}:\n${strays.join("\n")}`,
  );
});

test("a temporary override is deleted once the kit ships the same value", () => {
  // The block exists to bridge the gap to an npm bump. The moment the installed kit carries
  // the value, the mirror is dead weight that will drift on the next retune.
  const stale: string[] = [];
  for (const file of sources(FRONTEND_ROOT, [".css"])) {
    for (const { name, value, line, inBlock } of declarations(readFileSync(file, "utf8"))) {
      if (!inBlock) continue;
      assert.ok(kit.has(name), `${relative(FRONTEND_ROOT, file)}:${line}: ${name} is not a kit token, so it does not belong in the override block`);
      if (kit.get(name)!.has(canonical(value))) stale.push(`${relative(FRONTEND_ROOT, file)}:${line}: ${name}: ${value}`);
    }
  }
  assert.deepEqual(stale, [], `the installed uikit already ships these values — delete the override:\n${stale.join("\n")}`);
});
