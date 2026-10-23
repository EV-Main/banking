// Display vocabulary for the activity timeline. Amounts are formatted by the cabinet's
// one money module (`@/shared/lib/money`); everything here is the mapping from a wire
// `kind`/`state` onto the badge, tone and wording the Figma `operations` screens use.
//
// The wording is carried as *catalogue keys*, not as English: this is a plain module and
// cannot call `useT()`, so a label resolves at the render site (`kindLabel`, `stateLabel`)
// or takes the translator as an argument. Tone, direction and the settled/failed sets stay
// literal — they are logic, not copy, and no locale changes what a withdrawal is.

import { ArrowDownLeft, ArrowUpRight, Ban, Check, Clock, type LucideIcon, Minus, Percent, Plus, RefreshCw, TriangleAlert, X } from "lucide-react";

import type { Locale, Translate } from "@evinvest/i18n";

import type { Operation } from "@/shared/contracts";
import { intlLocale } from "@/shared/lib/intl-locale";
import { wordFor } from "@/shared/lib/wire-words";

export { formatUnits, formatUsdt, shortAddress } from "@/shared/lib/money";
// Rails already have one display vocabulary on the wallet surface — a timeline row must
// name a network exactly as the wallet does, so it is re-exported, never re-declared.
export { networkLabel } from "@/views/wallet/lib/format";

export type OperationKind = "deposit" | "withdrawal" | "subscription" | "redemption";

/** The tab set, in the order the filter row presents them. */
export const KIND_FILTERS = ["deposit", "withdrawal", "subscription", "redemption", "fee"] as const;

// Sign policy. Only the movements that actually change what the user holds carry a
// sign: money arriving (deposit), money leaving (withdrawal), and the fund's fee, which
// takes units from the holder and does not give them back. Subscribing and redeeming move
// value *between* the wallet and a fund without either entering or leaving the account, so
// they render unsigned in the neutral tone — the Figma's `MOVE` row. Signing them would
// tell an investor that subscribing lost them money; NOT signing a fee would tell them a
// charge was free.
export type Direction = "in" | "out" | "move";

export interface KindMeta {
  /** The badge mark. `null` for a kind the hub added after this build, whose badge falls
   *  back to the wire id itself (see {@link kindBadge}) — the same degrade-to-text rule
   *  the rail marks use. The direction the arrow points is the direction of the money, so
   *  the badge stops depending on the reader knowing what `IN` and `BUY` were shorthand for. */
  icon: LucideIcon | null;
  /** The kind's name, or `null` for an unrecognised kind. */
  label: ((t: Translate) => string) | null;
  direction: Direction;
  /** Badge tint. Semantic tokens only — these are the accent tiers, not raw colour. */
  tone: string;
}

const KINDS: Record<string, KindMeta> = {
  // `ops.kind.deposit`, not `ui.deposit`: this titles a row in the timeline, where the
  // word is a noun ("a deposit"), while `ui.deposit` is the wallet button, where it is a
  // verb ("deposit funds"). English spells both "Deposit" and hid the difference; German
  // and Russian each have to pick one, and both translators raised it independently.
  deposit: { icon: ArrowDownLeft, label: (t) => t("ops.kind.deposit", "Deposit"), direction: "in", tone: "bg-positive/15 text-positive" },
  withdrawal: { icon: ArrowUpRight, label: (t) => t("ops.kind.withdrawal", "Withdrawal"), direction: "out", tone: "bg-accent-error/15 text-accent-error" },
  subscription: { icon: Plus, label: (t) => t("ops.kind.subscription", "Subscription"), direction: "move", tone: "bg-accent-debug/15 text-accent-debug" },
  redemption: { icon: Minus, label: (t) => t("ops.kind.redemption", "Redemption"), direction: "move", tone: "bg-accent-warn/15 text-accent-warn" },
  fee: { icon: Percent, label: (t) => t("ops.kind.fee", "Fee"), direction: "out", tone: "bg-accent-error/15 text-accent-error" },
};

