# Architecture

`banking` is the **central public hub** of the EV banking platform. It owns the money,
the identities, the contracts, and the client shell; every other service lives in
its own repo and talks **only to the hub, over gRPC**. Services never talk to each
other — not directly, not even via a hub round trip.

The hub is a **cash-only money plane**: its parties are people (`user:`), scope accounts,
banking's own treasury and `clearing`. Units, allocations and valuations belong to the
scopes' services (invest), which move their money through the scope ledger; every approval
is concierge's or the investor's own. Implemented end to end: the `balance`, `withdrawals`,
`payments` and `scopes` bounded contexts, the TigerBeetle `Ledger` gateway (one cash ledger),
and the transactional outbox + single-worker saga relay (see
[`piggybank/core/PATTERNS.md`](../piggybank/core/PATTERNS.md)); the remaining
domain/application areas stay documented placeholders until a feature explicitly asks.

## Topology

```
   ┌───────────────────────────── banking (this repo) ────────────────────────────┐
   │  wallet/frontend ──/api──▶ wallet/backend ──gRPC──▶ piggybank │
   │      ▲  composes <mfe-*>            (the BFF, axum)        │   (one process)     │
   │      │  custom elements              │                     ├─ core : gRPC svcs   │
   │      (Next.js host shell)            │ ──gRPC──▶ concierge  └─ auth : issuance    │
   │                                      (identity plane, separate repo)            │
   │                                           Postgres (control) · TigerBeetle      │
   │                                           (money) · Redis (central auth)        │
   └──────────────────────────────────────────────────────────────────────────────┘
   browser ─HTTP─▶ wallet/frontend (Next.js) ─/api rewrite─▶ wallet/backend
     (the BFF) ─gRPC─▶ piggybank core (money) + concierge (identity, separate repo)
   other service repos (separate): own logic, their scope's money in scope accounts ─gRPC (evbanking_contracts)─▶ piggybank;
     verify client tokens locally via evbanking_auth; own microfrontends mount into the wallet host.
```

**Network segmentation (deployment requirement).** The core/auth gRPC seams do not yet
authenticate the network path between them — the BFF's only request-auth is the session
cookie, and the core/auth listeners trust any caller that can route to them. The
**hub↔signer seam IS authenticated**: the signer mounts the shared verify layer and
accepts only the hub's service token (`aud=banking-services`, `typ=service`), and requires
TLS (mTLS recommended via `SIGNER_TLS_*`) whenever its bind/target is non-loopback —
network reachability is not its trust boundary. Client mTLS is supported but not configured
in the deployment today. The token is not the boundary against a compromised hub either: the
signer fetches its JWKS from the hub's auth service, so whoever owns the hub can mint a
token it accepts. What bounds that case is the signer's own spend policy
(`piggybank/signer/src/policy.rs` — sweep destination, caps, windows, spend brake). Every listener still **defaults to
loopback** (`127.0.0.1`): the BFF (`CABINET_BACKEND_BIND`), the hub's
`GRPC_ADDR`/`AUTH_GRPC_ADDR`, and the signer's `SIGNER_GRPC_ADDR`. A wider bind (`0.0.0.0`)
is an explicit opt-in and is only safe **behind an upstream firewall / network ACL** that
exposes the BFF solely through the same-origin reverse proxy and keeps the core and auth
seams off any public interface — the signer (encrypted private keys) most of all, and a
non-loopback signer additionally refuses to start without TLS. Do not widen a bind without
that segmentation in place.

## Cargo workspace (crate graph)

```
domain            → ev (architecture feature)          [wasm-safe]
evbanking_contracts  → (tonic-build over proto/)
evbanking_auth       → evbanking_contracts
piggybank-core    → domain, evbanking_contracts, evbanking_auth
cabinet-backend   → evbanking_contracts, evconcierge_contracts (git: EV-invest/concierge)
```

