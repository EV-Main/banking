// Fee rates, in the two units the cabinet speaks.
//
// The money plane stores and charges **basis points** — integers, where 10000 is 100% —
// because a rate that charges real money must be exact, and a fraction of a percent held
// as a float is not. Every human surface, though, states a rate the way a term sheet does:
// "2 and 20". So the conversion happens exactly twice, here, and both directions are
// tested — the admin form writes a policy through `toBps`, and every screen that reads one
// back displays it through `pct`.
//
// Why this is not in `money.ts`: a rate is not an amount. It never crosses the wire as a
// decimal string, it has its own resolution (two decimal places, no more and no less), and
// mixing it into the amount formatters is how a rate ends up rendered to the cent.

/** Basis points as a human reads them: 200 → "2%", 250 → "2.5%", 0 → "0%". */
export function pct(bps: number | undefined): string {
  const value = (bps ?? 0) / 100;
  return `${Number.isInteger(value) ? value : Number(value.toFixed(2))}%`;
}

/** What a percent field admits. One basis point is 0.01%, so two decimal places is the
 *  whole resolution the money plane has — a third would be a rate it cannot store. The
 *  fraction is optional down to nothing so a half-typed "2." does not flash an error at
 *  someone in the middle of typing "2.5". Either separator on the way IN: four of the
 *  cabinet's five locales write a half as "2,5". The way OUT (`pct`, `toPercentInput`)
 *  stays the point in every locale — one policy here, and the hint under the field names
 *  both spellings rather than teaching a separator the figures beside it do not use.
 *
 *  No cap on the digits: "5001" is a well-formed percentage that is far too large, and the
 *  caller wants to say "cannot be more than 5%" about it, not "not a percentage". */
const PERCENT = /^\d+([.,]\d{0,2})?$/;

/** A percent field → the basis points the wire carries, or `null` if it is not a percent.
 *
 *  Integer arithmetic on the digits, never `Number(raw) * 100`: in binary floating point
 *  20.05 × 100 is 2004.9999999999998, and a rate is not a place to round and hope.
 *
 *  The upper bound is deliberately *not* enforced here. The domain caps every rate at
 *  10000 bps (`FeePolicy::new`), and a caller that refuses "150" wants to say *why* — "not
 *  a percentage" and "more than 100%" are different mistakes and want different words. */
export function toBps(percent: string): number | null {
  const raw = percent.trim();
  if (!PERCENT.test(raw)) return null;
  const [whole, fraction = ""] = raw.split(/[.,]/);
  return Number(whole) * 100 + Number(fraction.padEnd(2, "0"));
}

/** Basis points → what a percent field shows: 200 → "2", 250 → "2.5", 205 → "2.05".
 *  Trailing zeros come off so the ordinary round rate reads as the round number it is. */
export function toPercentInput(bps: number): string {
  const whole = Math.floor(bps / 100);
  const fraction = String(bps % 100)
    .padStart(2, "0")
    .replace(/0+$/, "");
  return fraction === "" ? String(whole) : `${whole}.${fraction}`;
}
