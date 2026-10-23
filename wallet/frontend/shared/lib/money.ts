// The cabinet's one money module. Amounts cross the wire as exact decimal strings (the
// authoritative value is the string); everything here is display-only — never feed a
// parsed number back into a money operation. The exact-math helpers at the bottom are the
// only place a money value is ever computed, and they work on bigint base units.
//
// Precision is a policy per unit of measure, not a per-screen choice, because the same
// amount used to render differently depending on which screen you were on:
//
//   summary money (USD)   exactly 2 dp  — a fund that rounds a P&L to the dollar reads
//                                         as an estimate rather than a statement
//   ledger money (USDT)   2–6 dp        — a wallet row has to show what actually moved
//   fund units (shares)   2–8 dp        — fractional shares read clearly
//   NAV per share         exactly 4 dp  — a NAV is a price; the day's move lives in the
//                                         fourth decimal
//
// Separators follow the reader's locale; the precision policy does not. A German reader
// sees "1.234,50" for the balance an English one sees as "1,234.50" — the same figure,
// and reading "1,234.50" with German separators is off by three orders of magnitude. What
// a locale is NOT allowed to touch is the unit: the "$" stays a bare "$" everywhere
// (`currencyDisplay: "narrowSymbol"`, never "US$"/"$US"), the "%" is appended as-is, and
// the sign column keeps its Unicode minus. Every display formatter takes the locale last
// and defaults to English, so a server path or a test that has no reader gets exactly the
// output this module has always produced.

import type { Locale } from "@evinvest/i18n";

/** A wire decimal parsed for on-screen arithmetic (sums, proportions). Display only. */
export function num(value: string | undefined): number {
  const n = Number(value ?? "0");
  return Number.isFinite(n) ? n : 0;
}

/** The locale a formatter falls back to when no reader is known: tests, server paths. */
export const DEFAULT_MONEY_LOCALE: Locale = "en";

// What `en` means to `Intl` here is `en-US`: that is the tag this module pinned before it
// learnt about locales, so English output is byte-for-byte what it was. (Dates map `en` to
// `en-GB` in `shared/lib/intl-locale.ts` — a different convention for a different unit.)
function intlTag(locale: Locale): string {
  return locale === "en" ? "en-US" : locale;
}

// `Intl.NumberFormat` is expensive to construct — hundreds of microseconds and a fresh
// locale-data lookup each time — and the order book, the tape and the holders table call
// these formatters per row on every tick. One instance per (locale, options) pair, built on
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

// Summary money is shown to the cent, always — a trailing "$1,234.5" breaks the column
// and a rounded "$85" loses the P&L.
const CENTS = { minimumFractionDigits: 2, maximumFractionDigits: 2 } as const;

/** Summary money: "$48,250.00", "$1,234.56". Dashboards, profile and the treasury bank line. */
export function formatUsd(value: string | number | undefined, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  const n = typeof value === "number" ? value : num(value);
  return numberFormat(locale, "usd", { style: "currency", currency: "USD", currencyDisplay: "narrowSymbol", ...CENTS }).format(n);
}

/** The same figure without the currency symbol, for columns that name the unit elsewhere. */
export function formatAmount(value: string | undefined, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  const n = Number(value ?? "0");
  if (!Number.isFinite(n)) return value ?? "0";
  return numberFormat(locale, "amount", CENTS).format(n);
}

// Signed summary money: "+$84.83" / "−$540.00" / "$0.00". The Unicode minus (U+2212) is
// the plus sign's mirror — a hyphen is narrower and the sign column stops lining up. Zero
// carries no sign: "+$0.00" claims a gain that did not happen.
export function formatSignedUsd(value: string | number | undefined, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  const n = typeof value === "number" ? value : num(value);
  const sign = n < 0 ? "−" : n > 0 ? "+" : "";
  return `${sign}${formatUsd(Math.abs(n), locale)}`;
}

/** NAV per share: "$1.0423" (Figma `cabinet/invest`). */
export function formatNav(value: string | number | undefined, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  const n = typeof value === "number" ? value : num(value);
  return numberFormat(locale, "nav", { style: "currency", currency: "USD", currencyDisplay: "narrowSymbol", minimumFractionDigits: 4, maximumFractionDigits: 4 }).format(n);
}

