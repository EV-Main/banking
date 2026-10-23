// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// Covers the imperative half of the cache — the half that decides whether a screen paints
// from memory or from the network. The React hook is exercised by the app; what is worth
// pinning here is the policy: a fresh value is never re-fetched, a stale one is, a failed
// read never destroys the value already held, and a mutation's tags reach every key.
import assert from "node:assert/strict";
import test, { afterEach, beforeEach, mock } from "node:test";

import { createElement } from "react";
import { renderToString } from "react-dom/server";

import { clearResources, defineResource, mountForTests, pollSweepForTests, resetResourcesForTests, revalidateTag, useSeededResource, type Resource, type ResourceSeed } from "./resource.ts";

beforeEach(() => {
  resetResourcesForTests();
});

/** A fetcher that counts its calls and can be made to fail. */
function counted<T>(value: () => T) {
  const state = { calls: 0, fail: null as Error | null };
  const fetch = async () => {
    state.calls += 1;
    if (state.fail) throw state.fail;
    return value();
  };
  return { state, fetch };
}

test("a fresh value is served without touching the network", async () => {
  const { state, fetch } = counted(() => "100.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60 });

  assert.equal(await balance.read(), "100.00");
  assert.equal(await balance.read(), "100.00");
  assert.equal(state.calls, 1, "the second read must come from cache");
});

test("concurrent readers share one request", async () => {
  const { state, fetch } = counted(() => "100.00");
  const balance = defineResource({ name: "t.balance", fetch });

  await Promise.all([balance.read(), balance.read(), balance.read()]);
  assert.equal(state.calls, 1, "three simultaneous screens must not make three requests");
});

test("a value past its window is re-read", async () => {
  const { state, fetch } = counted(() => "100.00");
  // 0s: every read revalidates. The cached value is still served first — that is the
  // difference between a background refresh and a skeleton.
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 0 });

  await balance.read();
  await balance.read();
  assert.equal(state.calls, 2);
});

test("arguments key separate entries", async () => {
  const seen: string[] = [];
  const nav = defineResource({
    name: "t.nav",
    fetch: async (service: string) => {
      seen.push(service);
      return `nav:${service}`;
    },
    key: (service) => service,
    revalidate: 60,
  });

  assert.equal(await nav.read("alpha"), "nav:alpha");
  assert.equal(await nav.read("beta"), "nav:beta");
  assert.equal(await nav.read("alpha"), "nav:alpha");
  assert.deepEqual(seen, ["alpha", "beta"], "the second alpha read must come from cache");
});

