// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The contract these cases lock down is what kept the cabinet showing
// `unauthenticated` after an idle spell: the short-TTL access cookie is only re-set by a
// round-trip to the shell session endpoint, so the refresh has to coalesce (one request
// per page, not one per consumer), has to happen again once the session goes stale, and
// must never report "signed out" for an answer it didn't actually get.
import assert from "node:assert/strict";
import test, { beforeEach } from "node:test";

import { STALE_MS, cachedSession, onSessionChange, readSession, refreshIfStale, refreshSession, resetSessionForTests, sessionGeneration } from "./session.ts";

interface FakeFetch {
  calls: number;
  respond: (body: unknown, status?: number) => void;
  fail: () => void;
}

function fakeFetch(): FakeFetch {
  const state = { calls: 0, body: { authenticated: true, user: { email: "a@b.c" } } as unknown, status: 200, throws: false };
  globalThis.fetch = (async () => {
    state.calls += 1;
    if (state.throws) throw new TypeError("network down");
    return { ok: state.status < 400, status: state.status, json: async () => state.body } as Response;
  }) as typeof fetch;
  return {
    get calls() {
      return state.calls;
    },
    respond(body, status = 200) {
      state.body = body;
      state.status = status;
      state.throws = false;
    },
    fail() {
      state.throws = true;
    },
  } as FakeFetch;
}

let http: FakeFetch;
beforeEach(() => {
  resetSessionForTests();
  http = fakeFetch();
});

test("concurrent refreshes coalesce into one request", async () => {
  const [a, b, c] = await Promise.all([refreshSession(), refreshSession(), refreshSession()]);
  assert.equal(http.calls, 1);
  assert.deepEqual(a, b);
  assert.deepEqual(b, c);
  assert.equal(a?.authenticated, true);
  assert.equal(sessionGeneration(), 1);
});

test("readSession serves the cache, refreshSession always rotates the cookie", async () => {
  await readSession();
  await readSession();
  assert.equal(http.calls, 1, "a cached principal needs no second request");
  await refreshSession();
  assert.equal(http.calls, 2, "an explicit refresh is what re-sets the access cookie");
});

test("a stale session refreshes, a fresh one does not", async () => {
  await refreshIfStale();
  assert.equal(http.calls, 1, "nothing has confirmed the session yet on a cold page");
  await refreshIfStale();
  assert.equal(http.calls, 1, "just confirmed — no round-trip");

  const realNow = Date.now;
  Date.now = () => realNow() + STALE_MS + 1;
  try {
    await refreshIfStale();
  } finally {
    Date.now = realNow;
  }
  assert.equal(http.calls, 2, "past the TTL window the cookie is rotated again");
});

test("an unreachable session endpoint is UNKNOWN, never signed out", async () => {
  http.fail();
  assert.equal(await refreshSession(), null);
  assert.equal(cachedSession(), null, "a blip must not be cached as a principal");
  assert.equal(sessionGeneration(), 0, "no shell answer, no generation bump");

  http.respond({ error: "session store unavailable" }, 500);
  assert.equal(await refreshSession(), null);
  assert.equal(sessionGeneration(), 0);
});

test("a genuine signed-out answer is distinguishable from unavailable", async () => {
  http.respond({ authenticated: false });
  const session = await refreshSession();
  assert.deepEqual(session, { authenticated: false });
  assert.equal(sessionGeneration(), 1);
});

test("subscribers see every shell answer", async () => {
  const seen: boolean[] = [];
  const stop = onSessionChange((s) => seen.push(s.authenticated));
  await refreshSession();
  http.respond({ authenticated: false });
  await refreshSession();
  stop();
  await refreshSession();
  assert.deepEqual(seen, [true, false], "unsubscribed listeners stop hearing");
});
