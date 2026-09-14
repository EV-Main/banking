import type { Locale, Messages } from "@evinvest/i18n";

// The four strings the account chip renders, and nothing else.
//
// The chip is not a cabinet screen. It is bundled as a custom element and injected
// by the conductor into **every page of the public site**, where there is no
// `I18nProvider` above it — so it reads the locale itself and builds its own
// translator. The obvious way to feed that translator is `messagesFor()`, and the
// obvious way is wrong: that module imports all five `common.json` catalogues and
// runs the resolve policy over them at module scope. The chip bundle came out at
// 964 KB, over half of it catalogue, and an anonymous visitor to the marketing site
// was downloading the German translation of the admin treasury invariant note to
// render the word "Verified".
//
// Four keys times four locales is small enough to inline, so it is inlined. The values
// here are copies, which is a real duplication — `chip-messages.test.ts` exists to make it
// a checked one: it fails if any of these drifts from `messages/<locale>/common.json`.
//
// No `en` block. English is written at the call site now — `t("ui.account", "Account")` in
// `account-chip.tsx` — and `translator()` does not consult a catalogue for the default
// locale at all, so an English block here would be a fourth copy nothing reads.
export const CHIP_KEYS = ["auth.investorPortal", "auth.signOut", "ui.account", "ui.verified"] as const;

const CHIP_MESSAGES: Record<Exclude<Locale, "en">, Messages> = {
  ru: {
    "auth.investorPortal": "Кабинет инвестора",
    "auth.signOut": "Выйти",
    "ui.account": "Аккаунт",
    "ui.verified": "Проверен"
  },
  vi: {
    "auth.investorPortal": "Cổng nhà đầu tư",
    "auth.signOut": "Đăng xuất",
    "ui.account": "Tài khoản",
    "ui.verified": "Đã xác minh"
  },
  fr: {
    "auth.investorPortal": "Espace investisseur",
    "auth.signOut": "Se déconnecter",
    "ui.account": "Compte",
    "ui.verified": "Vérifié"
  },
  de: {
    "auth.investorPortal": "Investorenportal",
    "auth.signOut": "Abmelden",
    "ui.account": "Konto",
    "ui.verified": "Bestätigt"
  }
};

/** The chip's catalogue for `locale`. Empty for English and for a locale we do not
 *  publish — `translator()` serves the inline English in both cases. */
export function chipMessages(locale: Locale): Messages {
  return CHIP_MESSAGES[locale as Exclude<Locale, "en">] ?? {};
}
