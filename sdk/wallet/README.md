# @evinvest/wallet

Host wrappers for the **banking wallet microfrontends** — the wallet, the
pay-invoice button and the mandate button — over [`@evinvest/mfe`](https://www.npmjs.com/package/@evinvest/mfe), plus the popup round trip
that carries a money action to evinvest.ltd and brings its result back, the
opt-in inline mode, the CSP sources a host needs, and the bundle integrity
checks. Hosts: the invest cabinet, the SA panel, admin. The elements are
banking's, served from `https://evinvest.ltd/wallet/mfe/`. This package only
puts the pieces together: it has no iframe and no money logic.

- `.` — server-safe core: registry constants, tags, `openWalletAction` and its
  URL and message checks (they touch `window` only when called),
  `parseWalletResultEvent`, `walletCsp`, the integrity helpers.
- `./react` — `Wallet`, `PayInvoiceButton`, `MandateButton` (a `"use client"`
  bundle; `react` is an optional peer).

`@evinvest/mfe` is a **peer**, shared with the host so each element loads once.

## Elements

| Element | Tag | Registry name |
| --- | --- | --- |
| Wallet | `mfe-banking-wallet` | `banking.wallet` |
| Pay invoice | `mfe-banking-pay-invoice` | `banking.pay-invoice` |
| Mandate | `mfe-banking-mandate` | `banking.mandate` |

All three come from the same `wallet.js`.

```ts
import { fetchWalletRegistry, walletEntry } from '@evinvest/wallet';

// WALLET_REGISTRY_URL by default; every bundle must sit under WALLET_MFE_PREFIX.
const registry = await fetchWalletRegistry();
if (!registry.ok) return fallback;
const walletResult = walletEntry(registry.value, 'wallet');
const payResult = walletEntry(registry.value, 'payInvoice');
// walletEntry(registry.value, 'mandate') for MandateButton.
if (!walletResult.ok || !payResult.ok) return fallback;
const wallet = walletResult.value;
const pay = payResult.value;
```

The host's CSP allows the bundle directory, not the whole origin. The full
list is in [Content-Security-Policy](#content-security-policy).

### Contract version

The elements speak contract version 3 (`WALLET_API_VERSION`): version 2 plus
the `actions` attribute, the `ev:wallet-result` event and the mandate element.
This is the wallet's own number; `@evinvest/mfe`'s `API_VERSION` stays 2. The
React wrappers pass `WALLET_API_VERSION` as the default `minApiVersion`, so an
element of version 2 (banking before the inline release) is refused, and the
host keeps showing `fallback`. Pass `minApiVersion={2}` only if you must show
the old element and need neither inline mode nor the mandate button.

## Money actions stay on evinvest.ltd

With `scope` (a scope host) the wallet shows balances and history only.
Without it the wallet is mounted on evinvest.ltd itself and the element carries
out its actions in place; the host does not intercept them. A payment,
withdrawal, move between compartments, mandate, or the answer to a payment out
of the user's account (`/wallet/pay`, `/wallet/withdraw`, `/wallet/move`,
`/wallet/mandate`, `/wallet/payment/<id>` — no other path is opened) happens on `https://evinvest.ltd`: in a
popup, whose page posts the result back to its opener, or — when the popup is
blocked — as a full navigation that returns to the host. A host in
[inline mode](#inline-mode) lets the element run some actions in its own page
instead, still through banking's routes on evinvest.ltd. Banking's mutating
routes accept only `Origin: https://evinvest.ltd`, so nothing on a host can move
money by itself.

```tsx
'use client';
import { PayInvoiceButton, Wallet } from '@evinvest/wallet/react';

<Wallet scriptUrl={wallet.scriptUrl} scope="sa" onResult={() => refresh()} />;

<PayInvoiceButton
  scriptUrl={pay.scriptUrl}
  invoice={invoiceId}
  scope="sa"
  // Any outcome: re-read the invoice from the host's backend. `done` is a UI
  // hint, not proof of payment; after `closed`/`timeout` it may still be paid.
  onResult={() => refetchInvoice(invoiceId)}
/>;
```

Contract, host side:

| | Name | Meaning |
| --- | --- | --- |
| attribute | `scope` | the scope host's id; absent = the full wallet, actions in place |
| attribute | `invoice` | the invoice id (pay-invoice only) |
| attribute | `request` | the mandate request id (mandate only), 1–128 of `A–Z a–z 0–9 _ -` |
| attribute | `actions` | `inline` opts into inline mode; absent = popup (see [Inline mode](#inline-mode)) |
| event | `ev:wallet-action` | cancelable, `detail: { href }`, one of `https://evinvest.ltd/wallet/{pay,withdraw,move,mandate}`; the host rebuilds it with only `invoice`, `scope` and `request` (a mandate request id) — each 1–128 of `A–Z a–z 0–9 _ -`, else the action is refused — and opens it |
| event | `ev:navigate` | `Wallet` only: an in-element link, as in `@evinvest/mfe` (`onNavigate`) |
| query | `opener` | the host's origin, added to the popup URL — where the page posts |
| query | `state` | 128 fresh random bits (`crypto.getRandomValues`, hex) per popup; the page echoes it in its message |
| query | `return_to` | where the page sends the user back when navigated, not popped up: origin + path only, on this page's origin (default, and in place of anything else: this page, no query or hash) |
| message | `{ type: 'ev:wallet-result', state, outcome: 'done' \| 'failed' \| 'cancelled', reference? }` | posted to the opener; counted only with this popup's `state`. A reference that is empty, not a string or over 128 characters reads as none — never cut — and the outcome stands |

The action page's side of the contract (banking): amount, address and recipient
never come from the URL — a payment's come from the server-side invoice;
`opener` counts only when its origin exactly matches one of the scope's
origins, `return_to` only when it is such an origin plus a path byte for byte in
the scope's `return_paths` (no query, no hash); the result is posted with `postMessage(msg, opener)`,
never `'*'`; the page is served with `frame-ancestors 'none'`.

`done` is a hint for the UI, not proof of payment: what is paid is decided by
the money flow (`InvoicePaid`), and the host reads it from its own backend.
After `closed` or `timeout` the action may still have gone through — re-read
the status; do not offer a retry.

## Inline mode

By default every money action goes to a popup on evinvest.ltd. A host can opt
into **inline mode**: the element then runs an action inside the host page,
through banking's intent routes on evinvest.ltd, with a second factor
(concierge's step-up element, `@evinvest/concierge`) where the action needs it.

```tsx
<Wallet scriptUrl={wallet.scriptUrl} scope="sa" mode="inline" onResult={() => refresh()} />;
<PayInvoiceButton scriptUrl={pay.scriptUrl} invoice={invoiceId} scope="sa" mode="inline" onResult={() => refetchInvoice(invoiceId)} />;
<MandateButton scriptUrl={mandate.scriptUrl} request={requestId} scope="sa" mode="inline" onResult={() => refetchMandates()} />;
```

`mode="inline"` sets the attribute `actions="inline"`. An action runs in the
page only when **both** sides agree: the host opted in, and banking's BFF lists
that kind (`move`, `withdraw`, `pay`, `mandate`) for the host. Banking turns a
kind on or off with a server setting, not a release. Every other action still
comes as the `ev:wallet-action` offer and opens the popup. So a host in inline
mode keeps all the popup's requirements: no `Cross-Origin-Opener-Policy:
same-origin`, and the popup opens inside the click.

When the in-page form closes, the element fires `ev:wallet-result`. The event
bubbles, is composed and is not cancelable. Its `detail` is
`{ kind, outcome, reference? }`: `kind` is one of `move`, `withdraw`, `pay`,
`mandate`, and `outcome` is one of `done`, `failed`, `cancelled`. A fallback to
the popup fires nothing; the popup reports by `postMessage` as before. The
event has the same name as the popup's message type, but it is a different
channel: a DOM event on the element.

The React wrappers deliver the result as the popup's `result` outcome,
`{ status: 'result', result }`, so a handler that refreshes on `result` works
in both modes. An in-page result also carries `result.kind`; use
`'kind' in result` to tell the two apart. A detail that does not parse (an
unknown kind or outcome, no detail) is ignored: nothing happened that the host
can trust. A reference that is not a string, is empty or is over 128
characters reads as `null`, and the outcome stands, as for the popup. As with
the popup, `done` is a hint for the UI: re-read the state from your backend.

Without React, read the event with `parseWalletResultEvent(event)`. It returns
the result or `null`.

## Content-Security-Policy

`walletCsp({ mode?, origin? })` lists the sources the wallet needs, by
directive. `serializeCsp(directives)` writes a policy as a header value:
directives sorted by name and joined with `; `, sources joined with ` `.
`mergeCsp(host, wallet)` adds the wallet's sources to the host's policy. It
keeps the host's sources and their order, and it does not repeat a source.

```ts
import { mergeCsp, serializeCsp, walletCsp } from '@evinvest/wallet';

const header = serializeCsp(mergeCsp(hostDirectives, walletCsp({ mode: 'inline' })));
```

`walletCsp()` (popup mode):

| Directive | Sources |
| --- | --- |
| `script-src` | `https://evinvest.ltd/wallet/mfe/` |
| `style-src` | `https://evinvest.ltd/wallet/mfe/` |
| `connect-src` | `https://evinvest.ltd/wallet/mfe/` (registry, stylesheet) · `https://evinvest.ltd/api/auth/session` · `https://evinvest.ltd/cabinet/api/wallet` · `https://evinvest.ltd/cabinet/api/wallet/csrf` (the kinds the BFF runs in the page, read on every connect) · `https://evinvest.ltd/cabinet/api/operations` (history; CSP ignores its query) |

`walletCsp({ mode: 'inline' })` adds concierge's bundle directory
`https://evinvest.ltd/api/auth/mfe/` (the step-up element and its registry) to
`script-src`, `style-src` and `connect-src`, and two more directories to
`connect-src`: `https://evinvest.ltd/cabinet/api/wallet/` (the intent routes)
and `https://evinvest.ltd/api/auth/stepup/` (step-up). A source that ends in
`/` matches every path under it.

There is no `frame-src`: the wallet never uses an iframe. The element loads no
fonts and no images, so there is no `font-src` or `img-src`.

Notes:

- `origin` is for a staging stand only. It must be an exact `https:` origin:
  no path, no trailing slash, no wildcard. Anything else throws a `TypeError`,
  because a wrong origin is a config error.
- `mergeCsp` keeps the host's own resources working. A fetch directive that the
  host leaves out starts from the host's `default-src`. A directive that the
  host does not restrict at all (neither it nor `default-src`) is left out.
  When the host sets `script-src-elem` or `style-src-elem`, they get the same
  sources as `script-src` and `style-src`. A `'none'` beside other sources is
  dropped, because browsers ignore it there.
- With `'strict-dynamic'` in the host's `script-src`, the browser ignores URL
  sources. The loader's `<script type="module">` is still allowed, because a
  trusted host script creates it.
- The element puts its stylesheet into the page as a `<style>` element when it
  can read the CSS with `fetch`, and as a `<link>` otherwise. Under a
  `style-src` without `'unsafe-inline'`, the browser blocks that `<style>`
  element and reports one CSP violation. An apiVersion-3 element then falls
  back to the `<link>`, which the `style-src` source above allows. An
  apiVersion-2 element has no such fallback and shows without styles.

## Bundle integrity

Banking publishes the sha256 of each shipped file as one line, from
`npm run --silent mfe:pins -- wallet` in `wallet/frontend`:

```
wallet.js=<64 lowercase hex>,wallet.css=<64 lowercase hex>
```

The names are relative to `https://evinvest.ltd/wallet/mfe/`. A host keeps
this line in its config and updates it with each banking release.

- `parseWalletPins(text)` checks the line and returns `{ ok: true, value }` or
  `{ ok: false, error }`. It refuses a file other than `wallet.js` and
  `wallet.css`, a hash that is not 64 lowercase hex, a file named twice, and a
  missing file.
- `pinToSri(pin)` turns a pin into an SRI value, `sha256-<base64>`, without a
  download. It returns `null` for a bad pin.
- `walletSri(bytes, alg = 'sha384')` computes the SRI value of bytes that you
  have, with `crypto.subtle` (Node 20 and later, browsers in a secure context).
- `verifyWalletBundle(pins, { fetch?, origin? })` fetches each pinned file from
  the bundle directory (`redirect: 'error'`, `cache: 'no-store'`) and compares
  its sha256. It never rejects. It returns `{ ok, files }` with one entry per
  file: `match`, `mismatch` (with the actual hash), `unavailable` (a network
  error, a redirect, an HTTP error) or `refused` (an unknown name, a bad pin, a
  bad `origin`). Run it at the host's startup or in CI. It checks the bytes
  served at that moment; it does not protect a browser that loads the file
  later.
- `walletImportMap(sri, scriptUrl?)` returns `{ integrity: { [scriptUrl]: sri } }`
  for `wallet.js` (the default URL). The `@evinvest/mfe` loader injects
  `<script type="module" src>` without an `integrity` attribute, so an import
  map is the place where a host adds it:

```tsx
const pins = parseWalletPins(walletPinsFromConfig);
const sri = pins.ok ? pinToSri(pins.value['wallet.js']) : null;
// In the document head, before any module script:
{sri && <script type="importmap" dangerouslySetInnerHTML={{ __html: JSON.stringify(walletImportMap(sri)) }} />}
```

Browser support of import-map integrity is not universal. Chromium-based
browsers support it since version 127; Chrome applies it to the loader's
injected `<script type="module" src>` and blocks a `wallet.js` whose hash does
not match. Check Safari and Firefox for your
targets before you rely on it. A browser that does not support the `integrity`
key ignores it and loads `wallet.js` without a check. The import map must come
before the first module script of the page. Some browsers accept only one
import map per document, so merge this entry into the host's own map if it has
one. The import map covers `wallet.js` only: the element reads `wallet.css`
with `fetch` or a `<link>`, and no browser check covers it. For the stylesheet,
`verifyWalletBundle` is the only check.

### `openWalletAction(url, options?)`

Opens the popup — one window named `ev-wallet-action` per page — and resolves
how it ended — never rejects:
`result` · `closed` (closed before reporting; settled one macrotask after the
poll sees it closed, so a result posted just before closing still wins) ·
`blocked` (navigate to `walletNavigationHref` instead; it returns `null` for a
URL that is not an action, and a `returnTo` off this page's origin falls back to
the page itself) · `busy` (another action's popup is open
on this page: it is focused, and this call reports nothing) · `timeout`
(default 15 min; the popup is closed) · `aborted` (`signal`) · `invalid_url`
(not one of the action paths on `https://evinvest.ltd`, or a malformed
`invoice`/`scope`/`request`) · `unsupported` (no `crypto` to draw a state from;
nothing opened). The React wrappers ignore `busy`,
so a double click gives one popup and one `onResult`.
A message counts only if `event.origin` is `https://evinvest.ltd`, its source
is this popup **and** it echoes this popup's `state`. Call it inside the user's click, or the browser blocks the
popup. `origin` is overridable for a staging stand only.

### `walletPaymentUrl(paymentId, scope, origin?)`

The popup URL for the user's answer to a payment out of their account — the
page that a scope host opens from the link in a payment-consent notice:
`https://evinvest.ltd/wallet/payment/<paymentId>?scope=<scope>`, or `null` when
the id or the scope is not 1–128 of `A–Z a–z 0–9 _ -`. Its result passes
`walletActionUrl` unchanged; open it with `openWalletAction`.

```ts
const url = walletPaymentUrl(paymentId, 'invest');
if (url) void openWalletAction(url).then(() => refetchPayment(paymentId));
```

The page reads the payment's terms from banking by the id; only the payer,
signed in on evinvest.ltd, sees or answers it. It reports `done` when the user
approved and `cancelled` when they declined, with the payment id as
`reference`, or `cancelled` without one when they left without answering. As
with every action, the host re-reads the payment's state from its backend.

Neither the host page nor the action page may send
`Cross-Origin-Opener-Policy: same-origin`: it severs the opener link, the page
cannot post back, and every popup ends as `closed`. On the host the strictest
that works is `same-origin-allow-popups`.

## Develop

The package lives next to the elements it wraps (banking `sdk/wallet`) so a
change to the wallet contract and its host side land in one PR. It is not an
npm workspace of the repo: it has its own lockfile, and `@evinvest/mfe` comes
from npm.

```sh
npm ci
npm run typecheck && npm test && npm run build
```

## Release

Bump `version` in `package.json` on `main` and add the version to
`CHANGELOG.md`. Then push the tag `@evinvest/wallet-v<version>`.
`.github/workflows/npm-publish.yml` checks the tag against `package.json`, runs
the typecheck and tests and publishes with npm trusted publishing (OIDC, no
token). The tag does not match `v*`, so it does not start a container release.

Order for 0.2.0: release banking first (a `v*` tag) with the wallet bundle of
contract version 3, and wait until evinvest.ltd serves it. Push the SDK tag
after that. A host on SDK 0.2.0 refuses an element of version 2 and shows its
fallback, so an SDK released first takes the wallet off every host that
upgrades.
