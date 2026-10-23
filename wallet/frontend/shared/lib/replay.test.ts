// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// What these cases lock down: a read survives the one-or-two-second window a pod
// rollout opens (a bodyless 502 from the zone proxy, a dropped connection) without the
// operator reloading the page, while a mutation is never re-sent and a settled answer
// (4xx) is never retried. The delays are shortened so the suite stays instant; the
// production values are a separate constant that nothing here depends on.
import assert from "node:assert/strict";
import test from "node:test";

import { REPLAY_DELAYS_MS, REPLAYABLE_STATUSES, replaying } from "./replay.ts";

const DELAYS = [1, 1] as const;

type Outcome = number | Error;

/** Yields the scripted outcomes in order; a number is a status, an Error is thrown. */
function scripted(outcomes: readonly Outcome[]): { attempt: () => Promise<Response>; calls: () => number } {
  let calls = 0;
  return {
    calls: () => calls,
    attempt: async () => {
      const outcome = outcomes[calls];
      calls += 1;
      if (outcome === undefined) throw new Error("attempted past the script");
      if (outcome instanceof Error) throw outcome;
      return { status: outcome } as Response;
    },
  };
}

test("replayable: transient statuses are retried until a settled answer arrives", async () => {
  const script = scripted([502, 502, 200]);
  const res = await replaying(script.attempt, true, DELAYS);
  assert.equal(res.status, 200);
  assert.equal(script.calls(), 3);
});

test("replayable: the last transient answer is returned as is, not thrown", async () => {
  const script = scripted([502, 503, 504]);
  const res = await replaying(script.attempt, true, DELAYS);
  assert.equal(res.status, 504);
  assert.equal(script.calls(), 3);
});

test("replayable: a thrown attempt is retried and the next answer wins", async () => {
  const script = scripted([new Error("reset"), 200]);
  const res = await replaying(script.attempt, true, DELAYS);
  assert.equal(res.status, 200);
  assert.equal(script.calls(), 2);
});

test("replayable: when every attempt throws, the last error is the one surfaced", async () => {
  const last = new Error("third");
  const script = scripted([new Error("first"), new Error("second"), last]);
  await assert.rejects(replaying(script.attempt, true, DELAYS), (error) => error === last);
  assert.equal(script.calls(), 3);
});

test("not replayable: a single attempt, transient status handed back untouched", async () => {
  const script = scripted([502, 200]);
  const res = await replaying(script.attempt, false, DELAYS);
  assert.equal(res.status, 502);
  assert.equal(script.calls(), 1);
});

test("not replayable: a thrown attempt is not retried", async () => {
  const error = new Error("reset");
  const script = scripted([error, 200]);
  await assert.rejects(replaying(script.attempt, false, DELAYS), (thrown) => thrown === error);
  assert.equal(script.calls(), 1);
});

test("a settled failure (4xx) is never retried even for a read", async () => {
  for (const status of [400, 404]) {
    const script = scripted([status, 200]);
    const res = await replaying(script.attempt, true, DELAYS);
    assert.equal(res.status, status);
    assert.equal(script.calls(), 1);
  }
});

test("defaults: two replays, spaced under two seconds, on the proxy's transient statuses", () => {
  assert.deepEqual([...REPLAY_DELAYS_MS], [400, 1200]);
  assert.deepEqual([...REPLAYABLE_STATUSES].sort(), [502, 503, 504]);
});
