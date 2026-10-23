// CI gate for the translation policy (rules 1.1 / 1.2 — @evinvest/i18n/policy).
//
// The runtime already degrades safely: a drifted entry falls back to canonical
// English and the page renders. That safety is exactly why this exists — a silent
// fallback is indistinguishable from a surface that was never translated, so
// without a noisy second channel a locale can rot to zero coverage unnoticed.
//
// Fails on *drift* and on *missing keys* alike. The check used to tolerate
// untranslated keys so a locale could be filled in over time — and three locales
// sat at 86 % for a release cycle while every new consilium screen shipped in
// English. All five catalogues are complete now, so the floor is 100 %: a new
// `t(key, English)` lands with all four translations in the same change, or CI is red.
//
// Drift has two faces. The policy catches *key* drift — the `en` field no longer
// matches today's English. It cannot catch *copy* drift: a translator who pastes
// the English text into `t` (or a rewrite that updates `en` and leaves `t` alone)
// passes the policy and ships English under a foreign locale. The one mechanical
// signal for that is `t === en`, so this script fails on it too — minus an
// allowlist of strings that legitimately read the same in every language.
import type { Locale } from "@evinvest/i18n";
import { auditCatalogues, type TranslatedCatalogue } from "@evinvest/i18n/policy";

import { catalogueReport } from "../shared/config/i18n";
import de from "../messages/de/common.json";
import en from "../messages/en/common.json";
import fr from "../messages/fr/common.json";
import ru from "../messages/ru/common.json";
import vi from "../messages/vi/common.json";

type Translated = Exclude<Locale, "en">;

// The authored catalogues, before the policy resolves them: `resolveCatalogue`
// already replaces a rejected `t` with English, so the identity check has to
// look at what the translator wrote, not at what is served.
const AUTHORED: Record<Translated, TranslatedCatalogue> = { ru, vi, fr, de };
const ENGLISH: Readonly<Record<string, string>> = en;

// A simple ICU argument — `{n}`, `{n, number}`, `{n, number, ::percent}`. Plural
// and select arguments are deliberately *not* matched: their branches carry
// prose, and an English plural pasted into a locale is exactly the copy drift
// this check exists for.
const SIMPLE_ARGUMENT = /\{\s*\w+\s*(?:,\s*\w+\s*(?:,\s*[^{}]*)?)?\}/gu;

// Nothing to translate: once the placeholders are gone only digits, punctuation
// and symbols remain — "{title} ({service})", "≈ {amount}", "{pct}%".
const hasNoProse = (text: string): boolean =>
  !/\p{L}/u.test(text.replace(SIMPLE_ARGUMENT, ""));

// Strings that are the same in every language: tickers and currency codes,
// environment names, chain and network names, crypto jargon that no locale
// translates ("gas", "tx", "memo", maker/taker), and column abbreviations.
const SHARED_TERMS: ReadonlySet<string> = new Set([
  "NAV",
  "AUM (USDT)",
  "P&L",
  "KYC",
  "KYC L{n}",
  "L{n}",
  "v{n}",
  "DEV",
  "PROD",
  "STAGING",
  "USD ($)",
  "EUR (€)",
  "{network} · USDT",
  "tx {ref}",
  "BEP20 · BNB Chain",
  "Polygon · PoS",
  "TON · Open Network",
  "TRC20 · TRON",
  "BNB Smart Chain",
  "Polygon PoS",
  "The Open Network",
  "TRON",
  "Seq",
  "Gas",
  "gas",
  "Maker",
  "Taker",
  "Memo",
  "off-ramp · FX",
]);

