import { type NextRequest, NextResponse } from "next/server";

import { createAbMiddleware } from "@evinvest/experiments/next";
import { isLocale, negotiate, type Locale } from "@evinvest/i18n";

import { experiments } from "@/application/experiments";
import { config as appConfig } from "@/config";
import { zonePagePath, isNonPagePath, localeRepairedPath, walletActionPage } from "@/shared/config/base-path";
import { COOKIES } from "@/shared/config/cookies";
import { movedPageTarget, renamedPaymentTarget } from "@/shared/config/moved-pages";
import { isTokenApprovalPath } from "@/shared/config/public-routes";
import { contentSecurityPolicy, websocketOrigin } from "@/shared/config/security";
import { isSessionId, needsRenewal, renewAccess, type Renewal } from "@/shared/lib/access-renewal";
import { localeIsGuessed } from "@/shared/lib/locale-guess";
import { decideSession } from "@/shared/lib/session-gate";

const CSP_HEADER = "content-security-policy";
const REFERRER_HEADER = "referrer-policy";

// A/B assignment boundary (Next 16 "proxy", formerly middleware; Node runtime).
// Assigns a sticky `ab_<key>` cookie per experiment in the registry on first
// visit. A no-op while `experiments` is empty.
const ab = createAbMiddleware(experiments);

// Every page renders for a guest; what it shows them is the route gate's
// (`application/layout/route-gate.tsx`), and the BFF still verifies the session on every
// API call. This module imports `next/server` and so cannot be unit-tested at all — what it
// decides about the session lives in `shared/lib/session-gate.ts`.

/** The locale segment of `/{locale}/cabinet/…`, or null when there is none. */
function localeOf(pathname: string): Locale | null {
  const first = pathname.split("/")[1];
  return isLocale(first) ? first : null;
}

