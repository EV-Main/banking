import { translator, type Translate } from "@evinvest/i18n";

import { documentLocale } from "@/shared/lib/locale-cookie";

import { chipMessages } from "../model/chip-messages";

/**
 * The chip's own translator.
 *
 * `useT()` cannot be used: this bundle mounts on the CONDUCTOR origin (and in host mode on
 * a different app altogether), where none of the cabinet's React tree — and so no
 * `I18nProvider` — exists above it. It reads the locale off the document it was mounted
 * into and translates over `chip-messages.ts`, the strings the chip renders and nothing
 * else. Not the cabinet's `messagesFor()`: that pulls all five `common.json` files into a
 * bundle injected on every public page (532 KB of catalogue, measured).
 *
 * Not memoised on purpose: `documentLocale()` is two property reads, and a cached
 * translator would go stale when the host swaps the document's language under a chip that
 * never remounts.
 */
export function chipTranslator(): Translate {
  const locale = documentLocale();
  return translator(chipMessages(locale), locale);
}
