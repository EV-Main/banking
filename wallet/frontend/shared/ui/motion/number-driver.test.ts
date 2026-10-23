// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// Pins the one guarantee `AnimatedNumber` makes to a money surface: the final
// figure lands within a bounded time whether or not the count gets animation
// frames. A hidden tab gets none (banking#346), so an animation alone is not
// enough — the driver must reach the figure by another route.
import assert from "node:assert/strict";
import test from "node:test";

import { driveNumber, type NumberDriverDeps } from "./number-driver.ts";

const format = (n: number) => `$${n.toFixed(2)}`;

/** A world with a hand-cranked frame loop, timers and visibility. */
function world(opts: { hidden?: boolean } = {}) {
  const writes: string[] = [];
  const shown: number[] = [];
  let hidden = opts.hidden ?? false;
  let frame: ((v: number) => void) | null = null;
  let complete: (() => void) | null = null;
  let stopped = 0;
  const hiddenListeners = new Set<() => void>();
  const timers = new Map<number, { cb: () => void; ms: number }>();
  let nextTimer = 1;
  let scheduledMs: number | null = null;
  const deps: NumberDriverDeps = {
    animate: (_from, _to, { onUpdate, onComplete }) => {
      frame = onUpdate;
      complete = onComplete;
      return { stop: () => { stopped += 1; } };
    },
    isHidden: () => hidden,
    onHidden: (cb) => {
      hiddenListeners.add(cb);
      return () => hiddenListeners.delete(cb);
    },
    setTimer: (cb, ms) => {
      const id = nextTimer++;
      scheduledMs = ms;
      timers.set(id, { cb, ms });
      return () => timers.delete(id);
    },
    fallbackMs: 500,
  };
  return {
    deps,
    writes,
    shown,
    get stopped() { return stopped; },
    get scheduledMs() { return scheduledMs; },
    get pendingTimers() { return timers.size; },
    get hiddenListeners() { return hiddenListeners.size; },
    tick: (v: number) => frame?.(v),
    finishAnimation: () => complete?.(),
    goHidden: () => { hidden = true; for (const cb of [...hiddenListeners]) cb(); },
    fireTimers: () => { for (const { cb } of [...timers.values()]) cb(); },
    run: (from: number, to: number) =>
      driveNumber({ from, to, format, write: (t) => writes.push(t), onShown: (n) => shown.push(n) }, deps),
  };
}

test("visible, frames flow: counts from the start and ends on the final figure", () => {
  const w = world();
  w.run(0, 999.5);
  assert.deepEqual(w.writes, ["$0.00"]);
  w.tick(250);
  w.tick(700);
  w.finishAnimation();
  assert.deepEqual(w.writes, ["$0.00", "$250.00", "$700.00", "$999.50"]);
  assert.equal(w.shown.at(-1), 999.5);
  // Completing the count releases the fallback and the visibility hook.
  assert.equal(w.pendingTimers, 0);
  assert.equal(w.hiddenListeners, 0);
});

test("animation that never emits a frame: the fallback timer still writes the final figure", () => {
  const w = world();
  w.run(0, 999.5);
  assert.deepEqual(w.writes, ["$0.00"]);
  assert.equal(w.scheduledMs, w.deps.fallbackMs);
  w.fireTimers();
  assert.deepEqual(w.writes, ["$0.00", "$999.50"]);
  assert.equal(w.shown.at(-1), 999.5);
  assert.equal(w.stopped, 1);
  // A frame arriving late (the tab came back) must not drag the figure back.
  w.tick(300);
  assert.equal(w.writes.at(-1), "$999.50");
});

test("hidden at the start: the final figure is written immediately, nothing scheduled", () => {
  const w = world({ hidden: true });
  w.run(0, 999.5);
  assert.deepEqual(w.writes, ["$999.50"]);
  assert.deepEqual(w.shown, [999.5]);
  assert.equal(w.pendingTimers, 0);
  assert.equal(w.hiddenListeners, 0);
});

test("goes hidden mid-count: the count stops and the final figure is written", () => {
  const w = world();
  w.run(0, 999.5);
  w.tick(120);
  w.goHidden();
  assert.deepEqual(w.writes, ["$0.00", "$120.00", "$999.50"]);
  assert.equal(w.stopped, 1);
  assert.equal(w.pendingTimers, 0);
});

test("stopped early: leaves the screen where it is, for the next count to continue from", () => {
  const w = world();
  const stop = w.run(0, 999.5);
  w.tick(120);
  stop();
  assert.equal(w.writes.at(-1), "$120.00");
  assert.equal(w.shown.at(-1), 120);
  assert.equal(w.stopped, 1);
  assert.equal(w.pendingTimers, 0);
  assert.equal(w.hiddenListeners, 0);
  // Neither the timer nor a late frame may revive it.
  w.fireTimers();
  w.tick(500);
  assert.equal(w.writes.at(-1), "$120.00");
});

test("already on the final figure: written once, no animation", () => {
  const w = world();
  w.run(999.5, 999.5);
  assert.deepEqual(w.writes, ["$999.50"]);
  assert.equal(w.pendingTimers, 0);
});
