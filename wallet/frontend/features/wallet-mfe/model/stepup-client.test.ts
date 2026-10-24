// Run with `npm run test`. The client runs against a fake fetch.
import assert from "node:assert/strict";
import test from "node:test";

import { createStepupClient, type CeremonyRequest } from "./stepup-client.ts";

const BASE = "https://wallet.test/api/auth/stepup";
const HASH = "b".repeat(64);
const PROOF = "aaa.bbb.ccc";
const SECRET_PROOF = "secretHead.secretBody.secretSig";

type Reply = { status: number; body?: unknown } | "throw" | "not_json";

interface Call {
  url: string;
  init: RequestInit;
  body: unknown;
}

const passkeyCeremony = { ceremony_id: "cer1", method: "passkey", options: { publicKey: { challenge: "ch" } } };
const emailCeremony = { ceremony_id: "cer2", method: "email_code" };
const PASSKEY: CeremonyRequest = { method: "passkey" };
const EMAIL: CeremonyRequest = { method: "email_code", summary: "Pay 1 USDT to Анна from https://h.test", turnstileToken: "ts-token" };

function harness(replies: Reply[], opts: { renew?: boolean } = {}) {
  const calls: Call[] = [];
  const queue = [...replies];
  let renewals = 0;
  const fakeFetch = (async (url: string, init: RequestInit) => {
    calls.push({ url: String(url), init, body: typeof init.body === "string" ? JSON.parse(init.body) : undefined });
    const reply = queue.shift();
    if (reply === undefined) throw new Error("fake fetch: no reply queued");
    if (reply === "throw") throw new TypeError("offline");
    if (reply === "not_json") return { status: 200, json: async () => Promise.reject(new SyntaxError("bad")) };
    return { status: reply.status, json: async () => reply.body };
  }) as unknown as typeof fetch;
  const client = createStepupClient({
    apiBase: BASE,
    fetch: fakeFetch,
    renewSession: async () => {
      renewals++;
      return opts.renew ?? true;
    },
  });
  return { client, calls, renewals: () => renewals, pending: () => queue.length };
}

test("begin for a passkey sends exactly intent_hash and method", async () => {
  const h = harness([{ status: 200, body: passkeyCeremony }]);
  const result = await h.client.begin(HASH, PASSKEY);
  assert.deepEqual(h.calls[0].body, { intent_hash: HASH, method: "passkey" });
  assert.deepEqual(result, { ok: true, value: { method: "passkey", ceremonyId: "cer1", options: { publicKey: { challenge: "ch" } } } });
});

test("begin for an email code sends exactly intent_hash, method, summary, turnstile_token", async () => {
  const h = harness([{ status: 200, body: emailCeremony }]);
  const result = await h.client.begin(HASH, EMAIL);
  assert.deepEqual(h.calls[0].body, { intent_hash: HASH, method: "email_code", summary: EMAIL.method === "email_code" ? EMAIL.summary : "", turnstile_token: "ts-token" });
  assert.deepEqual(result, { ok: true, value: { method: "email_code", ceremonyId: "cer2" } });
});

test("finish sends ceremony_id and response for a passkey assertion", async () => {
  const h = harness([{ status: 200, body: { proof: PROOF } }]);
  const result = await h.client.finish("cer1", { response: { id: "cred", type: "public-key" } });
  assert.deepEqual(h.calls[0].body, { ceremony_id: "cer1", response: { id: "cred", type: "public-key" } });
  assert.deepEqual(result, { ok: true, value: PROOF });
});

test("finish sends ceremony_id and code for an emailed code", async () => {
  const h = harness([{ status: 200, body: { proof: PROOF } }]);
  await h.client.finish("cer2", { code: "123456" });
  assert.deepEqual(h.calls[0].body, { ceremony_id: "cer2", code: "123456" });
});

test("requests post to the step-up routes on the wallet origin", async () => {
  const h = harness([
    { status: 200, body: passkeyCeremony },
    { status: 200, body: { proof: PROOF } },
  ]);
  await h.client.begin(HASH, PASSKEY);
  await h.client.finish("cer1", { code: "1" });
  assert.deepEqual(
    h.calls.map((c) => c.url),
    [`${BASE}/options`, `${BASE}/verify`],
  );
});

