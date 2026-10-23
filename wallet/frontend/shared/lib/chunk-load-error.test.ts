// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The terminal and the dashboard offer "Reload page" only for a chunk that never arrived —
// Turbopack keeps that failure for the life of the page — and keep their usual error text
// for anything else. This pins which failures count as the former.
import assert from "node:assert/strict";
import test from "node:test";

import { isChunkLoadError } from "./chunk-load-error.ts";

const named = (name: string, message = "x") => Object.assign(new Error(message), { name });

test("a bundler's ChunkLoadError is a chunk load error", () => {
  assert.equal(isChunkLoadError(named("ChunkLoadError", "Failed to load chunk /cabinet/_next/static/chunks/a.js from module 1")), true);
});

test("an ordinary error with chunk words in its message is not", () => {
  assert.equal(isChunkLoadError(new Error("Failed to load chunk /cabinet/_next/static/chunks/a.js")), false);
});

test("a request error or a type error is not", () => {
  assert.equal(isChunkLoadError(named("RequestError", "502")), false);
  assert.equal(isChunkLoadError(new TypeError("Failed to fetch")), false);
});

test("a plain object or string shaped like one is not", () => {
  assert.equal(isChunkLoadError({ name: "ChunkLoadError", message: "x" }), false);
  assert.equal(isChunkLoadError("ChunkLoadError"), false);
  assert.equal(isChunkLoadError(undefined), false);
});
