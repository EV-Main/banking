// Run with `npm run test`.
import assert from "node:assert/strict";
import test from "node:test";

import { escapeCloses, type FlowPhase } from "./escape-closes.ts";

// A Record forces this table to name every phase: a new FlowPhase fails `tsc` until it is decided here.
const expected: Record<FlowPhase, boolean> = {
  editing: true,
  done: true,
  creating: false,
  review: false,
  expired: false,
  unclear: false,
};

test("escape closes the form only in the phases that offer Cancel or Close", () => {
  for (const [phase, closes] of Object.entries(expected) as [FlowPhase, boolean][]) {
    assert.equal(escapeCloses(phase), closes, `phase "${phase}": expected ${closes}`);
  }
});