export async function proxy(req: NextRequest) {
  const { pathname, search } = req.nextUrl;

  // Assets, the BFF and the MFE bundles are not pages: no locale redirect, no
  // session gate, no cookie writes. The matcher below excludes them as well, so
  // this looks redundant — it is not. When that exclusion silently stopped
  // matching (basePath removal, v0.2.57) it reached production and served every
  // stylesheet and script as the login page. Correctness lives here, where it is
  // unit-tested; the matcher is an optimisation that keeps this proxy off the
  // asset hot path.
  if (isNonPagePath(pathname)) return NextResponse.next();

  // A value concierge could not have minted is no session at all — and must never be
  // written into an outgoing Cookie header (see `shared/lib/access-renewal.ts`).
  const signedIn = isSessionId(req.cookies.get(COOKIES.session)?.value);
  const locale = localeOf(pathname);

  // Per-request nonce: written onto the forwarded request headers so Next applies
  // it to its own inline bootstrap scripts (keeping script-src free of
  // 'unsafe-inline'), and echoed on the response so the browser enforces the CSP.
  const nonce = crypto.randomUUID().replaceAll("-", "");
  // The consilium's revision socket is same-origin, but `'self'` has not always covered a
  // websocket handshake and a blocked one fails silently — so the origin is named
  // explicitly, derived from the request because only it knows the public host.
  const csp = contentSecurityPolicy(
    nonce,
    websocketOrigin(
      req.headers.get("x-forwarded-host") ?? req.headers.get("host"),
      req.headers.get("x-forwarded-proto"),
    ),
  );
  req.headers.set(CSP_HEADER, csp);

  // Before the locale repair, so an unprefixed old link takes one hop to an off-site
  // target, and before the routes, where the MFE catch-all would answer 404. 307, not 308:
  // a browser keeps a 308 for good, and the account pages may move again.
  const moved = movedPageTarget(pathname, search, {
    platform: appConfig.public.platformUrl,
    investCabinet: appConfig.public.investCabinetUrl,
  });
  if (moved) return withCsp(NextResponse.redirect(new URL(moved, req.nextUrl), 307), csp);

  const renamed = renamedPaymentTarget(pathname, search);
  if (renamed) return withCsp(NextResponse.redirect(new URL(renamed, req.nextUrl), 308), csp);

  // A scope host's money action (`https://evinvest.ltd/wallet/withdraw?opener=…&state=…`)
  // lands on its zone page in the reader's language, query intact — the page reads its
  // opener and nonce from it. Same locale choice as the repair below; no guess mark, since
  // the popup is not where a reader settles their language.
  const actionPage = walletActionPage(pathname);
  if (actionPage) {
    const remembered = req.cookies.get(COOKIES.locale)?.value;
    const chosen = isLocale(remembered) ? remembered : negotiate(req.headers.get("accept-language"));
    const url = req.nextUrl.clone();
    url.pathname = zonePagePath(chosen, actionPage);
    return withCsp(NextResponse.redirect(url), csp);
  }

  // No usable locale in the path: an old link, a bookmark, the conductor's
  // unprefixed /cabinet mount, or a first segment that looks like a locale and is
  // not one (`/zz/cabinet/wallet`, `/pt/cabinet` — a typo, or a language we do not
  // serve). Send them all to a real URL rather than 404ing — the cookie is the
  // reader's last choice, Accept-Language the first guess, English the floor.
  //
  // The cookie is now written by the public site too (site_conductor
  // `scripts/locale-cookie.ts`), mirroring the locale of whatever page the reader
  // was on, so "their last choice" finally includes the language they were reading
  // on the landing a moment ago rather than only a previous wallet visit.
  //
  // The `/<not-a-locale>/cabinet/…` half is why this is not a plain prefix test.
  // Those requests used to reach `app/[locale]/layout.tsx` and its `notFound()`,
  // which the ROOT layout throws and so has no `not-found.tsx` above it to render
  // — the reader got Next's built-in black 404 rather than ours. `localeRepairedPath`
  // covers both shapes, so repairing the URL fixes the page they land on as well as
  // the page they would otherwise be shown.
  if (!locale) {
    const remembered = req.cookies.get(COOKIES.locale)?.value;
    const chosen = isLocale(remembered)
      ? remembered
      : negotiate(req.headers.get("accept-language"));
    const repaired = localeRepairedPath(pathname, chosen);
    if (repaired) {
      const url = req.nextUrl.clone();
      url.pathname = repaired;
      const res = withCsp(NextResponse.redirect(url), csp);
      // Accept-Language is a header the reader never set, so a locale derived from it
      // is a guess, not a preference — and the account holder may well have stored a
      // real one from another device. The app cannot tell the two apart on its own:
      // this redirect carries no cookie, but the request it redirects TO does get one
      // written from the (guessed) URL by `withLocale` below, so by the time any page
      // renders the evidence is gone. Mark it here, while it is still known;
      // `LocaleSync` reads the mark, prefers the stored profile language over the
      // guess, and clears it. Session-scoped: a guess is only about this visit.
      //
      // A bogus locale segment is as much a guess as a missing one — `/pt/cabinet`
      // tells us which language the reader wanted and precisely that we do not serve
      // it — so it is marked on the same terms.
      //
      // The mark carries the guessed locale rather than a bare flag, so it can only
      // ever describe the page it was minted for. A flag would outlive its redirect
      // whenever the reader never reaches a page that clears it (the profile fetch
      // fails, or they land on /login, which mounts no LocaleSync) and would then
      // read as "this URL was guessed" on the next deliberate /de/cabinet link they
      // follow — bouncing them off the page they chose. Matching on the value makes
      // a stale mark inert instead.
      //
      // A remembered locale Settings never confirmed counts as a guess too (#367): the
      // proxy and the public site seed `ev_locale` from URLs, and such a cookie must not
      // pin a device to a language the account does not have.
      if (localeIsGuessed(remembered, req.cookies.get(COOKIES.localeChosen)?.value)) {
        res.cookies.set(COOKIES.localeGuessed, chosen, {
          path: "/",
          sameSite: "lax",
          secure: appConfig.authCookieSecure,
          httpOnly: false,
        });
      }
      return res;
    }
  }

  // Renew a lapsing access cookie before anything renders with it — see
  // `shared/lib/access-renewal.ts`. The renewed token replaces the stale one on the
  // forwarded request (the A/B proxy forwards `req.headers`, so the CSP header set above
  // rides along), and the identity plane's Set-Cookie lines go to the browser verbatim.
  // What follows from the outcome is decided in `shared/lib/session-gate.ts`.
  const renewal = signedIn ? await renewIfLapsing(req) : null;
  const decision = decideSession({ pathname, search, locale, hasSession: signedIn, renewal, cookieNames: COOKIES });
  for (const [name, value] of Object.entries(decision.requestCookies)) req.cookies.set(name, value);
  for (const name of decision.deleteCookies) req.cookies.delete(name);
  const finish = (res: NextResponse) => withSetCookies(res, decision.setCookies);

  if (decision.redirect) {
    const url = req.nextUrl.clone();
    url.pathname = decision.redirect.pathname;
    url.search = decision.redirect.search;
    return finish(withCsp(NextResponse.redirect(url), csp));
  }

  return finish(withReferrerPolicy(withCsp(withLocale(req, ab(req)), csp), pathname));
}