test("requests carry only accept and content-type headers, with credentials included and no csrf", async () => {
  const h = harness([
    { status: 200, body: passkeyCeremony },
    { status: 200, body: { proof: PROOF } },
  ]);
  await h.client.begin(HASH, PASSKEY);
  await h.client.finish("cer1", { code: "1" });
  for (const call of h.calls) {
    assert.deepEqual(Object.keys(call.init.headers as object).sort(), ["accept", "content-type"]);
    assert.deepEqual(call.init.headers, { accept: "application/json", "content-type": "application/json" });
    assert.equal(call.init.credentials, "include");
    assert.equal(call.init.method, "POST");
  }
});

test("request bodies use snake_case keys only", async () => {
  const h = harness([
    { status: 200, body: emailCeremony },
    { status: 200, body: { proof: PROOF } },
  ]);
  await h.client.begin(HASH, EMAIL);
  await h.client.finish("cer2", { code: "1" });
  for (const call of h.calls) {
    for (const key of Object.keys(call.body as object)) assert.match(key, /^[a-z]+(_[a-z]+)*$/, key);
  }
});

test("a 401 renews the session once and resends the same request", async () => {
  const h = harness([{ status: 401, body: { error: "unauthorized" } }, { status: 200, body: passkeyCeremony }]);
  const result = await h.client.begin(HASH, PASSKEY);
  assert.equal(result.ok, true);
  assert.equal(h.renewals(), 1);
  assert.equal(h.calls.length, 2);
  assert.deepEqual(h.calls[1].body, h.calls[0].body);
});

test("a 401 with a refused renewal is signed_out and is not resent", async () => {
  const h = harness([{ status: 401 }], { renew: false });
  assert.deepEqual(await h.client.begin(HASH, PASSKEY), { ok: false, error: { code: "signed_out" } });
  assert.equal(h.calls.length, 1);
});

test("a second 401 after renewal is signed_out and renews only once", async () => {
  const h = harness([{ status: 401 }, { status: 401 }]);
  assert.deepEqual(await h.client.finish("c", { code: "1" }), { ok: false, error: { code: "signed_out" } });
  assert.equal(h.renewals(), 1);
  assert.equal(h.calls.length, 2);
});

test("a 429 with no body is throttled", async () => {
  const h = harness([{ status: 429 }]);
  assert.deepEqual(await h.client.begin(HASH, PASSKEY), { ok: false, error: { code: "throttled" } });
});

test("a 404 and an origin refusal are unavailable", async () => {
  for (const reply of [{ status: 404, body: { error: "not_found" } }, { status: 403, body: { error: "origin" } }, { status: 404 }]) {
    const h = harness([reply]);
    assert.deepEqual(await h.client.begin(HASH, PASSKEY), { ok: false, error: { code: "unavailable" } }, JSON.stringify(reply));
  }
});

test("each named refusal maps to its page code", async () => {
  const table: [string, string][] = [
    ["no_passkey", "no_passkey"],
    ["throttled", "throttled"],
    ["passkey_rejected", "passkey_rejected"],
    ["invalid_code", "invalid_code"],
    ["attempts_exceeded", "attempts_exceeded"],
    ["captcha", "captcha"],
    ["passkey_expired", "expired"],
    ["code_expired", "expired"],
  ];
  for (const [wire, code] of table) {
    const h = harness([{ status: 400, body: { error: wire } }]);
    assert.deepEqual(await h.client.finish("c", { code: "1" }), { ok: false, error: { code } }, wire);
  }
});

test("a named refusal wins over the status", async () => {
  const h = harness([{ status: 429, body: { error: "captcha" } }]);
  assert.deepEqual(await h.client.begin(HASH, EMAIL), { ok: false, error: { code: "captcha" } });
});

test("an inherited property name is not a named refusal", async () => {
  const h = harness([{ status: 500, body: { error: "constructor" } }]);
  assert.deepEqual(await h.client.begin(HASH, PASSKEY), { ok: false, error: { code: "unavailable" } });
});

test("a thrown fetch is network and a non-JSON 2xx is malformed", async () => {
  assert.deepEqual(await harness(["throw"]).client.begin(HASH, PASSKEY), { ok: false, error: { code: "network" } });
  assert.deepEqual(await harness(["not_json"]).client.begin(HASH, PASSKEY), { ok: false, error: { code: "malformed" } });
});

