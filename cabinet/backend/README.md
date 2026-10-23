# cabinet-backend

The cabinet's **BFF** (backend-for-frontend): a standalone, **stateless** HTTP service that
is the cabinet's egress boundary. It proxies the browser's same-origin `/api/*` JSON
requests to two gRPC planes:

- **concierge** (identity) — `UserDirectory` `GetMe`/`UpdateProfile`, the directory/platform
  admin RPCs, notifications, and the ownership plane (owners, removals, the live feed).
- **piggybank** (money) — `WalletService`, `FundsService`, `FeesService`, `AllocationsService`,
  `BookService`, `BalanceService`, `ConsiliumService`, `HealthService`.

## Auth is shell-owned

The BFF **runs no OAuth and holds no session**. The concierge auth web surface — reached
through the conductor's `/api/auth/*` on the shared origin — signs the user in and sets the
zone-shared `ev_access` JWT cookie. This service only **verifies** that cookie, locally,
against the concierge JWKS (cached from the plane's public `Jwks` RPC, so there is no
per-request round trip; it fails closed until the plane publishes keys). That verified
cookie IS the request's credential.

There is no session store, no opaque session cookie, and no `oauth.rs`. The one piece of
server-side state left is `session.rs`'s per-user **cache** of banking money tokens, which
a lost entry simply re-mints. The exception is opt-in: with `WALLET_ACTION_ORIGINS` set, the
wallet intents of a host page live in Redis (`REDIS_URL`), shared by every replica
(`docs/WALLET-INTENTS.md`).

Cookies read (never set): `ev_access` (the access JWT) and `ev_csrf` (the readable
double-submit token, echoed in `x-ev-csrf` on every mutation). Both are `__Host-`-prefixed
in production.

## Layout

