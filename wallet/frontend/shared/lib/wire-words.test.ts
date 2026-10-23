// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// `wordFor` is what stands between a value the hub sends and a blank cell: every word table
// in the cabinet is looked up through it, so the three ways a lookup can miss are pinned here.
import assert from "node:assert/strict";
import test from "node:test";

import { wordFor } from "./wire-words.ts";

const words = { open: "Open", closed: "Closed" } as const;

test("a known wire value gets its word", () => {
  assert.equal(wordFor(words, "open"), "Open");
});

test("a value this build has no word for misses, so the caller can fall back to it", () => {
  assert.equal(wordFor(words, "reconciling"), undefined);
  assert.equal(wordFor(words, ""), undefined);
});

test("an absent value misses rather than throwing", () => {
  assert.equal(wordFor(words, null), undefined);
  assert.equal(wordFor(words, undefined), undefined);
});

test("a value naming an Object.prototype member is not resolved up the prototype chain", () => {
  for (const inherited of ["toString", "constructor", "hasOwnProperty", "__proto__", "valueOf"]) {
    assert.equal(wordFor(words, inherited), undefined, inherited);
  }
});
