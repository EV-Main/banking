"use client";

import { PostHogProvider } from "@evinvest/analytics/react";
import { ErrorMonitoringProvider } from "@evinvest/error-monitoring/react";
import { ThemeProvider } from "next-themes";
import { usePathname } from "next/navigation";
import type { ReactNode } from "react";

import { config } from "@/config";
import { mayObserve } from "@/shared/config/public-routes";

// Client observability providers wrap the tree. Both no-op when unset (no DSN /
// no key), so the same tree renders unconfigured in local dev and CI. `nonce` is
// the per-request CSP nonce (from the root layout) so next-themes' inline script
// stays allowed.
//
// The PostHog key and host are handed over explicitly rather than left to the
// provider's own env fallback: the library reads `process.env[name]` through a
// variable, and Next only inlines the literal `process.env.NEXT_PUBLIC_*` form into
// the browser bundle — so the fallback resolves to `undefined` in every browser and
// the provider was silently a no-op. `config.ts` spells the names out literally.
//
// Except on the token approval pages, where they are not mounted at all.
//
// Those pages carry a single-use approval credential in their own URL. PostHog stamps
// `$current_url` onto every event including its mount pageview, and Sentry puts the same
// URL on `request.url` and on every fetch breadcrumb — so simply rendering an approval page
// under these providers sends a live token to two third parties before the reader has
// touched anything. This is the one place in the cabinet where the URL is a secret, so it
// is the one place that opts out. See `mayObserve` in `shared/config/public-routes.ts` for
// why the redaction that would otherwise fix this has nowhere to live.
//
// `ThemeProvider` stays on every route: it holds no telemetry, and dropping it would leave
// the approval pages unthemed.
export function Providers({ children, nonce }: { children: ReactNode; nonce?: string }) {
  const pathname = usePathname();
  const themed = (
    <ThemeProvider attribute="class" forcedTheme="dark" enableSystem={false} nonce={nonce}>
      {children}
    </ThemeProvider>
  );

  if (!mayObserve(pathname)) return themed;

  return (
    <ErrorMonitoringProvider>
      <PostHogProvider apiKey={config.public.posthogKey} host={config.public.posthogHost}>
        {themed}
      </PostHogProvider>
    </ErrorMonitoringProvider>
  );
}