/**
 * A wire decimal shown EXACTLY as it arrived, grouped for reading: "1,234.50",
 * "1,000.0000005", "0.000000000000000001".
 *
 * The other formatters here parse to a `number` first, which is right for a balance — a
 * float is plenty for a figure the reader is only comparing against other figures, and
 * capping the fraction is what keeps a column aligned. It is wrong for an amount the reader
 * is *authorizing*: `formatUsdt` caps at 6 dp, so an 18-decimal value is silently rounded on
 * screen, and `Number()` itself loses digits past ~15 significant figures.
 *
 * That matters on exactly one surface and it matters absolutely. `payload_hash` is computed
 * over the exact decimal string and re-verified before the money moves, so what an owner
 * approves must be what the hash covers, character for character (docs/CONSILIUM.md,
 * policy 12). An approval screen that rounds is an approval of something else.
 *
 * No float anywhere: the integer part is grouped through `BigInt`, which has no precision
 * ceiling, and the fraction is passed through untouched apart from being padded to the two
 * decimals every money figure in this cabinet shows. The locale only chooses the separators
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

// Signed USDT (P&L): "+1,234.50" / "−5.00" / "0.00". The sign is read off the wire string
// exactly, never off a float; the same Unicode minus as the signed USD, and like it no sign
// on zero — "+0.00" claims a gain that did not happen.
export function formatSignedUsdt(value: string | undefined, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  const s = (value ?? "0").trim();
  const v = valence(s);
  const formatted = formatUsdt(s.replace(/^-/, ""), locale);
  return `${v === "loss" ? "\u2212" : v === "gain" ? "+" : ""}${formatted}`;
}

// A signed figure is one of three things to the eye, and every investor screen colours
// them the same way: `positive` for a gain, `accent-error` for a loss, and the plain ink for
// a flat figure. A zero drawn green is the bug this exists to close — it used to be "a gain"
// on one screen and unstyled on the next. Strings are read exactly (no float); numbers and
// bigints by sign.
export type Valence = "gain" | "loss" | "flat";

export function valence(value: number | bigint | string | undefined): Valence {
  if (typeof value === "string" || value === undefined) {
    if (isZero(value)) return "flat";
    return isNegative(value) ? "loss" : "gain";
  }
  if (value < 0) return "loss";
  return value > 0 ? "gain" : "flat";
}

/** The text colour of each valence — for a caller that already holds the valence. */
export const VALENCE_CLASS: Readonly<Record<Valence, string>> = { gain: "text-positive", loss: "text-accent-error", flat: "text-ink" };
/** The outline that goes with it, for a pill or badge carrying a signed figure. */
export const VALENCE_BORDER_CLASS: Readonly<Record<Valence, string>> = { gain: "border-positive/40", loss: "border-accent-error/40", flat: "border-border" };

/** The text colour for a signed figure — the one gain/loss pair, neutral at zero. */
export function valenceClass(value: number | bigint | string | undefined): string {
  return VALENCE_CLASS[valence(value)];
}

// Fund units (shares) — same dimension as USDT for display, but no currency suffix and a
// touch more precision so fractional shares read clearly.
export function formatUnits(value: string | undefined, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  const n = Number(value ?? "0");
  if (!Number.isFinite(n)) return value ?? "0";
  return numberFormat(locale, "units", { minimumFractionDigits: 2, maximumFractionDigits: 8 }).format(n);
}

/** Signed percentage: "+4.2%" / "−1.8%" / "0.0%", same Unicode minus as the signed money
 *  and, like it, no sign on zero. The "%" is appended rather than left to `Intl`'s percent
 *  style, which pads it with a locale space ("4,2 %") that the figures beside it do not use. */
export function formatPct(value: number, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  const digits = numberFormat(locale, "pct", { minimumFractionDigits: 1, maximumFractionDigits: 1 }).format(Math.abs(value));
  const sign = value < 0 ? "−" : value > 0 ? "+" : "";
  return `${sign}${digits}%`;
}

/** An unsigned percentage in the locale's own percent form — "16.4%" / "16,4 %" (the space
 *  is Intl's NBSP). The form the landing writes its headline figures in, unlike `formatPct`,
 *  whose "%" hugs the digits to match the signed money beside it. */
