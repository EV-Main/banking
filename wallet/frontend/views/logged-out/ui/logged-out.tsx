import { translator } from "@evinvest/i18n";

import { Link } from "@/shared/ui/cabinet-link";

import { Logo } from "@/shared/ui/logo";
import { messagesFor } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";

// A true server component, so `useT()` is unavailable: the locale comes from the URL via
// `currentLocale()` and the catalogue is bound here, as in `views/status`.
export async function LoggedOutView() {
  const locale = await currentLocale();
  const t = translator(messagesFor(locale), locale);
  return (
    <div className="flex min-h-[calc(100dvh-var(--ev-shell-offset,0px))] items-center justify-center p-6">
      <div className="w-full max-w-sm space-y-6 rounded-xl border border-border bg-card p-8 text-center">
        <Logo className="mx-auto h-8 w-auto text-ink" />

        <div className="space-y-1">
          <h1 className="text-2xl">{t("auth.signedOut", "Signed out")}</h1>
          <p className="text-sm text-ink-soft">{t("auth.signedOutSub", "You've been signed out of your cabinet.")}</p>
        </div>

        <Link
          href="/login"
          className="flex w-full items-center justify-center rounded-md bg-primary px-4 py-2.5 text-sm font-medium text-on-primary outline-none transition hover:opacity-90 focus-visible:ring-2 focus-visible:ring-ring"
        >
          {t("auth.signInAgain", "Sign in again")}
        </Link>
      </div>
    </div>
  );
}
