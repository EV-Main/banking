// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { capRows, endsWhenRevoked, isRequestAlive, isUnavailable, keepsKeyAfter, replacedBy } from "./request.ts";

test("a request lives until its expires_at and not a second past it", () => {
  assert.equal(isRequestAlive({ expires_at: "101" }, 100), true);
  assert.equal(isRequestAlive({ expires_at: "100" }, 100), false);
  assert.equal(isRequestAlive({ expires_at: "" }, 100), false);
  assert.equal(isRequestAlive({ expires_at: "soon" }, 100), false);
});

const caps = { cap_per_op: "5.00", cap_24h: "50", cap_outstanding: "20.5", cap_book_fill_24h: "" };

test("caps come out verbatim and the book-fill cap only when the server sent one", () => {
  assert.deepEqual(capRows(caps), [
    { kind: "perOp", value: "5.00" },
    { kind: "per24h", value: "50" },
    { kind: "outstanding", value: "20.5" },
  ]);
  assert.deepEqual(capRows({ ...caps, cap_book_fill_24h: "7" }).at(-1), { kind: "bookFill24h", value: "7" });
});

test("only the stamp 0 means until revoked", () => {
  assert.equal(endsWhenRevoked("0"), true);
  assert.equal(endsWhenRevoked("1790000000"), false);
});

const held = { scope: "sa", client_id: "svc", revoked_at: "0", replaced_at: "0", expires_at: "0" };

test("a grant replaces only a live mandate of the same service and scope", () => {
  const req = { scope: "sa", client_id: "svc" };
  assert.equal(replacedBy(req, [held], 100), held);
  assert.equal(replacedBy(req, [{ ...held, scope: "invest" }], 100), null);
  assert.equal(replacedBy(req, [{ ...held, client_id: "other" }], 100), null);
  assert.equal(replacedBy(req, [{ ...held, revoked_at: "5" }], 100), null);
  assert.equal(replacedBy(req, [{ ...held, replaced_at: "5" }], 100), null);
  assert.equal(replacedBy(req, [{ ...held, expires_at: "100" }], 100), null);
  const later = { ...held, expires_at: "101" };
  assert.equal(replacedBy(req, [later], 100), later);
});

test("the idempotency key survives a lost answer and not the server's refusal", () => {
  assert.equal(keepsKeyAfter({ status: 400 }), false);
  assert.equal(keepsKeyAfter({ status: 401 }), false);
  assert.equal(keepsKeyAfter({ status: 404 }), false);
  assert.equal(keepsKeyAfter({ status: 502 }), true);
  assert.equal(keepsKeyAfter(new TypeError("network")), true);
  assert.equal(keepsKeyAfter("boom"), true);
});

test("a 404 reads as an unavailable request", () => {
  assert.equal(isUnavailable({ status: 404 }), true);
  assert.equal(isUnavailable({ status: 500 }), false);
  assert.equal(isUnavailable(null), false);
});
