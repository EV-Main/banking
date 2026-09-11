"use client";

import { useEffect } from "react";

import * as Sentry from "@sentry/nextjs";
import { createSentrySink } from "@evinvest/error-monitoring";
import { ServerError } from "@evinvest/uikit";

import "@/application/styles/globals.css";
import { BASE_PATH } from "@/shared/config/base-path";

// The last-resort 500: an error thrown by the root layout itself, which is above
// every other boundary. It replaces the root layout, so — like `global-not-found`
// — it owns the whole `<html>` document.
//
// English-only and provider-free on purpose. Everything the localised
// `[locale]/cabinet/error.tsx` needs (the `I18nProvider`, the font variable, the
// theme provider) is mounted *by* the layout that just failed, so reaching for any
// of it here risks a boundary that itself throws — and a status page that crashes
// is a blank tab. Plain copy on a plain document is the honest floor. The home
// link is the unprefixed `/cabinet`, which `proxy.ts` resolves into a locale.
const { reportError } = createSentrySink(Sentry);

export default function GlobalError({
  error,
  reset,
}: {
  error: Error & { digest?: string };
  reset: () => void;
}) {
  useEffect(() => {
    reportError(error, error.digest ? { digest: error.digest } : undefined);
  }, [error]);

  return (
    <html lang="en" className="dark">
      <body className="min-h-screen bg-background text-ink antialiased">
        <ServerError homeHref={BASE_PATH} reset={reset} />
      </body>
    </html>
  );
}
