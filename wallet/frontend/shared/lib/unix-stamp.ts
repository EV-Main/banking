// What a timestamp IS on this wire, in one place.
//
// Every timestamp in the contract is an `int64` of unix SECONDS — see
// `banking/v1/consilium.proto`, "Unix seconds; 0 while pending" — and the BFF hands it to
// the browser through `.to_string()`, so what actually arrives is a decimal string like
// "1757000000", with "0" standing for "no stamp yet".
//
// This module exists because two callers had drifted on that. `shared/lib/datetime.ts`
// parsed the value as RFC 3339, which no caller was ever given: `new Date("1757000000")`
// is an Invalid Date, so every date in the owners' room, on both emailed approval pages
// and on the removal/admission cards rendered "—". Nothing caught it — both shapes are
// `string`, so neither `tsc` nor eslint had anything to object to, and the only thing that
// showed was a dash where a date belonged.
//
// Deliberately import-free. That keeps it testable: `node --test` does not resolve the
// `@/` alias for value imports, so a module that reaches for one cannot be exercised
// directly, and the rule that broke would have stayed untested exactly where it hurt.

/**
 * Unix seconds (as a string) → `Date`, or null when there is no stamp.
 *
 * `> 0` is the absence test, not a sanity check: the contract writes 0 for "not decided
 * yet", so 0 must never surface as 1 Jan 1970 on a card.
 */
export function unixStampToDate(stamp: string | null | undefined): Date | null {
  if (!stamp) return null;
  const seconds = Number(stamp);
  return Number.isFinite(seconds) && seconds > 0 ? new Date(seconds * 1000) : null;
}

/**
 * Whether a stamp is present at all.
 *
 * Needed because the absent stamp is the STRING "0", which is truthy: `decided_at ??
 * created_at` silently keeps the "0" and renders "—" instead of falling back. Anywhere a
 * caller wants "this one, else that one", it has to ask this rather than lean on `??`.
 */
export function hasUnixStamp(stamp: string | null | undefined): boolean {
  return unixStampToDate(stamp) !== null;
}
