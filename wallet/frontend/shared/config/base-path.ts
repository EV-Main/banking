import { isLocale, type Locale } from "@evinvest/i18n";

// The zone mount prefix. The wallet host is a Next.js multi-zone served under the
// conductor's domain at /cabinet.
//
// Two different things now live under this prefix and they no longer have the
// same shape:
//
//   assets + API   /cabinet/_next/*, /cabinet/api/*      — no locale, ever
//   pages          /{locale}/cabinet/wallet              — locale first
//
// `basePath` used to make the first case automatic and is gone (it would have
// prepended itself to the second, turning /ru/cabinet/wallet into
// /cabinet/ru/cabinet/wallet — see docs/i18n-wallet-routing-spike.md).
// `assetPrefix` covers `/_next/*`; everything else is explicit through the two
// helpers below, and which one you want is decided by whether a *reader* sees it.
export const BASE_PATH = "/cabinet";

/**
 * A BFF route. Locale-free on purpose: the API answers in data, not prose, and
 * the one endpoint whose payload is language-dependent takes the locale as a
 * parameter rather than living at a different URL per language.
 */
export const apiPath = (path: `/${string}`): string => `${BASE_PATH}${path}`;

/**
 * A page URL the reader can end up looking at. Every in-zone link goes through
 * here — with `basePath` gone, a bare `<Link href="/wallet">` now points at the
 * conductor's origin root instead of the zone, which fails as a 404 on a page
 * that has nothing to do with the wallet host.
 */
export const zonePagePath = (locale: Locale, path: `/${string}`): string =>
  path === "/" ? `/${locale}${BASE_PATH}` : `/${locale}${BASE_PATH}${path}`;

/**
 * The page URL a zone request *should* have had, when its locale segment is
 * missing or is not a locale we serve — or `null` when the path is not a zone
 * page at all and there is nothing to repair.
 *
 *   /cabinet/wallet      → /{locale}/cabinet/wallet   (an old link, or the
 *                                                      conductor's unprefixed mount)
 *   /zz/cabinet/wallet   → /{locale}/cabinet/wallet   (a typo, or a language we
 *                                                      do not serve)
 *   /zz/anything         → null
 *
 * The second case is the one that used to fall through. It is reachable in
 * production — the conductor proxies `/{anything}/cabinet/*` here verbatim — and
 * it landed on `app/[locale]/layout.tsx`'s `notFound()`, which is thrown by the
 * ROOT layout and so has no `not-found.tsx` boundary above it to render: Next
 * served its own black "404 | This page could not be found" instead of ours.
 * Repairing the URL is better than any 404 anyway. Someone typing `/pt/cabinet`
 * wants their wallet, and we have five languages to offer them.
 */
export const localeRepairedPath = (pathname: string, locale: Locale): string | null => {
  const zone = BASE_PATH.slice(1);
  const [, first, ...rest] = pathname.split("/");
  // `/cabinet/…` — nothing in front of the zone prefix.
  if (first === zone) return zonePagePath(locale, zoneSuffix(rest));
  // `/<not-a-locale>/cabinet/…` — a segment in front of it that has to go.
  if (rest[0] === zone) return zonePagePath(locale, zoneSuffix(rest.slice(1)));
  return null;
};

/** Path segments after the zone prefix, rebuilt as the `/`-rooted tail `zonePagePath` takes. */
const zoneSuffix = (parts: string[]): `/${string}` => {
  const tail = parts.filter(Boolean).join("/");
  return (tail ? `/${tail}` : "/") as `/${string}`;
};

/**
 * The inverse of {@link zonePagePath}: a real request path back to the zone-relative
 * one the app reasons in.
 *
 * `usePathname()` used to hand back a basePath-stripped path for free, so the nav
 * could compare it against a plain `/wallet`. With `basePath` gone it returns the
 * whole thing — `/ru/cabinet/wallet` — and every such comparison silently stops
 * matching. Reading a path is therefore as much of a rule as writing one, and it
 * belongs next to its twin rather than inlined at each caller.
 *
 * Unrecognised input degrades instead of throwing: a missing or unknown locale segment
 * is left alone, and a path outside the zone comes back unchanged.
 */