// Only when the cookie is missing or close to lapsing, so the common request — a fresh
// token, an asset, a prefetch inside the TTL — never leaves this process. Unset
// `AUTH_WEB_URL` turns the whole step off.
//
// A router prefetch cannot be told apart here: Next strips `next-router-prefetch` (with
// the other flight headers) from the request before the proxy sees it. None arrive today
// because `shared/ui/zone-link.tsx` turns link prefetching off; turning it back on would
// let a hovered link slide the session's expiry.
async function renewIfLapsing(req: NextRequest): Promise<Renewal | null> {
  const authWebUrl = appConfig.authWebUrl;
  const session = req.cookies.get(COOKIES.session)?.value;
  if (!authWebUrl || !isSessionId(session) || !needsRenewal(req.cookies.get(COOKIES.access)?.value, Date.now())) return null;
  return renewAccess({
    authWebUrl,
    sessionCookie: { name: COOKIES.session, value: session },
    accessCookieName: COOKIES.access,
    csrfCookieName: COOKIES.csrf,
    secure: appConfig.authCookieSecure,
  });
}

// Appended last, as raw lines: `res.cookies.set` rewrites the Set-Cookie header from its
// own map, so anything it is asked to do after this would drop them.
//
// A response carrying someone's fresh credential must never be stored by a shared cache.
// Pages are no-store today only because the root layout happens to read `headers()`; this
// says it where the credential is added.
function withSetCookies(res: NextResponse, lines: readonly string[]): NextResponse {
  if (lines.length === 0) return res;
  for (const line of lines) res.headers.append("set-cookie", line);
  res.headers.set("cache-control", "private, no-store");
  return res;
}

// On the two approval pages the URL *is* the credential, so the wallet host's default
// `strict-origin-when-cross-origin` is not enough: it still sends the full URL along with
// same-origin requests, and the origin alone to everywhere else. `no-referrer` closes that
// channel for exactly the pages that have something to leak (docs/CONSILIUM.md, policy 6).
// Scoped rather than global — the rest of the wallet host gains nothing from it and loses the
// same-origin referrer that analytics and error reports read.
function withReferrerPolicy(res: NextResponse, pathname: string): NextResponse {
  if (isTokenApprovalPath(pathname)) res.headers.set(REFERRER_HEADER, "no-referrer");
  return res;
}

// Assigns a sticky locale on first visit — the same shape as the A/B cookie.
//
// `negotiate` reads Accept-Language, which is a *suggestion*. On the public site
// that distinction matters enormously: a language redirect can bury the other
// locales for a crawler arriving from a US IP, which is why the conductor never
// auto-switches. Here there is no crawler and no URL to redirect — the header is
// simply the best first guess for a signed-in human, and the cookie makes their
// correction stick.
//
// Deliberately never overwritten once set. A reader who chose English on a
// Russian-configured laptop must not be flipped back on the next navigation.
function withLocale(req: NextRequest, res: NextResponse): NextResponse {
  // The URL is authoritative now; this only remembers it, so an unprefixed
  // /cabinet entry later resolves to the language the reader was last reading
  // rather than re-guessing from a header they never set.
  const fromUrl = localeOf(req.nextUrl.pathname);
  if (!fromUrl && req.cookies.get(COOKIES.locale)?.value) return res;
  res.cookies.set(COOKIES.locale, fromUrl ?? negotiate(req.headers.get("accept-language")), {
    path: "/",
    sameSite: "lax",
    secure: appConfig.authCookieSecure,
    // Readable by the client switcher. It carries no authority, so HttpOnly
    // would buy nothing and cost the switcher a round trip.
    httpOnly: false,
    maxAge: 60 * 60 * 24 * 365,
  });
  return res;
}

function withCsp(res: NextResponse, csp: string): NextResponse {
  res.headers.set(CSP_HEADER, csp);
  return res;
}

export const config = {
  // "/" is listed explicitly: the negative lookahead alone never matches the bare
  // index, so an unauthenticated visitor reached the page at the zone root
  // while every sub-route correctly bounced to /login. Gate the index too.
  // `mfe/` is excluded like `_next/static`: the public/mfe/* element-remote bundles
  // are static assets the conductor injects even for anonymous visitors (the chip
  // renders its own signed-out CTA), so they must be reachable without a session.
  //
  // Each name is listed twice, bare and `cabinet/`-prefixed, because this zone no
  // longer has a `basePath` for Next to strip before matching — the real paths are
  // `/cabinet/_next/static/…`, `/cabinet/api/…`, `/cabinet/mfe/…`. The bare forms
  // stay for a direct (dev) run of the zone. `isNonPagePath` is the same rule in
  // code and is what the tests pin; keep the two in step.
  matcher: [
    "/",
    "/((?!(?:cabinet/)?(?:api|_next/static|_next/image|favicon.ico|mfe/|wallet/mfe/)).*)",
  ],
};