test("a failed refresh keeps the value already on screen", async () => {
  const { state, fetch } = counted(() => "100.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 0 });

  await balance.read();
  state.fail = new Error("gateway timeout");
  await balance.read().catch(() => undefined);

  // The regression this guards: blanking a balance because one poll timed out.
  assert.equal(balance.peek(), "100.00");
});

test("a tag reaches every key carrying it", async () => {
  const { state, fetch } = counted(() => "100.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 600, tags: ["wallet"] });

  await balance.read();
  assert.equal(state.calls, 1);

  // A withdrawal moved the balance. The window says "fresh"; the tag overrules it.
  revalidateTag("wallet");
  await balance.read();
  assert.equal(state.calls, 2);
});

test("a tag nothing carries changes nothing", async () => {
  const { state, fetch } = counted(() => "100.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 600, tags: ["wallet"] });

  await balance.read();
  revalidateTag("notifications");
  await balance.read();
  assert.equal(state.calls, 1);
});

test("invalidateAll reaches every key of one resource, and no others", async () => {
  const { state: navState, fetch: navFetch } = counted(() => "1.00");
  const nav = defineResource({ name: "t.nav", fetch: (service: string) => navFetch().then((v) => `${service}:${v}`), key: (s) => s, revalidate: 600 });
  const { state: otherState, fetch: otherFetch } = counted(() => "x");
  const other = defineResource({ name: "t.other", fetch: otherFetch, revalidate: 600 });

  await nav.read("alpha");
  await nav.read("beta");
  await other.read();
  assert.equal(navState.calls, 2);

  nav.invalidateAll();
  await nav.read("alpha");
  await nav.read("beta");
  await other.read();
  assert.equal(navState.calls, 4);
  assert.equal(otherState.calls, 1, "a sibling resource must be untouched");
});

test("publish writes through without a request", async () => {
  const { state, fetch } = counted(() => "100.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 600 });

  // A PATCH answers with the new state; asking the server what it just told us is waste.
  balance.publish("250.00");
  assert.equal(await balance.read(), "250.00");
  assert.equal(state.calls, 0);
});

test("clearResources drops every account-scoped value", async () => {
  const { fetch } = counted(() => "100.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 600 });

  await balance.read();
  assert.equal(balance.peek(), "100.00");

  // Sign-out. The next account in this tab must not see the previous one's figures.
  clearResources();
  assert.equal(balance.peek(), undefined);
});

test("a disabled read is never issued", async () => {
  const { state, fetch } = counted(() => "nav");
  const nav = defineResource({
    name: "t.nav",
    fetch: (service: string) => fetch().then((v) => `${service}:${v}`),
    key: (service) => service,
    enabled: (service) => service.trim().length > 0,
  });

  // The BFF 400s a NAV request with no fund; a screen that hasn't picked one asks nothing.
  nav.prefetch("");
  assert.equal(state.calls, 0);
  nav.prefetch("alpha");
  await nav.read("alpha");
  assert.equal(state.calls, 1);
});

test("a read that has never succeeded rejects rather than resolving empty", async () => {
  const { state, fetch } = counted(() => "100.00");
  state.fail = new Error("gateway timeout");
  const balance = defineResource({ name: "t.balance", fetch });

  await assert.rejects(() => balance.read(), /gateway timeout/);
  assert.equal(balance.peek(), undefined);
});

// ── a refusal is a verdict, not a blip ────────────────────────────────────────

/** The shape the transport throws for a 403 (`./api-client.ts` — `RequestError`). */
class Forbidden extends Error {
  readonly status = 403;
  constructor() {
    super("You don't have access to this.");
  }
}

test("a refused read is not repeated by the clock", async () => {
  const { state, fetch } = counted(() => "roster");
  state.fail = new Forbidden();
  // 0s, so every automatic trigger would otherwise find it stale and fire again.
  const owners = defineResource({ name: "t.owners", fetch, revalidate: 0 });

  await assert.rejects(() => owners.read(), /access/);
  await assert.rejects(() => owners.read(), /access/);
  owners.prefetch();
  assert.equal(state.calls, 1, "an operator's 403 must not be collected once per poll");
});

test("a refusal suppresses the repeat, never the error", async () => {
  const { state, fetch } = counted(() => "roster");
  state.fail = new Forbidden();
  const owners = defineResource({ name: "t.owners", fetch, revalidate: 0 });

  // The screens branch on this — the consilium renders its "you cannot see this room"
  // state off exactly this error. Swallowing it would trade a noisy console for a blank
  // page that never explains itself.
  await assert.rejects(() => owners.read(), /access/);
  assert.equal(state.calls, 1);
});

test("only a refusal latches — a timeout is still retried", async () => {
  const { state, fetch } = counted(() => "roster");
  state.fail = new Error("gateway timeout");
  const owners = defineResource({ name: "t.owners", fetch, revalidate: 0 });

  await assert.rejects(() => owners.read(), /timeout/);
  await assert.rejects(() => owners.read(), /timeout/);
  assert.equal(state.calls, 2, "a transient failure is exactly what the retry loop is for");
});

test("a tag invalidation lifts a refusal — a seat may have just been granted", async () => {
  const { state, fetch } = counted(() => "roster");
  state.fail = new Forbidden();
  const owners = defineResource({ name: "t.owners", fetch, revalidate: 0, tags: ["owners"] });

  await assert.rejects(() => owners.read(), /access/);
  assert.equal(state.calls, 1);

  // A mutation naming the tag is new information, unlike a timer firing again.
  revalidateTag("owners");
  state.fail = null;
  assert.equal(await owners.read(), "roster");
  assert.equal(state.calls, 2);
});

test("sign-out lifts a refusal — the next account is a different caller", async () => {
  const { state, fetch } = counted(() => "roster");
  state.fail = new Forbidden();
  const owners = defineResource({ name: "t.owners", fetch, revalidate: 0 });

  await assert.rejects(() => owners.read(), /access/);
  clearResources();
  state.fail = null;
  assert.equal(await owners.read(), "roster");
  assert.equal(state.calls, 2);
});

// ── the poll clock (a verdict that lands on a tab nobody touches) ─────────────

/** The one field `poll.while` reads on the real profile resource. */
interface Profile {
  kyc_level: number;
}

/** The profile resource's polling shape, with a small `maxMs` so the cap is reachable. */
const PENDING_KYC = { while: (p: Profile | undefined) => (p?.kyc_level ?? 0) === 0, startMs: 5_000, maxMs: 20_000 };

/** A profile whose tier the test moves, standing in for a verdict landing at the hub. */
function profile() {
  const state = { calls: 0, level: 0 };
  const resource = defineResource({
    name: "t.profile",
    fetch: async (): Promise<Profile> => {
      state.calls += 1;
      return { kyc_level: state.level };
    },
    revalidate: 600,
    poll: PENDING_KYC,
  });
  return { state, resource };
}

/** The sweep fires and forgets; let the revalidation it started settle before asserting. */
const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

test("a newly eligible entry is armed, not fired — the mount already read it", async () => {
  const { state, resource } = profile();
  await resource.read();
  mountForTests(resource);
  assert.equal(state.calls, 1);

  pollSweepForTests(0);
  await flush();
  assert.equal(state.calls, 1, "the tick that finds it eligible only arms it");

  pollSweepForTests(5_000);
  await flush();
  assert.equal(state.calls, 2, "the tick after startMs is the one that reads");
});

test("the delay doubles from startMs and stops at maxMs", async () => {
  const { state, resource } = profile();
  await resource.read();
  mountForTests(resource);

  // Each read is settled before the next tick — real ticks are seconds apart, and an
  // unsettled one would be deduplicated by `inflight` rather than counted.
  pollSweepForTests(0); // arms for 5_000
  pollSweepForTests(5_000); // reads; next gap 10_000
  await flush();
  pollSweepForTests(15_000); // reads; next gap 20_000
  await flush();
  pollSweepForTests(35_000); // reads; gap would double to 40_000, capped at 20_000
  await flush();
  assert.equal(state.calls, 4);

  // The cap is what keeps a case nobody closes from drifting out to minutes.
  pollSweepForTests(54_999);
  await flush();
  assert.equal(state.calls, 4, "still inside the capped gap");
  pollSweepForTests(55_000);
  await flush();
  assert.equal(state.calls, 5, "maxMs after the last read, not longer");
});

test("a backgrounded tab does not spend its backoff unseen", async () => {
  const { state, resource } = profile();
  await resource.read();
  mountForTests(resource);

  pollSweepForTests(0);
  pollSweepForTests(5_000, false);
  await flush();
  assert.equal(state.calls, 1, "a hidden tab polls nothing");

  // Nothing was consumed while hidden: the same due time still fires once visible.
  pollSweepForTests(5_000, true);
  await flush();
  assert.equal(state.calls, 2);
});

test("an entry no screen is showing is not polled", async () => {
  const { state, resource } = profile();
  await resource.read();

  pollSweepForTests(0);
  pollSweepForTests(5_000);
  await flush();
  assert.equal(state.calls, 1, "a resource read once and navigated away from must go quiet");
});

test("unmounting stops the poll", async () => {
  const { state, resource } = profile();
  await resource.read();
  const unmount = mountForTests(resource);

  pollSweepForTests(0);
  unmount();
  pollSweepForTests(5_000);
  await flush();
  assert.equal(state.calls, 1);
});

test("a closed case stops the poll and hands the next one a fresh backoff", async () => {
  const { state, resource } = profile();
  await resource.read();
  mountForTests(resource);

  pollSweepForTests(0);
  pollSweepForTests(5_000);
  await flush();
  assert.equal(state.calls, 2);

  // The verdict lands: tier 1. This is the signal `poll.while` exists to read.
  resource.publish({ kyc_level: 1 });
  pollSweepForTests(15_000);
  pollSweepForTests(60_000);
  await flush();
  assert.equal(state.calls, 2, "a settled profile is not polled");

  // A second, distinct case opens later. It must wait startMs, not the 10_000 the first
  // one had climbed to — the regression this guards is a new case inheriting old backoff.
  resource.publish({ kyc_level: 0 });
  pollSweepForTests(100_000); // arms for 105_000
  pollSweepForTests(104_999);
  await flush();
  assert.equal(state.calls, 2);
  pollSweepForTests(105_000);
  await flush();
  assert.equal(state.calls, 3, "the new case polls again at startMs");
});

test("a refused profile is not polled — a 403 is a verdict, not a pending case", async () => {
  const state = { calls: 0 };
  const resource = defineResource({
    name: "t.profile",
    fetch: async (): Promise<Profile> => {
      state.calls += 1;
      throw new Forbidden();
    },
    revalidate: 0,
    poll: PENDING_KYC,
  });
  mountForTests(resource);

  // Never loaded, so `while` reads it as pending — `denied` is the only thing stopping it.
  await assert.rejects(() => resource.read(), /access/);
  assert.equal(state.calls, 1);

  pollSweepForTests(0);
  pollSweepForTests(5_000);
  pollSweepForTests(25_000);
  await flush();
  assert.equal(state.calls, 1, "a refusal must not be collected once per poll");
});

// ── Seeds from server reads ─────────────────────────────────────────────────
//
// A server component hands its read to `useSeededResource`, which writes it into the shared
// entry during render. The hook is the only way in, so these cases render it — with
// `renderToString` and a stand-in `window`, which is what makes the hook take its browser
// path (the server path deliberately never touches the shared registry). A re-render with
// the same seed object is how a router-cache replay (browser Back) looks to the cache.
//
// A seed is timed by when this browser first sees it, never by its server `fetchedAt`: the
// two machines' clocks cannot be ordered against each other.

const NOW = 1_800_000_000_000;

/** Render one component that reads `resource` seeded with `initial`, as the browser would. */
function renderSeeded<T>(resource: Resource<T, []>, initial: ResourceSeed<T> | undefined): void {
  function Probe() {
    useSeededResource(resource, initial);
    return null;
  }
  Reflect.set(globalThis, "window", globalThis);
  try {
    renderToString(createElement(Probe));
  } finally {
    Reflect.deleteProperty(globalThis, "window");
  }
}

/** A Map-backed `sessionStorage`, so the persisted half of an entry can be observed. */
function installSessionStorage(initial: Record<string, string> = {}): Map<string, string> {
  const store = new Map(Object.entries(initial));
  Reflect.set(globalThis, "sessionStorage", {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => void store.set(key, value),
    removeItem: (key: string) => void store.delete(key),
  });
  return store;
}

afterEach(() => {
  mock.timers.reset();
  Reflect.deleteProperty(globalThis, "sessionStorage");
});

test("a seed fills an empty entry", () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60 });

  renderSeeded(balance, { data: "98765.43", fetchedAt: NOW });

  assert.equal(balance.peek(), "98765.43");
});

test("a seeded value is served without a request while it is fresh", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { state, fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60 });
  renderSeeded(balance, { data: "98765.43", fetchedAt: NOW });

  mock.timers.setTime(NOW + 59_999);
  assert.equal(await balance.read(), "98765.43");
  assert.equal(state.calls, 0, "a fresh seed must not be re-read from the browser");

  mock.timers.setTime(NOW + 60_000);
  assert.equal(await balance.read(), "from-browser");
  assert.equal(state.calls, 1, "the seed ages on its own fetchedAt like any read");
});

