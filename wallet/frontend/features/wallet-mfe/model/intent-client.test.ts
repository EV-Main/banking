// Run with `npm run test`. The client runs against a fake fetch and an injected clock.
import assert from "node:assert/strict";
import test from "node:test";

import { INTENT_REFUSALS } from "../lib/intent.ts";
import { CONFIRM_ATTEMPTS, createIntentClient } from "./intent-client.ts";

const BASE = "https://wallet.test/api/wallet";
const ID = "a".repeat(32);
const HASH = "b".repeat(64);
const PROOF = "proof-secret-9f3c";
const REQUEST = { kind: "mandate", params: { request_id: "req1" } } as const;

type Reply = { status: number; body?: unknown } | "throw" | "not_json";

const csrfReply = (token = "tok1"): Reply => ({ status: 200, body: { csrf: token, expires_at: 9_999_999_999 } });
const intentBody = {
  intent_id: ID,
  intent_hash: HASH,
  expires_at: 1_000,
  summary: { amount: "1.5", fee: "0", net: "1.5", recipient_full: "main", network: null, scope: "main", host_origin: "https://h.test", mandate: null },
  stepup: { required: false, methods: [] },
};
const confirmedBody = { intent_id: ID, status: "executed", result: { id: "ref-1" } };

interface Call {
  url: string;
  method: string;
  csrf: string | null;
  body: unknown;
}

function harness(replies: Reply[], opts: { renew?: boolean } = {}) {
  const calls: Call[] = [];
  const queue = [...replies];
  let renewals = 0;
  let clock = 0;
  const fakeFetch = (async (url: string, init: RequestInit) => {
    const headers = (init.headers ?? {}) as Record<string, string>;
    calls.push({
      url: String(url).slice(BASE.length),
      method: String(init.method),
      csrf: headers["x-ev-csrf"] ?? null,
      body: typeof init.body === "string" ? JSON.parse(init.body) : undefined,
    });
    const reply = queue.shift();
    if (reply === undefined) throw new Error("fake fetch: no reply queued");
    if (reply === "throw") throw new TypeError("offline");
    if (reply === "not_json") return { status: 200, json: async () => Promise.reject(new SyntaxError("bad")) };
    return { status: reply.status, json: async () => reply.body };
  }) as unknown as typeof fetch;
  const client = createIntentClient({
    apiBase: BASE,
    fetch: fakeFetch,
    renewSession: async () => {
      renewals++;
      return opts.renew ?? true;
    },
    now: () => clock,
  });
  return {
    client,
    calls,
    renewals: () => renewals,
    advance: (ms: number) => {
      clock += ms;
    },
    pending: () => queue.length,
  };
}

test("every named refusal on create maps to its own code", async () => {
  for (const code of INTENT_REFUSALS) {
    if (code === "csrf") continue; // csrf earns a resend; covered below
    const h = harness([csrfReply(), { status: 409, body: { error: code } }]);
    const result = await h.client.create(REQUEST);
    assert.deepEqual(result, { ok: false, error: { code } }, `refusal ${code}`);
  }
});

test("create returns the parsed intent and sends the csrf token", async () => {
  const h = harness([csrfReply("tokA"), { status: 200, body: intentBody }]);
  const result = await h.client.create(REQUEST);
  assert.ok(result.ok);
  assert.equal(result.value.id, ID);
  assert.deepEqual(h.calls.map((c) => [c.method, c.url, c.csrf]), [["GET", "/csrf", null], ["POST", "/intents", "tokA"]]);
  assert.deepEqual(h.calls[1]?.body, REQUEST);
});

test("a 401 renews the session once and resends the same body", async () => {
  const h = harness([csrfReply(), { status: 401, body: {} }, { status: 200, body: intentBody }]);
  const result = await h.client.create(REQUEST);
  assert.ok(result.ok);
  assert.equal(h.renewals(), 1);
  assert.deepEqual(h.calls[2]?.body, h.calls[1]?.body);
});

test("a 401 after a renewal is unauthenticated, not a loop", async () => {
  const h = harness([csrfReply(), { status: 401 }, { status: 401 }]);
  const result = await h.client.create(REQUEST);
  assert.deepEqual(result, { ok: false, error: { code: "unauthenticated" } });
  assert.equal(h.renewals(), 1);
});

