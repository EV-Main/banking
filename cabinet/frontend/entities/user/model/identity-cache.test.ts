// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The account chip's identity cache lives in localStorage, which is outside input: another
// release, another bundle or a hand edit may have left anything there. What is pinned here:
// only the exact v2 shape is believed; the v1 sessionStorage entry is dropped on first read;
// a write that changes nothing stays silent, so the chip does not re-render on every page;
// and the name the profile shows wins over the legal one, with the email as last resort.
//
// Node has no Web Storage or `window` without flags, so each test installs plain stand-ins
// on globalThis. The module remembers that it already dropped the legacy key, so tests that
// care get a fresh instance through a query-suffixed import.
import assert from "node:assert/strict";
import test, { afterEach, beforeEach } from "node:test";

import {
  IDENTITY_EVENT,
  IDENTITY_KEY,
  LEGACY_IDENTITY_KEY,
  clearIdentity,
  identityOf,
  parseIdentity,
  readIdentity,
  rememberProfile,
  subscribeIdentity,
  writeIdentity,
} from "./identity-cache.ts";

class MemoryStorage implements Storage {
  #items = new Map<string, string>();
  get length(): number {
    return this.#items.size;
  }
  clear(): void {
    this.#items.clear();
  }
  getItem(key: string): string | null {
    return this.#items.get(key) ?? null;
  }
  key(index: number): string | null {
    return [...this.#items.keys()][index] ?? null;
  }
  removeItem(key: string): void {
    this.#items.delete(key);
  }
  setItem(key: string, value: string): void {
    this.#items.set(key, String(value));
  }
}

let local: MemoryStorage;
let session: MemoryStorage;
let win: EventTarget;
/** How many times this document announced a cache change. */
let announced: number;

beforeEach(() => {
  local = new MemoryStorage();
  session = new MemoryStorage();
  win = new EventTarget();
  announced = 0;
  win.addEventListener(IDENTITY_EVENT, () => {
    announced += 1;
  });
  Reflect.set(globalThis, "localStorage", local);
  Reflect.set(globalThis, "sessionStorage", session);
  Reflect.set(globalThis, "window", win);
});

afterEach(() => {
  Reflect.deleteProperty(globalThis, "localStorage");
  Reflect.deleteProperty(globalThis, "sessionStorage");
  Reflect.deleteProperty(globalThis, "window");
});

let instance = 0;
/** A copy of the module with its own "legacy key already dropped" flag. */
const freshModule = (): Promise<typeof import("./identity-cache.ts")> => import(`./identity-cache.ts?instance=${++instance}`);

// Entries carry a `savedAt` stamp and expire; the fixtures are stamped at a fixed `NOW`.
const NOW = 1_800_000_000_000;
const VALENTINA = `{"email":"v@example.com","name":"Valentina Kuznetsova","initials":"VK","savedAt":${NOW}}`;

// ── key ─────────────────────────────────────────────────────────────────────

test("the cache key carries the v2 suffix", () => {
  assert.equal(IDENTITY_KEY, "ev.cabinet.account-chip.v2");
  assert.equal(LEGACY_IDENTITY_KEY, "ev.cabinet.account-chip");
});

// ── legacy entry ────────────────────────────────────────────────────────────

test("the first read drops the v1 entry from sessionStorage", async () => {
  session.setItem(LEGACY_IDENTITY_KEY, '{"email":"v@example.com","name":"V"}');
  session.setItem("unrelated", "kept");
  const cache = await freshModule();

  cache.readIdentityRaw();

  assert.equal(session.getItem(LEGACY_IDENTITY_KEY), null);
  assert.equal(session.getItem("unrelated"), "kept");
});

test("the v1 entry is never read as an identity", async () => {
  session.setItem(LEGACY_IDENTITY_KEY, '{"email":"v@example.com","name":"V","initials":"V"}');
  const cache = await freshModule();

  assert.equal(cache.readIdentity(), null);
});

test("clearing the cache drops the v1 entry too", async () => {
  session.setItem(LEGACY_IDENTITY_KEY, '{"email":"v@example.com","name":"V"}');
  const cache = await freshModule();

  cache.clearIdentity();

  assert.equal(session.getItem(LEGACY_IDENTITY_KEY), null);
});

// ── parseIdentity ───────────────────────────────────────────────────────────

test("the exact v2 shape parses", () => {
  assert.deepEqual(parseIdentity(VALENTINA, NOW), { email: "v@example.com", name: "Valentina Kuznetsova", initials: "VK" });
});

test("a null or empty name parses as no name", () => {
  assert.deepEqual(parseIdentity(`{"email":"v@example.com","name":null,"initials":"V","savedAt":${NOW}}`, NOW), { email: "v@example.com", name: null, initials: "V" });
  assert.deepEqual(parseIdentity(`{"email":"v@example.com","name":"","initials":"V","savedAt":${NOW}}`, NOW), { email: "v@example.com", name: null, initials: "V" });
});

test("a missing or empty entry parses as no cache", () => {
  assert.equal(parseIdentity(null), null);
  assert.equal(parseIdentity(""), null);
});

test("garbage parses as no cache rather than throwing", () => {
  assert.equal(parseIdentity("{"), null);
  assert.equal(parseIdentity("not json"), null);
  assert.equal(parseIdentity("42"), null);
  assert.equal(parseIdentity('"v@example.com"'), null);
  assert.equal(parseIdentity("null"), null);
  assert.equal(parseIdentity("[]"), null);
});

test("the v1 shape without initials parses as no cache", () => {
  assert.equal(parseIdentity('{"email":"v@example.com","name":"Valentina"}'), null);
});

test("a foreign shape parses as no cache", () => {
  assert.equal(parseIdentity('{"email":"","name":"V","initials":"V"}'), null, "empty email");
  assert.equal(parseIdentity('{"email":42,"name":"V","initials":"V"}'), null, "numeric email");
  assert.equal(parseIdentity('{"email":"v@example.com","name":7,"initials":"V"}'), null, "numeric name");
  assert.equal(parseIdentity('{"email":"v@example.com","initials":"V"}'), null, "name missing, not null");
  assert.equal(parseIdentity('{"email":"v@example.com","name":"V","initials":""}'), null, "empty initials");
  assert.equal(parseIdentity('{"user":{"email":"v@example.com"}}'), null, "nested");
});

test("storage that throws on access reads as no cache", () => {
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    get() {
      throw new DOMException("The operation is insecure.", "SecurityError");
    },
  });

