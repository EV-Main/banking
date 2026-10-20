import { cookies } from "next/headers";
import { type ReactNode, Suspense } from "react";

import { readAllocations } from "@/entities/fund/api/fund-server";
import { Sidebar } from "@/application/layout/sidebar";
import { BottomNavbar } from "@/application/layout/bottom-navbar";
import { RouteGate } from "@/application/layout/route-gate";
import { SignInDialog } from "@/features/auth/ui/sign-in-dialog";
import { COOKIES } from "@/shared/config/cookies";
import { isSessionId } from "@/shared/lib/access-renewal";
import { CacheWarmer } from "@/application/layout/cache-warmer";
import { LocaleSync } from "@/application/layout/locale-sync";
import { SessionAnalytics } from "@/application/layout/session-analytics";
import { SessionKeeper } from "@/application/layout/session-keeper";
import { SystemBanner } from "@/application/layout/system-banner";
import { TimeZoneSync } from "@/application/layout/time-zone-sync";
import { renderTimeZone } from "@/shared/config/time-zone";
import { TimeZoneProvider } from "@/shared/lib/time-zone";

// The authenticated app shell: a fixed left rail beside a scrollable content
// column. Chromeless by design — the brand header is conductor-owned and
// injected at the zone mount (the cabinet knows nothing about the outer site;
// `--ev-shell-offset` is the only shell contract). The rail is `fixed` (see
// Sidebar), so the content column reserves its width with a matching left
// padding — both read the shared `--cabinet-rail-w` token. No footer here by
// design. Auth is enforced upstream in `proxy.ts` — unauthenticated requests
// are redirected to /login before this layout renders; SessionKeeper mounts
// once here to keep the short-TTL access cookie alive for as long as this shell
// is open (that cookie, not the one the proxy gates on, is what the BFF
// verifies). The system banner
// (maintenance · read-only · announcement) mounts once here; (auth) pages have
// no session, so they are intentionally excluded.
//
// Responsive: below 1024px the sidebar is replaced by a fixed bottom nav bar
// (BottomNavbar). The content column goes full-width with no left offset and
// reserves room for the bottom nav so no content is occluded.
//
// Screens show times in the reader's own zone; the provider tells them which zone the
// server rendered in, so hydration reproduces it (`shared/lib/time-zone.tsx`).
export default async function AppLayout({ children }: { children: ReactNode }) {
  const timeZone = await renderTimeZone();
  const signedIn = isSessionId((await cookies()).get(COOKIES.session)?.value);
  return (
    <div className="flex min-h-[calc(100dvh-var(--ev-shell-offset,0px))] bg-background pb-[var(--cabinet-bottom-nav-h,64px)] lg:pl-[var(--cabinet-rail-w)] lg:pb-0">
      <SessionKeeper />
      {/* Identifies the PostHog person by user id and records `session_created`, once
          per tab; sits under the same providers as everything else in the shell. */}
      <SessionAnalytics />
      {signedIn && <CacheWarmer />}
      {/* Reconciles the URL's locale with the language stored on the account —
          adopting the stored one when the proxy had to guess, and recording the
          reader's actual locale when it did not. Renders nothing. */}
      {signedIn && <LocaleSync />}
      <TimeZoneSync />
      <div className="hidden lg:fixed lg:left-0 lg:top-[var(--ev-shell-offset,0px)] lg:flex lg:h-[calc(100dvh-var(--ev-shell-offset,0px))]">
        {/* Its own boundary: a slow BFF must not hold up the shell. */}
        <Suspense fallback={<Sidebar catalog={null} />}>
          <SeededSidebar signedIn={signedIn} />
        </Suspense>
      </div>
      <main className="min-w-0 flex-1">
        <SystemBanner />
        <TimeZoneProvider value={timeZone}>
          <Suspense>
            <RouteGate signedIn={signedIn}>{children}</RouteGate>
          </Suspense>
        </TimeZoneProvider>
      </main>
      <BottomNavbar />
      <Suspense>
        <SignInDialog />
      </Suspense>
    </div>
  );
}

// The rail's product list, read on the server so the browser does not read it again once
// the root hydrates. A failed or timed-out read is no seed, and the rail makes its own.
async function SeededSidebar({ signedIn }: { signedIn: boolean }) {
  const catalog = signedIn ? await readAllocations() : null;
  return <Sidebar catalog={catalog ?? undefined} />;
}
