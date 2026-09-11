import type { Metadata } from "next";

import { NotFound } from "@evinvest/uikit";

import "@/application/styles/globals.css";
import { fontInter } from "@/application/styles/fonts";
import { BASE_PATH } from "@/shared/config/base-path";

// The 404 for URLs that resolved no route at all — a bad locale segment
// (`/xx/cabinet/wallet`, which the root layout answers with `notFound()`), or a
// signed-in request for a path outside the zone entirely.
//
// Why a *global* not-found and not `app/not-found.tsx`: this app has no
// `app/layout.tsx`. The root layout is `app/[locale]/layout.tsx` (deliberately —
// `next/root-params` only exposes `[locale]` while it is a *root* param), so a URL
// that resolved no `[locale]` has no layout to render a nested `not-found.tsx`
// inside, and Next falls back to its built-in black "404 | This page could not be
// found". The `global-notFound` convention replaces the root layout for
// `/_not-found`, so it owns the whole `<html>` document and has to build it.
// It needs `experimental.globalNotFound` in next.config.ts — without the flag
// next-app-loader never resolves this file and the built-in page comes back.
//
// `app/[locale]/cabinet/not-found.tsx` still handles the other half — a
// `notFound()` from a route that *did* match, which is every unknown cabinet URL.
//
// English-only, and the copy comes straight from the uikit rather than the
// catalogue: reaching a catalogue needs a locale, and the whole definition of this
// page is a request that produced none. The home link is the unprefixed `/cabinet`
// for the same reason — `proxy.ts` resolves it into the reader's locale from their
// cookie or Accept-Language, which is a better guess than anything available here.
export const metadata: Metadata = {
  title: "Page not found — EV Investment",
  robots: { index: false },
};

export default function GlobalNotFound() {
  return (
    <html lang="en" className={`dark ${fontInter.variable}`} suppressHydrationWarning>
      <body className="min-h-screen bg-background text-ink antialiased">
        <NotFound homeHref={BASE_PATH} contactHref="/contact" />
      </body>
    </html>
  );
}