  assert.equal(readIdentity(), null);
  assert.doesNotThrow(() => writeIdentity(identityOf("v@example.com", "V")));
  assert.equal(announced, 0);
});

// ── writeIdentity / clearIdentity ───────────────────────────────────────────

test("a new identity is stored in the v2 shape and announced once", () => {
  writeIdentity({ email: "v@example.com", name: "Valentina Kuznetsova", initials: "VK" }, NOW);

  assert.equal(local.getItem(IDENTITY_KEY), VALENTINA);
  assert.equal(announced, 1);
});

test("writing the identity already stored announces nothing", () => {
  local.setItem(IDENTITY_KEY, VALENTINA);

  writeIdentity({ email: "v@example.com", name: "Valentina Kuznetsova", initials: "VK" }, NOW);

  assert.equal(local.getItem(IDENTITY_KEY), VALENTINA);
  assert.equal(announced, 0);
});

test("a changed name replaces the stored identity and is announced", () => {
  local.setItem(IDENTITY_KEY, VALENTINA);

  writeIdentity({ email: "v@example.com", name: "Valya", initials: "V" }, NOW);

  assert.equal(local.getItem(IDENTITY_KEY), `{"email":"v@example.com","name":"Valya","initials":"V","savedAt":${NOW}}`);
  assert.equal(announced, 1);
});

test("clearing removes the stored identity and is announced", () => {
  local.setItem(IDENTITY_KEY, VALENTINA);

  clearIdentity();

  assert.equal(local.getItem(IDENTITY_KEY), null);
  assert.equal(announced, 1);
});

test("clearing an empty cache announces nothing", () => {
  clearIdentity();

  assert.equal(announced, 0);
});

// ── rememberProfile ─────────────────────────────────────────────────────────

test("the preferred name wins over the legal name", () => {
  rememberProfile({ email: "v@example.com", preferred_name: "Valya Kuz", legal_name: "Valentina Kuznetsova" });

  assert.deepEqual(readIdentity(), { email: "v@example.com", name: "Valya Kuz", initials: "VK" });
});

test("a blank preferred name falls back to the legal name", () => {
  rememberProfile({ email: "v@example.com", preferred_name: "  ", legal_name: "Valentina Kuznetsova" });

  assert.deepEqual(readIdentity(), { email: "v@example.com", name: "Valentina Kuznetsova", initials: "VK" });
});

test("a profile without any name is stored with no name and email initials", () => {
  rememberProfile({ email: "ada.lovelace@example.com" });

  assert.deepEqual(readIdentity(), { email: "ada.lovelace@example.com", name: null, initials: "AL" });
});

test("a profile without an email writes nothing", () => {
  rememberProfile({ preferred_name: "Valentina Kuznetsova" });
  rememberProfile({ email: "", legal_name: "Valentina Kuznetsova" });

  assert.equal(local.getItem(IDENTITY_KEY), null);
  assert.equal(announced, 0);
});

// ── identityOf: initials ────────────────────────────────────────────────────

test("a two-word name gives two initials", () => {
  assert.equal(identityOf("v@example.com", "Valentina Kuznetsova").initials, "VK");
});

test("a one-word name gives one initial", () => {
  assert.equal(identityOf("v@example.com", "Valentina").initials, "V");
});

test("no name falls back to the email handle", () => {
  assert.equal(identityOf("ada.lovelace@example.com", null).initials, "AL");
  assert.equal(identityOf("ada.lovelace@example.com", "   ").initials, "AL");
  assert.equal(identityOf("ada.lovelace@example.com", "   ").name, null);
});

test("a Cyrillic name gives Cyrillic initials", () => {
  assert.equal(identityOf("v@example.com", "Валентина Кузнецова").initials, "ВК");
});

// ── subscribeIdentity ───────────────────────────────────────────────────────

test("a subscriber hears this document's writes and other tabs' changes to the key", () => {
  let heard = 0;
  const unsubscribe = subscribeIdentity(() => {
    heard += 1;
  });

  writeIdentity(identityOf("v@example.com", "V"));
  win.dispatchEvent(Object.assign(new Event("storage"), { key: IDENTITY_KEY }));
  win.dispatchEvent(Object.assign(new Event("storage"), { key: null }));
  win.dispatchEvent(Object.assign(new Event("storage"), { key: "someone.else" }));
  unsubscribe();
  writeIdentity(identityOf("v@example.com", "W"));

  assert.equal(heard, 3);
});
