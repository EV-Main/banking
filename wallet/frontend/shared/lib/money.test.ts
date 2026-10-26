// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { formatExactUsdt, formatUsdt } from "./money.ts";

test("an exact wire decimal is shown to the digit, never through a float", () => {
  // The approval screens bind `payload_hash` to this exact string, so a rounded render is
  // an approval of something the hash does not cover.
  // `formatUsdt` caps at 6 dp and parses through `Number`; both lose these.
  assert.equal(formatExactUsdt("1000.0000005"), "1,000.0000005");
  assert.equal(formatExactUsdt("0.000000000000000001"), "0.000000000000000001");
  assert.equal(formatExactUsdt("123456789012345678901234567890.5"), "123,456,789,012,345,678,901,234,567,890.50");
  assert.notEqual(formatUsdt("1000.0000005"), formatExactUsdt("1000.0000005"));
});

test("exact formatting still shows the cents every money figure here shows", () => {
  assert.equal(formatExactUsdt("1234"), "1,234.00");
  assert.equal(formatExactUsdt("1234.5"), "1,234.50");
  assert.equal(formatExactUsdt("-5.25"), "\u22125.25");
});

test("a value that is not a plain decimal is passed through, not coerced to zero", () => {
  // Showing an unrecognised value verbatim is honest; showing "0.00" for it is not — and
  // on an approval screen a fabricated zero is the worst possible failure.
  assert.equal(formatExactUsdt("abc"), "abc");
  assert.equal(formatExactUsdt(""), "\u2014");
  assert.equal(formatExactUsdt(undefined), "\u2014");
});

// ── Locale ───────────────────────────────────────────────────────────────────

test("the separators follow the reader's locale; the precision does not", () => {
  // German groups with a dot and takes a comma for the fraction; Russian groups with a
  // narrow no-break space. Read "1,234.50" with either convention and you are off by three
  // orders of magnitude — this is the bug the `en-US` pin used to ship to four locales.
  assert.equal(formatUsdt("1234.5", "de"), "1.234,50");
  assert.equal(formatUsdt("1234.5", "ru"), "1\u00a0234,50");
  // Still exactly the policy's digits: 2–6 dp for ledger money.
  assert.equal(formatUsdt("1000.0000005", "de"), "1.000,000001");
});

test("an exact wire decimal keeps every digit in every locale", () => {
  // The digits go through no float in any locale — only the separators change. The
  // approval screens bind `payload_hash` to the string these digits came from.
  assert.equal(formatExactUsdt("1000.0000005", "de"), "1.000,0000005");
  assert.equal(formatExactUsdt("0.000000000000000001", "ru"), "0,000000000000000001");
  assert.equal(formatExactUsdt("123456789012345678901234567890.5", "de"), "123.456.789.012.345.678.901.234.567.890,50");
  assert.equal(formatExactUsdt("-5.25", "de"), "\u22125,25");
  // Pass-through of a non-decimal is locale-blind.
  assert.equal(formatExactUsdt("abc", "de"), "abc");
});