test("a seed does not overwrite a value newer than itself", () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60 });
  balance.publish("70.00");

  renderSeeded(balance, { data: "100.00", fetchedAt: NOW - 5_000 });

  assert.equal(balance.peek(), "70.00");
});

test("a seed read before a tag invalidation is refused — the withdrawal moved the balance since", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { state, fetch } = counted(() => "70.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60, tags: ["wallet"] });
  const beforeWithdrawal = { data: "100.00", fetchedAt: NOW - 1_000 };
  renderSeeded(balance, beforeWithdrawal);

  mock.timers.setTime(NOW + 1_000);
  revalidateTag("wallet");
  mock.timers.setTime(NOW + 2_000);
  renderSeeded(balance, beforeWithdrawal);

  assert.equal(await balance.read(), "70.00", "the pre-withdrawal seed must not count as fresh again");
  assert.equal(state.calls, 1);
});

test("a seed read at the very moment of the invalidation is refused too", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { state, fetch } = counted(() => "70.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60, tags: ["wallet"] });
  renderSeeded(balance, undefined);

  revalidateTag("wallet");
  renderSeeded(balance, { data: "100.00", fetchedAt: NOW });

  assert.equal(await balance.read(), "70.00");
  assert.equal(state.calls, 1);
});

test("a seed read after a tag invalidation is adopted", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { state, fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60, tags: ["wallet"] });
  renderSeeded(balance, undefined);
  revalidateTag("wallet");

  mock.timers.setTime(NOW + 2_000);
  renderSeeded(balance, { data: "70.00", fetchedAt: NOW + 1_500 });

  assert.equal(await balance.read(), "70.00");
  assert.equal(state.calls, 0);
});

