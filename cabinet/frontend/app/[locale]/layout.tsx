import type { Metadata } from "next";
import type { ReactNode } from "react";

import { I18nProvider } from "@evinvest/i18n/react";

import "@/application/styles/globals.css";
import { Providers } from "@/application/providers";
import { fontInter } from "@/application/styles/fonts";
import { isLocale, translator } from "@evinvest/i18n";
import { notFound } from "next/navigation";
import { messagesFor } from "@/shared/config/i18n";
import { requestNonce } from "@/shared/config/security";

// The tab title is the first words of the cabinet a reader sees, often before the page has
// painted, so it is translated like everything else. It has to be `generateMetadata` to be:
// a static `metadata` export is evaluated once, with no request and therefore no locale, so
// whatever language it was authored in is the language every reader would get.
//
// The locale comes from this route's own `params` rather than `currentLocale()`. Metadata
// generation is not inside the render that `next/root-params` reads from — this function
// runs *for* the root layout, not within it — and `params` is right here in the signature
// anyway. A segment that is not one of the five falls back to English rather than throwing:
// the 404 for that is the layout's to raise, a few lines down, where there is a page to
// show for it.
export async function generateMetadata({
  params,
}: {
  params: Promise<{ locale: string }>;
}): Promise<Metadata> {
  const { locale: raw } = await params;
  const locale = isLocale(raw) ? raw : "en";
  const t = translator(messagesFor(locale), locale);
  return { title: t("meta.title"), description: t("meta.description") };
}

// Root layout: html/body + cross-cutting providers only. The visible chrome belongs to
// the route groups — the signed-in app shell (sidebar) lives in `(app)`, the centered
// auth framing in `(auth)`.
//
// This is the ROOT layout despite sitting under `[locale]`, and deliberately so:
// `[locale]` has to be a *root* param for `next/root-params` to expose it to the
// server components that need the locale but are handed no props (the status
// pages, `currentLocale()`). An `app/layout.tsx` above this one would demote it.
// Same shape as site_conductor.
export default async function RootLayout({
  children,
  params,
}: {
  children: ReactNode;
  params: Promise<{ locale: string }>;
}) {
  const nonce = (await requestNonce()) ?? undefined;
  // The locale is the URL now, not a cookie. `(auth)` needs it as much as `(app)`
  // — someone who cannot yet sign in is exactly the reader who should not be met
  // in a language they do not read.
  const { locale: raw } = await params;
  if (!isLocale(raw)) notFound();
  const locale = raw;
  return (
    <html lang={locale} className={`dark ${fontInter.variable}`} suppressHydrationWarning>
      <body className="min-h-screen bg-background text-ink antialiased">
        <I18nProvider locale={locale} messages={messagesFor(locale)}>
          <Providers nonce={nonce}>{children}</Providers>
        </I18nProvider>
      </body>
    </html>
  );
}
