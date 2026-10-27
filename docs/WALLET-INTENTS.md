# Wallet intents — every money-out act, on the wallet and inside a host page

A page asks the BFF for an **intent**, shows the terms the BFF computed, and confirms it. The
BFF executes the intent only for the same account, page and session that created it, and
with a concierge step-up proof when the risk class asks for one. The pages are the wallet
itself (`CABINET_WS_ORIGIN`) and the trusted scope hosts (the invest cabinet, the SA panel).
This file is the contract of the BFF side (controls (a), (b), (d) and (h) of
[`WALLET-INPAGE-SPEC.md`](./WALLET-INPAGE-SPEC.md)). The proof itself is concierge's
`docs/STEPUP-PROOF.md`.

The code: `wallet/backend/src/routes/intents/` (routes, CORS, the confirm order) and
`wallet/backend/src/wallet_intents/` (the pages, the risk classes, the key, the proof
verifier, the store).

## The pages

An intent page is an exact origin with a reach and a kind set:

| Page | Reach | Kinds |
| --- | --- | --- |
| the wallet, `CABINET_WS_ORIGIN` | every scope of the caller | `withdraw`, `pay`, `mandate`, `approve_payment`, fixed in code |
| a trusted host | its own scope, beside `main` | its matrix entry, narrowed by `WALLET_ACTION_KINDS` |

The wallet moves money between its own compartments on its first-party route; only money out
of the user's claim is an intent there.

With `WALLET_ACTION_ORIGINS` empty no host is an intent page: a host's request is refused like
any foreign page's, a preflight included, and no response carries a CORS header.

Before you turn a host kind on, know what it switches on:

- `WALLET_ACTION_KINDS` is one list for every enabled host, `invest` and `sa` alike. Adding
  `pay`, `mandate` or `withdraw` turns on its in-page step-up on every enabled host whose
  matrix entry has the kind.
- The in-page form runs in the host page's JavaScript realm. A script in the host page can
  show the person other terms than the ones the proof binds. Spec control (g) is the defence:
  a strict CSP, bundle pins or registry SRI, and Trusted Types.