const UNKNOWN_KIND: KindMeta = { icon: null, label: null, direction: "move", tone: "bg-muted text-ink-soft" };

/** An unrecognised kind renders neutrally rather than disappearing — a new hub kind is
 *  visible as an unstyled row instead of a silent gap in someone's history. */
export function kindMeta(kind: string | undefined): KindMeta {
  return KINDS[kind ?? ""] ?? UNKNOWN_KIND;
}

/** The text a badge falls back to when {@link KindMeta.icon} is `null` — a kind this
 *  build has no mark for. It wears its own wire id, which is an identifier rather than
 *  copy and so is never translated.
 *
 *  Never reached for a kind in {@link KINDS}: those all carry a mark. It exists so a kind
 *  the hub adds later is still a legible row rather than an empty badge. */
export function kindBadge(kind: string | undefined): string {
  const id = kind ?? "";
  return id.slice(0, 4).toUpperCase() || "—";
}

/** The kind's name. Same fallback rule as {@link kindBadge}: the wire id when there is
 *  one, and a generic noun only when the hub sent no kind at all. */
export function kindLabel(kind: string | undefined, t: Translate): string {
  const id = kind ?? "";
  const label = Object.hasOwn(KINDS, id) ? KINDS[id].label : null;
  return label ? label(t) : id || t("ops.kind.unknown", "Operation");
}

/** The amount colour that goes with a direction. Neutral moves keep the body colour. */
export function amountTone(direction: Direction): string {
  if (direction === "in") return "text-positive";
  if (direction === "out") return "text-accent-error";
  return "text-ink";
}

// `partly_deferred` is settled too: the charge itself completed, and what it could not
// collect became debt on the position rather than an operation still in flight.
const SETTLED = new Set(["credited", "completed", "charged", "partly_deferred"]);
const FAILED = new Set(["failed", "cancelled"]);

/** Still moving — the rows the "In progress" section lifts to the top. */
export function isPending(operation: Operation): boolean {
  const state = operation.state ?? "";
  return !SETTLED.has(state) && !FAILED.has(state);
}

// Every lifecycle state the hub sends, in words. It used to be the wire
// identifier with its underscores swapped for spaces, leaned on by a `capitalize` class —
// English-shaped twice over: `capitalize` left "Partly Deferred" on a compound state, and
// a translated label is not a lowercase identifier waiting to be title-cased.
const stateWords = (t: Translate): Readonly<Record<string, string>> => ({
  queued: t("ops.state.queued", "Queued"),
  processing: t("ops.state.processing", "Processing"),
  completed: t("ops.state.completed", "Completed"),
  credited: t("ops.state.credited", "Credited"),
  charged: t("ops.state.charged", "Charged"),
  partly_deferred: t("ops.state.partlyDeferred", "Partly deferred"),
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

/** Badge tint for a lifecycle state, matching the wallet activity screen's vocabulary. */
export function stateTone(state: string | undefined): string {
  switch (state) {
    case "queued":
      return "bg-accent-warn/15 text-accent-warn";
    case "processing":
      return "bg-accent-debug/15 text-accent-debug";
    case "completed":
    case "credited":
    case "charged":
      return "bg-positive/15 text-positive";
    // Part of the charge could not be collected and is carried to the next one — worth
    // the attention tint, since it is the only state where the row's figure is less than
    // what was actually assessed.
    case "partly_deferred":
      return "bg-accent-warn/15 text-accent-warn";
    case "failed":
      return "bg-accent-error/15 text-accent-error";
    default:
      return "bg-muted text-ink-soft";
  }
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
  charged: Check,
  // Same reading as its tint: settled, but for less than was assessed.
  partly_deferred: TriangleAlert,
  failed: X,
  cancelled: Ban,
};

/** Unix seconds on the wire arrive as a string (the BFF renders i64 as text so no
 *  client has to survive 2^53); `0`/absent means the hub never stamped one. */
export function seconds(value: string | number | undefined): number {
  const n = Number(value ?? 0);
  return Number.isFinite(n) ? n : 0;
}

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