export const zonePathname = (pathname: string): `/${string}` => {
  // The locale segment is matched against LOCALES directly rather than through the i18n
  // package's `splitLocalePath`, which leaves an explicit default-locale prefix in place
  // — `/en/cabinet/wallet` comes back whole from it, so every English URL (the majority)
  // would have gone unstripped.
  const [, first, ...rest] = pathname.split("/");
  const path = isLocale(first) ? `/${rest.join("/")}` : pathname;
  if (path === BASE_PATH) return "/";
  // Compared with the trailing slash so a sibling like `/cabinets` is not mistaken for
  // this zone and half-eaten.
  return (path.startsWith(`${BASE_PATH}/`) ? path.slice(BASE_PATH.length) : path) as `/${string}`;
};

/**
 * Every request shape under this prefix that is NOT a page: Next's build output,
 * the BFF, and the element-remote bundles the conductor injects for anonymous
 * visitors. None of them may be locale-redirected or gated behind the session.
 *
 * The optional zone prefix is the entire point. `basePath` used to strip
 * `/cabinet` before the proxy matcher ran, so naming these anchored at the
 * leading `/` was right. With `basePath` gone (see the note at the top of this
 * file) the proxy sees the real `/cabinet/_next/...`, an anchored test matches
 * nothing, and every stylesheet, script, font and API call is treated as a
 * locale-less page: 307 to `/{locale}/cabinet/…`, then bounced to `/login`,
 * which answers `200 text/html`. The browser refuses each one for its MIME type,
 * so the wallet host renders unstyled and never hydrates. That shipped in v0.2.57.
 *
 * The set of names is deliberately unchanged from the matcher's original list —
 * widening it (say, to all of `_next/`) would quietly ungate paths this zone has
 * never exposed.
 */
const NON_PAGE = new RegExp(
  `^(?:${BASE_PATH})?/(?:_next/static/|_next/image|api/|mfe/|wallet/mfe/|favicon\\.ico$)`
);

/** @see NON_PAGE — true when `pathname` is an asset, a BFF call or an MFE bundle.
 *  `/wallet/mfe/*` is the wallet bundles' public directory (`@evinvest/wallet`), served
 *  from `public/mfe/wallet/` by the next.config rewrite. */
export const isNonPagePath = (pathname: string): boolean => NON_PAGE.test(pathname);

/**
 * Whether `pathname` is a page of this zone — `/cabinet`, `/cabinet/…`, or the same
 * behind a locale segment. Assets, BFF calls and MFE bundles under the prefix are not
 * pages (see {@link isNonPagePath}), and a sibling route like `/cabinets` is not this zone.
 *
 * The account chip needs the answer from the document it was mounted into: the same
 * bundle renders on the public site and inside the wallet host, and where its link goes
 * depends on which of the two the reader is looking at.
 */
export const isZonePage = (pathname: string): boolean => {
  if (isNonPagePath(pathname)) return false;
  const zone = BASE_PATH.slice(1);
  // Whole segments, so `/cabinets` and `/en/cabinets/list` stay someone else's routes.
  const [, first, second] = pathname.split("/");
  return first === zone || (isLocale(first) && second === zone);
};

/**
 * The same zone page, in another locale — path, query and hash intact.
 *
 * Two callers relocalise the current page (the settings language switcher and
 * `LocaleSync`) and they must agree: one of them dropping the query string is a
 * bug that only appears the first time a zone page takes a parameter. Takes
 * the URL parts rather than reading `window` so it stays pure and testable
 * alongside its siblings here.
 */
export const relocalise = (
  locale: Locale,
  url: { pathname: string; search: string; hash: string },
): string => zonePagePath(locale, zonePathname(url.pathname)) + url.search + url.hash;

// The money actions scope hosts open on evinvest.ltd, by the exact paths the host SDK
// (`@evinvest/wallet` `WALLET_ACTION_PATHS`, `walletPaymentUrl`) builds: no locale, no zone
// prefix, because they are the wallet's public addresses rather than zone pages. The
// payment id has the SDK's id rule, so nothing but a plain segment reaches the page route.
const WALLET_ACTION = /^\/wallet\/(withdraw|move|pay|mandate|payment\/[A-Za-z0-9_-]{1,128})$/;

/**
 * The zone-relative page behind a wallet action address — `/wallet/withdraw` →
 * `/wallet/action/withdraw`, `/wallet/payment/<id>` → itself, under the zone — or
 * `null` for any other path. The proxy redirects there under the reader's locale with the
 * query intact (opener, state, scope, invoice).
 */
export const walletActionPage = (pathname: string): `/${string}` | null => {
  const match = WALLET_ACTION.exec(pathname);
  if (!match) return null;
  // The payment page has no `action/` segment: its path is the one notices carry.
  return match[1].startsWith("payment/") ? `/wallet/${match[1]}` : `/wallet/action/${match[1]}`;
};
