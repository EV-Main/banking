// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The timeline is rendered on the server and hydrated in the browser, so the day a row is
// grouped under and the clock time it shows must come out the same in both — which they do
// only when both format in the zone handed over by `useTimeZone`. These pin the one place
// the zone visibly matters: an event that is yesterday in UTC and already today in Vietnam.
//
// Without a zone the helpers keep their old meaning, the runtime's own zone. This file
// pins the runtime to Vietnam BEFORE anything formats, because the day helper caches one
// formatter per zone and a formatter keeps the zone it was built in.
import assert from "node:assert/strict";
import test from "node:test";

import type { Translate } from "@evinvest/i18n";

import { installModuleHooks } from "../../../shared/__tests__/react-hook-harness.ts";

process.env.TZ = "Asia/Ho_Chi_Minh";
installModuleHooks();

const { dayLabel, dayLabelInline, timeLabel } = await import("./format.ts");

/** The catalogue's English: every label resolves to the default it was written with. */
const t = ((_key: string, fallback: string) => fallback) as Translate;

// 2026-10-08T01:30:00Z — 08:30 on 8 October in Vietnam (UTC+7).
const NOW = new Date("2026-10-08T01:30:00Z");
// 2026-10-07T20:00:00Z — still 7 October in UTC, already 03:00 on 8 October in Vietnam.
const LATE_EVENING_UTC = 1791403200;
// 2026-10-05T20:00:00Z — 5 October in UTC, 6 October in Vietnam.
const THREE_DAYS_BACK_UTC = 1791230400;

test("an event from late last evening in UTC is headed Yesterday in UTC", () => {
  assert.equal(dayLabel(LATE_EVENING_UTC, t, "en", NOW, "UTC"), "Yesterday");
});

test("the same event is headed Today for a reader in Vietnam", () => {
  assert.equal(dayLabel(LATE_EVENING_UTC, t, "en", NOW, "Asia/Ho_Chi_Minh"), "Today");
});

test("without a zone the heading follows the runtime's own zone", () => {
  assert.equal(dayLabel(LATE_EVENING_UTC, t, "en", NOW), "Today");
});

test("an older event is headed by its calendar date in the given zone", () => {
  assert.equal(dayLabel(THREE_DAYS_BACK_UTC, t, "en", NOW, "UTC"), "5 Oct 2026");
  assert.equal(dayLabel(THREE_DAYS_BACK_UTC, t, "en", NOW, "Asia/Ho_Chi_Minh"), "6 Oct 2026");
});

test("an inline day reads yesterday in UTC for a late-evening event", () => {
  assert.equal(dayLabelInline(LATE_EVENING_UTC, t, "en", NOW, "UTC"), "yesterday");
});

test("an inline day reads today in Vietnam for the same event", () => {
  assert.equal(dayLabelInline(LATE_EVENING_UTC, t, "en", NOW, "Asia/Ho_Chi_Minh"), "today");
});

test("without a zone the inline day follows the runtime's own zone", () => {
  assert.equal(dayLabelInline(LATE_EVENING_UTC, t, "en", NOW), "today");
});

test("an older inline day is its calendar date in the given zone", () => {
  assert.equal(dayLabelInline(THREE_DAYS_BACK_UTC, t, "en", NOW, "UTC"), "5 Oct 2026");
  assert.equal(dayLabelInline(THREE_DAYS_BACK_UTC, t, "en", NOW, "Asia/Ho_Chi_Minh"), "6 Oct 2026");
});

test("a row's clock time is the given zone's", () => {
  assert.equal(timeLabel(LATE_EVENING_UTC, "en", "UTC"), "20:00");
  assert.equal(timeLabel(LATE_EVENING_UTC, "en", "Asia/Ho_Chi_Minh"), "03:00");
});

test("without a zone a row's clock time is the runtime's own", () => {
  assert.equal(timeLabel(LATE_EVENING_UTC, "en"), "03:00");
});

test("a missing stamp is undated whatever the zone", () => {
  assert.equal(dayLabel(0, t, "en", NOW, "UTC"), "Undated");
  assert.equal(dayLabelInline(0, t, "en", NOW, "UTC"), "Undated");
  assert.equal(timeLabel(0, "en", "UTC"), "—");
});
