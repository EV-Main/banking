// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { pct, toBps, toPercentInput } from "./rate.ts";

test("a percent field becomes the basis points the wire carries", () => {
  assert.equal(toBps("2"), 200);
  assert.equal(toBps("20"), 2000);
  assert.equal(toBps("0"), 0);
  // The cap itself is a legal rate — the domain's bound is inclusive.
  assert.equal(toBps("100"), 10_000);
});

test("a fraction of a percent survives the conversion exactly", () => {
  // The regression this guards: `Number("20.05") * 100` is 2004.9999999999998, which
  // truncates to 2004 — a rate one basis point below what the operator typed.
  assert.equal(toBps("20.05"), 2005);
  assert.equal(toBps("2.5"), 250);
  assert.equal(toBps("2.55"), 255);
  assert.equal(toBps("0.01"), 1);
  assert.equal(toBps("0.5"), 50);
});

test("a half-typed rate parses rather than flashing an error mid-keystroke", () => {
  // Someone typing "2.5" passes through "2." on the way. Refusing it would put a red
  // message under the field between two keystrokes.
  assert.equal(toBps("2."), 200);
  assert.equal(toBps(" 2.5 "), 250);
  assert.equal(toBps("2,"), 200);
});

test("a decimal comma is the same half a percent", () => {
  // ru, de, fr and vi all write it that way, and the hint under the field shows "2,5".
  assert.equal(toBps("2,5"), 250);
  assert.equal(toBps("20,05"), 2005);
  assert.equal(toBps("0,01"), 1);
  // One separator, not a thousands mark: "1,000" would be a rate with three decimals.
  assert.equal(toBps("1,000"), null);
  assert.equal(toBps("2,5.5"), null);
});

test("anything that is not a percentage is refused, not coerced to a number", () => {
  // `Number("")` is 0 and `Number(" ")` is 0: the old field saved a cleared box as a
  // zero rate without saying so.
  assert.equal(toBps(""), null);
  assert.equal(toBps("  "), null);
  assert.equal(toBps("-2"), null);
  assert.equal(toBps("abc"), null);
  assert.equal(toBps("2%"), null);
  assert.equal(toBps("1e2"), null);
  // A third decimal is a rate finer than a basis point — the money plane cannot hold it.
  assert.equal(toBps("2.555"), null);
});

test("an over-cap rate parses so the caller can name the mistake", () => {
  // Not `null`: "150" is a well-formed percentage that happens to exceed the domain's
  // 100% cap, and the form says so in different words than "that is not a percentage".
  assert.equal(toBps("150"), 15_000);
  assert.equal(toBps("999"), 99_900);
  // Four and more digits too: an operator who typed the basis points instead of the
  // percent is told the ceiling, not that 5001 is "not a percentage".
  assert.equal(toBps("1000"), 100_000);
  assert.equal(toBps("5001"), 500_100);
  assert.equal(toBps("10001"), 1_000_100);
});

test("a stored policy opens the form on the rate it was saved with", () => {
  assert.equal(toPercentInput(200), "2");
  assert.equal(toPercentInput(2000), "20");
  assert.equal(toPercentInput(0), "0");
  assert.equal(toPercentInput(250), "2.5");
  assert.equal(toPercentInput(255), "2.55");
  assert.equal(toPercentInput(205), "2.05");
  assert.equal(toPercentInput(1), "0.01");
  assert.equal(toPercentInput(10_000), "100");
});

test("editing nothing saves back exactly what was stored", () => {
  // The round trip that matters: opening a policy and pressing save must not move the
  // rate by a basis point.
  for (let bps = 0; bps <= 10_000; bps++) {
    assert.equal(toBps(toPercentInput(bps)), bps, `round trip lost ${bps} bps`);
  }
});

test("basis points render as the rate a reader recognises", () => {
  assert.equal(pct(200), "2%");
  assert.equal(pct(2000), "20%");
  assert.equal(pct(250), "2.5%");
  assert.equal(pct(255), "2.55%");
  assert.equal(pct(0), "0%");
  // The investor product page passes a rate straight off an optional wire field.
  assert.equal(pct(undefined), "0%");
});
