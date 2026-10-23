# ev_banking
![Minimum Supported Rust Version](https://img.shields.io/badge/nightly-1.92+-ab6000.svg)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs&style=flat-square" height="20">](https://docs.rs/ev_banking)
[<img alt="WebAssembly" src="https://img.shields.io/badge/WebAssembly-654FF0?logo=webassembly&logoColor=white" height="20">](https://webassembly.org)

`banking` is the central public hub of the `EV Investment` platform — the one process
that owns the money, the identities, and the contracts. A Cargo + Turborepo
monorepo: a Rust gRPC hub (`piggybank`) and Next.js clients over a shared,
wasm-safe `domain` crate. Every other service lives in its own repo and talks
**only to the hub, over gRPC** — never to another service.
<!-- markdownlint-disable -->
<details>
<summary>
<h2>Installation</h2>
</summary>

TODO

</details>
<!-- markdownlint-restore -->

## Usage
### Layout

| Path | What | Stack | Details |
| ---- | ---- | ----- | ------- |
| [`piggybank/core/`](piggybank/core) | hub server — gRPC services + composition root | Rust · tonic · sqlx (Postgres) · TigerBeetle | [piggybank/](piggybank) |
| [`piggybank/auth/`](piggybank/auth) | auth service + shared verification flow | Rust · tonic · JWKS | — |
| [`contracts/`](contracts) | gRPC wire contracts (`proto/` → tonic stubs) | Rust · tonic-build · proto3 | — |
| [`domain/`](domain) | shared domain types (pure, wasm-safe) over `ev::architecture` | Rust | — |
| [`wallet/`](wallet) | wallet host — host shell (`frontend/`) + BFF (`backend/`) + microfrontend runtime | Next.js 16 · TS · Tailwind · Rust · axum | [frontend](wallet/frontend/README.md) · [BFF](wallet/backend/README.md) |
| [`sdk/wallet/`](sdk/wallet) | `@evinvest/wallet` — host wrappers for the wallet MFEs, published to npm by the `@evinvest/wallet-v*` tag | TS · React · tsup | [README](sdk/wallet/README.md) |

`domain` is the shared source of truth for types — the hub and every service repo
depend on it, never on each other. `contracts` (vendoring `proto/`) is the single
gRPC dependency other repos import; the published `@evinvest/uikit` is the shared
design source of truth for the clients. There is **no HTTP on the hub** — browser
traffic reaches it through the wallet BFF (`cabinet-backend`), which proxies HTTP↔gRPC.

### Run

Every app is a flake app. `nix run` resolves the repo root at runtime, so there's
no need to enter the dev shell first.

| Command | Brings up |
| ------- | --------- |
| `nix run .#init` | one-shot env setup for a fresh clone (dev `.env` secrets, npm deps) |
| `nix run .#dev` | everything: Postgres + TigerBeetle + Redis + signer + piggybank + cabinet-backend + cabinet |
| `nix run .#piggybank` | hub server: core gRPC + auth tasks (tonic-web) |
| `nix run .#cabinet` | the wallet host frontend: Next.js host shell (proxies `/api/*` to the BFF) |
| `nix run .#cabinet-backend` | the wallet BFF (needs piggybank) |
| `nix run .#db` | SHARED ev_invest Postgres (data under `~/.local/state/ev_invest/pg`) |
| `nix run .#tb` | local TigerBeetle (data under `.tb/`, single replica) |
| `nix run .#redis` | SHARED ev_invest Redis (central auth refresh-token store) |

Every port comes from ONE place — the `ports` attrset in `flake.nix`. Postgres
and Redis are single instances shared by every ev_invest repo (database name ==
app name); they start detached and survive dev-stack exits. `.#dev` ensures
them, then owns the rest; one Ctrl-C tears down what it owns. Per-area build,
test, and layout details live in each folder's README — see the table above.

A dev shell with the full toolchain (Rust nightly + `wasm32`, Node, Postgres,
TigerBeetle, Redis, protobuf) is auto-activated by `.envrc` +
direnv, or via `nix develop`.

<!-- Per-area details live in each folder's README and the full architecture in docs/ARCHITECTURE.md — not duplicated here. -->

## Design

All UI lives in one Figma file (`e0V2P1cQpEFRuXTeNtEMh6`) — a dark-navy system in **Inter**, every value bound to `ev/color` · `ev/semantic` · `ev/radius` variables and shipped to clients as `@evinvest/uikit`.

| Surface | What | Figma |
| ------- | ---- | ----- |
| uikit | EV UIKit — tokens + component library (shadcn-class) | [node 10-2](https://www.figma.com/design/e0V2P1cQpEFRuXTeNtEMh6/Main?node-id=10-2) |
| cabinet | Investor portal — `cabinet` host shell: **Banking** nav + **Products** (mounted service MFEs) + per-service surfaces; desktop + mobile | [node 75-3](https://www.figma.com/design/e0V2P1cQpEFRuXTeNtEMh6/Main?node-id=75-3) |
| admin | Operator console over the central hub (`piggybank`) + microservices — users, outbox, MFE registry, feature flags; desktop + mobile | [node 346-27](https://www.figma.com/design/e0V2P1cQpEFRuXTeNtEMh6/Main?node-id=346-27) |

Admin surfaces **Sentry** (errors + tracing across hub and services) and **PostHog** (product analytics, feature flags, A/B experiments).


<br>

<sup>
	This repository follows <a href="https://github.com/valeratrades/.github/tree/master/best_practices">my best practices</a> and <a href="https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md">Tiger Style</a> (except "proper capitalization for acronyms": (VsrState, not VSRState) and formatting). For project's architecture, see <a href="./docs/ARCHITECTURE.md">ARCHITECTURE.md</a>.
</sup>

#### License

<sup>
	Licensed under <a href="LICENSE">Blue Oak 1.0.0</a>
</sup>

<br>

<sub>
	Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>

