// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// Pins the proxy's session decision (`proxy.ts` itself imports `next/server` and cannot be
// loaded here): a renewed token rides on the forwarded request, a session the identity plane
// calls gone has its cookies cleared, a verdict-less renewal changes nothing, no page keeps a
// guest out, and /login is the page to return to — with the sign-in dialog open over it.
import assert from "node:assert/strict";
import test from "node:test";

import type { Renewal } from "./access-renewal.ts";
import { decideSession, type SessionGateInput } from "./session-gate.ts";

const BARE = { session: "ev_session", access: "ev_access" };

const CLEARING = [
  "ev_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
  "ev_csrf=; Path=/; SameSite=Lax; Max-Age=0",
  "ev_access=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
];

function input(overrides: Partial<SessionGateInput>): SessionGateInput {
  return { pathname: "/en/cabinet/wallet", search: "", locale: "en", hasSession: true, renewal: null, cookieNames: BARE, ...overrides };
}

test("an alive renewal puts the new access token on the forwarded request and renders", () => {
  const renewal: Renewal = { kind: "alive", access: "new.jwt.sig", setCookies: ["ev_access=new.jwt.sig; Path=/; HttpOnly"] };

  assert.deepEqual(decideSession(input({ renewal })), {
    redirect: null,
    requestCookies: { ev_access: "new.jwt.sig" },
    deleteCookies: [],
    setCookies: ["ev_access=new.jwt.sig; Path=/; HttpOnly"],
  });
});

test("an alive renewal without a re-set token leaves the request alone but still forwards Set-Cookie", () => {
  const renewal: Renewal = { kind: "alive", access: null, setCookies: ["ev_csrf=c2; Path=/"] };

  assert.deepEqual(decideSession(input({ renewal })), {
    redirect: null,
    requestCookies: {},
    deleteCookies: [],
    setCookies: ["ev_csrf=c2; Path=/"],
  });
});

test("a gone session renders the page as a guest's, carrying the clearing cookies", () => {
  const renewal: Renewal = { kind: "gone", setCookies: CLEARING };

  assert.deepEqual(decideSession(input({ renewal })), {
    redirect: null,
    requestCookies: {},
    deleteCookies: ["ev_session", "ev_access"],
    setCookies: CLEARING,
  });
});

test("a renewal with no verdict changes nothing and lets a signed-in request render", () => {
  assert.deepEqual(decideSession(input({ renewal: { kind: "unknown" } })), {
    redirect: null,
    requestCookies: {},
    deleteCookies: [],
    setCookies: [],
  });
});

test("no page keeps a guest out", () => {
  for (const pathname of ["/en/cabinet", "/en/cabinet/wallet", "/en/cabinet/admin/users", "/en/cabinet/approve/tok-123"]) {
    assert.equal(decideSession(input({ pathname, hasSession: false })).redirect, null, pathname);
  }
});

test("/login opens the sign-in dialog over the page to return to", () => {
  const decision = decideSession(input({ pathname: "/de/cabinet/login", search: "?returnTo=%2Fwallet%2Fwithdraw%3Fnetwork%3Dtron", locale: "de", hasSession: false }));

  assert.deepEqual(decision.redirect, { pathname: "/de/cabinet/wallet/withdraw", search: "?network=tron&login=" });
});

test("/login with no returnTo opens the dialog over the zone root", () => {
  assert.deepEqual(decideSession(input({ pathname: "/en/cabinet/login", hasSession: false })).redirect, { pathname: "/en/cabinet", search: "?login=" });
});

test("a signed-in visitor on /login is sent where it would have returned, with no dialog", () => {
  assert.deepEqual(decideSession(input({ pathname: "/de/cabinet/login", search: "?returnTo=%2Fsettings", locale: "de" })).redirect, { pathname: "/de/cabinet/settings", search: "" });
  assert.deepEqual(decideSession(input({ pathname: "/de/cabinet/login", locale: "de" })).redirect, { pathname: "/de/cabinet", search: "" });
});

test("returnTo is zone-relative and same-origin, or it is the zone root", () => {
  const to = (returnTo: string) => decideSession(input({ pathname: "/en/cabinet/login", search: `?returnTo=${encodeURIComponent(returnTo)}`, hasSession: false })).redirect;

  assert.deepEqual(to("/en/cabinet/wallet"), { pathname: "/en/cabinet/wallet", search: "?login=" }, "a prefixed path is not prefixed twice");
  for (const hostile of ["//evil.example", "https://evil.example", "/\\evil.example", "/login"]) {
    assert.deepEqual(to(hostile), { pathname: "/en/cabinet", search: "?login=" }, hostile);
  }
});

test("a path without a locale lands on the English page", () => {
  assert.deepEqual(decideSession(input({ pathname: "/cabinet/login", locale: null, hasSession: false })).redirect, { pathname: "/en/cabinet", search: "?login=" });
});

test("__Host- cookie names are used as given for the renewed and the deleted cookies", () => {
  const cookieNames = { session: "__Host-ev_session", access: "__Host-ev_access" };

  const alive = decideSession(input({ cookieNames, renewal: { kind: "alive", access: "tok", setCookies: [] } }));
  const gone = decideSession(input({ cookieNames, renewal: { kind: "gone", setCookies: [] } }));

  assert.deepEqual(alive.requestCookies, { "__Host-ev_access": "tok" });
  assert.deepEqual(gone.deleteCookies, ["__Host-ev_session", "__Host-ev_access"]);
});
