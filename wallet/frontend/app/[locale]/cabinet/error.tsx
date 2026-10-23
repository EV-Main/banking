"use client";

import { useEffect } from "react";

import { createSentrySink } from "@evinvest/error-monitoring";
import { useLocale, useT } from "@evinvest/i18n/react";
import { StatusScreen, statusCtaClass } from "@evinvest/uikit";

import { cabinetPath } from "@/shared/config/base-path";

// Route-segment error boundary (500) for the whole `/{locale}/cabinet` tree.
//
// Next requires this file to be a Client Component, so it cannot read the locale
// the way the 404/403/401 do (`next/root-params`). It does not need to: the root
// layout mounts `I18nProvider` above this boundary — a segment's `error.tsx`
// renders *inside* its parent layouts — so the catalogue is already here. (The
// conductor threads the copy down through a context instead, because there the
// catalogues are not otherwise in the client graph; here they are, by design.)
// An error in the root layout itself is `global-error.tsx`'s problem, where there
// is no provider and no locale to read anyway.
//
// The boundary swallows the error, so nothing else reports it: `onRequestError`
// in instrumentation.ts covers the server side only, and a client-side render
// crash would otherwise be invisible. No-ops without a DSN.
// Sentry is imported lazily, on the error itself: this boundary sits in the static graph
// of every page under it, and a static import put the whole SDK (replay and the feedback
// widget included, ~170 KB gz) on each of them to report something no normal run hits.
function reportError(error: Error, context?: Record<string, unknown>): void {
  void import("@sentry/react").then(
    (Sentry) => createSentrySink(Sentry).reportError(error, context),
    () => {},
  );
}

export default function Error({
  error,
  reset,
}: {
  error: Error & { digest?: string };
  reset: () => void;
}) {
  const t = useT();
  const locale = useLocale();

  useEffect(() => {
    // `digest` is the only handle on a server-thrown error here — the message is
    // redacted in production — so attach it when there is one, and nothing when
    // there is not (the sink asks for an omitted context, not an empty object).
    reportError(error, error.digest ? { digest: error.digest } : undefined);
  }, [error]);

  return (
    <StatusScreen
      accent="error"
      code="500"
      eyebrow={t("status.serverError.eyebrow", "Server error")}
      headlineLead={t("status.serverError.headlineLead", "Our systems are ")}
      headlineAccent={t("status.serverError.headlineAccent", "recalibrating")}
      subtext={t("status.serverError.subtext", "Something broke on our end — not yours. We've been alerted and are restoring service. Please try again in a moment.")}
      links={[
        {
          label: t("status.backHome", "Back to home"),
          href: cabinetPath(locale, "/"),
          variant: "outline",
          leadingArrow: true,
        },
      ]}
    >
      <button type="button" className={statusCtaClass("error", "primary")} onClick={reset}>
        {t("status.tryAgain", "Try again")}
      </button>
    </StatusScreen>
  );
}
