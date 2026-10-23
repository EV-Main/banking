// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { kycChipState } from "./chip-state.ts";

test("tier 0 with nothing running has not started", () => {
  assert.deepEqual(kycChipState({ level: 0, runningCase: null, settled: true }), { state: "notStarted", tone: "neutral", labelKey: "kyc.chip.notStarted" });
});

test("a running case is in review, whatever word the plane uses for it", () => {
  for (const status of ["pending", "in_progress", "in_review", "a_status_shipped_after_this_cabinet"]) {
    assert.equal(kycChipState({ level: 0, runningCase: { status }, settled: true })?.state, "review", status);
  }
  assert.equal(kycChipState({ level: 0, runningCase: { status: "in_review" }, settled: true })?.tone, "pending");
});

test("a case sent back to the reader needs their attention", () => {
  assert.deepEqual(kycChipState({ level: 0, runningCase: { status: "resubmitted" }, settled: true }), { state: "attention", tone: "error", labelKey: "kyc.chip.attention" });
});

test("the tier outranks the case", () => {
  // A case row can lag the tier the hub already acts on; the chip must not tell a verified
  // reader they are still waiting.
  assert.equal(kycChipState({ level: 1, runningCase: { status: "in_review" }, settled: true })?.state, "verified");
  assert.deepEqual(kycChipState({ level: 2, runningCase: null, settled: true }), { state: "verified", tone: "success", labelKey: "kyc.chip.verified" });
});

test("a read that failed is no state at all", () => {
  // `level` is 0 when neither source answered; saying "Not verified" on it would mislabel a
  // verified reader over an outage that has nothing to do with them.
  assert.equal(kycChipState({ level: 0, runningCase: null, settled: false }), null);
});
