// Run with `npm run test`.
import assert from "node:assert/strict";
import test from "node:test";

import { INTENT_TTL_MS, intentDeadlineMs, isPastDeadline, mandateCapRows, nowMs, parseCsrf, parseIntent, secondsLeft, summaryRows, type IntentSummary, type MandateCaps } from "./intent.ts";

const ID = "c".repeat(32);
const HASH = "d".repeat(64);

function body(summary: Record<string, unknown> = {}, over: Record<string, unknown> = {}) {
  return {
    intent_id: ID,
    intent_hash: HASH,
    expires_at: 1_700_000_000,
    summary: { amount: "10.5", fee: "0.5", net: "10", recipient_full: "TXyz", network: "tron", scope: "main", host_origin: "https://h.test", mandate: null, ...summary },
    min_aal: 2,
    ...over,
  };
}

const mandate = (over: Record<string, unknown> = {}) => ({
  client_id: "cl1",
  client_display_name: "Acme",
  operations: ["MANDATE_OPERATION_RESERVE", "MANDATE_OPERATION_BOOK_FILL", "MANDATE_OPERATION_NEW"],
  cap_per_op: "1",
  cap_24h: "2",
  cap_outstanding: "3",
  cap_book_fill_24h: "4",
  mandate_expires_at: 0,
  ...over,
});

test("parseIntent accepts a full body", () => {
  const intent = parseIntent(body());
  assert.equal(intent?.id, ID);
  assert.equal(intent?.summary.recipient, "TXyz");
  assert.equal(intent?.minAal, 2);
});

test("parseIntent reads min_aal as a positive integer or null, and nothing else", () => {
  assert.equal(parseIntent(body({}, { min_aal: null }))?.minAal, null);
  for (const min_aal of [undefined, 0, -1, 1.5, "2", true]) assert.equal(parseIntent(body({}, { min_aal })), null, String(min_aal));
});

test("parseIntent rejects a bad id or hash", () => {
  assert.equal(parseIntent(body({}, { intent_id: "A".repeat(32) })), null);
  assert.equal(parseIntent(body({}, { intent_id: "c".repeat(31) })), null);
  assert.equal(parseIntent(body({}, { intent_hash: "d".repeat(63) })), null);
  assert.equal(parseIntent(body({}, { intent_hash: 5 })), null);
});

test("parseIntent rejects a bad decimal and a missing recipient", () => {
  for (const amount of ["1e3", "-1", "1.", ".5", "abc", 5]) assert.equal(parseIntent(body({ amount })), null, String(amount));
  assert.equal(parseIntent(body({ recipient_full: undefined })), null);
  assert.equal(parseIntent(body({ recipient_full: "" })), null);
});

test("parseIntent accepts null terms for a mandate", () => {
  const intent = parseIntent(body({ amount: null, fee: null, net: null, mandate: mandate() }));
  assert.equal(intent?.summary.amount, null);
});

test("parseIntent maps MANDATE_OPERATION_* names and keeps unknown ones as written", () => {
  const caps = parseIntent(body({ mandate: mandate() }))?.summary.mandate;
  assert.deepEqual(caps?.operations, ["reserve", "book_fill", "MANDATE_OPERATION_NEW"]);
  assert.equal(caps?.clientName, "Acme");
});

test("parseIntent accepts an empty book-fill cap and rejects a malformed one", () => {
  assert.equal(parseIntent(body({ mandate: mandate({ cap_book_fill_24h: "" }) }))?.summary.mandate?.capBookFill24h, "");
  assert.equal(parseIntent(body({ mandate: mandate({ cap_book_fill_24h: "x" }) })), null);
  assert.equal(parseIntent(body({ mandate: mandate({ cap_book_fill_24h: null }) })), null);
});

const summary = (over: Partial<IntentSummary> = {}): IntentSummary => ({
  amount: "10",
  fee: "1",
  net: "9",
  recipient: "TXyz",
  network: "tron",
  scope: "main",
  hostOrigin: "https://h.test",
  mandate: null,
  ...over,
});

test("summaryRows lists every stated term in reading order", () => {
  assert.deepEqual(summaryRows(summary()).map((r) => r.term), ["amount", "fee", "net", "recipient", "network", "scope", "host"]);
});

test("summaryRows omits null terms and marks only figures as money", () => {
  const rows = summaryRows(summary({ amount: null, fee: null, net: null, network: null }));
  assert.deepEqual(rows, [
    { term: "recipient", value: "TXyz", money: false },
    { term: "scope", value: "main", money: false },
    { term: "host", value: "https://h.test", money: false },
  ]);
});

