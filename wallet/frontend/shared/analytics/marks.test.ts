// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test, { beforeEach } from "node:test";

import { mark, marked, once, resetMarksForTests, unmark } from "./marks.ts";

/** Both storages, either of which can be made to throw the way a private window does. */
function stub({ throws = false } = {}) {
  const make = () => {
    const map = new Map<string, string>();
    const guard = () => {
      if (throws) throw new Error("storage unavailable");
    };
    return {
      map,
      api: {
        getItem: (k: string) => {
          guard();
          return map.get(k) ?? null;
        },
        setItem: (k: string, v: string) => {
          guard();
          map.set(k, v);
        },
        removeItem: (k: string) => {
          guard();
          map.delete(k);
        },
      },
    };
  };
  const tab = make();
  const browser = make();
  (globalThis as { window?: unknown }).window = {
    sessionStorage: tab.api,
    localStorage: browser.api,
  };
  return { tab: tab.map, browser: browser.map };
}

beforeEach(() => {
  resetMarksForTests();
  stub();
});

test("once is true exactly once per key and scope", () => {
  assert.equal(once("first_deposit:tx-1", "browser"), true);
  assert.equal(once("first_deposit:tx-1", "browser"), false);
  assert.equal(once("first_deposit:tx-2", "browser"), true, "another account's first deposit is its own event");
  assert.equal(once("first_deposit:tx-1", "tab"), true, "scopes do not share marks");
});

test("tab marks live in sessionStorage, browser marks in localStorage", () => {
  const store = stub();
  mark("kyc_pending", "tab");
  mark("first_subscription:u-1", "browser");
  assert.equal(store.tab.size, 1);
  assert.equal(store.browser.size, 1);
  assert.equal(marked("kyc_pending"), true);
  unmark("kyc_pending");
  assert.equal(marked("kyc_pending"), false);
  assert.equal(store.tab.size, 0);
});

test("a mark written by an earlier page load is still a mark", () => {
  const store = stub();
  mark("session_created:u-1");
  resetMarksForTests();
  assert.equal(store.tab.size, 1);
  assert.equal(once("session_created:u-1"), false);
});

test("storage that throws degrades to once-per-page, never to a throw", () => {
  stub({ throws: true });
  assert.equal(once("kyc_completed"), true);
  assert.equal(once("kyc_completed"), false);
  assert.doesNotThrow(() => unmark("kyc_completed"));
  assert.equal(marked("kyc_completed"), false);
});
