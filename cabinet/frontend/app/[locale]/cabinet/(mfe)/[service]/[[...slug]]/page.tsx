import type { Metadata } from "next";
import { notFound } from "next/navigation";

import { translator } from "@evinvest/i18n";

import { messagesFor } from "@/shared/config/i18n";
import { currentLocale } from "@/shared/config/locale";
import { RemoteElement } from "@/shared/mfe/RemoteElement";
import { findMfe } from "@/shared/mfe/registry";

// App surfaces, not marketing pages — keep page MFEs out of the search index.
export const metadata: Metadata = { robots: { index: false, follow: false } };

// Page-level microfrontends: a service owns a whole route. The optional catch-all
// `[[...slug]]` means this also matches the bare `/<service>` index, and the rest
// of the path belongs to the microfrontend's own internal router. The host keeps
// its chrome; the remote owns the content region. Same custom-element contract as
// inline widgets — just mounted at a route. (Not Multi-Zones: this is runtime
// component composition, which also covers inline widgets.)
//
// This route sits at the root of the cabinet tree, so it is also what every URL
// under `/{locale}/cabinet/` that matches nothing else falls into — a typo, a
// stale bookmark, a scanner. That makes its miss branch the cabinet's real 404,
// not a developer aside: it used to render a soft-200 "Unknown microfrontend —
// add it to mfe-registry.json" panel, which the conductor proxies straight onto
// the public origin. `notFound()` instead, so the reader gets the branded 404 in
// their own language and crawlers and monitoring get the status code to match.
export default async function MfePage({ params }: { params: Promise<{ service: string; slug?: string[] }> }) {
  const { service } = await params;
  const entry = await findMfe(service);

  // Unregistered, or registered as an inline component rather than a page.
  if (!entry || entry.kind !== "page") notFound();

  // A Server Component, so no hook: the locale comes from `next/root-params` the same way
  // the status pages read it. `entry.name` stays as the registry wrote it — a service's
  // name is its name in every language.
  const locale = await currentLocale();
  const t = translator(messagesFor(locale), locale);

  return (
    // The 60vh reserve keeps the page from collapsing while the remote boots. It is
    // viewport maths, not a spacing step — a fixed height would over-reserve on a short
    // viewport and under-reserve on a tall one — so it stays expressed in vh.
    <RemoteElement
      tag={entry.tag}
      scriptUrl={entry.scriptUrl}
      integrity={entry.integrity}
      className="block min-h-[60vh]"
      fallback={<div className="container py-24 text-ink-soft">{t("mfe.loading", { name: entry.name })}</div>}
    />
  );
}