// Strings that coincide with English in *one* language — loanwords, shared Latin
// roots, or a term the locale's own catalogue already uses untranslated
// ("Wallet", "Treasury", "Cabinet" and "Outbox" in German; "wallet", "rail" and
// "outbox" in French; "email" and "consilium" in Vietnamese). Keyed by value, not by key: whether
// "Status" is a German word does not depend on which screen shows it.
const LOANWORDS: Readonly<Record<Translated, ReadonlySet<string>>> = {
  de: new Set([
    "Admin",
    "Arbitrage",
    "Bank · USD",
    "Browser",
    "Cabinet",
    "Chart",
    "Code",
    "Details",
    "{title} — Details",
    "Hurdle",
    "IN ORDERS",
    "In Orders",
    "INVEST.",
    "Index",
    "Investor",
    "Limit",
    "Live",
    "Max",
    "Onboarding",
    // Duden lists it as a German noun; "quelloffen" is the adjective nobody uses as a label.
    "Open Source",
    "Operator",
    "ORDERS",
    "{n, plural, one {# Order} other {# Orders}}",
    "Outbox",
    "Passkey",
    "Passkeys",
    "Performance",
    "{amount} Performance",
    "Person",
    "Portfolio",
    "Rollout %",
    "Service",
    "Spread",
    "Spread {spread}",
    "Status",
    "STATUS",
    "Trades",
    "Treasury",
    "Version",
    "Wallet",
    "Whitepaper",
    "1M",
    "6M",
  ]),
  fr: new Set([
    "Actions",
    "Admin",
    "Admissions",
    // The fund term is the same word in French; the catalogue already says "allocation"
    // in every sentence about one.
    "Allocation",
    "Arbitrage",
    "Code",
    "Consilium",
    "Consilium {id}",
    "Documents",
    "Feature flags",
    "Max",
    "Onboarding",
    "Outbox",
    "Performance",
    "Rail",
    "Service",
    "Services",
    "Source",
    "Total",
    "Trading",
    "Transaction",
    "Type",
    "Version",
    "Wallet",
    "{amount} net",
    "{amount} performance",
    "{network} · {destination} · {amount} USDT net",
    "1M",
    "6M",
  ]),
  ru: new Set(),
  vi: new Set(["Cabinet", "Consilium", "Email"]),
};

const isLegitimatelyIdentical = (locale: Translated, text: string): boolean =>
  hasNoProse(text) || SHARED_TERMS.has(text) || LOANWORDS[locale].has(text);

// Compared loosely: a trailing full stop, a stray space or a capital letter is
// still the English text, not a translation of it — and a strict `===` would let
// exactly that byte through. Anything past this (a half-translated sentence)
// is a reviewer's job, as the policy module says of itself.
const canonical = (text: string): string =>
  text
    .normalize("NFKC")
    .toLowerCase()
    .replace(/\s+/gu, " ")
    .trim()
    .replace(/[\s.,;:!?…]+$/u, "");

const resolved = catalogueReport();
const { report } = auditCatalogues(resolved, 1);
console.log(report);

const drifted = resolved.flatMap((c) =>
  c.rejected.map((r) => `${c.locale}/${r.key}: ${r.reason} — ${r.detail}`),
);

const identical = (Object.keys(AUTHORED) as Translated[]).flatMap((locale) =>
  Object.entries(AUTHORED[locale]).flatMap(([key, entry]) =>
    ENGLISH[key] !== undefined &&
    canonical(entry.t) === canonical(ENGLISH[key]) &&
    !isLegitimatelyIdentical(locale, entry.t) &&
    !isLegitimatelyIdentical(locale, ENGLISH[key])
      ? [`${locale}/${key}: ${JSON.stringify(entry.t)}`]
      : [],
  ),
);

// Listed explicitly rather than trusting `auditCatalogues().ok`: the report only
// prints the first ten missing keys per locale, and a red CI job has to name
// every key that needs translating, not make the author go count.
const missing = resolved.flatMap((c) => c.missing.map((key) => `${c.locale}/${key}`));

let failed = false;

if (missing.length > 0) {
  failed = true;
  console.error(`\n${missing.length} key${missing.length === 1 ? "" : "s"} missing from a locale:`);
  for (const line of missing) console.error(`  ${line}`);
  console.error(
    "\nEvery key English defines must be translated in all four locales." +
      " Add the entry to each messages/<locale>/common.json with the exact `en` text.",
  );
}

if (drifted.length > 0) {
  failed = true;
  console.error(`\n${drifted.length} entr${drifted.length === 1 ? "y" : "ies"} rejected by policy:`);
  for (const line of drifted) console.error(`  ${line}`);
  console.error(
    "\nEnglish is being served for these. Retranslate and update the `en` field," +
      " or revert the English change.",
  );
}

if (identical.length > 0) {
  failed = true;
  console.error(
    `\n${identical.length} translation${identical.length === 1 ? "" : "s"} identical to the English source:`,
  );
  for (const line of identical) console.error(`  ${line}`);
  console.error(
    "\nThese pass the policy but ship English under a foreign locale. Translate them," +
      " or — for a ticker, a proper name or a loanword — add the string to the" +
      " allowlist in scripts/i18n-check.mts with a reason.",
  );
}

if (failed) process.exit(1);

console.log(
  "\ni18n: complete — every key is translated in every locale, none has drifted, none is a copy of English",
);
