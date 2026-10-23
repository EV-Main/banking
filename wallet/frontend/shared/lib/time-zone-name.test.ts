// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The `ev_tz` cookie is written by the browser but anyone can write it, and the server
// hands its value straight to `Intl` for every date on the page — where an unknown zone
// throws a RangeError instead of formatting. `isTimeZone` is the one gate in between.
import assert from "node:assert/strict";
import test from "node:test";

import { isTimeZone } from "./time-zone-name.ts";

test("UTC is a time zone", () => {
  assert.equal(isTimeZone("UTC"), true);
});

test("a region/city IANA name is a time zone", () => {
  assert.equal(isTimeZone("Asia/Saigon"), true);
});

test("a three-part IANA name is a time zone", () => {
  assert.equal(isTimeZone("America/Argentina/Buenos_Aires"), true);
});

test("an absent cookie is not a time zone", () => {
  assert.equal(isTimeZone(undefined), false);
});

test("an empty cookie is not a time zone", () => {
  assert.equal(isTimeZone(""), false);
});

test("a well-formed name Intl does not know is not a time zone", () => {
  assert.equal(isTimeZone("Foo/Bar"), false);
});

test("a name longer than 64 characters is not a time zone", () => {
  assert.equal(isTimeZone(`Asia/${"A".repeat(60)}`), false);
});

test("a fixed UTC offset Intl would take is still not an IANA name", () => {
  assert.equal(isTimeZone("+07:00"), false);
});

test("a value carrying a cookie separator is not a time zone", () => {
  assert.equal(isTimeZone("UTC;path=/"), false);
});
