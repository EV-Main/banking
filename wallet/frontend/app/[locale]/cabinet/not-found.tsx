import type { Metadata } from "next";

import { currentLocale } from "@/shared/config/locale";
import { LocalisedStatus } from "@/views/status";

// Explicit noindex: the cabinet is behind a session gate, but the conductor
// proxies this document onto the public origin, so state it rather than rely on
// Next's built-in not-found meta.
export const metadata: Metadata = { robots: { index: false } };

// The 404 for the whole `/{locale}/cabinet` tree — reached by `notFound()` from a
// route that matched (the MFE catch-all below it is what every unknown cabinet URL
// now lands on). Next hands this page no props at all, which is why the locale
// comes from `currentLocale()` (`next/root-params`) rather than `params`.
export default async function NotFoundPage() {
  return <LocalisedStatus kind="notFound" locale={await currentLocale()} />;
}