test("a 401 whose renewal says nobody is signed in is signed_out", async () => {
  const h = harness([csrfReply(), { status: 401 }], { renew: false });
  const result = await h.client.create(REQUEST);
  assert.deepEqual(result, { ok: false, error: { code: "signed_out" } });
  assert.equal(h.calls.length, 2);
});

test("a 401 on the csrf fetch renews and fetches again; signed out stops there", async () => {
  const renewed = harness([{ status: 401 }, csrfReply(), { status: 200, body: intentBody }]);
  assert.ok((await renewed.client.create(REQUEST)).ok);
  assert.equal(renewed.renewals(), 1);
  const out = harness([{ status: 401 }], { renew: false });
  assert.deepEqual(await out.client.create(REQUEST), { ok: false, error: { code: "signed_out" } });
});

test("a 403 csrf refetches the token once and resends", async () => {
  const h = harness([csrfReply("old"), { status: 403, body: { error: "csrf" } }, csrfReply("new"), { status: 200, body: intentBody }]);
  const result = await h.client.create(REQUEST);
  assert.ok(result.ok);
  assert.deepEqual(h.calls.map((c) => c.csrf), [null, "old", null, "new"]);
});

test("a second csrf refusal is returned, not retried again", async () => {
  const h = harness([csrfReply(), { status: 403, body: { error: "csrf" } }, csrfReply(), { status: 403, body: { error: "csrf" } }]);
  assert.deepEqual(await h.client.create(REQUEST), { ok: false, error: { code: "csrf" } });
  assert.equal(h.pending(), 0);
});

test("the csrf token is reused for 5 minutes and fetched again after", async () => {
  const h = harness([csrfReply("t1"), { status: 200, body: intentBody }, { status: 200, body: intentBody }, csrfReply("t2"), { status: 200, body: intentBody }]);
  await h.client.create(REQUEST);
  h.advance(5 * 60_000 - 1);
  await h.client.create(REQUEST);
  assert.equal(h.calls.filter((c) => c.url === "/csrf").length, 1);
  h.advance(1);
  await h.client.create(REQUEST);
  assert.deepEqual(h.calls.map((c) => c.csrf), [null, "t1", "t1", null, "t2"]);
});

test("status fallbacks: 415, 429, 404 not_found, unknown 500", async () => {
  const cases: [number, unknown, string][] = [
    [415, {}, "unsupported_media_type"],
    [429, {}, "rate_limited"],
    [404, { error: "not_found" }, "not_enabled"],
    [500, { error: "boom" }, "unavailable"],
  ];
  for (const [status, body, code] of cases) {
    const h = harness([csrfReply(), { status, body }]);
    assert.deepEqual(await h.client.create(REQUEST), { ok: false, error: { code } }, `status ${status}`);
  }
});

test("a 400 with prose names the input field", async () => {
  const cases: [string, string | null][] = [
    ["invalid amount", "amount"],
    ["invalid address", "address"],
    ["unknown network", "network"],
    ["invalid id", "id"],
    ["something else", null],
  ];
  for (const [error, field] of cases) {
    const h = harness([csrfReply(), { status: 400, body: { error } }]);
    assert.deepEqual(await h.client.create(REQUEST), { ok: false, error: { code: "invalid_input", field } }, error);
  }
});

test("a thrown fetch is a network error, on csrf and on the post", async () => {
  assert.deepEqual(await harness(["throw"]).client.create(REQUEST), { ok: false, error: { code: "network" } });
  assert.deepEqual(await harness([csrfReply(), "throw"]).client.create(REQUEST), { ok: false, error: { code: "network" } });
});

test("a 2xx body that is not the contract is malformed", async () => {
  const bad = [
    harness([csrfReply(), { status: 200, body: { intent_id: "short" } }]),
    harness([csrfReply(), "not_json"]),
    harness([{ status: 200, body: { csrf: "t" } }]),
  ];
  for (const h of bad) assert.deepEqual(await h.client.create(REQUEST), { ok: false, error: { code: "malformed" } });
});