const caps = (over: Partial<MandateCaps> = {}): MandateCaps => ({
  clientName: "Acme",
  operations: [],
  capPerOp: "1",
  cap24h: "2",
  capOutstanding: "3",
  capBookFill24h: "",
  expiresAt: 0,
  ...over,
});

test("mandateCapRows shows the book-fill cap only when sent", () => {
  assert.deepEqual(mandateCapRows(caps()).map((r) => r.term), ["perOp", "per24h", "outstanding"]);
  assert.deepEqual(mandateCapRows(caps({ capBookFill24h: "4" })).at(-1), { term: "bookFill24h", value: "4" });
});

test("intentDeadlineMs follows the BFF deadline when it is nearer than the TTL", () => {
  assert.equal(intentDeadlineMs(1_060, 1_000_000), 1_000_000 + 60_000);
});

test("intentDeadlineMs caps a far deadline at 120 seconds", () => {
  assert.equal(intentDeadlineMs(1_000_000, 5_000), 5_000 + INTENT_TTL_MS);
  assert.equal(INTENT_TTL_MS, 120_000);
});

test("intentDeadlineMs never lands before now when the deadline has passed", () => {
  assert.equal(intentDeadlineMs(10, 50_000), 50_000);
});

test("secondsLeft rounds up and never goes below zero", () => {
  assert.equal(secondsLeft(10_001, 10_000), 1);
  assert.equal(secondsLeft(12_000, 10_000), 2);
  assert.equal(secondsLeft(10_000, 10_000), 0);
  assert.equal(secondsLeft(5_000, 10_000), 0);
});

const CSRF_BODY = { csrf: "tok1", expires_at: 9_999_999_999 };

test("parseCsrf keeps known kinds in order, drops unknown names and duplicates", () => {
  const body = { ...CSRF_BODY, kinds: ["pay", "teleport", "pay", 7, null, "withdraw", "withdraw"] };
  assert.deepEqual(parseCsrf(body)?.kinds, ["pay", "withdraw"]);
});

test("parseCsrf gives no kinds when kinds is missing or not an array", () => {
  assert.deepEqual(parseCsrf(CSRF_BODY)?.kinds, []);
  assert.deepEqual(parseCsrf({ ...CSRF_BODY, kinds: "move" })?.kinds, []);
  assert.deepEqual(parseCsrf({ ...CSRF_BODY, kinds: { 0: "move" } })?.kinds, []);
});

test("parseCsrf still rejects a bad token or expires_at whatever kinds says", () => {
  const kinds = ["move"];
  assert.equal(parseCsrf({ csrf: 5, expires_at: 9_999_999_999, kinds }), null);
  assert.equal(parseCsrf({ csrf: "", expires_at: 9_999_999_999, kinds }), null);
  assert.equal(parseCsrf({ csrf: "tok1", expires_at: "soon", kinds }), null);
  assert.equal(parseCsrf({ kinds }), null);
  assert.equal(parseCsrf(null), null);
});

test("isPastDeadline is false 1 ms before the deadline", () => {
  assert.equal(isPastDeadline(10_000, 9_999), false);
});

test("isPastDeadline is true at the deadline and after it", () => {
  assert.equal(isPastDeadline(10_000, 10_000), true);
  assert.equal(isPastDeadline(10_000, 10_001), true);
  assert.equal(isPastDeadline(10_000, 99_000), true);
});

test("isPastDeadline agrees with a countdown that shows zero", () => {
  const deadline = 1_000_000;
  const offsets = [-120_001, -120_000, -119_999, -60_000, -1_001, -1_000, -999, -1, 0, 1, 999, 1_000, 1_001, 119_999, 120_000, 120_001];
  for (const offset of offsets) {
    const now = deadline + offset;
    assert.equal(isPastDeadline(deadline, now), secondsLeft(deadline, now) === 0, `offset ${offset}`);
  }
});

test("isPastDeadline flips exactly when the 120 s window ends", () => {
  const start = 5_000;
  const deadline = start + 120_000;
  assert.equal(isPastDeadline(deadline, start), false);
  assert.equal(isPastDeadline(deadline, deadline - 1), false);
  assert.equal(isPastDeadline(deadline, deadline), true);
});

test("nowMs returns the current time in milliseconds", () => {
  const before = Date.now();
  const got = nowMs();
  assert.ok(got >= before && got <= Date.now(), `got ${got}`);
});