| Module | Role |
| ------ | ---- |
| `config.rs` | the `ev::settings!` env surface — the deploy contract, pinned by tests |
| `state.rs` | `AppState` + `Grpc` (lazy channels + typed client calls to both planes) |
| `session.rs` | the per-user banking money-token cache + single-flight refresh |
| `cookies.rs` | the cookie names the BFF reads (must match the shell's) |
| `governance.rs` | the concierge ownership-plane seam — see the pin note below |
| `dto.rs` | browser-facing JSON DTOs (snake_case; 64-bit values as strings) |
| `error.rs` | gRPC status → HTTP status + `{ "error": … }` body |
| `wallet_intents/` | a trusted host's in-page wallet actions: the host matrix, the risk classes, the wallet key (CSRF HMAC + intent seal), the step-up proof verifier and the shared store (`docs/WALLET-INTENTS.md`) |
| `routes/` | one handler per endpoint: `identity`, `money`, `intents`, `book`, `admin`, `notifications`, `platform`, `system`, `consilium`, `payments`, `approval`; the two socket bridges `governance_ws` and `book_ws` over the shared `ws` (origin guard, close codes, keepalive, expiry) |

## The governance surface

Nothing about the platform's own money is one person's decision — a payment out of an
allocation's pooled claim, a new holder of the `fee` or `fund` allocation, a seed of the
platform's capital — and neither is taking an owner's seat away. All are gated by a
**consilium**; `docs/CONSILIUM.md` is the policy and the threat model. A **payment** — an
order between two named ends of the platform — is gated the same way: the plane seats the
one approval the order needs on open (the owners' consilium for a source that is an
allocation's claim, the investor's own consent for a user-owned one), so the payments
console has no approve or execute verb; the answer comes from a mailbox through
`/api/approval/**`. The platform holds no money outside an allocation (#245): what it
earns is the `fee` allocation's, held by people through units, and `GET /api/admin/revenue`
is that allocation in the treasury's shape; the old revenue payout is history only.

| Route | Plane | Token |
| ----- | ----- | ----- |
| `GET /api/consilium`, `GET /api/consilium/{id}` | money | banking |
| `POST /api/consilium/holder-grant` (`{ allocation, user_id, units }`), `POST /api/consilium/{id}/cancel` | money | banking |
| `POST /api/admin/treasury/seed-capital` (`{ tx_ref, network, expected_amount, depositor_user_id? }` → `{ recorded: false, amount, consilium_id }`) | money | banking (Admin\|Owner) |
| `GET /api/owners`, `POST /api/owners/resign` | ownership | concierge |
| `GET`/`POST /api/owners/removals`, `POST /api/owners/removals/{id}/vote`, `…/cancel` | ownership | concierge |
| `GET`/`POST /api/owners/admissions`, `POST /api/owners/admissions/{id}/vote`, `…/cancel` | ownership | concierge |
| `GET /api/owners/consilium/ws` | ownership | concierge |
| `GET /api/admin/payments`, `GET /api/admin/payments/{id}`, `…/cancel` (opening an order lives in the central admin) | money | banking (Admin\|Owner) |
| `GET`/`POST /api/approval/{payout,removal,consent}/{token}` | money / ownership / money | none — the emailed token |

The split is architectural: the money plane must be able to audit its own authorization, so
its tally is computed against the owner roster it already mirrors; ownership is a
concierge-owned fact, so only that plane may mutate it. Each handler forwards its own
plane's token and never the other's.

**Admission is governed too**, and it is why the rest holds: granting a seat needs
unanimity of every other owner, and it is the ONLY way `Role::Owner` is granted — the
directory's `SetRole` refuses it outside an executed admission. Without that, a bad actor
would mint sock puppets *before* opening an invoice and then reach quorum legitimately,
which snapshotting the roster cannot prevent because the stuffing already happened.
Admission and removal share one governance revision, so the socket follows both. Note the
two votes have deliberately different vocabularies — `admit`/`reject` against
`remove`/`keep` — because a page that rendered the wrong verb would still submit.

**The websocket** bridges `GovernanceService.WatchGovernance` to the browser — one revision covers removals and
admissions together, so a single subscription follows the whole ownership surface. A frame
carries a revision and a timestamp — never a tally, never a secret — and the client
refetches the authoritative snapshot when the revision moves. It verifies `ev_access` at
the handshake exactly as the REST routes do, checks the handshake `Origin` against
`CABINET_WS_ORIGIN` (a websocket handshake is exempt from CORS but still carries cookies),
and closes itself when the token expires. It is mounted **outside** the router's
request-deadline layer, which exists to kill a wedged request and would otherwise kill this
every 15 seconds.

## The wallet surface and the scope hosts

A scope's own host (`sa.evinvest.ltd`, `invest.evinvest.ltd`) shows the user's balances
and history but never acts on them (`wallet.proto`, CompartmentService header). Two rules
make that hold at the BFF:

- **Reads are cross-origin on three routes only.** `GET /api/wallet`,
  `/api/wallet/compartments` and `/api/operations` answer a request whose `Origin` is in
  `WALLET_CORS_ORIGINS` with that origin echoed and `Access-Control-Allow-Credentials:
  true` (`routes/cors.rs`). Nothing else carries CORS headers, no preflight is answered, and
  an empty list switches it off. The list is opted into per route rather than as a layer,
  so a new mutating route cannot inherit it.
- **Money mutations are first-party, CSRF-checked and rate-limited per user.** Move,
  withdraw, pay an invoice, grant or revoke a mandate pass `routes::money_mutation`:
  `Origin` must equal `CABINET_WS_ORIGIN` — and, unlike the websocket guard, an unset value
  refuses everything — then the CSRF pair, then a per-user window of 20 per minute keyed by
  the verified subject (in-process, per replica; the hub's limits and idempotency are the
  real bound). Locally, set `CABINET_WS_ORIGIN` to the browser origin to move money.

## The book surface

The secondary market in an allocation's units — holders trading with each other on the
hub's central limit order book (`BookService`; the policy and the ledger model are in
`piggybank/core/PATTERNS.md` § The book). Every route is a money-plane route: it forwards
the banking token, and an order is an escrow inside the ledger. Reads answer a fixed
generic message on failure; mutations relay the hub's client-safe wording under the mapped
status (`FAILED_PRECONDITION` → 412 for a closed book or an access below `invest`,
`INVALID_ARGUMENT` → 400, `ALREADY_EXISTS` → 409 for a reused `client_order_id`,
`NOT_FOUND` → 404 for a hidden allocation). The string vocabularies — `side`, `kind`,
`tif`, `resolution` — are pinned in `evbanking_contracts::book` and checked here, so a
client bug is refused with the words that would have worked before the hub is called.
Every amount, size, price, timestamp and revision crosses as a string.

| Route | Query / body | Answer | Gates |
| ----- | ------------ | ------ | ----- |
| `GET /api/book` | `service`, `depth?` | `BookSnapshot` | session |
| `GET /api/book/trades` | `service`, `limit?` | `{ trades: Trade[] }` — the public tape, no parties | session |
| `GET /api/book/candles` | `service`, `resolution`, `from?`, `to?` (unix s; `to` absent = now) | `{ service, resolution, candles: Candle[] }` | session |
| `GET /api/book/policy` | `service` | `BookPolicy` | session |
| `GET /api/book/orders` | `service?` (absent = every allocation) | `{ orders: Order[] }` — resting, oldest first | session |
| `GET /api/book/orders/history` | `service?`, `limit?` | `{ orders: Order[] }` — every state, newest first | session |
| `GET /api/book/fills` | `service?`, `limit?` | `{ trades: Trade[] }` — own fills with side, order and fee | session |
| `POST /api/book/orders` | `{ service, side, kind, tif?, price?, size, client_order_id }` | `Order` | session + CSRF |
| `POST /api/book/orders/cancel` | `{ order_id }` | `Order` | session + CSRF |
| `GET /api/book/ws` | `service`, `depth?` | websocket, see below | session + `Origin` |
| `POST /api/admin/allocations/book` | `{ service, book_open, taker_fee_bps, price_tick?, lot_size?, market_slippage_bps? }` | `BookPolicy` | admin + CSRF |

**The book websocket** bridges `BookService.WatchBook` exactly as the governance one does
(same handshake, origin guard, keepalive, expiry and close codes — the shared `routes/ws`),
but its frame IS the book: `{ "type": "book", snapshot: BookSnapshot, trades: Trade[],
orders_revision }` — the same snapshot `GET /api/book` answers, the latest public trades,
and the revision at which the caller's own orders last changed, so the client refetches
`/api/book/orders` only when `orders_revision` moves. Every 25 s a `{ "type":
"heartbeat", at }` keeps the socket alive and carries no book. No other user's orders and
no balances ride on it. Close codes: `1000` the feed ended (reconnect; the first frame of
the new subscription is a full snapshot), `4401` the access token expired (sign in again),
`4503` the hub could not serve the feed (poll instead).

**`/api/approval/{payout,removal}/{token}`** is the surface an emailed owner reaches. It
carries no session and requires none — the emailed token is the credential — so it is the
one part of this service with no cookie and no CSRF check. The `GET` is strictly read-only
because mail scanners fetch every URL in a message; the vote is a `POST` carrying the
secret code from the same mail. Every response sends `Referrer-Policy: no-referrer` and
`Cache-Control: no-store`, and every unusable token — unknown, expired, spent, burned,
wrong-state — produces one identical 404, so the endpoint cannot be used to probe which is
which. The POST is bounded by a small in-process per-IP limiter; the real anti-brute-force
bound is the plane's five-attempt token burn.

## The cap table

An operator's supply surface for one product, over `AllocationsService`. Units cross as
decimal strings; every `POST` needs the admin session plus CSRF and forwards the banking
money token. `UnitIssuance` is one shape for both writes — `source` says whether the row
grew the supply (`mint`) or burnt units (`retire`, supply shrank); rows that predate #245
may read `company` (moved out of the company's stake, supply unchanged); `units` is always
the magnitude. A holder is a PERSON (`holder_kind: "user"`) or the reserved `fee`
allocation holding a product's fee class (`holder_kind: "allocation"`, `holder_id` its
slug — never resolve it as a user); the company is no holder any more. `state` is
`queued` until the hub's relay posts the leg, then `applied`.

| Route | Query / body | Answer | Gates |
| ----- | ------------ | ------ | ----- |
| `GET /api/admin/allocations/holders` | `service` | `UnitHolders` — `units_outstanding`, `queued_units` (mints not yet posted — do not pin the cap while non-zero), `holders[]` (`{ holder: { kind, id }, units }`, largest first) | admin |
| `POST /api/admin/allocations/issue` | `{ service, user_id, units, idempotency_key, cost_basis? }` | `UnitIssuance` (`source: "mint"`) | admin + CSRF |
| `POST /api/admin/allocations/retire` | `{ service, user_id, units, idempotency_key, cost_basis?, force? }` | `UnitIssuance` (`source: "retire"`, positive `units`) | admin + CSRF |
| `POST /api/admin/allocations/backing` | `{ service, backing }` — `cash \| in_kind` | `Allocation` | admin + CSRF |

`idempotency_key` (1..64 chars) is the retry contract, one key space per product across
the two writes: the console generates one per form submission and re-sends the same one
on a timeout, so a double click lands one row. A repeat of the same request answers the
row as it stands (`200`); the same key for a different request — a mint and then a
retirement included — is `409`. `cost_basis` absent or empty defaults hub-side to
`units × NAV` at the dealing mark. A mint past the unit cap, or a retirement of more than
the holder has AVAILABLE (units resting on the book or reserved by a redemption do not
count) is `400`; an unknown `service` or `user_id` is `404`. A retirement out of a product
that is not `closed` is `412` unless the body carries `force: true`. Units of the reserved
`fee` / `fund` allocations are never minted here: a person is seated in them by the
owners' consilium (`POST /api/consilium/holder-grant`).

`Allocation.backing` (`cash` | `in_kind`) says what stands behind the units. The hub
flips a product to `in_kind` on its first mint; `/allocations/backing` is how an operator
says the fund now holds cash for the units — or corrects a product back. While a product
is `in_kind`, `POST /api/funds/redeem` on it is `412` with the hub's reason (holders exit
through the book instead); the client draws the redeem control off `backing`, never off
`state` alone.

