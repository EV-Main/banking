// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// These pin the browser's mirror of `domain/src/fees.rs` to the plane's rules. The mirror
// exists so the form can say "the owners will be asked — a reason is required" before the
// click; a mirror that drifted would tell an operator the opposite of what the plane is
// about to do, which is worse than saying nothing.
import assert from "node:assert/strict";
import test from "node:test";

import { HOUSE_TERMS, isPendingChange, MAX_MANAGEMENT_BPS, MAX_PERFORMANCE_BPS, NO_TERMS, requirementFor, tightensFrom, withinHouseEnvelope, type FeeTermsLike } from "./fee-terms.ts";

const terms = (over: Partial<FeeTermsLike> = {}): FeeTermsLike => ({ ...HOUSE_TERMS, ...over });

test("the ceilings are the domain's: 5% p.a. and half the gain", () => {
  assert.equal(MAX_MANAGEMENT_BPS, 500);
  assert.equal(MAX_PERFORMANCE_BPS, 5_000);
});

test("the house envelope is 2/20 or less, on invested capital, annually — any hurdle", () => {
  assert.equal(withinHouseEnvelope(HOUSE_TERMS), true);
  assert.equal(withinHouseEnvelope(terms({ management_bps: 100, performance_bps: 1_000 })), true);
  assert.equal(withinHouseEnvelope(terms({ hurdle_bps: 800 })), true);
  assert.equal(withinHouseEnvelope(terms({ management_bps: 201 })), false);
  assert.equal(withinHouseEnvelope(terms({ performance_bps: 2_001 })), false);
  assert.equal(withinHouseEnvelope(terms({ basis: "market_value" })), false);
  assert.equal(withinHouseEnvelope(terms({ crystallization: "quarterly" })), false);
});

test("any leg getting dearer for the investor tightens", () => {
  assert.equal(tightensFrom(HOUSE_TERMS, HOUSE_TERMS), false);
  assert.equal(tightensFrom(HOUSE_TERMS, terms({ management_bps: 201 })), true);
  assert.equal(tightensFrom(HOUSE_TERMS, terms({ performance_bps: 2_001 })), true);
  assert.equal(tightensFrom(terms({ hurdle_bps: 500 }), terms({ hurdle_bps: 400 })), true);
  assert.equal(tightensFrom(HOUSE_TERMS, terms({ basis: "market_value" })), true);
  assert.equal(tightensFrom(HOUSE_TERMS, terms({ crystallization: "monthly" })), true);
  assert.equal(tightensFrom(HOUSE_TERMS, terms({ crystallization: "semi_annual" })), true);
});

test("loosening never tightens, however the other legs sit", () => {
  assert.equal(tightensFrom(HOUSE_TERMS, terms({ management_bps: 100 })), false);
  assert.equal(tightensFrom(HOUSE_TERMS, terms({ hurdle_bps: 500 })), false);
  assert.equal(tightensFrom(terms({ basis: "market_value" }), terms({ basis: "invested_capital" })), false);
  assert.equal(tightensFrom(terms({ crystallization: "monthly" }), terms({ crystallization: "annual" })), false);
});

test("a period this build cannot rank is not read as a tightening", () => {
  assert.equal(tightensFrom(HOUSE_TERMS, terms({ crystallization: "fortnightly" })), false);
  assert.equal(tightensFrom(terms({ crystallization: "fortnightly" }), HOUSE_TERMS), false);
});

test("a first positive rate on an unpriced fund tightens from nothing", () => {
  assert.equal(tightensFrom(NO_TERMS, HOUSE_TERMS), true);
  // …but the house terms sit inside the envelope, so one administrator may set them.
  assert.equal(requirementFor(null, HOUSE_TERMS), "admin");
});

test("the owners are needed exactly when a tightening leaves the envelope", () => {
  assert.equal(requirementFor(HOUSE_TERMS, terms({ management_bps: 300 })), "owner_consilium");
  assert.equal(requirementFor(HOUSE_TERMS, terms({ performance_bps: 2_500 })), "owner_consilium");
  assert.equal(requirementFor(HOUSE_TERMS, terms({ basis: "market_value" })), "owner_consilium");
  assert.equal(requirementFor(HOUSE_TERMS, terms({ crystallization: "quarterly" })), "owner_consilium");
  // A tightening that stays inside the envelope is an administrator's call.
  assert.equal(requirementFor(terms({ management_bps: 100 }), terms({ management_bps: 200 })), "admin");
  // Loosening never needs a quorum, however far outside the envelope the terms sit.
  assert.equal(requirementFor(terms({ management_bps: 400, basis: "market_value" }), terms({ management_bps: 300, basis: "market_value" })), "admin");
  assert.equal(requirementFor(null, terms({ management_bps: 0, performance_bps: 0 })), "admin");
});

test("lowering the hurdle always needs the owners, wherever the other legs sit", () => {
  assert.equal(requirementFor(terms({ hurdle_bps: 500 }), terms({ hurdle_bps: 400 })), "owner_consilium");
  assert.equal(requirementFor(terms({ hurdle_bps: 500 }), terms({ hurdle_bps: 0, management_bps: 100 })), "owner_consilium");
  // Raising it, or leaving it, does not.
  assert.equal(requirementFor(terms({ hurdle_bps: 500 }), terms({ hurdle_bps: 600 })), "admin");
});

test("a change is pending in exactly the two states that still move without the screen acting", () => {
  // What the admin resources poll on: a settled fund must cost nothing.
  assert.equal(isPendingChange("scheduled"), true);
  assert.equal(isPendingChange("awaiting_consilium"), true);
  for (const state of ["active", "superseded", "rejected", "cancelled", "", undefined]) assert.equal(isPendingChange(state), false, String(state));
});
