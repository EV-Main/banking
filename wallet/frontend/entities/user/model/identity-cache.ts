// The account chip's memory of who was signed in, so a hard load paints the final chip on
// its first frame instead of cycling skeleton → email-derived name → real name.
//
// localStorage, not session: a new tab or tomorrow's visit is exactly when the skeleton
// used to show. The entry is the user's own, already-visible identity — email, display
// name, initials — and nothing else is ever written here.
//
// Two bundles share it. The wallet host app writes it when a profile save lands
// (`saveProfile`), and the chip — a separate bundle on the same origin, with its own copy
// of every module — reads it. A same-document write fires no `storage` event, so writers
// announce themselves with `IDENTITY_EVENT` on `window`; other tabs still hear `storage`.
//
// Every entry carries `savedAt` and goes stale after IDENTITY_TTL_MS. The cache is painted
// before the session is confirmed, so on a shared machine it is what the next person sees;
// bounding its age bounds that, and the chip keeps the email and the menu back until the
// session answers (account-chip.tsx).
//
// Relative imports, no `@/`: `node --test` runs this file and cannot resolve the alias.

import { initialsOfName } from "../../../shared/lib/identity.ts";

export type CachedIdentity = {
  email: string;
  /** The profile's own name; null when it has none and the chip falls back to the email. */
  name: string | null;
  initials: string;
};

/** Versioned: a shape change bumps the suffix instead of parsing yesterday's value as today's. */
export const IDENTITY_KEY = "ev.cabinet.account-chip.v2";

/** v1 lived in sessionStorage as `{ email, name }`; dropped on first read, never migrated. */
export const LEGACY_IDENTITY_KEY = "ev.cabinet.account-chip";

/** An entry older than this reads as no cache: a day-old identity is not "the last visit". */
export const IDENTITY_TTL_MS = 12 * 60 * 60 * 1000;

/** A confirmed identity re-stamps an entry at most this often, so steady visits stay quiet. */
const RESTAMP_MS = 60 * 60 * 1000;

/** Dispatched on `window` after every write or clear in this document. */
export const IDENTITY_EVENT = "ev:account-chip-identity";

function storage(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    // Safari private windows and disabled storage throw on access, not on use.
    return null;
  }
}

let legacyDropped = false;

function dropLegacy(): void {
  if (legacyDropped) return;
  legacyDropped = true;
  try {
    globalThis.sessionStorage?.removeItem(LEGACY_IDENTITY_KEY);
  } catch {
    /* nothing to drop */
  }
}

/** The stored string as is — a stable snapshot for `useSyncExternalStore`. */
export function readIdentityRaw(): string | null {
  dropLegacy();
  try {
    return storage()?.getItem(IDENTITY_KEY) ?? null;
  } catch {
    return null;
  }
}

/**
 * Storage is outside input: anything but the exact shape — three fields plus a `savedAt`
 * within IDENTITY_TTL_MS of `now` — reads as no cache. Entries from before `savedAt`
 * existed are simply stale.
 */
export function parseIdentity(raw: string | null, now: number = Date.now()): CachedIdentity | null {
  const entry = parseEntry(raw);
  if (!entry) return null;
  const age = now - entry.savedAt;
  // A future stamp is a clock change or a hand edit; neither vouches for freshness.
  if (age < 0 || age >= IDENTITY_TTL_MS) return null;
  return entry.identity;
}

function parseEntry(raw: string | null): { identity: CachedIdentity; savedAt: number } | null {
  if (!raw) return null;
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null) return null;
  const { email, name, initials, savedAt } = value as Record<string, unknown>;
  if (typeof email !== "string" || !email) return null;
  if (name !== null && typeof name !== "string") return null;
  if (typeof initials !== "string" || !initials) return null;
  if (typeof savedAt !== "number" || !Number.isFinite(savedAt)) return null;
  return { identity: { email, name: name || null, initials }, savedAt };
}

export function readIdentity(): CachedIdentity | null {
  return parseIdentity(readIdentityRaw());
}

/** The stored value for an account; initials from the name when there is one, else the email. */
export function identityOf(email: string, name: string | null): CachedIdentity {
  const resolved = name?.trim() || null;
  return { email, name: resolved, initials: initialsOfName(resolved ?? "", email) };
}

function announce(): void {
  if (typeof window !== "undefined") window.dispatchEvent(new Event(IDENTITY_EVENT));
}

/**
 * Store `identity`, stamped `now`. An unchanged identity stamped within RESTAMP_MS is left
 * alone and announces nothing; an older one is re-stamped, so a user who keeps coming back
 * keeps a live cache.
 */
export function writeIdentity(identity: CachedIdentity, now: number = Date.now()): void {
  const store = storage();
  if (!store) return;
  const next = JSON.stringify({ email: identity.email, name: identity.name, initials: identity.initials, savedAt: now });
  try {
    const stored = parseEntry(store.getItem(IDENTITY_KEY));
    if (stored && sameIdentity(stored.identity, identity) && now - stored.savedAt >= 0 && now - stored.savedAt < RESTAMP_MS) return;
    store.setItem(IDENTITY_KEY, next);
  } catch {
    // Quota or disabled storage: the chip degrades to the live fetch, correctness intact.
    return;
  }
  announce();
}

/**
 * The cache a confirmed session may start from: only an entry of that same account. A
 * cache left by another account (a switch in this browser) is ignored, never shown.
 */
export function seedFor(cached: CachedIdentity | null, email: string | null): CachedIdentity | null {
  return cached && email !== null && cached.email === email ? cached : null;
}

function sameIdentity(a: CachedIdentity, b: CachedIdentity): boolean {
  return a.email === b.email && a.name === b.name && a.initials === b.initials;
}

export function clearIdentity(): void {
  dropLegacy();
  const store = storage();
  if (!store) return;
  try {
    if (store.getItem(IDENTITY_KEY) === null) return;
    store.removeItem(IDENTITY_KEY);
  } catch {
    return;
  }
  announce();
}

type ProfileNames = { email?: string; preferred_name?: string; legal_name?: string };

/** The name a profile shows for its owner, or null when it has none set. */
export function profileName(profile: ProfileNames): string | null {
  return profile.preferred_name?.trim() || profile.legal_name?.trim() || null;
}

/** Refresh the cache from a profile the BFF just returned for the signed-in caller. */
export function rememberProfile(profile: ProfileNames): void {
  if (!profile.email) return;
  writeIdentity(identityOf(profile.email, profileName(profile)));
}

/** Follow the cache — this document's writes and other tabs' alike. Returns the unsubscribe. */
export function subscribeIdentity(onChange: () => void): () => void {
  if (typeof window === "undefined") return () => undefined;
  const onStorage = (e: StorageEvent) => {
    if (e.key === IDENTITY_KEY || e.key === null) onChange();
  };
  window.addEventListener(IDENTITY_EVENT, onChange);
  window.addEventListener("storage", onStorage);
  return () => {
    window.removeEventListener(IDENTITY_EVENT, onChange);
    window.removeEventListener("storage", onStorage);
  };
}
