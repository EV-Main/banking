// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// `performSignOut` is the chip's Sign out. What is pinned here, call order included:
//   • the identity cache is cleared FIRST, before the request, whatever the shell answers —
//     deliberate: on a shared machine the name of whoever pressed Sign out must not be what
//     the next paint shows, even when the logout itself failed and they walked away;
//   • one POST to the shell's /api/auth/logout carrying the CSRF header;
//   • only a 2xx clears the persisted reads and navigates to the locale's /loggedout, and
//     resolves true;
//   • a refusal (403, 500) or a rejected fetch resolves false with nothing cleared but the
//     identity and nothing navigated — the session is still alive, so /loggedout would lie —
//     and never throws to the click handler.
import assert from "node:assert/strict";
import test from "node:test";

import { LOGOUT_ENDPOINT, performSignOut, type SignOutDeps } from "./sign-out.ts";

const CSRF = { "x-ev-csrf": "token-1" };

/** Deps that log every call, in order, as one readable line each. */
function recorder(answer: () => Promise<{ ok: boolean }>, locale: SignOutDeps["locale"] = "en") {
  const calls: string[] = [];
  const deps: SignOutDeps = {
    fetch: (input, init) => {
      calls.push(`fetch ${init.method} ${input} ${JSON.stringify(init.headers)}`);
      return answer();
    },
    csrfHeader: () => CSRF,
    clearIdentity: () => void calls.push("clearIdentity"),
    clearResources: () => void calls.push("clearResources"),
    navigate: (href) => void calls.push(`navigate ${href}`),
    locale,
  };
  return { calls, deps };
}

test("the logout endpoint is the shell's", () => {
  assert.equal(LOGOUT_ENDPOINT, "/api/auth/logout");
});

test("a confirmed logout clears the identity, posts, clears the reads, then lands on /loggedout", async () => {
  const { calls, deps } = recorder(async () => ({ ok: true }));

  const signedOut = await performSignOut(deps);

  assert.equal(signedOut, true);
  assert.deepEqual(calls, [
    "clearIdentity",
    'fetch POST /api/auth/logout {"x-ev-csrf":"token-1"}',
    "clearResources",
    "navigate /en/cabinet/loggedout",
  ]);
});

test("a confirmed logout lands on the reader's own locale", async () => {
  const { calls, deps } = recorder(async () => ({ ok: true }), "ru");

  await performSignOut(deps);

  assert.equal(calls.at(-1), "navigate /ru/cabinet/loggedout");
});

// The function sees only `ok`: a 403 CSRF refusal and a 500 store error are the same answer.
test("a refused logout clears only the identity, navigates nowhere and resolves false", async () => {
  const { calls, deps } = recorder(async () => ({ ok: false }));

  const signedOut = await performSignOut(deps);

  assert.equal(signedOut, false);
  assert.deepEqual(calls, ["clearIdentity", 'fetch POST /api/auth/logout {"x-ev-csrf":"token-1"}']);
});

test("a rejected fetch resolves false without throwing, clearing only the identity", async () => {
  const { calls, deps } = recorder(() => Promise.reject(new TypeError("Failed to fetch")));

  const signedOut = await performSignOut(deps);

  assert.equal(signedOut, false);
  assert.deepEqual(calls, ["clearIdentity", 'fetch POST /api/auth/logout {"x-ev-csrf":"token-1"}']);
});

test("the identity is already cleared while the request is still in flight", async () => {
  let settle: (res: { ok: boolean }) => void = () => undefined;
  const { calls, deps } = recorder(() => new Promise((resolve) => (settle = resolve)));

  const pending = performSignOut(deps);
  assert.deepEqual(calls, ["clearIdentity", 'fetch POST /api/auth/logout {"x-ev-csrf":"token-1"}']);

  settle({ ok: false });
  assert.equal(await pending, false);
});
