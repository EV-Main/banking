// Catalogues and policy only — deliberately no cookie, config or `next/headers`
// import; the request-scoped locale lookup lives in ./locale.ts. The gate that used
// to reach in here for a policy report is now `evinvest-i18n-check`, which reads
// `messages/` off disk and boots nothing.
import type { Locale, Messages } from "@evinvest/i18n";
import {
  resolveCatalogue,
  type TranslatedCatalogue,
} from "@evinvest/i18n/policy";

import en from "@/messages/en/common.json";
import ru from "@/messages/ru/common.json";
import vi from "@/messages/vi/common.json";
import fr from "@/messages/fr/common.json";
import de from "@/messages/de/common.json";

const AUTHORED: Record<Exclude<Locale, "en">, TranslatedCatalogue> = {
  ru,
  vi,
  fr,
  de,
};

// The translation policy applied once at module scope, not per request:
// `resolveCatalogue` is pure over static input, so the answer is identical for
// every render. A drifted entry falls back to canonical English here exactly as
// it does on the public site — one policy, both surfaces.
const RESOLVED = Object.fromEntries(
  (Object.keys(AUTHORED) as Exclude<Locale, "en">[]).map((locale) => [
    locale,
    resolveCatalogue(locale, en, AUTHORED[locale]),
  ]),
);

export const messagesFor = (locale: Locale): Messages =>
  locale === "en" ? en : (RESOLVED[locale]?.messages ?? en);