test("a seed beats a snapshot persisted by a previous visit and is persisted at its receipt time", () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const store = installSessionStorage({ "ev.cabinet.resource:t.balance()": JSON.stringify({ v: "100.00", t: NOW - 10_000 }) });
  const { fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60, persist: true });

  // Stamped by the server's clock, which may run anywhere relative to this one.
  renderSeeded(balance, { data: "70.00", fetchedAt: NOW - 30_000 });

  assert.equal(balance.peek(), "70.00");
  assert.deepEqual(JSON.parse(store.get("ev.cabinet.resource:t.balance()") ?? "null"), { v: "70.00", t: NOW });
});

test("a seed stamped by a server clock running ahead ages from its receipt", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { state, fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 15 });
  renderSeeded(balance, { data: "98765.43", fetchedAt: NOW + 3_600_000 });

  mock.timers.setTime(NOW + 15_000);
  assert.equal(await balance.read(), "from-browser", "an hour-ahead stamp must not keep the value fresh for an hour");
  assert.equal(state.calls, 1);
});

test("clearResources refuses a seed read before the sign-out", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60 });
  const previousAccount = { data: "100.00", fetchedAt: NOW - 1_000 };
  renderSeeded(balance, previousAccount);

  clearResources();
  mock.timers.setTime(NOW + 1_000);
  renderSeeded(balance, previousAccount);

  assert.equal(balance.peek(), undefined, "the previous account's figure must not come back from a replayed render");
});

