// Run with `npm run test`.
import assert from "node:assert/strict";
import test from "node:test";

import { needsNewIntent, intentCancelled, type IntentError, type IntentKind, type IntentSummary, type StepupMethod } from "./intent.ts";
import { assertionErrorOf, needsFreshIntent, pageMethods, parseCeremony, parseProof, stepupSummary, type StepupError } from "./stepup.ts";

const KINDS: IntentKind[] = ["move", "withdraw", "pay", "mandate"];
const OFFERS: StepupMethod[][] = [["passkey"], ["email_code"], ["passkey", "email_code"], []];
const ABILITIES = [
  { passkey: true, emailCode: true },
  { passkey: true, emailCode: false },
  { passkey: false, emailCode: true },
  { passkey: false, emailCode: false },
];

test("pageMethods never offers email_code for withdraw or mandate", () => {
  for (const kind of ["withdraw", "mandate"] as const) {
    for (const offered of OFFERS) {
      for (const can of ABILITIES) {
        assert.ok(!pageMethods(kind, offered, can).includes("email_code"), `${kind} ${offered} ${JSON.stringify(can)}`);
      }
    }
  }
});

test("pageMethods never returns a method the BFF did not list, nor a password method", () => {
  for (const kind of KINDS) {
    for (const offered of OFFERS) {
      for (const can of ABILITIES) {
        for (const method of pageMethods(kind, offered, can)) {
          assert.ok(offered.includes(method), `${kind} ${offered} ${JSON.stringify(can)} -> ${method}`);
          assert.ok(method === "passkey" || method === "email_code");
        }
      }
    }
  }
});

test("pageMethods returns exactly the expected methods for every kind and ability", () => {
  const both: StepupMethod[] = ["passkey", "email_code"];
  assert.deepEqual(pageMethods("move", both, { passkey: true, emailCode: true }), ["passkey", "email_code"]);
  assert.deepEqual(pageMethods("pay", both, { passkey: true, emailCode: true }), ["passkey", "email_code"]);
  assert.deepEqual(pageMethods("withdraw", both, { passkey: true, emailCode: true }), ["passkey"]);
  assert.deepEqual(pageMethods("mandate", both, { passkey: true, emailCode: true }), ["passkey"]);
  assert.deepEqual(pageMethods("move", both, { passkey: false, emailCode: true }), ["email_code"]);
  assert.deepEqual(pageMethods("move", both, { passkey: true, emailCode: false }), ["passkey"]);
  assert.deepEqual(pageMethods("withdraw", both, { passkey: false, emailCode: true }), []);
  assert.deepEqual(pageMethods("pay", both, { passkey: false, emailCode: false }), []);
  assert.deepEqual(pageMethods("pay", ["email_code"], { passkey: true, emailCode: true }), ["email_code"]);
  assert.deepEqual(pageMethods("pay", ["passkey"], { passkey: true, emailCode: true }), ["passkey"]);
  assert.deepEqual(pageMethods("pay", [], { passkey: true, emailCode: true }), []);
});

test("pageMethods keeps the BFF order", () => {
  assert.deepEqual(pageMethods("move", ["email_code", "passkey"], { passkey: true, emailCode: true }), ["email_code", "passkey"]);
});

// Defect: any non-passkey method is treated as email_code, so a method outside the union passes.
test("pageMethods ignores a password method a hostile BFF slipped in", { todo: "pageMethods lets an unknown method through" }, () => {
  const offered = ["password", "passkey"] as unknown as StepupMethod[];
  const result = pageMethods("move", offered, { passkey: true, emailCode: true });
  assert.ok(!(result as string[]).includes("password"), `got ${JSON.stringify(result)}`);
});

const OPTIONS = { publicKey: { challenge: "abc", allowCredentials: [{ id: "cred1", type: "public-key" }] } };

test("parseCeremony accepts a passkey ceremony and keeps the options as sent", () => {
  const got = parseCeremony({ ceremony_id: "cer_1-A", method: "passkey", options: OPTIONS }, "passkey");
  assert.deepEqual(got, { method: "passkey", ceremonyId: "cer_1-A", options: { publicKey: OPTIONS.publicKey } });
});

test("parseCeremony accepts a passkey ceremony without allowCredentials", () => {
  const got = parseCeremony({ ceremony_id: "c1", method: "passkey", options: { publicKey: { challenge: "x" } } }, "passkey");
  assert.equal(got?.method, "passkey");
});

test("parseCeremony accepts an email_code ceremony without options", () => {
  assert.deepEqual(parseCeremony({ ceremony_id: "c1", method: "email_code" }, "email_code"), { method: "email_code", ceremonyId: "c1" });
});

