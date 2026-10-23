// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The cabinet half of a two-sided contract: concierge's `runner/tests/kyc.rs` pins the exact
// JSON documents these functions are given here. A rename on either side turns one of these
// red instead of turning into "please try again" in front of a healthy provider (#193).
import assert from "node:assert/strict";
import test from "node:test";

import { KYC_ERROR_CODES, parseErrorBody, parseStartResponse, parseStatus } from "./kyc-contract.ts";

test("the pinned start answer parses", () => {
  const parsed = parseStartResponse({ redirect_url: "https://verify.didit.me/s/abc", case_id: "c-1" });
  assert.deepEqual(parsed, { redirectUrl: "https://verify.didit.me/s/abc", caseId: "c-1" });
});

test("a start answer without a redirect is not a start answer", () => {
  assert.equal(parseStartResponse({ case_id: "c-1" }), null);
  assert.equal(parseStartResponse({ redirect_url: "" }), null);
  assert.equal(parseStartResponse({ redirect_url: null }), null);
  assert.equal(parseStartResponse({ redirectUrl: "https://verify.didit.me/s/abc" }), null, "a renamed field must not parse");
  assert.equal(parseStartResponse(null), null);
});

test("the pinned status answers parse", () => {
  assert.deepEqual(parseStatus({ level: 0, case: null }), { level: 0, case: null });
  assert.deepEqual(parseStatus({ level: 1, case: null }), { level: 1, case: null });
  assert.deepEqual(
    parseStatus({ level: 0, case: { status: "pending", requested_tier: 1, created_at: 1789459200, resumable: true } }),
    { level: 0, case: { status: "pending", requestedTier: 1, createdAt: 1789459200, resumable: true } },
  );
});

test("created_at is read as a number of seconds, never as a string", () => {
  assert.equal(parseStatus({ level: 0, case: { status: "pending", requested_tier: 1, created_at: "1789459200", resumable: true } }), null);
});

test("resumable false is carried, not defaulted away", () => {
  const parsed = parseStatus({ level: 0, case: { status: "in_review", requested_tier: 1, created_at: 1789459200, resumable: false } });
  assert.equal(parsed?.case?.resumable, false);
});

test("a case missing a field is drift, and takes the whole document down", () => {
  // Deliberate: a half-read case would be a half-read gate, and the caller's fallback for an
  // unreadable status (offer Start on tier 0) is the one we already ship.
  assert.equal(parseStatus({ level: 0, case: { status: "pending", requested_tier: 1, created_at: 1789459200 } }), null);
  assert.equal(parseStatus({ level: 0, case: {} }), null);
});

test("an unrecognised running status still counts as a running case", () => {
  // The status column is a persistent dictionary that can grow a value before this cabinet
  // ships again; refusing the word would re-open the second-paid-case hole (#190).
  const parsed = parseStatus({ level: 0, case: { status: "on_hold", requested_tier: 1, created_at: 1789459200, resumable: true } });
  assert.equal(parsed?.case?.status, "on_hold");
});

test("a body that is not a status document is refused", () => {
  assert.equal(parseStatus({ case: null }), null, "no level");
  assert.equal(parseStatus({ level: "0", case: null }), null, "level as a string");
  assert.equal(parseStatus({ level: 0 }), null, "no case key at all");
  assert.equal(parseStatus({ error: "unauthenticated" }), null);
  assert.equal(parseStatus(null), null);
});

test("every published error code parses, and only those", () => {
  for (const code of KYC_ERROR_CODES) {
    assert.deepEqual(parseErrorBody({ error: code }), { error: code, contact: null }, code);
  }
  assert.equal(parseErrorBody({ error: "not_a_code" }), null);
  assert.equal(parseErrorBody({}), null);
  assert.equal(parseErrorBody("csrf"), null);
});

test("contact rides on kyc_unavailable and on nothing else", () => {
  assert.deepEqual(parseErrorBody({ error: "kyc_unavailable", contact: "support@ev.invest" }), {
    error: "kyc_unavailable",
    contact: "support@ev.invest",
  });
  assert.deepEqual(parseErrorBody({ error: "kyc_unavailable" }), { error: "kyc_unavailable", contact: null });
  assert.deepEqual(parseErrorBody({ error: "internal", contact: "support@ev.invest" }), { error: "internal", contact: null });
});

test("a `case` that is present but not an object is drift, not 'nothing running'", () => {
  // The dangerous direction: reading these as `{ case: null }` is an AFFIRMATIVE "no attempt
  // is running", which re-opens the start gate and buys a second billed vendor session — and
  // does it silently, which is the opposite of what pinning the shape is for.
  for (const malformed of [5, "pending", true, [{ status: "pending" }], []]) {
    assert.equal(parseStatus({ level: 0, case: malformed }), null, JSON.stringify(malformed));
  }
});

test("a contact that is not an address is dropped", () => {
  // It is the one field of these bodies the cabinet puts back into a URL (`mailto:`), so
  // query syntax in it would turn "write to support" into a letter addressed elsewhere.
  for (const contact of ["kyc@evinvest.ltd?cc=attacker@evil.example", "kyc@evinvest.ltd&body=x", "https://evil.example", "plain text", "a@b"]) {
    assert.deepEqual(parseErrorBody({ error: "kyc_unavailable", contact }), { error: "kyc_unavailable", contact: null }, contact);
  }
});
