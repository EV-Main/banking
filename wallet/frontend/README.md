# wallet host frontend — host shell

The Next.js host shell for the bank's wallet host (npm workspace `@evbanking/wallet-host`). Two jobs:

1. **Microfrontend composition.** Every microfrontend (React or Rust/WASM, inline
   widget or whole page) is a self-registering **custom element**. The host mounts
   them with [`<RemoteElement>`](./shared/mfe/RemoteElement.tsx), resolving each by
   logical name from the [registry](./mfe-registry.json), read here server-side for
   page routes.
   Remotes deploy independently — change the registry, not the host. Light DOM only
   (Tailwind v4 tokens break in shadow DOM).
   - Inline widget: render `<RemoteElement>` anywhere in a page.
   - Whole page: the catch-all route `app/(mfe)/[service]/[[...slug]]` mounts a page MFE.

2. **BFF proxy.** The BFF itself is a separate Rust service
   ([`../backend`](../backend)). This app keeps calling same-origin `/api/*`;
   [`next.config.ts`](./next.config.ts) rewrites those to the backend
   (`CABINET_BACKEND_URL`), so the `__Host-`/HttpOnly session cookie + CSRF model
   stays same-origin. The browser never holds a token.

See [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for the full contract and
the React / Rust-WASM producer recipes.

## Pages

Since banking#565 the investor screens live in the invest cabinet and the staff
screens in the central admin. What stays here is what outside links and the
wallet element reach:

| Group | Pages | Why it stays |
|---|---|---|
| `(host)` | `/cabinet`, `/cabinet/wallet`, `/cabinet/wallet/deposit`, `/cabinet/wallet/activity` | The sign-in host and the concierge `KYC_RETURN_URL` target; the wallet panel (the fallback link of wallet notices); the two pages the wallet element links to |
| `(wallet-action)` | `/cabinet/wallet/action/*` | The money-action popups scope hosts open (`@evinvest/wallet`) and the payment answer link |
| `(mfe)` | `/cabinet/[service]/[[...slug]]` | Page microfrontends |
| `(auth)` | `/cabinet/loggedout` | The sign-out landing |

`(host)` and `(wallet-action)` share one session pattern: the route gate gives a
guest the sign-in wall, and `SessionKeeper` renews the access cookie.

## i18n

Five locales (`en` `ru` `vi` `fr` `de`) through `@evinvest/i18n`, under the same
translation policy as the public site: English is canonical, and a translation
whose English source has since changed is refused and falls back to English.

English is written where it renders: every call is `t("key", "English sentence")`
with both arguments literal, and `messages/en/common.json` is generated from the
code by `npm run i18n:extract` — do not edit it by hand. A wire value that picks
its words (a state, a role) goes through a table of literal `t()` calls looked up
by the value (`shared/lib/wire-words.ts`), never a key assembled at runtime: the
extractor refuses one, since it has no English to extract.

`npm run i18n:check` is two gates. `evinvest-i18n-check` fails when the committed
English catalogue no longer matches the code, or on drift. The wallet host's own
`scripts/i18n-check.mts` then holds every locale at 100 % coverage and refuses a
translation identical to its English. The second checker
runs under `tsx` rather than node's `--experimental-strip-types` (which the unit
tests use) because it imports `shared/config/i18n.ts`, and that module loads the
catalogues through the `@/messages/...` tsconfig path alias, which bare node
does not resolve.

**The locale is a cookie here, not a URL segment.** That asymmetry with the
conductor is deliberate. The public site prefixes every non-default locale
(`/ru/team`) because it must serve `hreflang` alternates to a crawler and must not
move already-indexed URLs. The wallet host is entirely behind auth: no crawler, no
indexed URL, and prefixing would mean restructuring every route around a
`[locale]` segment to buy nothing.

So `proxy.ts` negotiates `Accept-Language` once and mints a sticky `ev_locale`
cookie, and never overwrites it — a reader who picks English on a Russian laptop
stays in English. `currentLocale()` (`shared/config/locale.ts`) reads it; the root
layout feeds `I18nProvider` so every route group is covered.

No page here *confirms* that cookie (`ev_locale_chosen`) since the Settings screen
left with banking#565, so every `ev_locale` this zone sees is one the proxy or the
public site seeded. An unprefixed entry
whose `ev_locale` was not confirmed is treated as a guess, so the account's stored
language wins on that device; a URL never writes to the account. A confirmed
choice is kept on that device until it is changed there.

Catalogues live in `messages/<locale>/common.json`. English is a plain map; every
other locale records the English each entry was translated from, which is what
makes drift detectable:

```jsonc
{ "nav.wallet": { "en": "Wallet", "t": "Кошелёк" } }
```

Only the navigation is translated so far; it wants a native review before any
locale is offered to a real investor.

