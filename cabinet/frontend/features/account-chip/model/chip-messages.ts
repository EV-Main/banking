import type { Locale, Messages } from "@evinvest/i18n";

// The strings the account chip renders, and nothing else.
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
// A handful of keys times five locales is small enough to inline, so it is inlined. The
// values here are copies, which is a real duplication — `chip-messages.test.ts`
// exists to make it a checked one: it fails if any of these drifts from
// `messages/<locale>/common.json`.
export const CHIP_KEYS = ["auth.cabinet", "auth.err.signOut", "auth.manageAccount", "auth.signOut", "auth.switchAccount", "ui.account", "ui.profile", "ui.services"] as const;

const CHIP_MESSAGES: Record<Locale, Messages> = {
  en: {
    "auth.cabinet": "Cabinet",
    "auth.err.signOut": "We couldn't sign you out. Please try again.",
    "auth.manageAccount": "Manage account",
    "auth.signOut": "Sign out",
    "auth.switchAccount": "Switch account",
    "ui.account": "Account",
    "ui.profile": "Profile",
    "ui.services": "Services"
  },
  ru: {
    "auth.cabinet": "Кабинет",
    "auth.err.signOut": "Не удалось выйти. Попробуйте ещё раз.",
    "auth.manageAccount": "Управление аккаунтом",
    "auth.signOut": "Выйти",
    "auth.switchAccount": "Сменить аккаунт",
    "ui.account": "Аккаунт",
    "ui.profile": "Профиль",
    "ui.services": "Сервисы"
  },
  vi: {
    "auth.cabinet": "Cabinet",
    "auth.err.signOut": "Không thể đăng xuất. Vui lòng thử lại.",
    "auth.manageAccount": "Quản lý tài khoản",
    "auth.signOut": "Đăng xuất",
    "auth.switchAccount": "Chuyển tài khoản",
    "ui.account": "Tài khoản",
    "ui.profile": "Hồ sơ",
    "ui.services": "Dịch vụ"
  },
  fr: {
    "auth.cabinet": "Mon espace",
    "auth.err.signOut": "Impossible de vous déconnecter. Veuillez réessayer.",
    "auth.manageAccount": "Gérer le compte",
    "auth.signOut": "Se déconnecter",
    "auth.switchAccount": "Changer de compte",
    "ui.account": "Compte",
    "ui.profile": "Profil",
    "ui.services": "Services"
  },
  de: {
    "auth.cabinet": "Cabinet",
    "auth.err.signOut": "Abmelden fehlgeschlagen. Bitte versuchen Sie es erneut.",
    "auth.manageAccount": "Konto verwalten",
    "auth.signOut": "Abmelden",
    "auth.switchAccount": "Konto wechseln",
    "ui.account": "Konto",
    "ui.profile": "Profil",
    "ui.services": "Dienste"
  }
};

/** The chip's catalogue for `locale`, falling back to English. */
export function chipMessages(locale: Locale): Messages {
  return CHIP_MESSAGES[locale] ?? CHIP_MESSAGES.en;
}
