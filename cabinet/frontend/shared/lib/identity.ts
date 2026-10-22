// Name and initials derived from an account's email — the fallback identity every surface
// that shows a person uses before (or instead of) a set display name.
//
// In `shared/` for the same reason as `shared/lib/rail.ts`: the owners' roster and the
// account chip both show a person, and a `views/*` slice reaching into another one is a
// same-layer dependency.

import type { Translate } from "@evinvest/i18n";

/** Max chars a display name takes up before ellipsis in the chip and headings. */
const MAX_DISPLAY = 32;

/** Truncate a profile/real name for display so an overlong value doesn't break
 *  the header or chip layout. */
export function truncateName(name: string, max: number = MAX_DISPLAY): string {
  if (name.length <= max) return name;
  return `${name.slice(0, max - 1)}…`;
}

/** A human display name from an email handle: "ada.lovelace@x" → "Ada L."
 *  Takes the translator for its one worded fallback — the caller is a client
 *  component that already holds one, so this module stays React-free. */
export function displayName(email: string | null | undefined, t: Translate): string {
  if (email === undefined) return "…";
  if (!email) return t("ui.account", "Account");
  const handle = email.split("@")[0] ?? email;
  const parts = handle.split(/[._-]+/).filter(Boolean);
  const first = parts[0] ? cap(parts[0]) : handle;
  const initial = firstGrapheme(parts[1]);
  const last = initial ? `${initial.toUpperCase()}.` : "";
  return [first, last].filter(Boolean).join(" ");
}

/** Up-to-two-letter initials: "ada.lovelace@x" → "AL". */
export function initialsOf(email: string | null | undefined): string {
  if (!email) return "EV";
  const parts = (email.split("@")[0] ?? "").split(/[._-]+/).filter(Boolean);
  const a = firstGrapheme(parts[0]) ?? firstGrapheme(email) ?? "E";
  const b = firstGrapheme(parts[1]) ?? "";
  return (a + b).toUpperCase();
}

function cap(s: string): string {
  const first = firstGrapheme(s) ?? "";
  return first.toUpperCase() + s.slice(first.length);
}

const GRAPHEMES = new Intl.Segmenter(undefined, { granularity: "grapheme" });

/**
 * The first user-perceived character, not `s[0]`: that is half a surrogate pair for an
 * emoji or astral letter (a lone surrogate the avatar renders as U+FFFD and that is not
 * well-formed for `encodeURIComponent` or storage), and drops the accent of a decomposed "é".
 */
function firstGrapheme(s: string | undefined): string | undefined {
  if (!s) return undefined;
  for (const { segment } of GRAPHEMES.segment(s)) return segment;
  return undefined;
}

/** Initials from a resolved display name ("Fen Astra" → "FA"), falling back to the
 *  email heuristic while the name is still unknown. */
export function initialsOfName(name: string, email: string | null | undefined): string {
  const parts = name.replace(/@.*/, "").split(/[\s._-]+/).filter(Boolean);
  const a = firstGrapheme(parts[0]);
  if (!a) return initialsOf(email);
  return (a + (firstGrapheme(parts[1]) ?? "")).toUpperCase();
}
