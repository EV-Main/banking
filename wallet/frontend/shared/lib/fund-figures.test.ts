// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { FUND_FIGURES } from "../config/fund-figures.ts";
import { formatFundFigures } from "./fund-figures.ts";

test("the figures render the shape the landing's hero renders", () => {
  const en = formatFundFigures("en");
  assert.equal(en.targetIrr, "16.4% +");
  assert.equal(en.closingTarget, "$100M");
  // Four of the five locales put a space before the sign and a comma in the fraction.
  const ru = formatFundFigures("ru");
  assert.match(ru.targetIrr, /^16,4\s%\s\+$/u);
  assert.equal(ru.closingTarget, "$100M");
});

test("the as-of line carries the owner's confirmation date, in the reader's words", () => {
  assert.equal(FUND_FIGURES.asOf, "2026-09-18");
  assert.equal(formatFundFigures("en").asOf, "18 September 2026");
  // No trailing "г.": the caption is one line beside the figures, not a document date.
  assert.equal(formatFundFigures("ru").asOf, "18 сентября 2026");
  assert.equal(formatFundFigures("de").asOf, "18. September 2026");
  assert.equal(formatFundFigures("fr").asOf, "18 septembre 2026");
});

test("no as-of line while the owner has not confirmed a date", () => {
  // The regression this guards: a placeholder date beside a return figure reads as a fact.
  assert.equal("asOf" in formatFundFigures("en", { ...FUND_FIGURES, asOf: undefined }), false);
});

test("the mirror carries the site's values", () => {
  // site_conductor `frontend/shared/config/fund-figures.ts` — change both or neither.
  assert.equal(FUND_FIGURES.targetIrrPct, 16.4);
  assert.equal(FUND_FIGURES.closingTargetUsdM, 100);
  assert.equal(FUND_FIGURES.minSubscriptionUsd, undefined);
});
