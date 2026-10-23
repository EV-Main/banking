// A fund's fee terms, and the rules the money plane holds a CHANGE of them to.
//
// The five fields are one statement — "2 and 20 over a 5% hurdle, on invested capital,
// annually" — and the plane never edits one leg in place (docs/FEES.md § Changing the
// terms). Three of its rules are mirrored here, not because the browser decides anything,
// but because the form has to say BEFORE the click whether the owners will be asked and
// whether a reason is therefore required. The plane's answer is the one that binds; a
// mirror that disagreed with it would only mislead, so each rule below is a transcription
// of `domain/src/fees.rs` and is pinned by `fee-terms.test.ts`.
//
// Deliberately import-free (type imports aside), for the reason `unix-stamp.ts` gives:
// `node --test` does not resolve the `@/` alias, and the rule that decides who must agree
// to a price is exactly the one that must stay testable.

import type { Translate } from "@evinvest/i18n";

import { wordFor } from "./wire-words.ts";
/** The schedule a fund charges by. Rates in basis points; the two words are the plane's
 *  closed vocabularies (`BASES`, `CRYSTALLIZATIONS`), carried as open strings so a member
 *  added later renders as its wire word rather than breaking the build. */
export interface FeeTermsLike {
  management_bps: number;
  performance_bps: number;
  hurdle_bps: number;
  basis: string;
  crystallization: string;
}

/** `MAX_MANAGEMENT_BPS` in the domain: 5% p.a. A rate above it is a fat finger, not a term. */
export const MAX_MANAGEMENT_BPS = 500;
/** `MAX_PERFORMANCE_BPS`: half the gain. */
export const MAX_PERFORMANCE_BPS = 5_000;
/** The hurdle only ever lowers the fee, so it keeps the arithmetic cap of 100%. */
export const MAX_HURDLE_BPS = 10_000;

/** Below which a change of terms, once scheduled, may bind while anyone holds units. */
export const MIN_NOTICE_SECS = 24 * 60 * 60;

/** `MAX_EFFECTIVE_FROM_HORIZON_SECS`: the furthest ahead a change may be asked to bind. */
export const MAX_EFFECTIVE_FROM_HORIZON_SECS = 366 * 24 * 60 * 60;

/** `MAX_REASON_BYTES`: the longest reason a change may carry — BYTES, not characters, as
 *  the plane measures it, so a Cyrillic reason runs out at half the letters. */
export const MAX_REASON_BYTES = 500;

/** What the prospectus promised, and what an unconfigured fund's form opens on. */
export const HOUSE_TERMS: FeeTermsLike = {
  management_bps: 200,
  performance_bps: 2_000,
  hurdle_bps: 0,
  basis: "invested_capital",
  crystallization: "annual",
};

/** How a fund with no policy row is measured: it charges nothing, so any first positive
 *  rate tightens (`FeePolicy::NONE`). */
export const NO_TERMS: FeeTermsLike = { ...HOUSE_TERMS, management_bps: 0, performance_bps: 0 };

export const BASES = ["invested_capital", "market_value"] as const;

/** Most frequent first: the order in which each step is a dearer price for the investor. */
export const CRYSTALLIZATIONS = ["monthly", "quarterly", "semi_annual", "annual"] as const;

/** Pending means the change is still on its way — the two states a cancel can reach, and
 *  the ones the sweeper (or the owners) will move without anyone on this screen acting. */
export function isPendingChange(state: string | undefined): boolean {
  return state === "scheduled" || state === "awaiting_consilium";
}

const PERIOD_SECONDS: Record<string, number> = {
  monthly: 30 * 86_400,
  quarterly: 91 * 86_400,
  semi_annual: 182 * 86_400,
  annual: 365 * 86_400,
};

/** `FeePolicy::within_house_envelope`: 2/20 or less, on invested capital, annually. The
 *  hurdle is free because it only ever helps the investor. */
export function withinHouseEnvelope(terms: FeeTermsLike): boolean {
  return (
    terms.management_bps <= HOUSE_TERMS.management_bps &&
    terms.performance_bps <= HOUSE_TERMS.performance_bps &&
    terms.basis === "invested_capital" &&
    terms.crystallization === "annual"
  );
}

/** `FeePolicy::tightens_from`: true when ANY leg gets dearer for the investor. A
 *  crystallization word this build cannot rank is treated as no change — the plane still
 *  decides, and a guess here would only add a spurious "reason required" to the form. */
export function tightensFrom(current: FeeTermsLike, next: FeeTermsLike): boolean {
  const currentPeriod = PERIOD_SECONDS[current.crystallization];
  const nextPeriod = PERIOD_SECONDS[next.crystallization];
  return (
    next.management_bps > current.management_bps ||
    next.performance_bps > current.performance_bps ||
    next.hurdle_bps < current.hurdle_bps ||
    (current.basis === "invested_capital" && next.basis === "market_value") ||
    (currentPeriod !== undefined && nextPeriod !== undefined && nextPeriod < currentPeriod)
  );
}

export type ChangeRequirement = "admin" | "owner_consilium";

/**
 * `requirement_for`: the owners' consilium when the change tightens the terms AND lands
 * outside the envelope, or whenever it lowers the hurdle — taking a promised hurdle away is
 * a new bargain wherever the other legs sit. Everything else is one administrator's call.
 * `null` for the current terms means the fund charges nothing yet.
 */
export function requirementFor(current: FeeTermsLike | null, next: FeeTermsLike): ChangeRequirement {
  const from = current ?? NO_TERMS;
  const hurdleLowered = next.hurdle_bps < from.hurdle_bps;
  return hurdleLowered || (tightensFrom(from, next) && !withinHouseEnvelope(next)) ? "owner_consilium" : "admin";
}

// ── words ─────────────────────────────────────────────────────────────────────
// One wire value has one name across the cabinet: the admin console, the product page and
// the owners' room all point at the same catalogue entries.

const basisWords = (t: Translate): Readonly<Record<string, string>> => ({
  invested_capital: t("admin.fees.basis.investedCapital", "Invested capital"),
  market_value: t("admin.fees.basis.marketValue", "Market value"),
});

const crystallizationWords = (t: Translate): Readonly<Record<string, string>> => ({
  monthly: t("admin.fees.period.monthly", "Monthly"),
  quarterly: t("admin.fees.period.quarterly", "Quarterly"),
  semi_annual: t("admin.fees.period.semiAnnual", "Every 6 months"),
  annual: t("admin.fees.period.annual", "Annually"),
});

/** A basis in the reader's language; a word this build has no name for is shown as the
 *  hub sent it rather than swallowed. */
export function basisLabel(basis: string | undefined, t: Translate): string {
  return wordFor(basisWords(t), basis) ?? (basis || "—");
}

export function crystallizationLabel(period: string | undefined, t: Translate): string {
  return wordFor(crystallizationWords(t), period) ?? (period || "—");
}
