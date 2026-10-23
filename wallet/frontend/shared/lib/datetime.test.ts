// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// A stamp read on the server is formatted twice: once in the server's HTML and again in the
// render that hydrates it. The views hand both the same zone from `useTimeZone`
// (`shared/lib/time-zone.tsx`) — the reader's stored zone, else UTC, while hydrating; the
// device's own after — so a formatter given a zone must use it whatever the runtime's zone
// is, and one given none keeps the runtime's own. These pin both halves at the one place
// they visibly differ — a stamp that falls on the other side of midnight in the runtime's
// zone. `process.env.TZ` stands in for that zone; Node re-reads it on assignment.
import assert from "node:assert/strict";
import test, { afterEach, beforeEach } from "node:test";

import { formatDay, formatMoment } from "./datetime.ts";

// 2026-03-11T23:30:00Z — already 12 March east of Greenwich.
const LATE_EVENING_UTC = "1773271800";
// 2026-03-12T00:30:00Z — still 11 March west of Greenwich.
const JUST_AFTER_MIDNIGHT_UTC = "1773275400";

let savedZone: string | undefined;
beforeEach(() => {
  savedZone = process.env.TZ;
});
afterEach(() => {
  if (savedZone === undefined) delete process.env.TZ;
  else process.env.TZ = savedZone;
});

test("a given zone decides the day even where the runtime is already past midnight", () => {
  process.env.TZ = "Asia/Tokyo";

  assert.equal(formatDay(LATE_EVENING_UTC, "en", "UTC"), "11 Mar 2026");
});

test("without a zone the day is the runtime's own, past midnight east of Greenwich", () => {
  process.env.TZ = "Asia/Tokyo";

  assert.equal(formatDay(LATE_EVENING_UTC, "en"), "12 Mar 2026");
});

test("a given zone decides the day even where the runtime has not reached midnight yet", () => {
  process.env.TZ = "America/New_York";

  assert.equal(formatDay(JUST_AFTER_MIDNIGHT_UTC, "en", "UTC"), "12 Mar 2026");
});

test("without a zone the day is the runtime's own, before midnight west of Greenwich", () => {
  process.env.TZ = "America/New_York";

  assert.equal(formatDay(JUST_AFTER_MIDNIGHT_UTC, "en"), "11 Mar 2026");
});

test("a given zone decides a moment's date and time", () => {
  process.env.TZ = "Asia/Tokyo";

  assert.equal(formatMoment(LATE_EVENING_UTC, "en", "UTC"), "11 Mar 2026, 23:30");
});

test("without a zone a moment is the runtime's own date and time", () => {
  process.env.TZ = "Asia/Tokyo";

  assert.equal(formatMoment(LATE_EVENING_UTC, "en"), "12 Mar 2026, 08:30");
});

test("a given zone never turns a missing stamp into a date", () => {
  assert.equal(formatDay("0", "en", "UTC"), "—");
  assert.equal(formatMoment(undefined, "en", "UTC"), "—");
});
