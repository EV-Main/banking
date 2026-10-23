# Wallet intents — wallet actions inside a host page

A trusted host page (the invest cabinet, the SA panel) runs a wallet action in its own page:
it asks the BFF for an **intent**, shows the terms the BFF computed, and confirms it. The
BFF executes the intent only for the same account, page and session that created it, and
with a concierge step-up proof when the risk class asks for one. This file is the contract
of the BFF side (controls (a), (b), (d) and (h) of
[`WALLET-INPAGE-SPEC.md`](./WALLET-INPAGE-SPEC.md)). The proof itself is concierge's
`docs/STEPUP-PROOF.md`.

The code: `wallet/backend/src/routes/intents/` (routes, CORS, the confirm order) and
`wallet/backend/src/wallet_intents/` (the host matrix, the risk classes, the key, the proof
verifier, the store).

## Off by default

With `WALLET_ACTION_ORIGINS` empty, none of the three routes is mounted: each answers the
router's `404`, a preflight included, and no response carries a CORS header. The first-party
wallet routes do not change in any case.

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
- an origin equal to `CABINET_WS_ORIGIN` (the wallet host's own routes are the first-party ones);
- in production: an origin that is not `https`, has a port or a path, or is not a subdomain
  of `evinvest.ltd`. `*`, `null` and a trailing slash do not parse at all;
- no `WALLET_INTENT_KEY`, or one shorter than 32 bytes;
- in production: no `REDIS_URL`.

## Routes

| Route | Body | Answer |
| --- | --- | --- |
| `GET /api/wallet/csrf` | — | `{ csrf, expires_at }` |
| `POST /api/wallet/intents` | `{ kind, params }` | the intent, below |
| `POST /api/wallet/intents/{id}/confirm` | `{ proof? }` | `{ intent_id, status: "executed", stepup_method, result }` |

`result` is the hub's answer in the shape of the matching first-party route: a compartment
move, a withdrawal, a payable invoice or a mandate. Times are unix seconds, as JSON numbers.

### Checks on every request

1. `Origin` is exactly an enabled host. Otherwise `403 csrf`.
2. `Sec-Fetch-Site` is `same-origin` or `same-site`. A missing header is a refusal here,
   unlike on the first-party routes: `403 csrf`.
3. The `ev_access` cookie verifies: else `401`.
4. POST only: `x-ev-csrf` is the HMAC token for this account and this origin (`403 csrf`);
   `Content-Type` is exactly `application/json` (`415`); the per-user money budget of the
   first-party routes allows one more (`429`).
5. Create and confirm: concierge's `GetMe` still holds the account `active`
   (`403 account_not_active`) and with the token's `token_version` (`401`).

### CORS

Only these three routes, only for an enabled host, as a `route_layer` on each route. A
preflight is answered `204` with `Access-Control-Allow-Methods: POST` (`GET` for the CSRF
route), `Allow-Headers: content-type, x-ev-csrf`, `Allow-Credentials: true`, `Max-Age: 60`
and `Vary: Origin`. A preflight from any other origin, or for another method or header, is a
bare `403`. Every answer carries `Cache-Control: no-store` and `Vary: Origin`.

### The CSRF token

`<window>.<hex HMAC-SHA-256(k_csrf, sub, origin, window)>`, where `window = floor(now / 600)`.
The token of the current and of the previous window is accepted, so a token lives 10 to 20
minutes. It is in the response body and never in a URL. The first-party routes keep the
cookie double-submit.

## Kinds and params

The server ignores nothing in the body: an unknown field anywhere is `400 invalid_request`,
so a `scope` in the body is refused, not overridden. The scope is always the host's own.

| Kind | Params | What the BFF reads from the hub | Scope rule |
| --- | --- | --- | --- |
| `move` | `from_scope`, `to_scope`, `amount` | nothing | one side is the host's scope, the other `main` |
| `withdraw` | `network`, `address`, `amount` | `GetWallet`: the rail's fee and minimum | money leaves from `main` |
| `pay` | `invoice_id`, `pay_from` | `GetPayableInvoice`: amount, scope, state | the invoice's scope is the host's; `pay_from` is `main` or the host's scope |
| `mandate` | `request_id` | `GetMandateRequest`: caps, scope, expiry | the request's scope is the host's |

A refusal of a scope rule is `403 scope_mismatch`.

## The intent

```json
{ "intent_id": "<32 hex>", "intent_hash": "<64 hex>",
  "summary": { "amount", "fee", "net", "recipient_full", "network", "scope", "host_origin", "mandate" },
  "expires_at": 1790000120,
  "stepup": { "required": true, "methods": ["passkey", "email_code"] },
  "new_recipient": null, "release_at": null }
```

- A `null` in the summary means "the hub does not say": a payment has no fee or net in the
  hub's invoice, and a mandate moves no amount now (its caps are in `summary.mandate`).
- `new_recipient` and `release_at` are always `null` in this version. The hub decides them
  when the withdrawal opens (the address book and `release_at` of banking#559) and exposes no
  read of them before.
- The page sends `intent_hash` to concierge's `/auth/stepup/options`.

### Risk classes (spec (h))

| Class | When | Step-up |
| --- | --- | --- |
| B | a move ≤ 100 USDT, and the user's moves through all hosts in 24 h stay ≤ 300 USDT | none |
| C | a move over either line, a pay | `passkey` or `email_code` (see the email lines below), else `passkey` |
| C | a withdrawal, a mandate grant | `passkey` only |

`email_code` is offered only when the amount is ≤ 250 USDT **and** the amount plus what the
user already confirmed by emailed code in the last 24 h, through all hosts, is ≤ 500 USDT.
The method list is decided at create, so the page knows it before it asks concierge.

The numbers are `WALLET_MOVE_NO_STEPUP_PER_OP_USDT` (100), `WALLET_MOVE_NO_STEPUP_24H_USDT`
(300), `WALLET_EMAIL_STEPUP_CAP_USDT` (250) and `WALLET_EMAIL_STEPUP_24H_USDT` (500). Both
24 h totals are rolling windows, per user, across all hosts:

- The move total counts every move the BFF sent, with or without a step-up. A B move is held
  to the line again at confirm, atomically: of two intents created side by side under it, only
  the first runs (`409 limit_reached`).
- The email total counts every operation confirmed with an `email_code` proof, rounded up to
  the micro-USDT. It is checked again at confirm, atomically (`409 email_limit_reached`).
- An operation stays counted when the hub then refuses it (the safe side).

Revoking a mandate and cancelling a withdrawal stay first-party in this version.

## Stored state

| Key | Value | Lifetime |
| --- | --- | --- |
| `cabinet:wallet:intent:<intent_id>` | hash: `record` (the JSON bytes), `seal`, `status`, and from `executing` on `method` | 120 s + 300 s; 24 h from the first send once `executing` |
| `stepup:jti:<jti>` | `1` (`SET NX EX`) | until the proof's `exp` + 5 s |
| `cabinet:wallet:moved:<sub>` | sorted set: `<intent_id>:<micro-USDT>`, scored by time | 24 h |
| `cabinet:wallet:email:<sub>` | the same, for operations confirmed by emailed code | 24 h |

- `record` holds `intent_id` (128 random bits), `sub`, `origin`, `scope`, the action, the
  summary, the step-up demand, `created_at`, `expires_at` and the session binding
  (`token_version`, `auth_time`).
- `intent_hash = SHA-256(label, record bytes)`, so it is unique to the intent and covers every
  term the person confirms.
- `seal = HMAC-SHA-256(k_seal, intent_hash)`. A record edited in Redis fails the seal and is
  answered as unknown. The status is outside the seal; a status forced back to `created` still
  executes only under the hub's idempotency key, which is the intent id.
- `k_csrf` and `k_seal` are derived from `WALLET_INTENT_KEY` with two fixed labels.

Statuses: `created → authorized → executing → executed | cancelled`, `authorized → cancelled`,
and `created → expired | cancelled`. Each move is a compare-and-set: one caller wins.

- `cancelled` means the hub refused the first send of the act, the proof's `jti` was already
  spent, a 24 h line was reached at confirm, or the terms changed since create.
- `executing` means the act was sent to the hub under `idempotency_key = intent_id`. If the
  answer is lost or unclear (a transport error, a timeout, the `→ executed` write failing), the
  intent stays `executing`, and the page **confirms the same intent again**. That confirm
  checks the account, page and session binding as for `created`, then sends the same act again,
  without a proof, a `jti` or a budget, and answers the hub's result. The hub returns its first
  act for a repeated key (below), so the page gets one act whatever happened to the answer.
- **Retry window**: 24 h from the first send. It does not end at `expires_at`: refusing the
  resend would leave a new intent, with a new key, as the only way on.
- `executed` is refused (`409 intent_not_pending`): its result was answered once.

What the hub does on a repeated key, per kind (the per-user key is the intent id):

| Kind | Repeat with the same key |
| --- | --- |
| `withdraw` | `find_by_key` before the gates and limits returns the first withdrawal; two sends at once are serialised on the claim lock, and the second finds the first's row (`open_keyed`) |
| `move` | `find_move` returns the first move; a send that races it finds it under the key lock (`MoveOutcome::Existing`) |
| `pay` | under a named lock on the key, the invoice paid with it is returned (`Paid::Earlier`) before the invoice is judged open |
| `mandate` | the store's `grant` returns the mandate granted with the key (`Granted::Existing`) |

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
5. A proof is present exactly when the class asks for one (`403 proof_required`,
   `400 proof_not_expected`).
6. `GetMe` re-check; the terms are read from the hub again and must equal the stored summary
   (else `created → cancelled`, `409 terms_changed`).
7. **Verify the proof** with `evconcierge_auth::stepup::verify_stepup_proof`: the stored hash,
   the access token's `sub`, the stored origin and the stored methods. An unknown `kid` refreshes
   the JWKS at most once per 30 s per process. `iat` before the intent's `created_at` is a
   refusal. Any refusal is `403 stepup_invalid`.
8. `expires_at` again: steps 6 and 7 can take seconds (`created → expired`,
   `410 intent_expired`).
9. **CAS** `created → authorized` (else `409 intent_not_pending`). Every step from here on runs
   on its own task (see above).
10. **Spend the `jti`**: `SET stepup:jti:<jti> 1 NX EX <exp - now + 5>` (else
    `authorized → cancelled`, `409 proof_replayed`).
11. An `email_code` proof: count the amount against the email total (else
    `authorized → cancelled`, `409 email_limit_reached`).
12. A move: count it against the move total (else `authorized → cancelled`,
    `409 limit_reached`).
13. `authorized → executing`, recording the method and keeping the intent for 24 h.
14. **Execute** with `idempotency_key = intent_id` and an `AuditContext` of the host origin,
    the trusted client IP, the user agent, the intent id and the method used (`passkey`,
    `email_code` or `none`). Then `executing → executed`; a refusal of this first send →
    `cancelled`; an unclear answer → stays `executing`, `503 intent_executing`.

## Environment

| Variable | Default | Secret | Note |
| --- | --- | --- | --- |
| `WALLET_ACTION_ORIGINS` | empty (off) | no | comma list of matrix hosts |
| `WALLET_ACTION_KINDS` | `move` | no | `move`, `pay`, `mandate`, `withdraw`; one list for all enabled hosts |
| `WALLET_INTENT_KEY` | — | **yes** | ≥ 32 bytes; required once a host is on |
| `REDIS_URL` | — | **yes** | it carries the Redis password; required in production once a host is on. No error message prints it |
| `WALLET_MOVE_NO_STEPUP_PER_OP_USDT` | `100` | no | |
| `WALLET_MOVE_NO_STEPUP_24H_USDT` | `300` | no | per user, all hosts together |
| `WALLET_EMAIL_STEPUP_CAP_USDT` | `250` | no | per operation |
| `WALLET_EMAIL_STEPUP_24H_USDT` | `500` | no | per user, all hosts together |

The JWKS for proofs is read from the same endpoint as for access tokens
(`AUTH_JWKS_GRPC_ENDPOINT`, else `CONCIERGE_GRPC_ADDR`), and the proof's issuer is
`AUTH_ISSUER`.

## Not enforced yet

- **"Require a passkey when the account has one."** Concierge exposes no such signal to the
  bank. So an emailed code is accepted for a pay or a move up to the email cap even when the
  account has a passkey.
- **The bank's own words in the step-up mail** (`summary_mac`, concierge#182). Until then the
  mail quotes the page's summary, and the email cap is the only bound on what it can approve.
- **A session id.** Concierge's access token carries none; the binding is `token_version` plus
  `auth_time`, which a new sign-in or a re-authentication moves.
- **The account freeze of the money plane** is checked by the hub when it executes
  (`user_frozen`), not by the BFF before.
- **`new_recipient` and `release_at`** in the create answer (see above).
