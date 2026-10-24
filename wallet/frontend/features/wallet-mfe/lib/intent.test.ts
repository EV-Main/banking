// Run with `npm run test`.
import assert from "node:assert/strict";
import test from "node:test";

import { INTENT_TTL_MS, intentDeadlineMs, mandateCapRows, parseIntent, secondsLeft, summaryRows, type IntentSummary, type MandateCaps } from "./intent.ts";

const ID = "c".repeat(32);
const HASH = "d".repeat(64);

function body(summary: Record<string, unknown> = {}, over: Record<string, unknown> = {}) {
  return {
    intent_id: ID,
    intent_hash: HASH,
    expires_at: 1_700_000_000,
    summary: { amount: "10.5", fee: "0.5", net: "10", recipient_full: "TXyz", network: "tron", scope: "main", host_origin: "https://h.test", mandate: null, ...summary },
    stepup: { required: true, methods: ["passkey", "bogus", "email_code"] },
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

test("parseIntent accepts a full body and keeps only known step-up methods", () => {
  const intent = parseIntent(body());
  assert.equal(intent?.id, ID);
  assert.equal(intent?.summary.recipient, "TXyz");
  assert.deepEqual(intent?.stepup, { required: true, methods: ["passkey", "email_code"] });
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
