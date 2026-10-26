// Display vocabulary for the activity timeline: the mapping from a wire `state` onto the
// badge mark and wording, and the day/time labels rows are grouped by. The wording is
// carried as *catalogue keys*: this is a plain module and cannot call `useT()`, so a label
// takes the translator as an argument.

import { Ban, Check, Clock, type LucideIcon, RefreshCw, X } from "lucide-react";

import type { Locale, Translate } from "@evinvest/i18n";

import { intlLocale } from "@/shared/lib/intl-locale";
import { wordFor } from "@/shared/lib/wire-words";

const stateWords = (t: Translate): Readonly<Record<string, string>> => ({
  queued: t("ops.state.queued", "Queued"),
  processing: t("ops.state.processing", "Processing"),
  completed: t("ops.state.completed", "Completed"),
  credited: t("ops.state.credited", "Credited"),
  failed: t("ops.state.failed", "Failed"),
  cancelled: t("ops.state.cancelled", "Cancelled"),
});

/** A lifecycle state as a human reads it. An unmapped state falls back to the wire
 *  identifier rather than an empty badge, so a new hub state is visible, not invisible.
 *
 *  i18n-max: 12 — badge in a `shrink-0` column; a longer label eats the row title. */
export function stateLabel(state: string | undefined, t: Translate): string {
  const id = state ?? "";
  return wordFor(stateWords(t), id) ?? id.replace(/_/g, " ");
}

/** The mark that goes with a lifecycle state. Colour alone cannot carry a state — it is
 *  invisible to a red/green-blind reader and to anyone on a monochrome display — so the
 *  badge's tint is doubled by a shape. The word is always there too; the mark is
 *  reinforcement, which is why every call site renders it `aria-hidden`.
 *
 *  A map rather than a `stateIcon()` accessor on purpose: `react-hooks/static-components`
 *  rejects a component bound from a *call* during render (it cannot tell a lookup from a
 *  freshly built component), while an index into a module-level table is accepted — and
 *  the table is what this always was.
 *
 *  A state that is missing here has no mark, matching {@link stateLabel}'s fallback: the
 *  badge shows the wire identifier as text rather than an arbitrary shape. */
export const STATE_ICONS: Record<string, LucideIcon> = {
  queued: Clock,
  processing: RefreshCw,
  completed: Check,
  credited: Check,
  failed: X,
  cancelled: Ban,
};

// Grouping and time labels are computed in the viewer's own zone. The timeline is now
// rendered on the server too, so every helper takes the zone explicitly (`useTimeZone`):
// the server and the hydrating browser must agree on which day a row belongs to, or the
// rows would regroup under different headings. Omitted, it is the runtime's own zone.
const DAY_MS = 86_400_000;

// One formatter per zone: `daysAgo` runs twice per row, and building an
// `Intl.DateTimeFormat` is the expensive part of it.
const DAY_PARTS = new Map<string, Intl.DateTimeFormat>();

/** The calendar day `date` falls on in `timeZone`, as a day count — comparable by subtraction. */
function dayNumber(date: Date, timeZone: string | undefined): number {
  // No zone means the runtime's current one, which can change between calls — caching it
  // would let a day heading disagree with the row's own `timeLabel`, so only named zones are kept.
  let format = timeZone === undefined ? undefined : DAY_PARTS.get(timeZone);
  if (!format) {
    format = new Intl.DateTimeFormat("en-US", { timeZone, year: "numeric", month: "numeric", day: "numeric" });
    if (timeZone !== undefined) DAY_PARTS.set(timeZone, format);
  }
  const parts = format.formatToParts(date);
  const part = (type: Intl.DateTimeFormatPartTypes) => Number(parts.find((p) => p.type === type)?.value);
  return Date.UTC(part("year"), part("month") - 1, part("day")) / DAY_MS;
}

function daysAgo(unixSeconds: number, now: Date, timeZone: string | undefined): number {
  return dayNumber(now, timeZone) - dayNumber(new Date(unixSeconds * 1000), timeZone);
}

function calendarDate(unixSeconds: number, locale: Locale, timeZone: string | undefined): string {
  return new Date(unixSeconds * 1000).toLocaleDateString(intlLocale(locale), { day: "numeric", month: "short", year: "numeric", timeZone });
}

/** The date heading a run of rows sits under: `Today`, `Yesterday`, or `12 Mar 2026`. */
export function dayLabel(unixSeconds: number, t: Translate, locale: Locale, now: Date = new Date(), timeZone?: string): string {
  if (!unixSeconds) return t("ops.day.undated", "Undated");
  const days = daysAgo(unixSeconds, now, timeZone);
  if (days === 0) return t("ops.day.today", "Today");
  if (days === 1) return t("ops.day.yesterday", "Yesterday");
  return calendarDate(unixSeconds, locale, timeZone);
}

/** The same day, worded to sit mid-sentence after a rail name: "TON · today 14:32".
 *
 *  Its own keys rather than `dayLabel(...).toLowerCase()`. Lower-casing a *translated*
 *  word is wrong in German, where a noun is capitalised in every position, and it also
 *  mangled the dated case into "12 mar 2026" — a calendar date is not a word, so it is
 *  left exactly as the locale formatted it. */
export function dayLabelInline(unixSeconds: number, t: Translate, locale: Locale, now: Date = new Date(), timeZone?: string): string {
  if (!unixSeconds) return t("ops.day.undated", "Undated");
  const days = daysAgo(unixSeconds, now, timeZone);
  if (days === 0) return t("ops.day.todayInline", "today");
  if (days === 1) return t("ops.day.yesterdayInline", "yesterday");
  return calendarDate(unixSeconds, locale, timeZone);
}

/** The clock time on a row — the day is already carried by its group heading. */
export function timeLabel(unixSeconds: number, locale: Locale, timeZone?: string): string {
  if (!unixSeconds) return "—";
  return new Date(unixSeconds * 1000).toLocaleTimeString(intlLocale(locale), { hour: "2-digit", minute: "2-digit", timeZone });
}
