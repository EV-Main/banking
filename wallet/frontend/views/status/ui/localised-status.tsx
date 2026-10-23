import { StatusScreen } from "@evinvest/uikit";
import { localePath, translator, type Locale, type Translate } from "@evinvest/i18n";

import { messagesFor } from "@/shared/config/i18n";
import { cabinetPath } from "@/shared/config/base-path";

/**
 * The 404 / 403 / 401 surfaces, in the reader's language.
 *
 * The uikit's ready-made `NotFound` / `Forbidden` pages bake their copy in, which
 * is what the cabinet rendered until now: an English apology under Russian chrome,
 * at the one moment the product is already failing the reader. `StatusScreen` is
 * the same component underneath and takes every string as a prop, so this needs no
 * uikit release. Same shape as the conductor's `views/status`, and the catalogue
 * entries are ported from it verbatim — a reader who crosses the zone boundary
 * onto a 404 should not meet a differently-worded one.
 *
 * Server-rendered on purpose: Next hands these pages no props, so the locale comes
 * from `next/root-params` at the call site (`currentLocale()`), never a client
 * hook. The 500 cannot use this — Next requires `error.tsx` to be a Client
 * Component — and reads the catalogue through `I18nProvider` instead.
 *
 * "Home" here is the cabinet dashboard, not the landing: someone signed in who hit
 * a dead cabinet URL wants their portfolio back, not the marketing site. The
 * secondary CTA does leave the zone — contact and sign-in are shell-owned.
 */
export type StatusKind = "notFound" | "forbidden" | "unauthorized";

const ACCENT = { notFound: "debug", forbidden: "warn", unauthorized: "warn" } as const;
const CODE = { notFound: "404", forbidden: "403", unauthorized: "401" } as const;

interface StatusCopy {
  eyebrow: string;
  headlineLead: string;
  headlineAccent: string;
  subtext: string;
}

const COPY: Record<StatusKind, (t: Translate) => StatusCopy> = {
  notFound: (t) => ({
    eyebrow: t("status.notFound.eyebrow", "Page not found"),
    headlineLead: t("status.notFound.headlineLead", "You've reached "),
    headlineAccent: t("status.notFound.headlineAccent", "open water"),
    subtext: t("status.notFound.subtext", "The page you're looking for has drifted off our coastline — moved, renamed, or never charted. Let's get you back to shore."),
  }),
  forbidden: (t) => ({
    eyebrow: t("status.forbidden.eyebrow", "Access forbidden"),
    headlineLead: t("status.forbidden.headlineLead", "This harbour is "),
    headlineAccent: t("status.forbidden.headlineAccent", "private"),
    subtext: t("status.forbidden.subtext", "You don't have the credentials to view this page. If you believe you should, our team can open the right doors."),
  }),
  unauthorized: (t) => ({
    eyebrow: t("status.unauthorized.eyebrow", "Sign-in required"),
    headlineLead: t("status.unauthorized.headlineLead", "This deck is "),
    headlineAccent: t("status.unauthorized.headlineAccent", "crew only"),
    subtext: t("status.unauthorized.subtext", "You need to be signed in to view this page. Sign in and we'll bring you right back."),
  }),
};

export function LocalisedStatus({ kind, locale }: { kind: StatusKind; locale: Locale }) {
  const t = translator(messagesFor(locale), locale);
  const secondary =
    kind === "unauthorized"
      ? { label: t("status.signIn", "Sign in"), href: cabinetPath(locale, "/login") }
      : {
          // The landing's contact page is a conductor route on the same origin, so
          // `localePath`, not `cabinetPath`.
          label: kind === "forbidden" ? t("status.requestAccess", "Request access") : t("status.contactTeam", "Contact the team"),
          href: localePath(locale, "/contact"),
        };
  return (
    <StatusScreen
      accent={ACCENT[kind]}
      code={CODE[kind]}
      {...COPY[kind](t)}
      links={[
        { label: t("status.backHome", "Back to home"), href: cabinetPath(locale, "/"), leadingArrow: true },
        { ...secondary, variant: "outline" },
      ]}
    />
  );
}
