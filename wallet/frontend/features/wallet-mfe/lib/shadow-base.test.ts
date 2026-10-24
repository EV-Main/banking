// Run with `npm run test`.
import assert from "node:assert/strict";
import test from "node:test";

import { FORM_CONTAINER_ATTR, SHADOW_BASE_CSS } from "./shadow-base.ts";

/** The declaration text of the first rule whose selector list matches `selector`, or null. */
function declarationsOf(css: string, selector: RegExp): string | null {
  const match = new RegExp(`(?:^|[}{;\\s])(${selector.source})\\s*\\{([^{}]*)\\}`, "m").exec(css);
  return match === null ? null : match[2];
}

const container = new RegExp(`\\[\\s*${FORM_CONTAINER_ATTR}\\s*\\]`);

test("the container attribute is the one the wall selects", () => {
  assert.equal(FORM_CONTAINER_ATTR, "data-wallet-form");
});

test("the base sheet lives in the wallet-base layer", () => {
  assert.match(SHADOW_BASE_CSS, /^\s*@layer\s+wallet-base\s*\{/);
});

test("the container resets every inherited property with all: initial", () => {
  const block = declarationsOf(SHADOW_BASE_CSS, container);
  assert.notEqual(block, null, "no [data-wallet-form] rule found");
  assert.match(block!, /(?:^|[;\s])all\s*:\s*initial\b/);
});

test("the container forbids a host text-transform on the terms", () => {
  const block = declarationsOf(SHADOW_BASE_CSS, container);
  assert.notEqual(block, null, "no [data-wallet-form] rule found");
  assert.match(block!, /text-transform\s*:\s*none\b/);
});

test("every element is border-box", () => {
  const block = declarationsOf(SHADOW_BASE_CSS, /\*\s*,\s*\*::before\s*,\s*\*::after/);
  assert.notEqual(block, null, "no universal rule found");
  assert.match(block!, /box-sizing\s*:\s*border-box\b/);
});

test("form controls inherit the font instead of drawing in the browser's own", () => {
  const block = declarationsOf(SHADOW_BASE_CSS, /button\s*,\s*input\b[^{}]*/);
  assert.notEqual(block, null, "no button/input rule found");
  assert.match(block!, /(?:^|[;\s])font\s*:\s*inherit\b/);
});

test("the hidden attribute wins over any utility with display: none !important", () => {
  const block = declarationsOf(SHADOW_BASE_CSS, /\[hidden\][^{}]*/);
  assert.notEqual(block, null, "no [hidden] rule found");
  assert.match(block!, /display\s*:\s*none\s*!important/);
});
