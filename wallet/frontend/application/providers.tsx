"use client";

import { PostHogProvider } from "@evinvest/analytics/react";
import { ErrorMonitoringProvider } from "@evinvest/error-monitoring/react";
import { ThemeProvider } from "next-themes";
import type { ReactNode } from "react";

import { config } from "@/config";

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

export function Providers({ children, nonce }: { children: ReactNode; nonce?: string }) {
  return (
    <ErrorMonitoringProvider>
      <PostHogProvider apiKey={config.public.posthogKey} host={config.public.posthogHost}>
        <ThemeProvider attribute="class" forcedTheme="dark" enableSystem={false} nonce={nonce}>
          {children}
        </ThemeProvider>
      </PostHogProvider>
    </ErrorMonitoringProvider>
  );
}
