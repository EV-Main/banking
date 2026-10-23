import { currentLocale } from "@/shared/config/locale";
import { LocalisedStatus } from "@/views/status";

// Next's `unauthorized.tsx` file convention (experimental `authInterrupts`, the
// same flag that backs `forbidden.tsx`): rendered with a 401 whenever the
// `unauthorized()` interrupt is invoked. Not a browsable route.
//
// 401 vs 403 is "who are you" vs "not you": a missing or expired session is a 401,
// a valid session without the role is a 403. The common signed-out case never
// reaches here — `proxy.ts` bounces an unauthenticated page request to /login with
// a `returnTo`, which is better than an error page when there is an obvious next
// step. This is for the case the proxy cannot see: the session cookie is present
// so the gate lets the request through, but the short-TTL access cookie behind it
// has expired by the time a server render calls the BFF and gets a 401 back. With
// no file for the interrupt that fell through to the generic error boundary and
// showed a 500 for what is really "please sign in again".
export default async function UnauthorizedPage() {
  return <LocalisedStatus kind="unauthorized" locale={await currentLocale()} />;
}