test("a ceremony for the wrong method is malformed", async () => {
  const h = harness([{ status: 200, body: emailCeremony }]);
  assert.deepEqual(await h.client.begin(HASH, PASSKEY), { ok: false, error: { code: "malformed" } });
});

test("a proof of the wrong shape is malformed", async () => {
  for (const proof of ["not-a-jwt", "a.b", "a.b.c.d", 5]) {
    const h = harness([{ status: 200, body: { proof } }]);
    assert.deepEqual(await h.client.finish("c", { code: "1" }), { ok: false, error: { code: "malformed" } }, String(proof));
  }
});

// Privacy: the proof authorises one intent and must reach the caller and nothing else.
async function withSilentConsole<T>(run: () => Promise<T>): Promise<{ value: T; logged: string[] }> {
  const logged: string[] = [];
  const methods = ["log", "info", "warn", "error", "debug", "trace"] as const;
  const saved = methods.map((name) => console[name]);
  for (const name of methods) console[name] = (...args: unknown[]) => void logged.push(args.map(String).join(" "));
  try {
    return { value: await run(), logged };
  } finally {
    methods.forEach((name, index) => {
      console[name] = saved[index];
    });
  }
}

test("a successful finish returns the proof alone and logs nothing", async () => {
  const h = harness([{ status: 200, body: { proof: SECRET_PROOF, extra: "ignored" } }]);
  const { value, logged } = await withSilentConsole(() => h.client.finish("c", { code: "1" }));
  assert.deepEqual(value, { ok: true, value: SECRET_PROOF });
  assert.deepEqual(logged, []);
});

test("a refused finish whose body echoes a proof never puts it in the result or the console", async () => {
  const replies: Reply[] = [
    { status: 400, body: { error: "invalid_code", proof: SECRET_PROOF } },
    { status: 500, body: { message: SECRET_PROOF, proof: SECRET_PROOF } },
    { status: 200, body: { proof: `${SECRET_PROOF} with spaces` } },
    { status: 200, body: { proof: `${SECRET_PROOF}.extra.segment` } },
  ];
  for (const reply of replies) {
    const h = harness([reply]);
    const { value, logged } = await withSilentConsole(() => h.client.finish("c", { code: "1" }));
    assert.equal(value.ok, false);
    assert.ok(!JSON.stringify(value).includes("secretHead"), `result leaked for ${JSON.stringify(reply)}`);
    assert.deepEqual(logged, []);
  }
});

test("a failing fetch whose error message holds a proof does not leak it", async () => {
  const fakeFetch = (async () => {
    throw new TypeError(`boom ${SECRET_PROOF}`);
  }) as unknown as typeof fetch;
  const client = createStepupClient({ apiBase: BASE, fetch: fakeFetch, renewSession: async () => true });
  const { value, logged } = await withSilentConsole(() => client.finish("c", { code: "1" }));
  assert.deepEqual(value, { ok: false, error: { code: "network" } });
  assert.deepEqual(logged, []);
});

test("a 401 passkey_rejected on verify is returned at once, with no renewal and one fetch", async () => {
  const h = harness([{ status: 401, body: { error: "passkey_rejected" } }]);
  assert.deepEqual(await h.client.finish("cer1", { response: { id: "cred" } }), { ok: false, error: { code: "passkey_rejected" } });
  assert.equal(h.renewals(), 0);
  assert.equal(h.calls.length, 1);
});

test("a 401 session failure on verify renews once and resends once", async () => {
  const h = harness([{ status: 401, body: { error: "unauthorized" } }, { status: 200, body: { proof: PROOF } }]);
  assert.deepEqual(await h.client.finish("cer1", { response: { id: "cred" } }), { ok: true, value: PROOF });
  assert.equal(h.renewals(), 1);
  assert.equal(h.calls.length, 2);
  assert.deepEqual(h.calls[1].body, h.calls[0].body);
});

test("a resend answered by 401 passkey_rejected is returned at once, with no second renewal", async () => {
  const h = harness([{ status: 401, body: { error: "unauthorized" } }, { status: 401, body: { error: "passkey_rejected" } }]);
  assert.deepEqual(await h.client.finish("cer1", { response: { id: "cred" } }), { ok: false, error: { code: "passkey_rejected" } });
  assert.equal(h.renewals(), 1);
  assert.equal(h.calls.length, 2);
});