## Run

`nix run .#cabinet-backend`. It needs the piggybank hub (`nix run .#piggybank`, or `.#dev`);
identity flows additionally need the concierge runner, started from the sibling `concierge`
repo, and a signed-in browser session from the shell's auth surface. Every port comes from
the flake's `ports` attrset; secrets and per-machine overrides live in `.env.example` (copy
to `.env`).

> **Network segmentation.** `CABINET_BACKEND_BIND` stays loopback-only because the BFF's
> request-auth is a cookie: it must be reached **only** through the frontend's same-origin
> `/api/*` reverse proxy. Widen the bind (`0.0.0.0`) only behind an upstream firewall that
> keeps `/api/*` off any public interface — see
> [`docs/ARCHITECTURE.md`](../../../docs/ARCHITECTURE.md).

> **Two token pairs (cross-plane trust).** The two planes sign under separate issuers and
> distinct audiences (concierge `aud=concierge`, banking `aud=banking-core`). The BFF
> forwards each plane its **own** token and never the other's, so a leaked identity token
> cannot move money. The banking pair is **exchange-based**: for a verified JWT subject the
> BFF calls banking `AuthService.IssueUserToken` — authenticated by the shared
> `BANKING_ISSUANCE_TOKEN`, *not* by piggybank trusting concierge's issuer — and banking
> mints an `aud=banking-core` pair for the bridge-mirrored user. If the bridge has not
> mirrored a brand-new user yet, the money routes surface `NotConfigured` (503) until a
> later request re-mints. Cross-plane revocation: a concierge `SUSPENDED` freezes money ops
> immediately (per-op gate); a `SESSIONS_REVOKED` invalidates the money family within the
> banking access TTL (enforced at refresh).

## Checking the deploy contract

`cabinet-backend --print-required-vars[=PROFILE]` prints the variables a profile must
provide. The gitops preflight runs it against the built image and diffs it with the cluster
Secret's keys, so a missing variable is caught before the rollout rather than as a
`CrashLoopBackOff` after it. Production additionally requires `SENTRY_DSN` (errors must
reach someone) and `CABINET_WS_ORIGIN` (the socket's cross-site guard must not fail open).
