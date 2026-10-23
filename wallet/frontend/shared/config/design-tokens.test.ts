// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// These guard a failure that shipped once and did not announce itself: the account-chip
// remote being built from a stylesheet that silently drops half of what it needs. The
// palette itself is guarded next door, in token-override.test.ts.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const mfeEntry = new URL("../../mfe/account-chip/mfe.css", import.meta.url);

test("the element remote is built against the shared tokens", () => {
  // The remote compiles from its own stylesheet, so it does not inherit anything the app
  // entry point sets up. Without this import it renders an untokenised palette.
  const remote = readFileSync(mfeEntry, "utf8");
  assert.match(remote, /@evinvest\/uikit\/styles\/tokens\.css/, "mfe.css must import the same token sheet as globals.css");
});

test("the remote's theme import stays unlayered", () => {
  // Layering the theme import hides the spacing scale from the utility generator, so
  // fractional steps compile to nothing: `size-8.5` and `gap-2.5` vanished and the chip
  // collapsed, while integer steps kept working — which is what made it read as a layout
  // bug rather than a build one.
  assert.match(readFileSync(mfeEntry, "utf8"), /@import "tailwindcss\/theme\.css";/, "the theme import must not carry a layer()");
});
