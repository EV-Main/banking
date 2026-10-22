import { cookies } from "next/headers";
import { type ReactNode, Suspense } from "react";

import { CacheWarmer } from "@/application/layout/cache-warmer";
import { RouteGate } from "@/application/layout/route-gate";
import { SessionAnalytics } from "@/application/layout/session-analytics";
import { SessionKeeper } from "@/application/layout/session-keeper";
import { TimeZoneSync } from "@/application/layout/time-zone-sync";
import { SignInDialog } from "@/features/auth/ui/sign-in-dialog";
import { COOKIES } from "@/shared/config/cookies";
import { renderTimeZone } from "@/shared/config/time-zone";
import { isSessionId } from "@/shared/lib/access-renewal";
import { TimeZoneProvider } from "@/shared/lib/time-zone";

// What is left of the banking cabinet after banking#565: a sign-in host at `/cabinet` (the
// identity plane's `KYC_RETURN_URL` lands here) and the wallet pages the wallet element links
// to — `/wallet` itself (the fallback target of wallet notices for scopes without their own
// link base), `/wallet/deposit` and `/wallet/activity`. Investor screens live in the invest
// cabinet and staff screens in the central admin, so there is no rail to walk to.
//
// The session parts are the `(wallet-action)` layout's: the route gate gives a guest the
// sign-in wall, and the keeper renews the short-lived access cookie while the page is open.
// Unlike the popup, these pages sit under the conductor's header, so the height keeps
// `--ev-shell-offset`. `SessionAnalytics` stays from the old shell because `session_created`
// is the first step of the activation funnel and these pages are now where a session starts.
// `CacheWarmer` stays for its other half: it drops every cached figure when the session
// ends, so a tab handed to another account shows none of the previous one's. `TimeZoneSync`
// stores the device's zone, so the activity timeline's server HTML is in the reader's zone.
export default async function HostLayout({ children }: { children: ReactNode }) {
  const timeZone = await renderTimeZone();
  const signedIn = isSessionId((await cookies()).get(COOKIES.session)?.value);
  return (
    <main className="min-h-[calc(100dvh-var(--ev-shell-offset,0px))] bg-background">
      <SessionKeeper />
      <SessionAnalytics />
      {signedIn && <CacheWarmer />}
      <TimeZoneSync />
      <TimeZoneProvider value={timeZone}>
        <Suspense>
          <RouteGate signedIn={signedIn}>{children}</RouteGate>
        </Suspense>
      </TimeZoneProvider>
      <Suspense>
        <SignInDialog />
      </Suspense>
    </main>
  );
}