test("a seed stamped by a server clock running behind is fresh from its receipt", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { state, fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 15 });
  renderSeeded(balance, { data: "98765.43", fetchedAt: NOW - 3_600_000 });

  mock.timers.setTime(NOW + 14_999);
  assert.equal(await balance.read(), "98765.43", "an hour-behind stamp must not make a just-rendered value stale");
  assert.equal(state.calls, 0);
});

test("a seed stamped ahead of the browser clock does not overwrite a later browser read on re-render", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { fetch } = counted(() => "70.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60, tags: ["wallet"] });
  // The server's clock is 60 s ahead of this browser's — a device clock off by a minute.
  const serverRead = { data: "100.00", fetchedAt: NOW + 60_000 };
  renderSeeded(balance, serverRead);

  mock.timers.setTime(NOW + 1_000);
  revalidateTag("wallet");
  assert.equal(await balance.read(), "70.00");

  mock.timers.setTime(NOW + 1_001);
  renderSeeded(balance, serverRead);

  assert.equal(balance.peek(), "70.00", "the pre-withdrawal seed must not come back over the newer read");
});

test("re-rendering with the same seed object does not overwrite a read made after it", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { state, fetch } = counted(() => "70.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 1 });
  const serverRead = { data: "100.00", fetchedAt: NOW };
  renderSeeded(balance, serverRead);

  mock.timers.setTime(NOW + 1_000);
  assert.equal(await balance.read(), "70.00");
  mock.timers.setTime(NOW + 2_000);
  renderSeeded(balance, serverRead);

  assert.equal(state.calls, 1);
  assert.equal(balance.peek(), "70.00");
});

test("a new seed object received after an invalidation is adopted", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { state, fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60, tags: ["wallet"] });
  renderSeeded(balance, { data: "100.00", fetchedAt: NOW });

  mock.timers.setTime(NOW + 1_000);
  revalidateTag("wallet");
  mock.timers.setTime(NOW + 2_000);
  renderSeeded(balance, { data: "70.00", fetchedAt: NOW + 1_500 });

  assert.equal(await balance.read(), "70.00", "a fresh server render after the withdrawal carries the new balance");
  assert.equal(state.calls, 0);
});

test("a seed refused at the invalidation instant stays refused on a later re-render", async () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { state, fetch } = counted(() => "70.00");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60, tags: ["wallet"] });
  renderSeeded(balance, undefined);
  const raced = { data: "100.00", fetchedAt: NOW };

  revalidateTag("wallet");
  renderSeeded(balance, raced);
  mock.timers.setTime(NOW + 1_000);
  renderSeeded(balance, raced);

  assert.equal(balance.peek(), undefined, "a refused seed must not be adopted just because time has passed");
  assert.equal(await balance.read(), "70.00");
  assert.equal(state.calls, 1);
});

test("a seed refused for a newer held value stays refused once that value is invalidated", () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60, tags: ["wallet"] });
  balance.publish("70.00");
  const older = { data: "100.00", fetchedAt: NOW - 5_000 };
  renderSeeded(balance, older);

  mock.timers.setTime(NOW + 1_000);
  revalidateTag("wallet");
  mock.timers.setTime(NOW + 2_000);
  renderSeeded(balance, older);

  assert.equal(balance.peek(), "70.00", "the stale-while-revalidate value stays; the refused seed does not replace it");
});

test("sign-out counts as an invalidation — a seed received in the same instant is refused", () => {
  mock.timers.enable({ apis: ["Date"], now: NOW });
  const { fetch } = counted(() => "from-browser");
  const balance = defineResource({ name: "t.balance", fetch, revalidate: 60 });
  renderSeeded(balance, undefined);

  clearResources();
  renderSeeded(balance, { data: "100.00", fetchedAt: NOW });

  assert.equal(balance.peek(), undefined);
});