export function formatPlainPct(value: number, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  return numberFormat(locale, "pct-plain", { style: "percent", minimumFractionDigits: 1, maximumFractionDigits: 1 }).format(value / 100);
}

/** A whole number in the locale's digits and grouping — "1,000" / "1 000". */
export function formatWhole(value: number, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  return numberFormat(locale, "whole", { maximumFractionDigits: 0 }).format(value);
}

// A decimal amount as the wire carries it. No sign, no exponent, no grouping, no bare
// point — the hub parses this with the same strictness, so admitting less here only moves
// the refusal. Stricter than `toBaseUnits`, which tolerates `.5` for display math.
const WIRE_DECIMAL = /^\d+(\.\d+)?$/;

export function isWireDecimal(raw: string): boolean {
  return WIRE_DECIMAL.test(raw.trim());
}

/** A wire decimal with at least one non-zero digit — what an amount field must hold. */
export function isPositiveWireDecimal(raw: string): boolean {
  return isWireDecimal(raw) && /[1-9]/.test(raw);
}

// Whether a signed decimal P&L string is negative (a loss) — exact, no float.
export function isNegative(value: string | undefined): boolean {
  return (value ?? "").trim().startsWith("-");
}

// Whether a decimal P&L string is exactly zero — neither a gain nor a loss for colour.
export function isZero(value: string | undefined): boolean {
  return toBaseUnits((value ?? "").trim().replace(/^-/, "")) === 0n;
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

// Unit counts get large — a hundred-million-unit cap is unreadable written out, and the
// figure it is compared against has to be readable at the same glance. Compacts to 3
// significant figures ("21.0M", "940K", "1.5B") and only above a thousand, so a fund
// sized to 500 units still reads as "500".
export function compactUnits(value: string | undefined, locale: Locale = DEFAULT_MONEY_LOCALE): string {
  const n = num(value);
  if (!Number.isFinite(n) || n < 1000) return formatUnits(value, locale);
  for (const [scale, suffix] of [[1e12, "T"], [1e9, "B"], [1e6, "M"], [1e3, "K"]] as const) {
    if (n >= scale) {
      const scaled = n / scale;
      const rounded = scaled >= 100 ? Math.round(scaled) : Number(scaled.toFixed(scaled >= 10 ? 1 : 2));
      // Up to two decimals with trailing zeros dropped — the `Number(toFixed)` above already
      // did the rounding; the formatter only writes the locale's separator.
      return `${numberFormat(locale, "compact", { maximumFractionDigits: 2 }).format(rounded)}${suffix}`;
    }
  }
  return formatUnits(value, locale);
}

// What fraction of `cap` is taken by `issued`, 0–1, for a progress bar. Exact bigint
// division — a 1e26 cap overflows a float's integer precision, so
// `Number(issued) / Number(cap)` would quietly lie at the sizes this feature is for.
//
// The scale is 1e12 rather than basis points because these caps are large: one unit of a
// hundred-million-unit fund is 1e-8, which basis-point division floors to a flat zero.
// That is fine for a bar (it is invisible either way) and wrong for anything that asks
// this function a question about a small holding.
const CAP_FRACTION_SCALE = 1_000_000_000_000n;

export function fractionOfCap(issued: string | undefined, cap: string | undefined): number {
  const a = toBaseUnits(issued);
  const b = toBaseUnits(cap);
  if (b <= 0n) return 0;
  if (a >= b) return 1;
  return Number((a * CAP_FRACTION_SCALE) / b) / Number(CAP_FRACTION_SCALE);
}

// One holder class's share of a supply, in the basis points `pct()` (`./rate`) renders —
// "Company stake 12.5%" on the product card, the holders split on the admin panel. Exact
// bigint division for the same reason `fractionOfCap` is; the floor to a whole basis
// point is deliberate, since a stake under 0.01% of the fund reads as 0% and should.
export function shareBps(part: string | undefined, whole: string | undefined): number {
  const a = toBaseUnits(part);
  const b = toBaseUnits(whole);
  if (a <= 0n || b <= 0n) return 0;
  if (a >= b) return 10_000;
  return Number((a * 10_000n) / b);
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
