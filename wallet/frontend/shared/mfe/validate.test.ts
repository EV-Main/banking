// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { isAllowedScriptUrl, isSameOrigin, parseRegistry } from "./validate.ts";

function withAllowed(origins: string, fn: () => void) {
  const prev = process.env.MFE_ALLOWED_ORIGINS;
  process.env.MFE_ALLOWED_ORIGINS = origins;
  try {
    fn();
  } finally {
    if (prev === undefined) delete process.env.MFE_ALLOWED_ORIGINS;
    else process.env.MFE_ALLOWED_ORIGINS = prev;
  }
}

test("relative bundle URLs are same-origin and always allowed", () => {
  assert.equal(isSameOrigin("/mfe/x.js"), true);
  assert.equal(isAllowedScriptUrl("/mfe/x.js"), true);
});

test("cross-origin bundle URLs are only allowed when their origin is on the allow-list", () => {
  withAllowed("https://cdn.trusted.example", () => {
    assert.equal(isAllowedScriptUrl("https://cdn.trusted.example/remotes/x.js"), true);
    assert.equal(isAllowedScriptUrl("https://evil.example/x.js"), false);
  });
});

test("non-http(s) and empty bundle URLs are rejected", () => {
  withAllowed("https://cdn.trusted.example", () => {
    assert.equal(isAllowedScriptUrl("javascript:alert(1)"), false);
    assert.equal(isAllowedScriptUrl("data:text/javascript,alert(1)"), false);
    assert.equal(isAllowedScriptUrl(""), false);
  });
});

test("a same-origin registry parses and round-trips", () => {
  const entries = parseRegistry([
    { name: "example", tag: "mfe-example", scriptUrl: "/mfe/example.js", kind: "page" },
  ]);
  assert.equal(entries.length, 1);
  assert.equal(entries[0].tag, "mfe-example");
});

test("an off-allow-list origin fails registry validation", () => {
  withAllowed("https://cdn.trusted.example", () => {
    assert.throws(
      () => parseRegistry([{ name: "x", tag: "t", scriptUrl: "https://evil.example/x.js", integrity: "sha384-abc", kind: "page" }]),
      /failed validation/,
    );
  });
});

test("a cross-origin entry without an SRI hash fails validation", () => {
  withAllowed("https://cdn.trusted.example", () => {
    assert.throws(
      () => parseRegistry([{ name: "x", tag: "t", scriptUrl: "https://cdn.trusted.example/x.js", kind: "page" }]),
      /failed validation/,
    );
    // With a hash it passes.
    const ok = parseRegistry([{ name: "x", tag: "t", scriptUrl: "https://cdn.trusted.example/x.js", integrity: "sha384-abc", kind: "page" }]);
    assert.equal(ok.length, 1);
  });
});

test("malformed registry shapes are rejected (no unchecked cast)", () => {
  assert.throws(() => parseRegistry({} as unknown), /must be an array/);
  assert.throws(() => parseRegistry([{ name: "x", tag: "t", scriptUrl: "/x.js" }]), /failed validation/); // missing kind
  assert.throws(() => parseRegistry([{ name: "x", tag: "t", scriptUrl: "/x.js", kind: "widget" }]), /failed validation/); // bad kind
  assert.throws(() => parseRegistry([{ name: "", tag: "t", scriptUrl: "/x.js", kind: "page" }]), /failed validation/); // empty name
});