test("parseCeremony rejects an answer for the wrong method", () => {
  assert.equal(parseCeremony({ ceremony_id: "c1", method: "email_code" }, "passkey"), null);
  assert.equal(parseCeremony({ ceremony_id: "c1", method: "passkey", options: OPTIONS }, "email_code"), null);
});

test("parseCeremony rejects a passkey ceremony without a string challenge", () => {
  assert.equal(parseCeremony({ ceremony_id: "c1", method: "passkey", options: { publicKey: {} } }, "passkey"), null);
  assert.equal(parseCeremony({ ceremony_id: "c1", method: "passkey", options: { publicKey: { challenge: 7 } } }, "passkey"), null);
  assert.equal(parseCeremony({ ceremony_id: "c1", method: "passkey", options: {} }, "passkey"), null);
  assert.equal(parseCeremony({ ceremony_id: "c1", method: "passkey" }, "passkey"), null);
});

test("parseCeremony rejects allowCredentials entries without a string id", () => {
  for (const allowCredentials of [[{ type: "public-key" }], [{ id: 5 }], [null], ["cred"], "cred", [{ id: "ok" }, {}]]) {
    const body = { ceremony_id: "c1", method: "passkey", options: { publicKey: { challenge: "x", allowCredentials } } };
    assert.equal(parseCeremony(body, "passkey"), null, JSON.stringify(allowCredentials));
  }
});

test("parseCeremony rejects a bad ceremony id", () => {
  for (const id of [undefined, 5, "", "a b", "a".repeat(129), "идент", "a/b"]) {
    assert.equal(parseCeremony({ ceremony_id: id, method: "email_code" }, "email_code"), null, String(id));
  }
  assert.notEqual(parseCeremony({ ceremony_id: "a".repeat(128), method: "email_code" }, "email_code"), null);
});

test("parseCeremony rejects a body that is not an object", () => {
  for (const body of [null, undefined, "x", 3, []]) assert.equal(parseCeremony(body, "email_code"), null);
});

test("parseProof accepts exactly three base64url segments", () => {
  assert.equal(parseProof({ proof: "aB-_1.cD-_2.eF-_3" }), "aB-_1.cD-_2.eF-_3");
});

test("parseProof rejects other shapes", () => {
  for (const proof of ["a.b", "a.b.c.d", "a..c", ".b.c", "a.b.", "a.b.c=", "a b.c.d", "a.b.c\n", "а.б.в", "", 7, null, undefined]) {
    assert.equal(parseProof({ proof }), null, JSON.stringify(proof));
  }
  assert.equal(parseProof(null), null);
  assert.equal(parseProof("a.b.c"), null);
});

test("parseProof enforces the 4096 character limit", () => {
  const exact = `a.b.${"c".repeat(4092)}`;
  assert.equal(exact.length, 4096);
  assert.equal(parseProof({ proof: exact }), exact);
  assert.equal(parseProof({ proof: `${exact}c` }), null);
});

test("assertionErrorOf maps a closed or timed-out prompt to cancelled", () => {
  assert.deepEqual(assertionErrorOf("NotAllowedError"), { code: "cancelled" });
  assert.deepEqual(assertionErrorOf("AbortError"), { code: "cancelled" });
});

test("assertionErrorOf maps every other failure to authenticator_failed", () => {
  for (const name of ["SecurityError", "InvalidStateError", "NotSupportedError", "TypeError", "", "notallowederror"]) {
    assert.deepEqual(assertionErrorOf(name), { code: "authenticator_failed" }, name);
  }
});

test("needsFreshIntent is true only for the final step-up codes", () => {
  const final = ["expired", "throttled", "no_passkey", "attempts_exceeded"];
  const retryable = ["passkey_rejected", "invalid_code", "captcha", "cancelled", "authenticator_failed", "signed_out", "unavailable", "network", "malformed"];
  for (const code of final) assert.equal(needsFreshIntent({ code } as StepupError), true, code);
  for (const code of retryable) assert.equal(needsFreshIntent({ code } as StepupError), false, code);
});

test("intentCancelled is true for a spent proof and the 24 h limits", () => {
  for (const code of ["proof_replayed", "stepup_invalid", "limit_reached", "email_limit_reached"]) {
    assert.equal(intentCancelled({ code } as IntentError), true, code);
  }
});

test("intentCancelled is false for an expired intent and changed terms", () => {
  for (const code of ["intent_expired", "terms_changed", "intent_executing", "proof_required", "network"]) {
    assert.equal(intentCancelled({ code } as IntentError), false, code);
  }
});

