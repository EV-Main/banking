import type { NextConfig } from "next";

import { BASE_PATH } from "./shared/config/base-path";

// This file deliberately reads its own env directly instead of routing
// through ./config (backed by @evinvest/settings) or shared/config/security.ts
// (which imports ./config for IN_PRODUCTION). Next's next-config-ts
// transpiler bundles this file's static local imports into a single
// next.config.compiled.js, so importing either — even lazily via a dynamic
// import(), which the transpiler doesn't follow and which has no loader for
// bare .ts paths at runtime — drags in @evinvest/settings, an ESM-only
// package (its exports map has no "require" condition) that breaks
// `next build` with ERR_PACKAGE_PATH_NOT_EXPORTED. Same class of problem as
// the Sentry note below, same resolution: forgo full integration here.
function requiredEnv(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`missing required env var ${name}`);
  return value;
}
// Where next dev/production proxies same-origin /api/* to the Rust BFF —
// baked into routes-manifest.json at build time, so it must resolve
// synchronously here (see shared/config/base-path.ts for the analogous
// basePath note).
const BACKEND = requiredEnv("CABINET_BACKEND_URL");
const IN_PRODUCTION = process.env.NODE_ENV === "production";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  // Self-contained production server (.next/standalone) — the container runs
  // `node server.js` without an `npm install` (mirrors the conductor's setup).
  output: "standalone",
  // Multi-zone mount: the cabinet lives under the conductor's domain at
  // /cabinet. basePath prefixes every page AND /_next asset with it, so nothing
  // collides with the conductor's own routes/assets; the conductor rewrites
  // /cabinet/* to this deployment. The rewrite source below is basePath-aware
  // (externally it matches /cabinet/api/*); hand-built browser URLs go through
  // shared/config/base-path.ts.
  // Pages carry the locale, assets do not. `basePath` cannot express that: it is
  // static, and Next prepends it to every `<Link>` href — so a locale-prefixed
  // `/ru/cabinet/wallet` would come out as `/cabinet/ru/cabinet/wallet`. Trading
  // it for `assetPrefix` keeps `/_next/*` isolated under /cabinet (so the
  // conductor's beforeFiles rewrite is untouched) while letting the route tree
  // own the whole public path. Measured both ways in docs/i18n-cabinet-routing-spike.md.
  experimental: {
    // No `rootParams` flag: since Next 16.4 `next/root-params` is on by default and
    // the option is gone from the config type. `currentLocale()` still depends on
    // it — it reads the root layout's `[locale]` from components Next hands no props.
    // The `forbidden.tsx` / `unauthorized.tsx` file conventions. Required, not
    // cosmetic: without it next-app-loader never resolves either file, so
    // `forbidden.tsx` sat in the tree doing nothing and a `forbidden()` /
    // `unauthorized()` call fell through to the 500 boundary.
    authInterrupts: true,
    // Enables `app/global-not-found.tsx` — the 404 for URLs that matched no route
    // at all. Required for the same reason: the root layout is
    // `app/[locale]/layout.tsx`, so a URL that resolved no `[locale]` (a bad locale
    // segment, a path outside the zone) has no layout to render a nested
    // `not-found.tsx` inside, and Next serves its built-in black 404 instead.
    globalNotFound: true,
    // Off because the conductor already runs this check for every `/{locale}/cabinet/*`
    // request, and the two checks cannot agree. The `_rsc` hash the router appends
    // covers `Next-Url`; the conductor validates it with that header present, then
    // drops the header before its zone proxy forwards the request (site_conductor#229),
    // so a second validation here expects a different hash — each side 307s to the
    // other's value until the browser gives up and reloads the document (banking#491).
    // The check guards CDN caches that ignore `Vary`; every cabinet page is private,
    // no-store and never cached, so nothing is lost by leaving it to the conductor.
    validateRSCRequestHeaders: false,
  },
  assetPrefix: BASE_PATH,
  async rewrites() {
    // Explicit /cabinet now that basePath no longer prefixes this for us. Getting
    // this wrong does not 404 — a bare `/api/*` collides with the conductor's own
    // shell-owned auth routes on the same origin.
    return [
      { source: "/cabinet/api/:path*", destination: `${BACKEND}/api/:path*` },
      // `public/` is served from the origin root, so the element-remote bundles
      // land on `/mfe/*` — but both MFE registries, and therefore the conductor's
      // injected shell, ask for `/cabinet/mfe/*`. `basePath` used to add that
      // prefix for free; without it the account chip 404s on every page of the
      // site. `assetPrefix` does not cover this: it rewrites `/_next/*` only.
      { source: "/cabinet/mfe/:path*", destination: "/mfe/:path*" },
      // The wallet bundles' public directory — `https://evinvest.ltd/wallet/mfe/`, the
      // prefix `@evinvest/wallet` holds every registry entry to and hosts name in their
      // CSP. Built to `public/mfe/wallet/` beside the chip.
      { source: "/wallet/mfe/:path*", destination: "/mfe/wallet/:path*" },
    ];
  },
  // Request-invariant hardening on every response, mirroring
  // shared/config/security.ts's staticSecurityHeaders() (kept in sync by
  // hand — see the module comment above for why that helper can't be
  // imported here). The nonce-bearing CSP itself is set per-request in
  // proxy.ts, which imports shared/config/security.ts directly (a normal
  // ESM runtime context, not this file's CJS-transpiled one).
  async headers() {
    const headers = [
      { key: "X-Frame-Options", value: "DENY" },
      { key: "X-Content-Type-Options", value: "nosniff" },
      { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
    ];
    if (IN_PRODUCTION) {
      headers.push({
        key: "Strict-Transport-Security",
        value: "max-age=63072000; includeSubDomains; preload",
      });
    }
    // The two token approval pages need `no-referrer`, not the site-wide
    // `strict-origin-when-cross-origin`: their URL contains a single-use approval
    // credential, and the default still sends the whole URL to same-origin destinations
    // (docs/CONSILIUM.md, policy 6). `proxy.ts` sets the same header per-request; stating
    // it here as well makes the outcome deterministic rather than depending on which of
    // the two writers Next lets win. A more specific `source` is listed first so it is not
    // shadowed by the catch-all below.
    //
    // `:locale` matches the one segment, so this covers `/en/cabinet/approve/<token>` and
    // its four siblings without matching anything deeper.
    const tokenPageHeaders = headers
      .filter((h) => h.key !== "Referrer-Policy")
      .concat({ key: "Referrer-Policy", value: "no-referrer" });
    // The element-remote bundles keep fixed names (`account-chip.js`/`.css`: a content hash
    // would make the conductor read a manifest to find them), so they cannot be immutable.
    // Without any Cache-Control every page of the site revalidated them; five minutes fresh
    // plus ten minutes of stale-while-revalidate bounds how long a release — or a security
    // fix — takes to reach a returning visitor (15 min) while sparing the header a round trip
    // on every navigation. A day of SWR kept a revoked bundle alive far too long. Both
    // spellings: the conductor asks for `/cabinet/mfe/*`, a direct run of the zone for `/mfe/*`.
    const mfeHeaders = [{ key: "Cache-Control", value: "public, max-age=300, stale-while-revalidate=600" }];
    const walletCors = [{ key: "Access-Control-Allow-Origin", value: "*" }];
    return [
      { source: "/mfe/:path*", headers: mfeHeaders },
      { source: "/cabinet/mfe/:path*", headers: mfeHeaders },
      // The wallet elements are loaded by OTHER origins (the scope hosts): a module script
      // and the sheet's fetch are CORS requests, refused without this. Public, cookie-less
      // files, so `*` grants nothing a direct download would not.
      { source: "/wallet/mfe/:path*", headers: [...mfeHeaders, ...walletCors] },
      { source: "/mfe/wallet/:path*", headers: walletCors },
      { source: "/:locale/cabinet/approve/:token", headers: tokenPageHeaders },
      { source: "/:locale/cabinet/owner-removal/:token", headers: tokenPageHeaders },
      { source: "/:path*", headers },
    ];
  },
};

// No build-time Sentry wrapper here: @evinvest/error-monitoring's `./next` export
// is ESM-only, but Next loads next.config.ts as CJS, so `withSentry` can't be
// imported in this file. Runtime error capture is wired instead in
// `instrumentation.ts` (server) and `ErrorMonitoringProvider` (browser); only
// build-time source-map upload (which needs SENTRY_* secrets) is forgone.
export default nextConfig;
