// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The branch that decides what a reader is told when a start is refused. It is the only
// behaviour in this slice that a release of the identity plane can change underneath it, and
// it lived inside `./kyc-client` — behind the `@/` alias, where this runner cannot reach —
// until it was split out.
import assert from "node:assert/strict";
import test from "node:test";

import { classifyRefusal } from "./start-outcome.ts";

test("the plane's published codes decide, whatever the status says", () => {
  assert.deepEqual(classifyRefusal(503, { error: "kyc_unavailable", contact: "kyc@evinvest.ltd" }), {
    kind: "unavailable",
    contact: "kyc@evinvest.ltd",
  });
  // The status is corroboration, not evidence: the same code on a 500 is the same outcome.
  assert.deepEqual(classifyRefusal(500, { error: "kyc_unavailable" }), { kind: "unavailable", contact: null });
  assert.deepEqual(classifyRefusal(429, { error: "throttled" }), { kind: "throttled" });
  assert.deepEqual(classifyRefusal(403, { error: "csrf" }), { kind: "stale" });
  assert.deepEqual(classifyRefusal(403, { error: "email_unverified" }), { kind: "emailUnverified" });
});

test("a contact that is not an address degrades to null rather than into a mailto:", () => {
  // `mailto:${contact}` is what the screen builds out of this. Query syntax in it turns
  // "write to support" into a pre-addressed letter to someone else.
  for (const contact of ["kyc@evinvest.ltd?cc=attacker@evil.example&subject=hi", "not an address", "", "a@b"]) {
    assert.deepEqual(classifyRefusal(503, { error: "kyc_unavailable", contact }), { kind: "unavailable", contact: null }, contact);
  }
});

test("codes the transport already words are left to it", () => {
  // `internal` → err.serverUnavailable and `unauthenticated` → err.unauthenticated are
  // entries in `shared/lib/api-client`'s FRIENDLY table; re-keying them here would be a
  // second place the sentence is written.
  assert.deepEqual(classifyRefusal(500, { error: "internal" }), { kind: "plain" });
  assert.deepEqual(classifyRefusal(401, { error: "unauthenticated" }), { kind: "plain" });
});

test("a code outside the dictionary is drift, and is not guessed at", () => {
  assert.deepEqual(classifyRefusal(500, { error: "kyc_unavailable_v2" }), { kind: "plain" });
  assert.deepEqual(classifyRefusal(429, { error: "rate_limited" }), { kind: "throttled" }, "…but the status still speaks");
});

test("the status-only fallback is the path against the plane that is deployed today", () => {
  // Every refusal but `kyc_unavailable` is still plain text with no JSON body until
  // concierge#76 ships, so these branches are load-bearing, not legacy.
  assert.deepEqual(classifyRefusal(429, {}), { kind: "throttled" });
  assert.deepEqual(classifyRefusal(403, {}), { kind: "stale" });
  assert.deepEqual(classifyRefusal(401, {}), { kind: "plain" });
  assert.deepEqual(classifyRefusal(500, {}), { kind: "plain" });
});

test("a 403 that names a reason is a refusal on the merits, not a stale token", () => {
  // Telling that reader the page went stale sends them reloading forever instead of to
  // support — the heuristic this rule replaced.
  assert.deepEqual(classifyRefusal(403, { error: "account suspended" }), { kind: "plain" });
  assert.deepEqual(classifyRefusal(403, "csrf check failed"), { kind: "stale" }, "plain text carries no `error` field");
});