test("needsNewIntent is true for intent_expired and terms_changed but not for a spent proof", () => {
  for (const code of ["intent_expired", "terms_changed", "intent_not_found", "session_changed"]) {
    assert.equal(needsNewIntent({ code } as IntentError), true, code);
  }
  for (const code of ["proof_replayed", "stepup_invalid", "limit_reached", "email_limit_reached"]) {
    assert.equal(needsNewIntent({ code } as IntentError), false, code);
  }
});

const summary = (over: Partial<IntentSummary> = {}): IntentSummary => ({
  amount: "10.5",
  fee: "0.5",
  net: "10",
  recipient: "TXyz",
  network: "tron",
  scope: "main",
  hostOrigin: "https://h.test",
  mandate: null,
  ...over,
});

test("stepupSummary builds one English line from the summary terms", () => {
  assert.equal(stepupSummary("pay", summary()), "Pay 10.5 USDT to TXyz on tron (recipient gets 10 USDT) from https://h.test");
  assert.equal(stepupSummary("move", summary({ network: null, net: "10.5" })), "Move 10.5 USDT to TXyz from https://h.test");
  assert.equal(stepupSummary("mandate", summary({ amount: null, net: null, network: null })), "Allow to TXyz from https://h.test");
});

test("stepupSummary uses only summary fields, never fee, scope, or mandate text", () => {
  const injected = "INJECTED-SCOPE-9z";
  const line = stepupSummary(
    "pay",
    summary({
      fee: "FEE-777",
      scope: injected,
      mandate: { clientName: "MANDATE-NAME", operations: ["OPS-X"], capPerOp: "1", cap24h: "2", capOutstanding: "3", capBookFill24h: "", expiresAt: 0 },
    }),
  );
  for (const text of [injected, "FEE-777", "MANDATE-NAME", "OPS-X"]) assert.ok(!line.includes(text), `${text} leaked into ${line}`);
});

// Invisible characters are built from code points so the test source stays readable.
const chars = (...points: number[]) => String.fromCodePoint(...points);
const CONTROL_OR_BIDI = new RegExp("[\\u0000-\\u001F\\u007F-\\u009F\\u200B-\\u200F\\u202A-\\u202E\\u2060-\\u2069\\uFEFF]", "u");

test("stepupSummary strips control and bidi characters from a Cyrillic payee", () => {
  const payee = `${chars(0x202e)}Иван${chars(0x202c, 0x200f)} Петров${chars(0x2066, 0x2069, 0x0000, 0x001b, 0x0085, 0x200b)}`;
  const line = stepupSummary("pay", summary({ recipient: payee }));
  assert.equal(line, "Pay 10.5 USDT to Иван Петров on tron (recipient gets 10 USDT) from https://h.test");
  assert.doesNotMatch(line, CONTROL_OR_BIDI);
});

test("stepupSummary collapses whitespace including line separators", () => {
  const line = stepupSummary("pay", summary({ recipient: `Анна \n\t  Мария${chars(0x2028)}К${chars(0x00a0, 0x3000)}Ж` }));
  assert.ok(line.includes("to Анна Мария К Ж on"), line);
});

test("stepupSummary cuts to 240 characters", () => {
  const line = stepupSummary("pay", summary({ recipient: "Я".repeat(512) }));
  assert.equal(line.length, 240);
  assert.ok(line.endsWith("…"));
  const filler = "Я".repeat(240 - "Move to  from h".length);
  const exact = stepupSummary("move", summary({ amount: null, net: null, network: null, hostOrigin: "h", recipient: filler }));
  assert.equal(exact.length, 240);
  assert.ok(!exact.endsWith("…"));
});

test("stepupSummary strips each bidi control character on its own", () => {
  for (const point of [0x200e, 0x200f, 0x202a, 0x202b, 0x202c, 0x202d, 0x202e, 0x2066, 0x2067, 0x2068, 0x2069]) {
    const line = stepupSummary("pay", summary({ recipient: `a${chars(point)}b` }));
    assert.ok(line.includes("to ab on"), `U+${point.toString(16)} survived: ${JSON.stringify(line)}`);
  }
});

// U+061C ARABIC LETTER MARK is a bidi control, but the strip regex does not list it.
test("stepupSummary strips U+061C ARABIC LETTER MARK", { todo: "UNPRINTABLE omits U+061C" }, () => {
  const line = stepupSummary("pay", summary({ recipient: `a${chars(0x061c)}b` }));
  assert.ok(line.includes("to ab on"), `survived: ${JSON.stringify(line)}`);
});
