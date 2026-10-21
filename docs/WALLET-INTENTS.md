# Wallet intents — wallet actions inside a host page

A trusted host page (the invest cabinet, the SA panel) runs a wallet action in its own page:
it asks the BFF for an **intent**, shows the terms the BFF computed, and confirms it. The
BFF executes the intent only for the same account, page and session that created it, and
with a concierge step-up proof when the risk class asks for one. This file is the contract
of the BFF side (controls (a), (b), (d) and (h) of
[`WALLET-INPAGE-SPEC.md`](./WALLET-INPAGE-SPEC.md)). The proof itself is concierge's
`docs/STEPUP-PROOF.md`.

The code: `cabinet/backend/src/routes/intents/` (routes, CORS, the confirm order) and
`cabinet/backend/src/wallet_intents/` (the host matrix, the risk classes, the key, the proof
verifier, the store).

## Off by default

With `WALLET_ACTION_ORIGINS` empty, none of the three routes is mounted: each answers the
router's `404`, a preflight included, and no response carries a CORS header. The first-party
wallet routes do not change in any case.

## The host matrix

The matrix in `wallet_intents/mod.rs` (`TRUSTED_HOSTS`) is the ceiling. The env can switch
a host on and narrow its kinds; it cannot add a host or a kind.

| Origin | Scope | Kinds the matrix allows |
| --- | --- | --- |
| `https://invest.evinvest.ltd` | `invest` | move, pay, mandate, withdraw |
| `https://sa.evinvest.ltd` | `sa` | move, pay, mandate |

The SA panel has no withdrawal. The spec ships it there only after the panel's strict CSP is
live, and that is a reviewed change of the matrix, not an env flip.

The boot refuses (exit 78):

- an origin that is not a matrix entry (in every environment);
- an origin equal to `CABINET_WS_ORIGIN` (the cabinet's own routes are the first-party ones);
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
| B | a move ≤ 100 USDT, and the user's moves through this host in 24 h stay ≤ 300 USDT | none |
| C | a move over either line, a pay | `passkey` or `email_code` up to 250 USDT, `passkey` above |
| C | a withdrawal, a mandate grant | `passkey` only |

The three numbers are `WALLET_MOVE_NO_STEPUP_PER_OP_USDT`, `WALLET_MOVE_NO_STEPUP_24H_USDT`
and `WALLET_EMAIL_STEPUP_CAP_USDT`. The 24 h total counts every executed move through the
host, with or without a step-up, per user and per origin. A B move is held to the 24 h line
again at confirm, atomically: of two intents created side by side under the line, only the
first runs (`409 limit_reached`). A move that the hub then refuses stays counted.

Revoking a mandate and cancelling a withdrawal stay first-party in this version.

## Stored state

| Key | Value | Lifetime |
| --- | --- | --- |
| `cabinet:wallet:intent:<intent_id>` | hash: `record` (the JSON bytes), `seal`, `status` | 120 s + 300 s |
| `stepup:jti:<jti>` | `1` (`SET NX EX`) | until the proof's `exp` + 5 s |
| `cabinet:wallet:moved:<sub>:<origin>` | sorted set: `<intent_id>:<micro-USDT>`, scored by time | 24 h |

- `record` holds `intent_id` (128 random bits), `sub`, `origin`, `scope`, the action, the
  summary, the step-up demand, `created_at`, `expires_at` and the session binding
  (`token_version`, `auth_time`).
- `intent_hash = SHA-256(label, record bytes)`, so it is unique to the intent and covers every
  term the person confirms.
- `seal = HMAC-SHA-256(k_seal, intent_hash)`. A record edited in Redis fails the seal and is
  answered as unknown. The status is outside the seal; a status forced back to `created` still
  executes only under the hub's idempotency key, which is the intent id.
- `k_csrf` and `k_seal` are derived from `WALLET_INTENT_KEY` with two fixed labels.

Statuses: `created → authorized → executed | cancelled`, and `created → expired | cancelled`.
Each move is a compare-and-set: one caller wins. `cancelled` means the hub refused the act,
the proof's `jti` was already spent, the 24 h line was reached at confirm, or the terms changed
since create. A transport failure of the hub leaves the intent `authorized`: it cannot be
confirmed again, and the hub's idempotency key makes any retry return the first result.

## The confirm order

1. The gate above, then the body: `{ "proof": string | null }` and nothing else.
2. Load the intent; check the seal; the intent's `sub` and `origin` are the caller's
   (else `404 intent_not_found`, as for an unknown id).
3. The session binding: the access token's `token_version` and `auth_time` are the ones the
   intent recorded (`403 session_changed`).
4. Status `created` (else `409 intent_not_pending`); not past `expires_at` (else
   `created → expired`, `410 intent_expired`).
5. A proof is present exactly when the class asks for one (`403 proof_required`,
   `400 proof_not_expected`).
6. `GetMe` re-check; the terms are read from the hub again and must equal the stored summary
   (else `created → cancelled`, `409 terms_changed`).
7. **Verify the proof** with `evconcierge_auth::stepup::verify_stepup_proof`: the stored hash,
   the access token's `sub`, the stored origin and the stored methods. An unknown `kid` refreshes
   the JWKS at most once per 30 s per process. `iat` before the intent's `created_at` is a
   refusal. Any refusal is `403 stepup_invalid`.
8. **CAS** `created → authorized` (else `409 intent_not_pending`).
9. **Spend the `jti`**: `SET stepup:jti:<jti> 1 NX EX <exp - now + 5>` (else
   `authorized → cancelled`, `409 proof_replayed`).
10. A move: count it against the 24 h line (else `authorized → cancelled`,
    `409 limit_reached`).
11. **Execute** with `idempotency_key = intent_id` and an `AuditContext` of the host origin,
    the trusted client IP, the user agent, the intent id and the method used (`passkey`,
    `email_code` or `none`). Then `authorized → executed`.

## Environment

| Variable | Default | Secret | Note |
| --- | --- | --- | --- |
| `WALLET_ACTION_ORIGINS` | empty (off) | no | comma list of matrix hosts |
| `WALLET_ACTION_KINDS` | `move` | no | `move`, `pay`, `mandate`, `withdraw` |
| `WALLET_INTENT_KEY` | — | **yes** | ≥ 32 bytes; required once a host is on |
| `REDIS_URL` | — | no | required in production once a host is on |
| `WALLET_MOVE_NO_STEPUP_PER_OP_USDT` | `100` | no | |
| `WALLET_MOVE_NO_STEPUP_24H_USDT` | `300` | no | |
| `WALLET_EMAIL_STEPUP_CAP_USDT` | `250` | no | |

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