| Crate                                | Role                                                                                                                                            | wasm-safe |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------- | --------- |
| `domain/`                            | Pure DDD types over `ev::architecture`; the bounded contexts (`balance`, `scopes`, `scope_ledger`, `users`, `withdrawals`, `payments`, `auth`). Source of truth across platform. | **yes**   |
| `evbanking_contracts` (`contracts/`) | gRPC wire contracts: tonic client+server stubs from `proto/`. Other repos import it for the client stubs.                                       | no        |
| `evbanking_auth` (`piggybank/auth/`) | The auth **service** (issuance gRPC + in-process `Authorizer` channel) **and** the shared verification flow (JWKS verify + interceptor).        | no        |
| `piggybank-core` (`piggybank/core/`) | The hub server: composition root that runs the core gRPC services and the auth service as in-process tasks; data-plane services + infra.        | no        |
| `cabinet-backend` (`wallet/backend/`) | The wallet **BFF** (the crate name stays `cabinet-backend`; gitops names the image): a standalone axum HTTP service that runs OAuth, holds the session server-side, and proxies the browser's `/api/*` to piggybank (money) and concierge (identity) over gRPC. The one crate spanning both planes. | no        |

`domain` never depends on an adapter, and the wasm-unsafe `evbanking_auth` is never a
dependency of `domain` — so `domain` stays wasm-safe for service frontends.
`evbanking_auth` depends on `evbanking_contracts` (it serves the `AuthService` routes), so
`contracts` must never depend back on it.

## Bounded contexts

| Context       | Owns                                                    | Authoritative store        |
| ------------- | ------------------------------------------------------- | -------------------------- |
| `balance`       | the chart of accounts and the parties; the treasury/deposit reads. Banking's own money is its treasury, `scope_account:main:treasury`, where the withdrawal fees land and the fees of every scope but invest are swept; it pays nothing out until concierge's scope approvals decide its spending (`bank:treasury_surplus`, D24) | TigerBeetle              |
| `scopes`        | a user's wallet split by scope: `main` (the user's claim) and a compartment per scope entered (`scope_claim`), moved between only by the user (`CompartmentService`); the scope registry and policies; scope accounts `(scope, label)` and who owns them | Postgres + TigerBeetle     |
| `users`         | the bridge-mirrored person: concierge link, freeze, KYC tier, permissions | Postgres          |
| `withdrawals`   | withdrawals to chain (accept-and-queue saga): a user's, or the treasury's for an approved payment | Postgres + TigerBeetle     |
| `payments`      | payment orders out of a user's claim (their own consent in the wallet host) or the treasury (a concierge `bank:payment` approval) | Postgres + TigerBeetle     |
| `auth`          | identities + token issuance/verification                | Postgres + Redis (central) |

## Contracts pipeline

`contracts/proto/banking/v1/*.proto` is the single source of truth, and there is a
**single codegen path**: `contracts/build.rs` runs `tonic-build` (client **and**
server) on every `cargo build`. `piggybank-core` uses the data-plane servers,
`evbanking_auth` uses the `AuthService` server (issuance routes), and other repos use
the clients. `evbanking_contracts` vendors the proto, so a downstream repo adds it as
one git dependency (plus `evbanking_auth` for the verification flow) with no protoc
toolchain.

The wallet **BFF** (Rust) consumes the generated tonic **client** stubs, so it needs
no separate toolchain. The wallet host **frontend** ships no wire client at all — it calls the
backend's same-origin `/api/*` over `fetch`, typed by the generated TS types (`proto →
protoc-gen-connect-openapi → @hey-api/openapi-ts → wallet/frontend/shared/contracts/gen`,
via `nix run .#gen-api`). The backend emits that same snake_case wire shape, so the committed
types stay valid. No buf, no second toolchain — tonic + tonic-build do everything.

The directory + auth surface the wallet BFF also consumes comes from **concierge's own
protos**, read from the `evconcierge_contracts` git checkout that `Cargo.toml` pins. Which
of them are generated is listed once, in `contracts/concierge-protos.txt`; the same list is
what `nix run .#concierge-pin-check` guards (pin is an ancestor of concierge `origin/main`;
bytes equal at the pin, on main and in the local cargo checkout). Both run in CI from
`.github/workflows/drift.yml`. Bumping the pin: `Cargo.toml` → `cargo update -p
evconcierge_contracts` (so `Cargo.lock` takes the new rev — `drift-check` runs `cargo
metadata --locked` and fails a PR whose lock still holds the old one) → `concierge-pin-check`
→ `gen-api`; all four sides — `Cargo.toml`, `Cargo.lock`, `openapi.json`, `shared/contracts/gen`
— in one commit.

