# @evinvest/wallet

Host wrappers for the **banking wallet microfrontends** — the wallet and the
pay-invoice button — over [`@evinvest/mfe`](https://www.npmjs.com/package/@evinvest/mfe), plus the popup round trip
that carries a money action to evinvest.ltd and brings its result back. Hosts:
the invest cabinet, the SA panel, admin. The elements are banking's, served from
`https://evinvest.ltd/wallet/mfe/`.

- `.` — server-safe core: registry constants, tags, `openWalletAction` and its
  URL and message checks (they touch `window` only when called).
- `./react` — `Wallet`, `PayInvoiceButton` (a `"use client"` bundle; `react` is
  an optional peer).

`@evinvest/mfe` is a **peer**, shared with the host so each element loads once.

## Elements

| Element | Tag | Registry name |
| --- | --- | --- |
| Wallet | `mfe-banking-wallet` | `banking.wallet` |
| Pay invoice | `mfe-banking-pay-invoice` | `banking.pay-invoice` |

```ts
import { fetchWalletRegistry, walletEntry } from '@evinvest/wallet';

// WALLET_REGISTRY_URL by default; every bundle must sit under WALLET_MFE_PREFIX.
const registry = await fetchWalletRegistry();
if (!registry.ok) return fallback;
const walletResult = walletEntry(registry.value, 'wallet');
const payResult = walletEntry(registry.value, 'payInvoice');
if (!walletResult.ok || !payResult.ok) return fallback;
const wallet = walletResult.value;
const pay = payResult.value;
```

The host's CSP allows the bundle directory, not the whole origin:
`script-src https://evinvest.ltd/wallet/mfe/`.

## Money actions stay on evinvest.ltd

With `scope` (a scope host) the wallet shows balances and history only.
Without it the wallet is mounted on evinvest.ltd itself and the element carries
out its actions in place; the host does not intercept them. A payment,
withdrawal, move between compartments or mandate (`/wallet/pay`, `/wallet/withdraw`,
`/wallet/move`, `/wallet/mandate` — no other path is opened) happens on `https://evinvest.ltd`: in a
popup, whose page posts the result back to its opener, or — when the popup is
blocked — as a full navigation that returns to the host. Banking's mutating
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
| event | `ev:wallet-action` | cancelable, `detail: { href }`, one of `https://evinvest.ltd/wallet/{pay,withdraw,move,mandate}`; the host rebuilds it with only `invoice` and `scope` — each 1–128 of `A–Z a–z 0–9 _ -`, else the action is refused — and opens it |
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
(not one of the four action paths on `https://evinvest.ltd`, or a malformed
`invoice`/`scope`) · `unsupported` (no `crypto` to draw a state from;
nothing opened). The React wrappers ignore `busy`,
so a double click gives one popup and one `onResult`.
A message counts only if `event.origin` is `https://evinvest.ltd`, its source
is this popup **and** it echoes this popup's `state`. Call it inside the user's click, or the browser blocks the
popup. `origin` is overridable for a staging stand only.

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

Bump `version` in `package.json` on `main`, then push the tag
`@evinvest/wallet-v<version>`; `.github/workflows/npm-publish.yml` checks the
tag against `package.json`, runs the typecheck and tests and publishes with npm
trusted publishing (OIDC, no token). The tag does not match `v*`, so it does
not start a container release.
