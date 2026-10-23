import { DEFAULT_LOCALE, isLocale, type Locale } from "@evinvest/i18n";

// Browser-side access to the `ev_locale` cookie — the one cookie this zone mints
// rather than reads (shared/config/cookies.ts), and the carrier that lets the
// public site hand a reader's chosen language to the cabinet.
//
// Two callers need it from the browser and neither can use the server-side
// COOKIES map:
//   - the account chip, which renders on the CONDUCTOR origin inside a custom
//     element and so has no Next request context at all;
//   - the settings language switcher, which must make its choice stick before
//     the navigation that applies it.
//
// The name is computed from the live protocol rather than from config, and that
// is not a shortcut: `__Host-` requires Secure and Secure requires https, so the
// protocol is what actually decides which name the browser will accept. It
// therefore agrees with the server's AUTH_COOKIE_SECURE-derived name on both
// hosts by construction — `__Host-ev_locale` in production, bare `ev_locale`
// over plain http in dev — and with site_conductor's `scripts/locale-cookie.ts`,
// which computes it the same way.
function cookieName(base = "ev_locale"): string {
  return (location.protocol === "https:" ? "__Host-" : "") + base;
}

/** The reader's remembered locale, or null when they have never had one set. */
export function readLocaleCookie(): Locale | null {
  if (typeof document === "undefined") return null;
  const name = cookieName();
  for (const part of document.cookie.split(";")) {
    const [k, ...rest] = part.trim().split("=");
    if (k === name) {
      const value = decodeURIComponent(rest.join("="));
      return isLocale(value) ? value : null;
    }
  }
  return null;
}

/** Remember `locale` for a year, in the shape the proxy reads on the next entry. */
export function writeLocaleCookie(locale: Locale): void {
  writeCookie(cookieName(), locale);
}

/**
 * Remember `locale` AND that the reader asked for it. Settings is the only caller: the
 * `ev_locale_chosen` twin is what lets the proxy tell this cookie from one it seeded
 * itself from a URL or a header, which yields to the account's language.
 */
export function confirmLocaleCookie(locale: Locale): void {
  writeLocaleCookie(locale);
  writeCookie(cookieName("ev_locale_chosen"), locale);
}

function writeCookie(name: string, value: string): void {
  if (typeof document === "undefined") return;
  const secure = location.protocol === "https:";
  document.cookie = `${name}=${value};path=/;max-age=${60 * 60 * 24 * 365};samesite=lax${secure ? ";secure" : ""}`;
}

/**
 * The locale of the document this code is running inside.
 *
 * `<html lang>` is set from the URL by both apps' root layouts, so on a
 * conductor page it is the language the reader is actually reading and on a
 * cabinet page it is the `[locale]` segment. That makes it the right source for
 * the account chip, which renders on one host and links into the other: the
 * cookie is a remembered preference and can be stale, the rendered document
 * cannot be.
 */
export function documentLocale(): Locale {
  if (typeof document === "undefined") return DEFAULT_LOCALE;
  const lang = document.documentElement.lang;
  return isLocale(lang) ? lang : (readLocaleCookie() ?? DEFAULT_LOCALE);
}