## Auth

The model is **stateless verification everywhere, state in exactly one place** —
with the hub's own core ↔ auth check done **in-process**, not over the wire.

**Inside the hub (`piggybank`).** `core` and `auth` run as two tasks in one
process (spawned by `piggybank-core`'s composition root). The `auth` task
(`evbanking_auth`) owns the signing keys / JWKS / refresh store, serves its **own
issuance gRPC routes** (`auth_grpc_addr`, e.g. `:50052` —
`IssueUserToken`/`Refresh`/`Logout`/`Jwks`), and exchanges two cloneable in-process
channel handles with `core`, both crossing a task boundary, **never the network**:

- [`Authorizer`] (core → auth): `core` mounts the async [`grpc_auth_layer`] on each
  data service; the layer calls `authorizer.authorize(token)` to verify a request
  and inject the `Claims`. Auth holds the keys; core never does.
- [`Provisioner`] (auth → core): `auth` asks `core` to **resolve** the user it is about
  to mint for (core owns Postgres) — by concierge id for `IssueUserToken`, by hub id at
  refresh — returning the hub user id + the folded revoke version + a disabled flag. Users
  are NOT provisioned here: that is the one-way bridge's job (concierge `CREATED`).

**Issuance.** This is the MONEY plane — it does **NO third-party (Google) OAuth**; sign-in
lives wholly in concierge, and users are mirrored here by the one-way bridge. The auth
service mints the hub's **own** short-TTL (5–15 min) asymmetric JWTs (EdDSA/Ed25519,
`aud=banking-core`) for an already-identified user via `IssueUserToken` (the concierge→banking
seam, authenticated by the shared `BANKING_ISSUANCE_TOKEN`), with `sub` = the hub user id
(never Google's `sub`). It publishes verification keys via the **`Jwks` gRPC RPC** (the hub
speaks only gRPC — there is no HTTP `.well-known`).

**Token separation.** The two **signed JWT** directions carry a `typ`
(`access`/`service`) and a **distinct `aud`**: client access → `banking-core`,
inter-service → `banking-services`. A verifier states the issuer, the accepted
**audience set**, and the accepted **`typ` set**, so http clients and grpc services
are cryptographically kept apart. **Refresh tokens are not JWTs** — they are opaque,
rotated, server-side handles (which is what enables reuse detection), so they can't
hit the data plane at all and carry no `aud`/`typ`.

