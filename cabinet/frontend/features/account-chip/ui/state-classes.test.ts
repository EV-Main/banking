// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The chip's CSS sits in the `reamfe` layer, below every layer of the page that hosts it
// (chip-parts.ts). A host whose own `utilities` layer generates the same stock utility —
// `opacity-0`, `hidden`, `bg-transparent` — beats every variant of ours on that element: an
// open panel stays invisible but clickable, a hover fill never shows. So a look the chip
// switches by state never takes its base from one of these utilities; the closed look is the
// variant (`data-[state=closed]:opacity-0`) and the base is the default or an arbitrary
// property (`[background-color:transparent]`) no host generates.
//
// This is a source scan, not a render: the failure only shows on a host page with that
// utility in a higher layer, which no unit test here has. It reads every class-looking
// double-quoted string in the chip's ui/ files and flags the listed utilities when they stand
// bare, with no variant before them. A template literal or a class built by concatenation is
// not seen; the chip writes its classes as plain strings through `cn`, which this relies on.
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import test from "node:test";

const UI = new URL("./", import.meta.url);

/** Stock utilities a host is likely to ship that set a property the chip varies by state. */
const STATE_UTILITIES = new Set(["opacity-0", "scale-97", "-translate-y-1", "hidden", "bg-transparent"]);

const SOURCES = readdirSync(UI).filter((f) => /\.tsx?$/.test(f) && !f.endsWith(".test.ts"));

/** `file: "class"` for every bare state utility in a double-quoted string of the file. */
function bareStateUtilities(file: string): string[] {
  const source = readFileSync(new URL(file, UI), "utf8");
  return [...source.matchAll(/"([^"\n]*)"/g)].flatMap(([, literal = ""]) =>
    literal
      .split(/\s+/)
      .filter((token) => STATE_UTILITIES.has(token))
      .map((token) => `${file}: "${token}" in "${literal}"`),
  );
}

test("the scan sees the chip's ui sources", () => {
  assert.ok(SOURCES.includes("menu-panel.tsx"), `ui/ has: ${SOURCES.join(", ")}`);
  assert.ok(SOURCES.includes("chip-parts.ts"), `ui/ has: ${SOURCES.join(", ")}`);
});

test("no chip state takes its base look from a stock utility a host may outrank", () => {
  const found = SOURCES.flatMap(bareStateUtilities);

  assert.deepEqual(
    found,
    [],
    "a bare state utility loses to the host's own utilities layer; make the state the variant " +
      "(data-[state=…]:, aria-*:, motion-safe:) and leave the base to the default or an arbitrary property",
  );
});
