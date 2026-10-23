/**
 * Read one field off a body of unknown shape.
 *
 * Every body this slice reads comes off the wire, so they are probed rather than asserted:
 * `as StartResponse` on a `redirect_url` that arrived as `null` would put `"null"` in
 * `window.location`. The narrowers here are the whole dependency budget of that probing —
 * a runtime schema library would be a new package in `package.json` for four shapes.
 */
export function readString(body: unknown, key: string): string | null {
  const value = readField(body, key);
  return typeof value === "string" && value.length > 0 ? value : null;
}

/**
 * Finite numbers only. `created_at` arrives as unix SECONDS; `NaN` or `Infinity` would
 * survive a `typeof` check and reach a date formatter as "Invalid Date".
 */
export function readNumber(body: unknown, key: string): number | null {
  const value = readField(body, key);
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/** Strict: a missing `resumable` is not `false`, it is an answer we did not get. */
export function readBoolean(body: unknown, key: string): boolean | null {
  const value = readField(body, key);
  return typeof value === "boolean" ? value : null;
}

/**
 * The raw value at `key`, with `undefined` reserved for "the key is not there".
 *
 * The three narrowers above all answer `null` for both "absent" and "present but the wrong
 * type", which is right when either one means the same thing to the caller. It does not when
 * a key is OPTIONAL on the wire: `/kyc/status` sends `case: null` for "no attempt is running"
 * and an object for one that is, so a reader has to tell an absent `case` (not a status
 * document), a null one (nothing running) and a malformed one (drift) apart — three answers,
 * which no `T | null` can carry. Callers that need that distinction branch on this directly.
 */
export function readField(body: unknown, key: string): unknown {
  if (typeof body !== "object" || body === null || !(key in body)) return undefined;
  return (body as Record<string, unknown>)[key];
}
