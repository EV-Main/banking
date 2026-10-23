// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// These pin the bug that made the whole feature unusable: the wire never sends null for an
// unanswered seat. It sends the string "pending" (the money plane's own enum member) or an
// empty string, both truthy — so `decision ?? null` read every fresh invitation as already
// settled, hid the vote form, and told an owner opening their email that they had rejected
// a payout they had never seen.
import assert from "node:assert/strict";
import test from "node:test";

import { admissionVote, peerVote, settledAdmission, settledConsent, settledPayout, settledRemoval } from "./decision.ts";

test("an unanswered seat is not a settled answer, however the wire spells it", () => {
  for (const raw of ["pending", "PENDING", "", "   ", null, undefined, "unknown_future_state"]) {
    assert.equal(settledPayout(raw), null, `payout: ${String(raw)}`);
    assert.equal(settledRemoval(raw), null, `removal: ${String(raw)}`);
    assert.equal(peerVote(raw), null, `vote: ${String(raw)}`);
    assert.equal(settledAdmission(raw), null, `admission: ${String(raw)}`);
    assert.equal(admissionVote(raw), null, `admission vote: ${String(raw)}`);
    assert.equal(settledConsent(raw), null, `consent: ${String(raw)}`);
  }
});

test("a payment consent is read in either tense, like a payout", () => {
  assert.equal(settledConsent("approve"), "approve");
  assert.equal(settledConsent("APPROVED"), "approve");
  assert.equal(settledConsent("reject"), "reject");
  assert.equal(settledConsent(" rejected "), "reject");
});

test("a real payout answer is read, in either tense the wire uses", () => {
  assert.equal(settledPayout("approve"), "approve");
  assert.equal(settledPayout("APPROVED"), "approve");
  assert.equal(settledPayout("reject"), "reject");
  assert.equal(settledPayout("rejected"), "reject");
});

test("a removal answer is read in both vocabularies", () => {
  // The public route speaks approve/reject for both endpoints (the BFF maps them onto
  // Remove/Keep); the authenticated peer-vote route speaks remove/keep. A settled removal
  // can reach the client through either, so both spellings have to land on the same answer.
  assert.equal(settledRemoval("remove"), "remove");
  assert.equal(settledRemoval("approve"), "remove");
  assert.equal(settledRemoval("removed"), "remove");
  assert.equal(settledRemoval("keep"), "keep");
  assert.equal(settledRemoval("reject"), "keep");
  assert.equal(settledRemoval("kept"), "keep");
});

test("a peer who has not voted keeps their vote", () => {
  // The failure this guards is quiet and two-sided: the roster labels them as having voted,
  // and the buttons they were entitled to disappear.
  assert.equal(peerVote(""), null);
  assert.equal(peerVote("remove"), "remove");
  assert.equal(peerVote("keep"), "keep");
});

test("an admission answer is read in its own vocabulary", () => {
  assert.equal(settledAdmission("admit"), "admit");
  assert.equal(settledAdmission("ADMIT"), "admit");
  assert.equal(settledAdmission("admitted"), "admit");
  assert.equal(settledAdmission("reject"), "reject");
  assert.equal(settledAdmission("rejected"), "reject");
  assert.equal(admissionVote("admit"), "admit");
});

test("the two governance vocabularies do not leak into one another", () => {
  // The trap this pins is not a rename. `reject` means KEEP THIS OWNER on a removal and
  // REFUSE THIS CANDIDATE on an admission, so the same word read by the wrong reader is a
  // vote inverted, not a label mistranslated. Neither reader may answer for the other's
  // words at all.
  assert.equal(settledRemoval("admit"), null);
  assert.equal(settledAdmission("remove"), null);
  assert.equal(settledAdmission("keep"), null);
  assert.equal(settledAdmission("approve"), null);
  // And the one word they share means opposite things, so it must not round-trip.
  assert.equal(settledRemoval("reject"), "keep");
  assert.equal(settledAdmission("reject"), "reject");
});

test("a non-string is never mistaken for an answer", () => {
  for (const raw of [0, 1, {}, [], true]) {
    assert.equal(settledPayout(raw), null, String(raw));
    assert.equal(settledRemoval(raw), null, String(raw));
    assert.equal(settledAdmission(raw), null, String(raw));
  }
});