- Turn `withdraw` on only after control (g) is live on each enabled host, and after the hub
  holds new withdrawal addresses (`release_at`, banking#559).
- Each host must exclude `/api/auth/stepup/*` and `/cabinet/api/wallet/intents*` from the
  network-payload capture of Sentry Replay and PostHog. Those bodies carry the step-up proof
  and the CSRF token.

## The host matrix

The matrix in `wallet_intents/mod.rs` (`TRUSTED_HOSTS`) is the ceiling. The env can switch
a host on and narrow its kinds; it cannot add a host or a kind. `WALLET_ACTION_KINDS` is one
list for all enabled hosts at once: a kind in it is on for every enabled host whose matrix
entry has it, and off for all of them when it is missing. To give one host a kind and not
another, change the matrix.

| Origin | Scope | Kinds the matrix allows |
| --- | --- | --- |
| `https://invest.evinvest.ltd` | `invest` | move, pay, mandate, withdraw |
| `https://sa.evinvest.ltd` | `sa` | move, pay, mandate |

The SA panel has no withdrawal. The spec ships it there only after the panel's strict CSP is
live, and that is a reviewed change of the matrix, not an env flip.

The boot refuses (exit 78):

- an origin that is not a matrix entry (in every environment);
- an origin equal to `CABINET_WS_ORIGIN` (the wallet is a page without a host entry);
- `approve_payment` in `WALLET_ACTION_KINDS` (the wallet's own kind);
- in production: an origin that is not `https`, has a port or a path, or is not a subdomain
  of `evinvest.ltd`. `*`, `null` and a trailing slash do not parse at all;
- no `WALLET_INTENT_KEY`, or one shorter than 32 bytes;
- in production: no `REDIS_URL`.

## The element side (apiVersion 3)

The host page does not call these routes itself. It mounts the wallet elements from
`https://evinvest.ltd/wallet/mfe/wallet.js` through `@evinvest/wallet` (banking
`sdk/wallet`), and the element calls the routes. The code is
`wallet/frontend/mfe/wallet/entry.tsx`, `wallet/frontend/features/wallet-mfe/` and the flow
both surfaces share, `wallet/frontend/features/wallet-intent/`. Each element class has
`apiVersion = 3`. A host checks it with `>=`, because version 3 only adds to version 2.

The step-up is concierge's element (`@evinvest/concierge`, `StepupDialog`): the page hands it
`intent_hash` and `min_aal` and forwards the proof it returns. Which factor the account
answers with, and enrolling one when it has none, stay inside that element. When the element
cannot load, a host falls back to the popup below; the wallet shows an error.

### The elements

| Tag | Attributes | What it shows |
| --- | --- | --- |
| `mfe-banking-wallet` | `scope?`, `actions?` | balances, recent history, and the action buttons |
| `mfe-banking-pay-invoice` | `invoice`, `scope?`, `actions?` | the pay button for one invoice |
| `mfe-banking-mandate` | `request`, `scope?`, `actions?` | the button for one mandate request |

The registry `https://evinvest.ltd/wallet/mfe/registry.json` names the three elements as
`banking.wallet`, `banking.pay-invoice` and `banking.mandate`. All three come from the one
`wallet.js`.

`invoice` and `request` are ids of what banking stores: 1 to 128 characters of
`A-Z a-z 0-9 _ -`. The element does not read amounts or caps from the host. When the id has
another shape, the button is disabled and shows the reason under it. The SDK refuses the same
ids. A mandate needs no scope: the BFF reads the request's scope from the hub and checks it
against the host's scope.

### When an action runs in the page

An action runs inside the element only when **both** conditions hold:

1. The host opted in: the element has `actions="inline"`. Only this exact value counts.
2. The BFF lists the kind for the host page (`GET /api/wallet/csrf` → `kinds`). A move and a
   payment also need the element's `scope`.

In every other case the element offers the action to the host as the cancelable
`ev:wallet-action` event (`detail: { href }`). The host opens the action page in a popup, and
the popup reports the result by `postMessage`. This also happens when the kind list did not
arrive yet: a host must open its popup inside the click, so the click does not wait.

The opt-in exists for older hosts. An `@evinvest/wallet` 0.1.x host does not listen for
`ev:wallet-result`. Without the opt-in, an action that ran in its page would end without a
result, and the host would never refresh its state. So the element keeps the popup for such a
host, even when the BFF lists the kind.

### The result event

When the in-page form closes, the element dispatches `ev:wallet-result` on itself. The event
bubbles and is composed, so a host that wraps the element in its own shadow root still gets
it. It is not cancelable.

```json
{ "kind": "pay", "outcome": "done", "reference": "inv_1" }
```

- `kind` is `move`, `withdraw`, `pay` or `mandate`.
- `outcome` uses the same words as the popup's message (`WalletResult` in the SDK):
  - `done`: the BFF executed the confirm, and the person closed the form.
  - `failed`: the person closed the form on an error from the BFF, on terms that ran out, or
    on a confirm whose result is unclear.
  - `cancelled`: the person closed a form that showed no error.
- `reference` is the hub's id of what it did, only with `done`. The element leaves it out
  when there is none, or when it is longer than 128 characters. It never cuts a reference: a
  cut reference names a different payment, or none.

The detail holds nothing else: no amount, no proof, no summary. `done` is a hint for the UI,
not proof that money moved. A host that unlocks something on payment reads the status from
its own backend.

The element fires one event for one form. It fires nothing in these cases:

- The form falls back to the popup (`ev:wallet-action`). The popup reports that action
  itself.
- The host changes an attribute or removes the element while the form is open. The form did
  not end; the host replaced it.

## Routes

| Route | Body | Answer |
| --- | --- | --- |
| `GET /api/wallet/csrf` | — | `{ csrf, expires_at, kinds }` |
| `POST /api/wallet/intents` | `{ kind, params }` | the intent, below |
| `POST /api/wallet/intents/{id}/confirm` | `{ proof? }` | `{ intent_id, status: "executed", stepup_method, result }` |

`stepup_method` is the assurance the act was authorised with: `aal<N>` from the proof's
claims, or `none`. `result` is the hub's answer in the shape of its matching read: a
compartment move, a withdrawal, a payable invoice, a mandate or a payment confirmation. Times
are unix seconds, as JSON numbers.

`kinds` lists the kinds that the calling page may create now, in the order `move`, `pay`,
`mandate`, `withdraw` for a host, and the wallet's fixed set for the wallet. A host opens an
in-page form only for a kind in this list. An empty list means the popup, so a rollback is
an env change, not a release.

### Checks on every request

1. `Origin` is exactly an intent page. Otherwise `403 csrf`.
2. `Sec-Fetch-Site` is `same-origin` or `same-site`. A missing header is a refusal here,
   unlike on the other first-party routes: `403 csrf`.
3. The `ev_access` cookie verifies: else `401`.
4. POST only: `x-ev-csrf` is the HMAC token for this account and this origin (`403 csrf`);
   `Content-Type` is exactly `application/json` (`415`); the per-user money budget of the
   first-party routes allows one more (`429`).
5. Create and confirm: concierge's `GetMe` still holds the account `active`
   (`403 account_not_active`) and with the token's `token_version` (`401`).

### CORS

Only these three routes, only for an enabled host, as a `route_layer` on each route; the
wallet is same-origin and gets none. A preflight is answered `204` with
`Access-Control-Allow-Methods: POST` (`GET` for the CSRF route),
`Allow-Headers: content-type, x-ev-csrf`, `Allow-Credentials: true`, `Max-Age: 60` and
`Vary: Origin`. A preflight from any other origin, or for another method or header, is a bare
`403`. Every answer carries `Cache-Control: no-store` and `Vary: Origin`.

### The CSRF token

`<window>.<hex HMAC-SHA-256(k_csrf, sub, origin, window)>`, where `window = floor(now / 600)`.
The token of the current and of the previous window is accepted, so a token lives 10 to 20
minutes. It is in the response body and never in a URL. It is the one CSRF token of these
routes on both surfaces; the wallet's other first-party routes keep the cookie double-submit.

## Kinds and params

The server ignores nothing in the body: an unknown field anywhere is `400 invalid_request`,
so a `scope` in the body is refused, not overridden. Every scope the act touches beside
`main` must be in the page's reach.

| Kind | Params | What the BFF reads from the hub | Scope rule |
| --- | --- | --- | --- |
| `move` | `from_scope`, `to_scope`, `amount` | nothing | one side is `main`, the other in reach |
| `withdraw` | `network`, `address`, `amount` | `GetWallet`: the rail's fee and minimum | money leaves from `main` |
| `pay` | `invoice_id`, `pay_from` | `GetPayableInvoice`: amount, scope, state | the invoice's scope is in reach; `pay_from` is `main` or the invoice's scope |
| `mandate` | `request_id` | `GetMandateRequest`: caps, scope, expiry | the request's scope is in reach |
| `approve_payment` | `payment_id` | `GetPaymentConfirmation`: amount, recipient, network, and still open to this seat's answer | out of the caller's claim, `main` |

A refusal of a scope rule is `403 scope_mismatch`; a payment that can no longer be answered is
`412 payment_not_answerable`.

## The intent

```json
{ "intent_id": "<32 hex>", "intent_hash": "<64 hex>",
  "summary": { "amount", "fee", "net", "recipient_full", "network", "scope", "host_origin", "mandate" },
  "expires_at": 1790000120,
  "min_aal": 2,
  "new_recipient": null, "release_at": null }
```

- `summary.scope` is the act's scope: the invoice's, the request's, the moved compartment's,
  or `main`. `host_origin` is the page.
- A `null` in the summary means "the hub does not say": a payment has no fee or net in the
  hub's invoice, and a mandate moves no amount now (its caps are in `summary.mandate`).
- `min_aal` is the least assurance the confirm's proof must carry; `null` asks for no proof.
- `new_recipient` and `release_at` are always `null` in this version. The hub decides them
  when the withdrawal opens (the address book and `release_at` of banking#559) and exposes no
  read of them before.
- The page hands `intent_hash` and `min_aal` to concierge's step-up element.

### Risk classes (spec (h))

| Class | When | `min_aal` |
| --- | --- | --- |
| B | an in-page move ≤ 100 USDT, while the user's moves through all pages in 24 h stay ≤ 300 USDT | `null` |
| C | a move over either line, a withdrawal, a pay, a mandate grant, a payment approval | `2` |

The numbers are `WALLET_MOVE_NO_STEPUP_PER_OP_USDT` (100) and `WALLET_MOVE_NO_STEPUP_24H_USDT`
(300). The 24 h total is a rolling window, per user, across all pages. It counts every move the
BFF sent, with or without a step-up. A B move is held to the line again at confirm, atomically:
of two intents created side by side under it, only the first runs (`409 limit_reached`). An
operation stays counted when the hub then refuses it (the safe side).

The first-party acts that keep money inside the user's claim or only reduce risk ask for no
step-up and are not intents: a compartment move, cancelling a withdrawal, revoking a mandate,
and rejecting a payment (`POST /api/wallet/payments/{id}/reject`).

## Stored state

| Key | Value | Lifetime |
| --- | --- | --- |
| `cabinet:wallet:intent:<intent_id>` | hash: `record` (the JSON bytes), `seal`, `status`, and from `executing` on `assurance` | 120 s + 300 s; 24 h from the first send once `executing` |
| `stepup:jti:<jti>` | `1` (`SET NX EX`) | until the proof's `exp` + 5 s |
| `cabinet:wallet:moved:<sub>` | sorted set: `<intent_id>:<micro-USDT>`, scored by time | 24 h |

- `record` holds `intent_id` (128 random bits), `sub`, `origin`, `scope`, the action, the
  summary, `min_aal`, `created_at`, `expires_at` and the session binding (`token_version`,
  `auth_time`).
- `intent_hash = SHA-256(label, record bytes)`, so it is unique to the intent and covers every
  term the person confirms.
- `seal = HMAC-SHA-256(k_seal, intent_hash)`. A record edited in Redis fails the seal and is
  answered as unknown. The status is outside the seal; a status forced back to `created` still
  executes only under the hub's idempotency key, which is the intent id.
- `k_csrf` and `k_seal` are derived from `WALLET_INTENT_KEY` with two fixed labels.

Statuses: `created → authorized → executing → executed | cancelled`, `authorized → cancelled`,
and `created → expired | cancelled`. Each move is a compare-and-set: one caller wins.

- `cancelled` means the hub refused the first send of the act, the proof's `jti` was already
  spent, the 24 h line was reached at confirm, or the terms changed since create.
- `executing` means the act was sent to the hub under `idempotency_key = intent_id`. If the
  answer is lost or unclear (a transport error, a timeout, the `→ executed` write failing), the
  intent stays `executing`, and the page **confirms the same intent again**. That confirm
  checks the account, page and session binding as for `created`, then sends the same act again,
  without a proof, a `jti` or a budget, and answers the hub's result. The hub returns its first
  act for a repeated key (below), so the page gets one act whatever happened to the answer.
- **Retry window**: 24 h from the first send. It does not end at `expires_at`: refusing the
  resend would leave a new intent, with a new key, as the only way on.
- `executed` is refused (`409 intent_not_pending`): its result was answered once.

What the hub does on a repeated send, per kind (the per-user key is the intent id):

| Kind | Repeat |
| --- | --- |
| `withdraw` | `find_by_key` before the gates and limits returns the first withdrawal; two sends at once are serialised on the claim lock, and the second finds the first's row (`open_keyed`) |
| `move` | `find_move` returns the first move; a send that races it finds it under the key lock (`MoveOutcome::Existing`) |
| `pay` | under a named lock on the key, the invoice paid with it is returned (`Paid::Earlier`) before the invoice is judged open |
| `mandate` | the store's `grant` returns the mandate granted with the key (`Granted::Existing`) |
| `approve_payment` | no key: the seat's answer is the key. The same decision again answers the order as it stands; another one is `ALREADY_EXISTS` |

So two resends at the same time need no guard of their own: both reach the hub under one key
and get the same act. Both may write `executing → executed`; the second write loses its
compare-and-set and is only logged.

A confirm waits for its act at most 13 s. That is inside the router's 15 s deadline, whose
`504` carries no CORS header. Past it, the answer is `503 intent_executing`, and the act goes
on to the end on its own task, so neither the deadline nor a dropped connection can cut the
act off from the status write. An unclear hub answer is the same `503 intent_executing`:
confirm again.

## The confirm order

1. The gate above, then the body: `{ "proof": string | null }` and nothing else.
2. Load the intent; check the seal; the intent's `sub` and `origin` are the caller's
   (else `404 intent_not_found`, as for an unknown id).
3. The session binding: the access token's `token_version` and `auth_time` are the ones the
   intent recorded (`403 session_changed`).
4. Status `executing`: send the act again and answer (see above). Status `created`: go on.
   Any other status: `409 intent_not_pending`. Past `expires_at`: `created → expired`,
   `410 intent_expired`.
5. A proof is present exactly when `min_aal` is set (`403 proof_required`,
   `400 proof_not_expected`).
6. `GetMe` re-check; the terms are read from the hub again and must equal the stored summary
   (else `created → cancelled`, `409 terms_changed`).
7. **Verify the proof** with `evconcierge_auth::stepup::verify_stepup_proof`: the stored hash,
   the access token's `sub`, the stored origin and the stored `min_aal`. An unknown `kid`
   refreshes the JWKS at most once per 30 s per process. `iat` before the intent's `created_at`
   is a refusal. Any refusal is `403 stepup_invalid`.
8. `expires_at` again: steps 6 and 7 can take seconds (`created → expired`,
   `410 intent_expired`).
9. **CAS** `created → authorized` (else `409 intent_not_pending`). Every step from here on runs
   on its own task (see above).
10. **Spend the `jti`**: `SET stepup:jti:<jti> 1 NX EX <exp - now + 5>` (else
    `authorized → cancelled`, `409 proof_replayed`).
11. A move: count it against the move total (else `authorized → cancelled`,
    `409 limit_reached`).
12. `authorized → executing`, recording the achieved assurance and keeping the intent for 24 h.
13. **Execute** with `idempotency_key = intent_id` and an `AuditContext` of the page's origin,
    the trusted client IP, the user agent, the intent id and the achieved assurance (`aal<N>`
    or `none`). Then `executing → executed`; a refusal of this first send → `cancelled`; an
    unclear answer → stays `executing`, `503 intent_executing`.

## Environment

| Variable | Default | Secret | Note |
| --- | --- | --- | --- |
| `WALLET_ACTION_ORIGINS` | empty (no host) | no | comma list of matrix hosts |
| `WALLET_ACTION_KINDS` | `move` | no | `move`, `pay`, `mandate`, `withdraw`; one list for all enabled hosts |
| `WALLET_INTENT_KEY` | — | **yes** | ≥ 32 bytes; always required |
| `REDIS_URL` | — | **yes** | it carries the Redis password; required in production. No error message prints it |
| `WALLET_MOVE_NO_STEPUP_PER_OP_USDT` | `100` | no | |
| `WALLET_MOVE_NO_STEPUP_24H_USDT` | `300` | no | per user, all pages together |

The JWKS for proofs is read from the same endpoint as for access tokens
(`AUTH_JWKS_GRPC_ENDPOINT`, else `CONCIERGE_GRPC_ADDR`), and the proof's issuer is
`AUTH_ISSUER`.

## Not enforced yet

- **A session id.** Concierge's access token carries none; the binding is `token_version` plus
  `auth_time`, which a new sign-in or a re-authentication moves.
- **The account freeze of the money plane** is checked by the hub when it executes
  (`user_frozen`), not by the BFF before.
- **`new_recipient` and `release_at`** in the create answer (see above).
