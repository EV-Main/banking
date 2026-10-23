# `evbanking_auth` patterns

The load-bearing decisions behind this crate. The how-to is in
[`README.md`](./README.md); the platform rationale in
[`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md).

## Stateless verification, state in one place

Access/service tokens are short-TTL **EdDSA** JWTs verified entirely against cached
JWKS public keys (`jwks::verify_token`). No verifier — downstream or the hub's own
in-process path — makes a network call or holds token state on the hot path. The
only stateful auth store is the central refresh store (rotation + reuse detection).

## Token separation by `typ` + `aud` (the client/service split)

Each **signed JWT** carries a `typ` (`access` | `service`) and a distinct audience:

| Token | `typ` | `aud` | `sub` | reaches |
| ----- | ----- | ----- | ----- | ------- |
| client access | `access` | `banking-core` | user UUID | the data plane |
| inter-service | `service` | `banking-services` | service name | the hub's own services (the signer) |
| scope client | `service` | `banking-scope` | client id (from its mTLS certificate) | the scope listener only |
| refresh | _(opaque handle, not a JWT)_ | — | — | only the auth service |

A [`VerifyPolicy`](src/jwks.rs) names the accepted issuer, the accepted **audience
set**, and the accepted **`typ` set**. So a service-only endpoint rejects a human
access token (and vice versa). The audience is a *set* so the hub's own in-process
verifier can accept the several audiences the hub itself issues, while a downstream
pins exactly one. Refresh tokens are opaque, rotated, server-side handles (see
below) — never JWTs — so they can't be replayed against `core` at all.

## Algorithm pinned at the verifier

`verify_token` pins `Algorithm::EdDSA` and refuses anything else (never `none`,
never HS\*, never a header-chosen algorithm). The money plane does no third-party
(Google) OAuth — that lives wholly in concierge — so there is no RS256 / `id_token`
verification here at all.

## Async layer, not a tonic interceptor

tonic 0.13's `Interceptor` is synchronous and can't await verification, so
authorization is a bespoke [`tower::Layer`](src/interceptor.rs) (`GrpcAuth`). It
extracts the bearer token, awaits an [`Authenticate`] implementor, injects `Claims`
into the request extensions, and short-circuits with gRPC `UNAUTHENTICATED`
otherwise. Two implementors share it: [`Verifier`](src/verifier.rs) (downstream,
JWKS) and [`Authorizer`](src/authorizer.rs) (the hub, in-process channel). Mount it
**per service** so public surfaces (health) stay open.

## Refresh rotation with reuse detection

Refresh tokens are opaque `"<family>.<secret>"` handles
([`management`](src/management.rs)). Each use rotates the secret; presenting an
already-rotated secret is treated as theft and revokes the whole family (OWASP
refresh-rotation). The family table is in-process by default (single-instance /
dev, mirroring the wallet BFF's session store); `RefreshStore::from_env` engages
the Redis arm — the one central Redis, same semantics, rotate/reuse as one atomic
Lua script — when `REDIS_URL` is set. A per-service Redis is never introduced.

Sessions are bounded three ways, all enforced in `rotate` **before** the sliding
window: the sliding `expires_at` (reset to `now + AUTH_REFRESH_TTL_SECS` per
rotation), an immutable `absolute_expires_at` stamped at issue time as
`created_at + AUTH_MAX_SESSION_SECS` (default 90d) that no rotation can extend,
and an optional idle timeout (`AUTH_IDLE_TIMEOUT_SECS`, `0` = off) over the
family's `last_seen`. The absolute cap stops a thief who quietly rotates a token
they exclusively hold from keeping a session alive indefinitely. These bounds are
carried in the family record, so they survive the move to Redis unchanged.

## `token_version` revocation, where the truth is

`users.token_version` (Postgres, owned by `core`) is authoritative. A "revoke all"
bumps it; the auth service notices on the next **refresh** (it re-reads the current
version over the in-process `Provisioner` channel and refuses to mint), so all
refreshes are blocked immediately and access tokens die within their short TTL.
Stateless downstream verifiers do **not** consult `token_version` — they rely on
the TTL. Per-request hard revocation hub-side (a `token_version` check on the
authorize path) is the documented next step.

## Error → gRPC `Status` mapping

One `From<AuthError> for tonic::Status` ([`lib.rs`](src/lib.rs)) is the single
mapping: missing/invalid/unknown-kid/provider-rejected → `UNAUTHENTICATED`;
not-configured/unavailable/jwks-refresh-failed → `UNAVAILABLE`. Only the genuinely
operational variants (`Unavailable`, `JwksFetch`) are reported to error monitoring
([`telemetry`](src/telemetry.rs)); a rejected token is a client outcome, not an
incident.
