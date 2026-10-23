// Idempotency for the funnel's "first"/"once" events, kept in the browser rather than asked
// of the backend: the funnel is a client-side join with the landing session, and an event
// fired twice for one person costs a dashboard nothing while a round-trip per event would.
//
// Two scopes. `tab` (sessionStorage) survives reloads and the round-trip to a vendor's page
// but not a closed tab — right for "once per session". `browser` (localStorage) is for the
// "first ever" steps; callers key those by something that identifies the account (a user
// id, a tx ref) so the next account on the same machine is not silenced by the previous one.
//
// Storage throws outright in a Safari private window and when site data is blocked, and a
// blank cabinet is not a price worth paying for a metric — so every access is wrapped, and
// the fallback is an in-memory set that still holds for the life of the page.

export type MarkScope = "tab" | "browser";

const PREFIX = "ev.analytics.mark:";

const memory = new Set<string>();

function storage(scope: MarkScope): Storage | null {
  try {
    return scope === "tab" ? window.sessionStorage : window.localStorage;
  } catch {
    return null;
  }
}

/** Whether `key` has been marked in this scope. */
export function marked(key: string, scope: MarkScope = "tab"): boolean {
  if (memory.has(`${scope}:${key}`)) return true;
  try {
    return storage(scope)?.getItem(PREFIX + key) !== null;
  } catch {
    return false;
  }
}

/** Mark `key` in this scope. */
export function mark(key: string, scope: MarkScope = "tab"): void {
  memory.add(`${scope}:${key}`);
  try {
    storage(scope)?.setItem(PREFIX + key, "1");
  } catch {
    // Full, blocked or unavailable: the in-memory set still holds for this page.
  }
}

/** Forget `key` in this scope. */
export function unmark(key: string, scope: MarkScope = "tab"): void {
  memory.delete(`${scope}:${key}`);
  try {
    storage(scope)?.removeItem(PREFIX + key);
  } catch {
    // See `mark`.
  }
}

/** `true` exactly once per scope for a given key — the caller fires its event on `true`. */
export function once(key: string, scope: MarkScope = "tab"): boolean {
  if (marked(key, scope)) return false;
  mark(key, scope);
  return true;
}

/** Test seam: drop the in-memory fallback so each case starts from a cold page. */
export function resetMarksForTests(): void {
  memory.clear();
}