## Observability

Wired through the shared `@evinvest/*` libraries; every integration no-ops until
its env var is set, so local dev needs no configuration.

- **Analytics** (`@evinvest/analytics`) — `PostHogProvider` in
  [`application/providers.tsx`](./application/providers.tsx); capture from client
  components with `useCapture()` / `useAnalytics()`. The key and host come from
  `config.public.posthogKey` / `posthogHost` (`NEXT_PUBLIC_POSTHOG_KEY` / `_HOST`
  through `config.ts`) and are passed to the provider as props — its own env
  fallback reads `process.env[name]` dynamically, which Next never inlines into
  the browser bundle. Both are build-time literals in the root `flake.nix`
  (`cabinetApp.env`); the host also feeds the CSP `connect-src`. What the shared
  seam lacks lives in [`shared/analytics`](./shared/analytics): the activation
  funnel's event names (`login_view`, `session_created`, `kyc_started`,
  `kyc_completed`, `first_deposit` — a contract with the
  site and the PostHog funnel), the `once`/`mark` idempotency marks
  (sessionStorage per tab, localStorage per "first ever"), and `identity` — the
  one hand-rolled vendor call, `posthog.identify(userId)` on the first
  authenticated render, which waits for the provider's lazy `init` before it
  fires. That exception ends when `@evinvest/analytics` grows `identify()`.
- **Error monitoring** (`@evinvest/error-monitoring`) — `ErrorMonitoringProvider`
  (browser) in providers; server/runtime init + request-error capture in
  [`instrumentation.ts`](./instrumentation.ts); build integration via `withSentry`
  in [`next.config.ts`](./next.config.ts). Reads `NEXT_PUBLIC_SENTRY_DSN` (browser)
  / `SENTRY_DSN` (server) and the `SENTRY_ORG`/`PROJECT`/`AUTH_TOKEN` build vars.
- **Experiments** (`@evinvest/experiments`) — the A/B registry lives in
  [`application/experiments.ts`](./application/experiments.ts); sticky variant
  assignment runs in [`proxy.ts`](./proxy.ts). Read a variant in a Server
  Component with `getVariant`, render with `ExperimentTracker` (bridge `onEvent`
  to `useCapture`). Empty until the first experiment is declared.

See [`.env.example`](./.env.example) for the full env surface.

## Dev

```
nix run .#cabinet           # this app (proxies /api/* → the wallet BFF)
nix run .#cabinet-backend   # the BFF (needs piggybank; ports: flake.nix `ports`)
nix run .#dev               # full stack: postgres + tigerbeetle + redis + signer + piggybank + cabinet-backend + cabinet
```

## Data

Every browser→BFF read goes through the cache in `shared/lib/resource.ts`. A view
does **not** call an entity client from a `useEffect`; it declares what it reads
and the cache decides whether that costs a request.

```ts
const wallet = useResource(walletResource);          // no argument
const address = useResource(depositAddressResource, network);   // keyed per rail
```

Why it exists: every screen used to own its own `useEffect(() => fetchX().then(setX))`,
so leaving a page threw its answer away. Moving between two screens of the same
account re-fetched the same balance and preceded each arrival with a skeleton —
which reads as loading a *different* account. Nothing was wrong with any one of
those reads; the problem was that none of them was shared.

Why not Next's `fetch` extension: `next: { revalidate, tags }` is a **server**
extension, applied to fetches Next issues while rendering. This wallet host reads
nothing on the server — `/api/*` is a rewrite straight to the Rust BFF
(`next.config.ts`), there are no route handlers, and every read is a browser call
carrying the user's session cookie. So the cache is client-side, but keeps Next's
vocabulary — `revalidate` in seconds, `tags`, and a `revalidateTag()` — because
the semantics are the same.

Rules:

- **Declare each read once**, in `entities/<x>/model/<x>-resource.ts`, with the
  window it stays fresh for and the tags its mutations move. Views import from
  `model/`, never from `api/`.
- **A mutation names what it moved**, it does not refetch. `cancelWithdrawal`
  names `wallet · withdrawals · operations`, so the balance on *every* open
  surface follows — no call site has to know which screens are mounted. A write
  whose response IS the new state publishes it rather than
  invalidating.
- **`isLoading` is the only state that earns a skeleton.** It is true only when
  there is nothing to show and nothing has failed. Cached data renders on the
  first frame and refreshes behind itself; see the `Settled` rule below, which
  this pairs with exactly.
- **A failed refresh never blanks a figure.** The stale value stays and the error
  is reported beside it. Only a read that has *never* succeeded should surface as
  an error state.
- **Memory only.** Every cached value dies with the page, and sign-out clears it.
- **Warm at idle.** `CacheWarmer` in the `(host)` layout warms the balance at
  idle, so the wallet pages usually paint their figures on the first frame.

