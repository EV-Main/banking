// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The one predicate that decides whether a paid vendor session can be bought (#190), and the
// tier gate the money screens share with it.
import assert from "node:assert/strict";
import test from "node:test";

import type { UserProfile } from "../../../shared/contracts/index.ts";
import { canStartVerification, isUnverified, kycLevel } from "./kyc.ts";

const profile = (kyc_level?: number) => ({ kyc_level }) as UserProfile;

test("a missing tier reads as 0, because proto3 omits a zero scalar", () => {
  assert.equal(kycLevel(profile()), 0);
  assert.equal(isUnverified(profile()), true);
  assert.equal(isUnverified(profile(0)), true);
});

test("an absent profile is not an unverified one", () => {
  // Nothing is known about that caller's tier yet; guessing would hide a deposit address
  // from someone entitled to it.
  assert.equal(isUnverified(null), false);
  assert.equal(isUnverified(undefined), false);
  assert.equal(kycLevel(null), 0);
});

test("any tier above 0 is verified", () => {
  assert.equal(isUnverified(profile(1)), false);
  assert.equal(isUnverified(profile(3)), false);
});

test("with no case running, tier 0 may start and nobody else may", () => {
  assert.equal(canStartVerification(0, null), true);
  assert.equal(canStartVerification(1, null), false);
  assert.equal(canStartVerification(2, null), false);
});

test("a resumable case is a continuation, not a second case", () => {
  // A repeat start returns the SAME vendor session (concierge#55), so refusing here would
  // only strand a user who closed the vendor tab.
  assert.equal(canStartVerification(0, { resumable: true }), true);
});

test("a case with nowhere to return to refuses the start", () => {
  // It still holds the start gate on the plane, so the button could only fail — the screen
  // owes that user a sentence instead.
  assert.equal(canStartVerification(0, { resumable: false }), false);
});

test("a running case never re-opens the start above the entry tier", () => {
  assert.equal(canStartVerification(1, { resumable: true }), false);
});
