// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import {
  formatAmount,
  formatExactUsdt,
  formatPct,
  formatSignedUsd,
  formatSignedUsdt,
  formatUsd,
  formatUsdt,
  valence,
  valenceClass,
} from "./money.ts";

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

test("signed summary money carries a sign only when there is something to sign", () => {
  // "+$0.00" on a P&L tile claims a gain that did not happen; a zero is just a zero.
  assert.equal(formatSignedUsd(84.83), "+$84.83");
  assert.equal(formatSignedUsd(-84.83), "\u2212$84.83");
  assert.equal(formatSignedUsd(0), "$0.00");
  assert.equal(formatSignedUsd("0"), "$0.00");
  assert.equal(formatSignedUsd(-0), "$0.00");
  assert.equal(formatSignedUsd(undefined), "$0.00");
  assert.equal(formatPct(4.2), "+4.2%");
  assert.equal(formatPct(0), "0.0%");
  assert.equal(formatPct(-0), "0.0%");
});

// ── Locale ───────────────────────────────────────────────────────────────────

test("with no locale, every formatter still speaks the English it always did", () => {
  // Server paths and older call sites pass nothing; their output must not move.
  assert.equal(formatUsd("48250"), "$48,250.00");
  assert.equal(formatSignedUsd(-84.83), "\u2212$84.83");
  assert.equal(formatAmount("1234.5"), "1,234.50");
  assert.equal(formatUsdt("1234.5"), "1,234.50");
  assert.equal(formatSignedUsdt("-5"), "\u22125.00");
  assert.equal(formatPct(4.2), "+4.2%");
});

test("the separators follow the reader's locale; the precision does not", () => {
  // German groups with a dot and takes a comma for the fraction; Russian groups with a
  // narrow no-break space. Read "1,234.50" with either convention and you are off by three
  // orders of magnitude — this is the bug the `en-US` pin used to ship to four locales.
  assert.equal(formatUsdt("1234.5", "de"), "1.234,50");
  assert.equal(formatUsdt("1234.5", "ru"), "1\u00a0234,50");
  assert.equal(formatAmount("1234.5", "de"), "1.234,50");
  // Still exactly the policy's digits: 2–6 dp for ledger money.
  assert.equal(formatUsdt("1000.0000005", "de"), "1.000,000001");
});

test("the currency symbol is placed by the locale but never respelled", () => {
  // `Intl` would write "1.234,50 $" in German and "1 234,50 $US" in French with its default
  // symbol; the narrow symbol keeps a bare "$" everywhere so a figure reads the same unit
  // in every locale, and only its position moves.
  assert.equal(formatUsd("1234.5", "de"), "1.234,50\u00a0$");
  assert.equal(formatUsd("1234.5", "fr"), "1\u202f234,50\u00a0$");
  assert.equal(formatUsd("1234.5", "vi"), "1.234,50\u00a0$");
  assert.equal(formatSignedUsd(-84.83, "de"), "\u221284,83\u00a0$");
  for (const locale of ["ru", "vi", "fr", "de"] as const) {
    const out = formatUsd("1234.5", locale);
    assert.ok(out.includes("$") && !out.includes("US"), `bare "$" expected in ${JSON.stringify(out)}`);
  }
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

test("signs and percent are appended, not localised", () => {
  assert.equal(formatSignedUsdt("-5", "de"), "\u22125,00");
  assert.equal(formatSignedUsdt("5", "ru"), "+5,00");
  assert.equal(formatPct(4.2, "de"), "+4,2%");
  assert.equal(formatPct(-1.8, "ru"), "\u22121,8%");
});

test("a signed figure is a gain, a loss or flat — and zero is never a gain", () => {
  // The wire string is read exactly: "-0" and "0.000" are flat, not a loss or a gain.
  assert.equal(valence("12.5"), "gain");
  assert.equal(valence("-0.000001"), "loss");
  assert.equal(valence("0"), "flat");
  assert.equal(valence("-0"), "flat");
  assert.equal(valence("0.000"), "flat");
  assert.equal(valence(undefined), "flat");
  assert.equal(valence(84.83), "gain");
  assert.equal(valence(-540), "loss");
  assert.equal(valence(0), "flat");
  assert.equal(valence(-0), "flat");
  assert.equal(valence(5n), "gain");
  assert.equal(valence(-5n), "loss");
  assert.equal(valence(0n), "flat");
  // One pair for every investor screen; flat takes the plain ink, never green.
  assert.equal(valenceClass("12.5"), "text-positive");
  assert.equal(valenceClass(-1), "text-accent-error");
  assert.equal(valenceClass("0"), "text-ink");
  assert.equal(valenceClass(0n), "text-ink");
});

test("signed USDT carries no sign on zero and the same minus as signed USD", () => {
  assert.equal(formatSignedUsdt("0"), "0.00");
  assert.equal(formatSignedUsdt("0.000"), "0.00");
  assert.equal(formatSignedUsdt(undefined), "0.00");
  assert.equal(formatSignedUsdt("84.83"), "+84.83");
  assert.equal(formatSignedUsdt("-84.83"), "\u221284.83");
  assert.equal(formatSignedUsd(0), "$0.00");
});
