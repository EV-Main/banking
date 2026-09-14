// Name and initials derived from an account's email — the fallback identity every surface
// that shows a person uses before (or instead of) a set display name.
//
// Moved down from `views/settings/lib/format.ts` for the same reason as `shared/lib/rail.ts`:
// the owners' roster shows a person too, and a `views/*` slice reaching into another one is
// a same-layer dependency. `views/settings/lib/format.ts` re-exports these, so the settings,
// profile and account-chip call sites are unchanged.

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
  const last = parts[1] ? `${parts[1][0]!.toUpperCase()}.` : "";
  return [first, last].filter(Boolean).join(" ");
}

/** Up-to-two-letter initials: "ada.lovelace@x" → "AL". */
export function initialsOf(email: string | null | undefined): string {
  if (!email) return "EV";
  const parts = (email.split("@")[0] ?? "").split(/[._-]+/).filter(Boolean);
  const a = parts[0]?.[0] ?? email[0] ?? "E";
  const b = parts[1]?.[0] ?? "";
  return (a + b).toUpperCase();
}

function cap(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

/** Initials from a resolved display name ("Fen Astra" → "FA"), falling back to the
 *  email heuristic while the name is still unknown. */
export function initialsOfName(name: string, email: string | null | undefined): string {
  const parts = name.replace(/@.*/, "").split(/[\s._-]+/).filter(Boolean);
  const a = parts[0]?.[0];
  if (!a) return initialsOf(email);
  return (a + (parts[1]?.[0] ?? "")).toUpperCase();
}
