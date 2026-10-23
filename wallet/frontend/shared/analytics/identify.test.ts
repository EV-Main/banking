// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { createIdentifier, type IdentifyClient } from "./identify.ts";

/** A posthog-like whose `init` the "provider" runs later, by flipping `loaded`. */
function fake() {
  const calls: string[] = [];
  const client: IdentifyClient = {
    __loaded: false,
    identify: (id) => {
      calls.push(`identify:${id}`);
    },
    reset: () => {
      calls.push("reset");
    },
  };
  return { client, calls };
}

/** A clock that only moves when the identifier sleeps, so timeouts are deterministic. */
function harness(client: IdentifyClient, { loadedAfterPolls = 0, timeoutMs = 1_000, enabled = true } = {}) {
  let clock = 0;
  let polls = 0;
  let loads = 0;
  const identifier = createIdentifier({
    enabled: () => enabled,
    load: () => {
      loads += 1;
      return Promise.resolve(client);
    },
    pollMs: 10,
    timeoutMs,
    now: () => clock,
    wait: (ms) => {
      clock += ms;
      polls += 1;
      if (polls >= loadedAfterPolls) client.__loaded = true;
      return Promise.resolve();
    },
  });
  return { identifier, polls: () => polls, loads: () => loads };
}

test("identify waits for the provider's init before calling the SDK", async () => {
  const { client, calls } = fake();
  const h = harness(client, { loadedAfterPolls: 3 });
  const done = await h.identifier.identify("u-1");
  assert.equal(done, true);
  assert.deepEqual(calls, ["identify:u-1"]);
  assert.equal(h.polls(), 3, "identify did not fire until __loaded flipped");
});

test("the same id is identified once, however many renders ask", async () => {
  const { client, calls } = fake();
  client.__loaded = true;
  const h = harness(client);
  const results = await Promise.all([h.identifier.identify("u-1"), h.identifier.identify("u-1"), h.identifier.identify("u-1")]);
  assert.deepEqual(results, [true, true, true], "concurrent callers share the one attempt");
  assert.equal(await h.identifier.identify("u-1"), false);
  assert.deepEqual(calls, ["identify:u-1"]);
  assert.equal(h.loads(), 1, "the SDK is imported once");
});

test("a different account on the same tab resets before it identifies", async () => {
  const { client, calls } = fake();
  client.__loaded = true;
  const h = harness(client);
  await h.identifier.identify("u-1");
  await h.identifier.identify("u-2");
  assert.deepEqual(calls, ["identify:u-1", "reset", "identify:u-2"]);
  assert.equal(h.identifier.current(), "u-2");
});

test("sign-out resets the SDK and lets the next sign-in identify again", async () => {
  const { client, calls } = fake();
  client.__loaded = true;
  const h = harness(client);
  await h.identifier.identify("u-1");
  h.identifier.reset();
  h.identifier.reset();
  assert.equal(h.identifier.current(), null);
  await h.identifier.identify("u-1");
  assert.deepEqual(calls, ["identify:u-1", "reset", "identify:u-1"]);
});

test("an SDK that never boots is given up on, and a later call may try again", async () => {
  const { client, calls } = fake();
  const h = harness(client, { loadedAfterPolls: Number.POSITIVE_INFINITY, timeoutMs: 100 });
  assert.equal(await h.identifier.identify("u-1"), false);
  assert.deepEqual(calls, []);
  assert.equal(h.identifier.current(), null);
  client.__loaded = true;
  assert.equal(await h.identifier.identify("u-1"), true);
  assert.deepEqual(calls, ["identify:u-1"]);
});

test("without a key nothing is loaded and nothing is called", async () => {
  const { client, calls } = fake();
  client.__loaded = true;
  const h = harness(client, { enabled: false });
  assert.equal(await h.identifier.identify("u-1"), false);
  assert.equal(h.loads(), 0);
  assert.deepEqual(calls, []);
});