**Per-plane env namespacing.** Both planes read the same `AUTH_*` env-var names
(`AUTH_ISSUER`, `AUTH_CLIENT_AUDIENCE`, `AUTH_SERVICE_AUDIENCE`, `AUTH_SIGNING_*`,
`AUTH_JWKS_*`); they are kept disjoint only by their default strings (banking
`*.banking.*`, concierge `*.concierge.*`). The two planes therefore **MUST never share
an `AUTH_*` environment** (one `.env` / ConfigMap / CI matrix) — overriding both to the
same issuer + audience + signing key would make one plane's token byte-for-byte valid
on the other, silently collapsing the audience separation. As defense-in-depth each
binary asserts at boot (in `evbanking_auth`/`evconcierge_auth`'s `*Config::from_env`)
that its configured issuer and audiences carry its own plane's identity, and **refuses
to start** otherwise — so banking rejects an `iss`/`aud` that isn't `*banking*`.

**External services (separate processes).** They can't share the in-process
channel, so they build a [`Verifier`] ([`evbanking_auth`]) that caches the hub's
JWKS (refreshed via the `Jwks` RPC, and on an unknown-`kid` miss) and verify the
hub's JWTs **locally**: **no per-request round trip, no per-service token storage.**
A per-service Redis denylist is an anti-pattern — it reintroduces the round trip
JWTs exist to avoid. They mount the same [`grpc_auth_layer`], authenticate their own
onward calls with a [`ServiceTokenSource`], and depend on `evbanking_auth` (the
flow) + `evbanking_contracts` (the stubs). Downstream adoption guide:
[`piggybank/auth/README.md`](../piggybank/auth/README.md).

**State.** The **only** stateful auth store is one **central** Redis (`REDIS_URL`):
refresh-token rotation + reuse detection, optional `jti` revocation. A per-user
`token_version` claim gives coarse "revoke all" without fleet state — enforced at
**refresh** (the auth service re-reads the authoritative version over the
`Provisioner` channel and refuses to mint), while stateless downstream verifiers
rely on the short access TTL. _Slice note:_ the refresh store is in-process by
default (single-instance/dev); `RefreshStore::from_env` engages the central-Redis
arm when `REDIS_URL` is set.

**Browser.** The BFF token-handler pattern: the wallet **BFF** (`wallet/backend`,
a standalone Rust service) is the OAuth confidential client, holds tokens server-side, and
gives the browser only a `__Host-`/`HttpOnly`/`SameSite` cookie + CSRF defense, scoped to a
real apex domain. The Next.js **frontend** rewrites same-origin `/api/*` to it. Implemented as
the backend's `/api/auth/{login,callback,logout,session}` routes: `login` mints
PKCE+state+nonce and redirects to Google; `callback` validates `state` against the HttpOnly tx
cookie and calls `AuthService.Exchange` (on the concierge identity plane); the issued JWTs live
in a server-side session (in-process for now, `SESSION_REDIS_URL` in production — distinct from
the auth refresh Redis), the browser holds only the opaque session id + a readable CSRF cookie
(double-submit on mutations).

**Wallet mutations: first-party, plus one host surface.** Every mutating wallet route
(`/api/wallet/**`) accepts only the first-party origin
(`CABINET_WS_ORIGIN`) with the cookie double-submit. A scope host (`invest.`, `sa.`) may read
the wallet cross-origin (`WALLET_CORS_ORIGINS`), and it acts only through the intent routes
`GET /api/wallet/csrf`, `POST /api/wallet/intents` and `POST /api/wallet/intents/{id}/confirm`.
These routes are mounted only when `WALLET_ACTION_ORIGINS` names a host of the in-code matrix
(`wallet/backend/src/wallet_intents/mod.rs`). The BFF computes the terms from the hub, stores
the intent in Redis, and executes it only on a confirmation from the same account, page and
session, with a concierge step-up proof when the risk class asks for one. Contract and
limits: [`WALLET-INTENTS.md`](./WALLET-INTENTS.md).

**Cross-plane token-trust (the BFF holds two token pairs).** The wallet host spans both planes,
which sign tokens under **separate** issuers and **distinct `aud`** (concierge `aud=concierge`
vs. banking `aud=banking-core`). So the BFF keeps a token pair **per plane** in its session:
the concierge pair authorizes identity RPCs (`UserDirectory`), the banking pair authorizes
money RPCs (`WalletService`/`CompartmentService`/`OperationsService`). It forwards each plane its **own** token and never
forwards one plane's token to the other — a leaked identity token therefore cannot move money.
The trust direction is **exchange-based**: after the concierge sign-in, the BFF calls banking
`AuthService.IssueUserToken` (authenticated by the shared `BANKING_ISSUANCE_TOKEN`, NOT a user
token), and the **banking** plane mints the `aud=banking-core` pair for the bridge-mirrored user —
so each plane stays the sole authority for its own audience. We explicitly **reject** the
alternative of making piggybank trust concierge's issuer/JWKS for the money plane: a single
identity token would then authorize money movement, collapsing the blast-radius isolation the
two-plane split exists to provide (and see the hard ordering rule below). The pair is minted at
login (best-effort) and re-minted/rotated on demand by `money_token` (`session.rs`), which keeps
the banking slot separate; `require_money_token` (`routes/mod.rs`) surfaces `NotConfigured` only
when no banking token can be obtained (e.g. a brand-new user the bridge hasn't mirrored yet),
never the wrong-plane token. Revocation is layered: a concierge SUSPENDED freezes money ops at
once (per-op gate) — every one but a move of the user's own money from a scope compartment
back to `main`, which never leaves the platform (moves into a compartment are refused), and a SESSIONS_REVOKED (or a banking-side revoke) invalidates the money family
at its next refresh, within the access TTL.

**Inter-service.** mTLS + short-lived service JWTs (same stateless verify path,
distinct `aud`); graduate to SPIFFE/SPIRE only at platform scale.

[`Authorizer`]: ../piggybank/auth/src/authorizer.rs
[`Provisioner`]: ../piggybank/auth/src/provisioner.rs
[`Verifier`]: ../piggybank/auth/src/verifier.rs
[`grpc_auth_layer`]: ../piggybank/auth/src/interceptor.rs
[`ServiceTokenSource`]: ../piggybank/auth/src/service_token.rs
[`evbanking_auth`]: ../piggybank/auth

## Microfrontends

The host (`wallet/frontend`, Next.js 16 App Router) composes microfrontends from
other repos at runtime. A microfrontend can be a **whole page or an inline
component**, and may be React or Rust/WASM.

**Universal contract — a custom element.** Every microfrontend ships one
self-registering ESM bundle that calls `customElements.define('mfe-<team>-<name>',
…)`. The host renders it with [`<RemoteElement>`](../wallet/frontend/shared/mfe/RemoteElement.tsx):
load the bundle by URL → `customElements.whenDefined(tag)` → render the element,
mapping props to attributes/properties and CustomEvents to callbacks. The boundary
is identical for React, Dioxus, and Leptos, so the wallet host treats every microfrontend
the same. **Light DOM only** — Tailwind v4 `@property` tokens break inside shadow
roots, and global tokens/uikit must cascade in.

- **Registry.** The wallet host resolves a logical name → `{tag, scriptUrl, kind}` from a
  per-env registry (`wallet/frontend/mfe-registry.json`, which the wallet host frontend
  reads server-side). Independent deploys land by editing the
  registry, not rebuilding the wallet host. Tags are globally unique and versioned (the custom-element
  registry is global).
- **Page-level** = the same element mounted at a route
  (`wallet/frontend/app/(mfe)/[service]/[[...slug]]`); the wallet host keeps its chrome.
- **React producer** (other repos): wrap a component as a custom element with
  `@r2wc/react-to-web-component` directly — the hub ships no producer SDK.
  _Optional_ optimization for React-to-React widgets: Module
  Federation 2.0 **runtime** (`@module-federation/runtime` + `bridge-react`) to
  share one React instance — never `@module-federation/nextjs-mf`
  (App-Router-unsupported, sunsetting).
- **Host SDK for banking's own elements** — [`sdk/wallet`](../sdk/wallet)
  (`@evinvest/wallet`): the wallet and pay-invoice elements as React wrappers for
  other hosts (invest cabinet, SA panel, admin), plus `openWalletAction`, the popup
  that carries a money action to evinvest.ltd and posts the result back. In-page
  actions on a host go through the BFF's intent routes instead
  ([`WALLET-INTENTS.md`](./WALLET-INTENTS.md)); the popup stays as the fallback. It lives
  here, not in `EV-Main/lib`, because it is banking's contract; lib holds no
  service logic. Not an npm workspace (own lockfile, `@evinvest/mfe` from npm);
  published by the `@evinvest/wallet-v<version>` tag via
  `.github/workflows/npm-publish.yml`, which never triggers a container release.
- **Rust/WASM producer** (other repos): Dioxus 0.7 mounts via
  `dioxus-web` `Config::rootelement(Element)` into the custom element (don't use
  `dioxus-web-component` yet — it pins Dioxus 0.6); Leptos mounts via
  `mount_to(HtmlElement, …)`. CSR-only, light DOM, `wasm-bindgen =0.2.118`.
  _Open item:_ prove multiple independent Dioxus instances per page before relying
  on it.
- **Rejected:** Next.js Multi-Zones as the primary mechanism (path-only; can't
  embed a widget). It IS used one level up, in the sanctioned standalone-sub-site
  form: the wallet host itself is mounted as a zone of the `site_conductor` domain
  (`basePath: "/cabinet"`, a public URL prefix that stays; the conductor rewrites `/cabinet/*` here — see
  `wallet/frontend/shared/config/base-path.ts`). Widget-level composition inside
  wallet host pages stays custom-elements-only.

**BFF (orthogonal).** The wallet **BFF** (`wallet/backend`, axum) proxies
browser HTTP to the hub's tonic backend (and the concierge identity plane) over gRPC, using
the generated tonic client stubs. No microfrontend talks to a plane directly — the wallet
BFF is the single auth/egress boundary, reached same-origin via the frontend's `/api/*`
rewrite. WASM MFEs call that same-origin `/api/*` over `fetch` (the planes' `tonic-web` layer
also allows direct gRPC-Web when latency demands it).

## Event sourcing + CQRS

Two consistency boundaries, never joined in one transaction:

- **TigerBeetle = data plane** — authoritative money (balances, transfers). Never bookkept a second time in Postgres.
- **Postgres = control plane** — metadata, the UUID↔u128 id-mapping, the domain
  event log + transactional outbox, and CQRS read projections.

**Write path (Write-Last, Read-First).** A command hands its aggregate to a single
repository method, and that method is its own atomic unit: it opens one Postgres
transaction, mutates the aggregate, and drains its `EmitsEvents` into the event log
+ `outbox` in that same transaction (the only ACID point). The transaction boundary
lives in the adapter — there is no application-layer unit-of-work, because
cross-boundary moves are TB sagas (money written last), never multi-aggregate
Postgres transactions.
The [outbox relay](../piggybank/core/src/infrastructure/relay.rs) then publishes events to
projections and issues TigerBeetle transfers — money written **last**, after the
Postgres commit; existence checks read TigerBeetle **first**. Cross-boundary moves
are sagas over TigerBeetle two-phase (pending) transfers. Delivery is
at-least-once, so consumers are idempotent (deterministic TB transfer ids; upsert
projections by event id). A reconciliation job compares Postgres projections to
authoritative TB balances; TB always wins.

**Single-drainer invariant (deploy constraint).** The relay's ordering/atomicity
argument — strict `seq`, reserve-before-complete, and the settle-time liquidity
pre-check — holds **only if exactly one relay drains the outbox** (`next_batch`
deliberately omits `SKIP LOCKED` to keep the order total; `SKIP LOCKED` is *not* a
drop-in here, as disjoint workers would apply a reservation's pending and its
completion out of order and race the cross-event liquidity check). The relay enforces
this in-process: at startup it takes a fixed-key session `pg_advisory_lock` on a
dedicated connection and only drains while held, so a second `piggybank` core instance
(a rolling deploy, an extra replica, a stuck old pod) blocks on the lock and owns
nothing instead of double-draining. The release is automatic on the holder's
connection closing (process exit), so a standby takes over without coordination.
Deploy core as **maxReplicas=1** (or with leader election) as defence in depth — the
lock is the hard guarantee, the replica cap keeps spare instances from idling on it.

This matches the `ev::architecture` kernel (`EmitsEvents`/`EventEnvelope`, `Reader`
= CQRS read port, `Gateway` forbidden from `UnitOfWork`). We do **not** adopt a
full event-sourcing framework: `cqrs-es`/`postgres-es` require `sqlx 0.8` (we pin
`0.9`), and a ledger is already an immutable audit log — event-sourcing the same
facts in Postgres would double-bookkeep.

**Cross-plane events (envelope convention).** The one documented coupling between the
planes is concierge's `UserLifecycleEvent` (`concierge/v1/events.proto`): concierge's
outbox emits it on an identity change, the banking money plane consumes it to gate or
freeze money ops (a `SUSPENDED` user or `SESSIONS_REVOKED` bump must stop downstream
authorization). It rides the **same** at-least-once / idempotent-consumer model as the
internal outbox, so the envelope carries the keys that model needs: an **`event_id`**
(the concierge outbox row key — the dedupe key the consumer upserts by) and a per-user
strictly-increasing **`sequence`** (the order key — apply only if it exceeds the last
stored, so a redelivered or skew-reordered stale `REINSTATED` can't un-freeze a user a
later `SUSPENDED` already froze; `event_id` dedupes, `sequence` orders). The payload is
self-contained so the bridge stays **one-way with no callback**: `auth_subject` (the
shared correlation key — the provider/Google `sub` both planes provision against; the
event's `user_id` is concierge's own canonical id, opaque to banking, so the consumer
resolves a CREATED to a local `UserId` through `auth_subject`, never by parsing the
foreign `user_id`), `email` + `email_verified` (to materialize a user from CREATED), and
`token_version` (the new floor a `token_version` gate stores).

**Hard ordering rule — money-plane token-trust MUST NOT ship before the lifecycle
consumer.** Today the money plane only trusts banking-minted tokens (its own issuer /
`banking-core` audience / `token_version`), so a concierge identity can never authorize
on the money plane. If a future change lets banking trust concierge tokens directly, the
money plane would authorize on an identity it has no way to learn was suspended,
KYC-downgraded, or sessions-revoked — there is no token_version visibility across the
seam, only this event. So the banking-side `UserLifecycleEvent` consumer (mapping
`auth_subject` → banking `UserId` and setting a freeze / KYC gate on the money aggregate)
is a **hard prerequisite** for any cross-plane token-trust: it must land **first**.

**Money model.** Cash lives in TigerBeetle on one USDT ledger, in **two layers**:
**treasury/custody** (debit-normal wallets, **per rail**) and **claims** (credit-normal
`user:<id>` / `scope_claim:<scope>:<id>` / `scope_account:<scope>:<label>` / `clearing`,
**network-agnostic** — one fungible balance per party and compartment). Every claim has a
holder: a person, or a scope through its own accounts, banking's treasury among them. A
retired kind's codes are data, not variants (`AccountCode::RETIRED`,
`TransferCode::RETIRED`): TigerBeetle keeps its accounts and transfers forever, so the
numbers are never reused, and value on a retired claim fails reconciliation. History of the
removed paths is archived in schema `retired`. The invariant is **global**: `sum(custody) == sum(claims)`; a deposit is a single
`Dr wallet:<net> / Cr claim` transfer (no external account). Network lives only at the
custody + transaction edges. Per-rail liquidity is a treasury concern: a withdrawal on a
short rail is **accepted and queued** (reserved against a `clearing` account, decoupled
from any rail) until the treasury tops the rail up. Non-negativity is enforced by TB
account flags. Saga moves are idempotent by a stable `event_id` (deterministic TB transfer
ids; reservations are two-phase pending with `timeout = 0`). Full chart of accounts, the
queued withdrawal saga, idempotency, and the authorization matrix: [`piggybank/core/PATTERNS.md`](../piggybank/core/PATTERNS.md).

## Run matrix

| `nix run .#`          | What                                                                            |
| --------------------- | ------------------------------------------------------------------------------- |
| `init`                | one-shot env setup for a fresh clone (dev `.env` secrets, npm deps)             |
| `dev`                 | postgres + tigerbeetle + redis + signer + piggybank + cabinet-backend + cabinet |
| `piggybank`           | hub server: core gRPC + auth tasks (tonic-web)                                  |
| `cabinet-backend`     | wallet BFF (axum) → piggybank (money) + concierge (identity)                   |
| `cabinet`             | Next.js host shell (proxies `/api/*` → the BFF)                                 |
| `db` / `tb` / `redis` | SHARED ev_invest Postgres / local TigerBeetle / SHARED Redis                    |

Every port lives in ONE place — the `ports` attrset in [`flake.nix`](../flake.nix):
gRPC planes cluster on 5005x, web UIs on 5006x. Postgres and Redis are single
instances shared by every ev_invest repo (data under `~/.local/state/ev_invest/`,
database name == app name); TigerBeetle is banking-only.

Identity flows additionally need the **concierge** runner (`CONCIERGE_PORT`), started
from the sibling `concierge` repo — banking's flake orchestrates only this repo's processes.

Control-plane migrations live in `piggybank/core/migrations/` and are **applied by
the hub on boot** (idempotent). Author new ones with the sqlx CLI (in the dev shell),
never by hand: `sqlx migrate add --source piggybank/core/migrations --sequential <name>`.

See [`flake.nix`](../flake.nix) for the apps and dev shell, and per-area READMEs
(e.g. [`wallet/frontend/README.md`](../wallet/frontend/README.md) and
[`wallet/backend/README.md`](../wallet/backend/README.md)) for details.