test("confirm sends {} without a proof and { proof } with one", async () => {
  const bare = harness([csrfReply(), { status: 200, body: confirmedBody }]);
  const done = await bare.client.confirm(ID);
  assert.deepEqual(done, { ok: true, value: { intentId: ID, reference: "ref-1" } });
  assert.deepEqual(bare.calls[1]?.body, {});
  assert.equal(bare.calls[1]?.url, `/intents/${ID}/confirm`);

  const proven = harness([csrfReply(), { status: 200, body: confirmedBody }]);
  await proven.client.confirm(ID, PROOF);
  assert.deepEqual(proven.calls[1]?.body, { proof: PROOF });
});

test("confirm treats a body that is not executed as malformed", async () => {
  const h = harness([csrfReply(), { status: 200, body: { intent_id: ID, status: "pending" } }]);
  assert.deepEqual(await h.client.confirm(ID), { ok: false, error: { code: "malformed" } });
});

test("intent_executing resends the confirm up to CONFIRM_ATTEMPTS times in total", async () => {
  const executing: Reply = { status: 409, body: { error: "intent_executing" } };
  const h = harness([csrfReply(), ...Array.from({ length: CONFIRM_ATTEMPTS + 2 }, () => executing)]);
  const result = await h.client.confirm(ID, PROOF);
  assert.deepEqual(result, { ok: false, error: { code: "intent_executing" } });
  const posts = h.calls.filter((c) => c.method === "POST");
  assert.equal(posts.length, CONFIRM_ATTEMPTS);
  assert.ok(posts.every((c) => c.url === `/intents/${ID}/confirm` && JSON.stringify(c.body) === JSON.stringify({ proof: PROOF })));
});

test("a confirm that was executing and then lands returns the confirmation", async () => {
  const h = harness([csrfReply(), { status: 409, body: { error: "intent_executing" } }, { status: 200, body: confirmedBody }]);
  assert.ok((await h.client.confirm(ID)).ok);
});

test("a confirm with no answer is resent for the same intent", async () => {
  const h = harness([csrfReply(), "throw", { status: 200, body: confirmedBody }]);
  assert.ok((await h.client.confirm(ID)).ok);
  assert.equal(h.calls.filter((c) => c.method === "POST").length, 2);
});

test("replay and expiry codes are final and sent once", async () => {
  for (const code of ["proof_replayed", "intent_expired", "terms_changed", "intent_not_pending", "stepup_invalid"] as const) {
    const h = harness([csrfReply(), { status: 409, body: { error: code } }]);
    assert.deepEqual(await h.client.confirm(ID, PROOF), { ok: false, error: { code } });
    assert.equal(h.calls.filter((c) => c.method === "POST").length, 1, code);
  }
});

test("the proof never appears in any result, error, or logged output", async () => {
  const logged: string[] = [];
  const methods = ["log", "info", "warn", "error", "debug"] as const;
  const originals = methods.map((m) => console[m]);
  for (const m of methods) console[m] = (...args: unknown[]) => void logged.push(args.map(String).join(" "));
  const seen: string[] = [];
  try {
    const scenarios: Reply[][] = [
      [csrfReply(), { status: 409, body: { error: "proof_replayed" } }],
      [csrfReply(), "throw", "throw", "throw"],
      [csrfReply(), { status: 200, body: { nonsense: true } }],
      [csrfReply(), { status: 500, body: { error: "x" } }],
      [csrfReply(), { status: 200, body: confirmedBody }],
    ];
    for (const replies of scenarios) {
      const h = harness(replies);
      try {
        seen.push(JSON.stringify(await h.client.confirm(ID, PROOF)));
      } catch (error) {
        seen.push(String(error), (error as Error).stack ?? "");
      }
    }
  } finally {
    methods.forEach((m, i) => {
      console[m] = originals[i] as never;
    });
  }
  assert.deepEqual(logged, []);
  assert.ok(seen.every((s) => !s.includes(PROOF)));
});

test("the proof is sent in the body only, never in the url or headers", async () => {
  const h = harness([csrfReply(), { status: 200, body: confirmedBody }]);
  await h.client.confirm(ID, PROOF);
  const post = h.calls[1];
  assert.ok(post && !post.url.includes(PROOF) && post.csrf !== PROOF);
});