## Motion

Everything animated in the wallet host comes from `shared/ui/motion`. Import the
primitives from the slice root; do **not** import `motion/react` in a view. The
point of the slice is that curves, durations and travel distances are decided
once — in `shared/ui/motion/tokens.ts` — rather than per screen, exactly as
colour is decided once in the uikit's variables.

| Primitive | Use for |
|---|---|
| `Settled` | a skeleton handing over to the content it stood in for |
| `Panel` + `PanelPresence` | a popup/drawer/result card mounting and unmounting |
| `PanelSwap` | the content inside an already-open panel changing record |
| `Reveal` | a single block arriving on mount |
| `Stagger` + `StaggerItem` | a screen, list or grid arriving in sequence |
| `AnimatedNumber` | a figure travelling to its new value instead of being replaced |

Rules:

- **This is a money surface.** Motion exists to say *what changed* — never to
  decorate, and never in a way that delays a figure landing on screen. The
  wallet host's `DUR.base` is deliberately about half the landing's.
- **`opacity` and `transform` only**, always once, always collapsing to a plain
  fade under `prefers-reduced-motion` (every primitive handles this; hand-written
  motion must call `useReducedMotion()` itself).
- **A screen arrives as its sections, and the sections animate in place.** Put
  `Stagger` on the container the screen already has — the grid, the flex column
  — and give each section `as` so `StaggerItem` renders the element that was
  already there. Never wrap a section in a new `div`: the wallet host's sections are
  grid and flex items carrying their own placement (`xl:col-start-1`,
  `lg:order-3`, `flex-1`), and a wrapper takes that placement for itself and
  changes the layout. `SECTION_STAGGER` is the step for cards, `STAGGER` for
  rows. A component passed to `as` has to spread its unnamed props — `ref` and
  `style` — onto its DOM node, which is how motion reaches it; the uikit
  primitives do.
- **Two entrances never compose on one element.** A skeleton handover, or a
  nested `Reveal`, that happens while the section around it is still arriving
  keeps its fade and drops its travel — otherwise the content sets off again as
  the card is settling and the card stutters. Sections publish this through
  `shared/ui/motion/entrance`; `Settled`, `Reveal` and `StaggerItem` all read
  it, so it is automatic and there is nothing to pass down.
- **Chrome does not animate on navigation.** The entrance belongs to page
  content. The mobile app bar is the exception, and only because it *is* the
  page's title.
- **A figure lands without an animation frame.** `AnimatedNumber` counts on
  `requestAnimationFrame`, which a hidden tab never gets — so the count alone
  would leave a page opened in the background reading `$0.00` until it was
  brought to the front (#346). Its driver (`shared/ui/motion/number-driver.ts`)
  writes the final figure by whichever comes first: the count completing, the
  page going hidden, or a timer just past `DUR.slow`. Hand-written motion that
  gates a figure on a frame needs the same guarantee.
- **A figure that arrived in the server's HTML does not count in.** `AnimatedNumber`
  starts from 0 only for a figure the browser draws; one hydrated from a server read
  (`useSeededResource`) has been on screen since the HTML landed, and counting it
  up would take it away from the reader. Same for a screen's entrance: a page that
  renders server reads passes `entrance={false}` for them, and no arrival plays in a
  streaming fallback, which is never hydrated (`shared/lib/use-hydrated`).
- **`Settled` does not animate when no skeleton was shown.** Data already present
  on the first render cuts straight in — fading it would invent a delay the data
  never had. It decides by adjusting state *during render*, not in an effect: an
  effect would paint the content opaque once and only then start the fade, and
  that flash is worse than the cut it replaced.
- **A panel that swaps records is two motions, not one.** Key `Panel` on whether
  the panel is open — never on which record it shows, or every row click plays a
  full exit and enter — and put `PanelSwap` inside for the record change.
- **Alerts and result cards that replace one another share one
  `PanelPresence`.** A submit that fails should swap review → error in place
  rather than emptying the column and refilling it. See `views/wallet/withdraw`.
- **A marker that moves is one node per surface, mounted once and translated** —
  never a per-item background that blinks between rows, and never a `layoutId`
  pill that mounts inside whichever item is active. Position it by arithmetic or
  by measurement, and move it with a CSS transition on `transform`. `layoutId`
  is not used, for two reasons found the hard way: the slide it produces is a
  layout projection driven from the main thread, which drops frames while the
  page just navigated to is rendering; and its measurement adds `window.scroll`
  to every box unless the element itself is `position: fixed`, so a marker
  inside a fixed wrapper takes its origin from the previous page's scroll offset.
- Note the eslint guard on arbitrary Tailwind values applies here too — motion
  values live in props and tokens, not in class strings.
