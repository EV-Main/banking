import { cookies } from "next/headers";
import { type ReactNode, Suspense } from "react";

import { RouteGate } from "@/application/layout/route-gate";
import { SessionKeeper } from "@/application/layout/session-keeper";
import { SignInDialog } from "@/features/auth/ui/sign-in-dialog";
import { COOKIES } from "@/shared/config/cookies";
import { renderTimeZone } from "@/shared/config/time-zone";
import { isSessionId } from "@/shared/lib/access-renewal";
import { TimeZoneProvider } from "@/shared/lib/time-zone";

// The money actions scope hosts open on evinvest.ltd (`/wallet/{withdraw,move,pay,mandate}`,
// redirected here by the proxy) — usually in a 480 px popup. Chromeless on purpose: no rail,
// no bottom nav, nothing that walks the reader off the action before it reports back to the
// window that opened it. The session parts of the app shell stay: the route gate (a guest
// gets the sign-in wall, not a form that can only answer 401) and the keeper that renews the
// short-lived access cookie while the form is open.
//
// No `frame-ancestors` work here: every cabinet response already carries
// `frame-ancestors 'none'` and `X-Frame-Options: DENY` (proxy.ts, next.config.ts), which is
// what keeps a host from framing this page and clicking through it.
export default async function WalletActionLayout({ children }: { children: ReactNode }) {
  const timeZone = await renderTimeZone();
  const signedIn = isSessionId((await cookies()).get(COOKIES.session)?.value);
  return (
    <main className="min-h-dvh bg-background">
      <SessionKeeper />
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
