# Wallet host i18n routing spike — `/{locale}/cabinet/*`

Verified 2026-08-21 against Next 16.2.11 (Turbopack), in `next dev` **and** a
production `next build`. Everything below is measured, not inferred.

**Result: `/{locale}/cabinet/wallet` is achievable natively.** It requires trading
`basePath` for `assetPrefix` and moving the app under a `[locale]` segment. The
obvious approach — keeping `basePath` and prefixing links — cannot work, and fails
in a way that is easy to miss.

## Why the obvious approach fails

`next/link` prepends `basePath` to every internal href, unconditionally. Measured
by rendering four links on a page with `basePath: "/cabinet"`:

| authored | emitted |
| --- | --- |
| `<Link href="/wallet">` | `/cabinet/wallet` |
| `<Link href="/ru/cabinet/wallet">` | `/cabinet/ru/cabinet/wallet` ← doubled |
| `<Link href="/ru/wallet">` | `/cabinet/ru/wallet` |
| `<a href="/ru/cabinet/wallet">` | `/ru/cabinet/wallet` (untouched) |

So a locale segment *before* the basePath doubles it. Only a raw `<a>` survives,
and rewriting the wallet host's navigation to raw anchors would cost client-side
routing and prefetching across the whole app — a real regression, not a detail.

`basePath` is static at build time; there is no per-request form of it, and App
Router has no built-in i18n routing (that was Pages Router). So the locale cannot
be made part of the basePath either.

## What works

`basePath` and `assetPrefix` are separable. Drop the former, keep the latter:

```ts
// next.config.ts
assetPrefix: BASE_PATH,   // "/cabinet" — keeps /_next isolated
// basePath: removed
```

and move the routes under `app/[locale]/cabinet/…` so the wallet host's own route
tree *is* the public URL. Measured with that config:

- `GET /ru/cabinet/spike` → **200**, `params.locale === "ru"`.
- `<Link href="/ru/cabinet/wallet">` emits exactly `/ru/cabinet/wallet` — no
  doubling, because there is no basePath left to prepend.
- Assets still serve from `/cabinet/_next/static/…`.

That last point is what makes this cheap on the conductor side: the existing
`beforeFiles` rewrite for `/cabinet/_next/:path*` keeps working untouched, and so
does `shared/config/base-path.ts`'s `apiPath()`, since API and assets stay under
`/cabinet` while only *pages* gain the locale.

Production build confirms the config survives the build:

```
basePath   : ''
assetPrefix: '/cabinet'
```

## Three things this turned up

**1. The root-level `[service]` slug collides.** The wallet host already has a
root-level dynamic segment for microfrontends (`app/(mfe)/[service]`). Adding
`app/[locale]` next to it fails the build outright:

```
Error: You cannot use different slug names for the same dynamic path
('service' !== 'locale').
```

The MFE group has to move under the locale segment too — `app/[locale]/cabinet/(mfe)/[service]`
— along with `(app)` and `(auth)`. This is the largest mechanical part of the work
and it is not optional.

**2. The `/api/*` rewrite stops being basePath-aware.** Today `next.config.ts`
declares `source: "/api/:path*"`, which `basePath` silently turns into
`/cabinet/api/*` externally. With `basePath` gone that source matches bare
`/api/*`, which under the conductor's origin is the shell's own auth surface. The
source must become explicit:

```ts
{ source: "/cabinet/api/:path*", destination: `${BACKEND}/api/:path*` }
```

Getting this wrong would not 404 — it would collide with shell auth routes, which
is a far worse failure than a missing page.

**3. The conductor's SSG landmine does not apply here.** Every wallet host route
builds as `ƒ (Dynamic) server-rendered on demand`, so the `dynamicParams = false` /
`generateStaticParams` trap documented for the public site
(`site_conductor/docs/i18n-routing-spike.md`) has no equivalent here. Nothing needs
enumerating, and there is no build-time backend fetch to fail in a sandbox.

## Conductor side

`proxyZone` forwards `url.pathname` verbatim. Because the zone will own
`/ru/cabinet/*` as real routes, **no path rewriting is needed at all** — the
conductor only needs the locale-prefixed HTML entry point beside the existing one:

```
app/cabinet/[[...path]]/route.ts            (unchanged)
app/[locale]/cabinet/[[...path]]/route.ts   (new — same proxyZone call)
```

The unprefixed `/cabinet/*` mount should stay and redirect to the reader's locale,
so existing links and bookmarks keep working.

Still to decide when implementing: `proxy.ts`'s `PUBLIC` list (`/login`,
`/loggedout`) and its matcher are written against unprefixed paths and need to
become locale-aware, and the shell's per-locale header fragments are a separate
piece of work (see the language-switcher task).
