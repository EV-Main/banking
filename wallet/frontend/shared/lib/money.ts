// The wallet host's one money module. Amounts cross the wire as exact decimal strings (the
// authoritative value is the string); everything here is display-only — never feed a
// parsed number back into a money operation. The exact-math helpers at the bottom are the
// only place a money value is ever computed, and they work on bigint base units.
//
// Ledger USDT shows 2–6 dp: a wallet row has to show what actually moved. Separators follow
// the reader's locale; the precision does not. A German reader sees "1.234,50" for the
// balance an English one sees as "1,234.50" — the same figure, and reading "1,234.50" with
// German separators is off by three orders of magnitude. The sign keeps its Unicode minus.
// Every display formatter takes the locale last and defaults to English.

import type { Locale } from "@evinvest/i18n";

/** The locale a formatter falls back to when no reader is known: tests, server paths. */
const DEFAULT_MONEY_LOCALE: Locale = "en";

// What `en` means to `Intl` here is `en-US`: that is the tag this module pinned before it
// learnt about locales, so English output is byte-for-byte what it was. (Dates map `en` to
// `en-GB` in `shared/lib/intl-locale.ts` — a different convention for a different unit.)
function intlTag(locale: Locale): string {
  return locale === "en" ? "en-US" : locale;
}

// `Intl.NumberFormat` is expensive to construct — hundreds of microseconds and a fresh
// locale-data lookup each time — and the activity lists call these formatters per row on
// every render. One instance per (locale, options) pair, built on
// first use and kept for the life of the module; the key space is five locales times a
// handful of option shapes, so the cache never needs eviction.
const FORMATTERS = new Map<string, Intl.NumberFormat>();

function numberFormat(locale: Locale, key: string, options: Intl.NumberFormatOptions): Intl.NumberFormat {
  const cacheKey = `${locale}|${key}`;
  let f = FORMATTERS.get(cacheKey);
  if (!f) {
    f = new Intl.NumberFormat(intlTag(locale), options);
    FORMATTERS.set(cacheKey, f);
  }
  return f;
}

/** The decimal separator the locale writes — "." for `en`, "," for the other four. */
function decimalSeparator(locale: Locale): string {
  return numberFormat(locale, "decimal-probe", { maximumFractionDigits: 1 }).formatToParts(1.1).find((p) => p.type === "decimal")?.value ?? ".";
}

/**
 * A wire decimal shown EXACTLY as it arrived, grouped for reading: "1,234.50",
 * "1,000.0000005", "0.000000000000000001".
 *
 * `formatUsdt` parses to a `number` first, which is right for a balance — a
 * float is plenty for a figure the reader is only comparing against other figures, and
 * capping the fraction is what keeps a column aligned. It is wrong for an amount the reader
 * is *authorizing*: `formatUsdt` caps at 6 dp, so an 18-decimal value is silently rounded on
 * screen, and `Number()` itself loses digits past ~15 significant figures.
 *
 * That matters on exactly one surface and it matters absolutely. `payload_hash` is computed
 * over the exact decimal string and re-verified before the money moves, so what an owner
 * approves must be what the hash covers, character for character. An approval screen that
 * rounds is an approval of something else.
 *
 * No float anywhere: the integer part is grouped through `BigInt`, which has no precision
 * ceiling, and the fraction is passed through untouched apart from being padded to the two
 * decimals every money figure in this wallet host shows. The locale only chooses the separators
 * — the digits themselves never pass through anything that could round them.
 */
export function formatExactUsdt(value: string | undefined, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  const raw = (value ?? "").trim();
  // Anything that is not a plain decimal is returned as-is rather than coerced: a caller
  // showing an unrecognised value verbatim is honest, one showing "0.00" for it is not.
  if (!/^-?\d*\.?\d*$/.test(raw) || raw === "" || raw === "." || raw === "-" || raw === "-.") return raw || "—";
  const negative = raw.startsWith("-");
  const [intRaw = "", fracRaw = ""] = raw.replace(/^-/, "").split(".");
  const grouped = numberFormat(locale, "exact-int", { maximumFractionDigits: 0 }).format(BigInt(intRaw || "0"));
  const frac = fracRaw.length < 2 ? fracRaw.padEnd(2, "0") : fracRaw;
  return `${negative ? "\u2212" : ""}${grouped}${decimalSeparator(locale)}${frac}`;
}

/** Ledger USDT: "1,234.50", "0.000001". No currency symbol — the unit is spelled out. */
export function formatUsdt(value: string | number | undefined, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  const n = typeof value === "number" ? value : Number(value ?? "0");
  if (!Number.isFinite(n)) return typeof value === "string" ? value : "0";
  return numberFormat(locale, "usdt", { minimumFractionDigits: 2, maximumFractionDigits: 6 }).format(n);
}

// A decimal amount as the wire carries it. No sign, no exponent, no grouping, no bare
// point — the hub parses this with the same strictness, so admitting less here only moves
// the refusal. Stricter than `toBaseUnits`, which tolerates `.5` for display math.
const WIRE_DECIMAL = /^\d+(\.\d+)?$/;

/** A wire decimal with at least one non-zero digit — what an amount field must hold. */
export function isPositiveWireDecimal(raw: string): boolean {
  return WIRE_DECIMAL.test(raw.trim()) && /[1-9]/.test(raw);
}

// Exact 18-decimal USDT math on decimal strings (display-only previews). Avoids the
// float error Number() introduces on 18-dp values — money stays exact end to end.
export const USDT_DECIMALS = 18;

export function toBaseUnits(value: string | undefined): bigint {
  const s = (value ?? "").trim();
  if (!s || !/^\d*\.?\d*$/.test(s)) return 0n;
  const [int = "0", frac = ""] = s.split(".");
  const fracPadded = (frac + "0".repeat(USDT_DECIMALS)).slice(0, USDT_DECIMALS);
  try {
    return BigInt(int || "0") * 10n ** BigInt(USDT_DECIMALS) + BigInt(fracPadded || "0");
  } catch {
    return 0n;
  }
}

export function fromBaseUnits(units: bigint): string {
  const scale = 10n ** BigInt(USDT_DECIMALS);
  const abs = units < 0n ? -units : units;
  const int = abs / scale;
  const frac = (abs % scale).toString().padStart(USDT_DECIMALS, "0").replace(/0+$/, "");
  return `${units < 0n ? "-" : ""}${int}${frac ? `.${frac}` : ""}`;
}

// Saturating `a - b` over decimal USDT strings, returned as a decimal string (never < 0).
export function subUsdt(a: string | undefined, b: string | undefined): string {
  const r = toBaseUnits(a) - toBaseUnits(b);
  return fromBaseUnits(r < 0n ? 0n : r);
}

// How much of an address survives truncation. The wallet (the default) keeps enough of a
// deposit address to check it against a wallet app; the dashboard's activity lines have
// far less room and cut harder. `min` rides along because each screen also picked its own
// length below which an address is left whole.
export interface AddressCut {
  head?: number;
  tail?: number;
  min?: number;
}

export function shortAddress(address: string | undefined, cut: AddressCut = {}): string {
  const { head = 8, tail = 6, min = 18 } = cut;
  if (!address) return "—";
  return address.length > min ? `${address.slice(0, head)}…${address.slice(-tail)}` : address;
}
