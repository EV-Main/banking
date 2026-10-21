# Wallet actions inside the host page — spec (2026-10-19)

Owner decision: pay / withdraw / move / mandate run INSIDE the host page (SA panel, invest cabinet; admin stays first-party on
evinvest.ltd), as part of the wallet micro-frontend (`<mfe-banking-wallet>` from evinvest.ltd/wallet/mfe/*). No popups by default,
no iframe, no redirect. The popup `openWalletAction` stays as a fallback behind a flag. The hosts and the bank are one product
("powered by Expected Value Investment"). The security model below is what makes this acceptable; do not weaken it.

Source of truth for the analysis: the security review of 2026-10-19 (threat model T1–T16, controls a–z). This file is its contract.

## Owner decisions (final)
- Limits: "softer" profile. Withdrawals through a host: 1000 USDT per operation, 2000 USDT per 24 h per user, at most 6 per hour.
  First-party (evinvest.ltd, admin): 4000 per operation, 10000 per 24 h. Above 4000 through a host: manual approval (existing approvals).
  All numbers are env-tunable with these defaults (no redeploy of code to change them).
- New withdrawal address: held 6 h (`release_at`), cancellable from the email link; 6 h without withdrawals to NEW addresses after a
  password / email / passkey change. Email on every withdrawal, mandate grant/revoke and new address.
- Step-up: passkey is REQUIRED for withdraw and mandate (grant, and revoke that raises limits), bound to that operation. Email code
  (with the operation's details in the mail) is allowed for pay and for move above the threshold. Password is NOT accepted on trusted hosts.
- Withdrawal on SA ships in the same release as the other actions, but only after the SA panel's strict CSP (no `'unsafe-inline'`) is live.
- Popup fallback stays, off by default behind a flag.

## Controls (must all exist before a trusted host is enabled)
(a) Bank backend (banking cabinet/backend): keep `first_party_origin` for old routes; ADD `trusted_wallet_origin` with an in-code matrix
    origin -> (scope, allowed kinds, limits) (reviewable in code, like the frontend `wallet-scopes.ts`); env `WALLET_ACTION_ORIGINS`
    (empty = off, fail-closed; prod startup refuses non-https, `*`, paths, origins outside *.evinvest.ltd) and `WALLET_ACTION_KINDS`
    (default only `move`). New cross-origin API only on `/api/wallet/intents*` and `/api/wallet/csrf`. Checks: exact Origin match,
    `Sec-Fetch-Site in {same-origin, same-site}`, strict `Content-Type: application/json` (415 otherwise), custom header `x-ev-csrf`
    (forces preflight). CORS for these routes: OPTIONS only for the allowlist, `Allow-Methods: POST`, `Allow-Headers: content-type,
    x-ev-csrf`, `Allow-Credentials: true`, `Vary: Origin`, `Max-Age: 60`; `route_layer` on the specific routes only.
    Also close existing gaps: `require_fresh_login` + mandatory `idempotency_key` on withdrawals (money.rs ~159/167), step-up by
    threshold on `pay_invoice` (~203), `money_gate` on subscribe/redeem/cancel_redemption/book orders, strict Content-Type on
    first-party money routes.
(b) CSRF for a host without a readable cookie: `GET /api/wallet/csrf` (CORS for the allowlist) returns a token = HMAC(BFF key, sub, origin,
    10 min window; current + previous window), `Cache-Control: no-store`, `Vary: Origin`, never in a URL. First-party keeps cookie double-submit.
(c) Step-up inside the page, no redirect: new concierge `/auth/stepup/options|verify` bound to an operation (server keeps ceremony -> intent_hash),
    returns a short proof JWT (aud banking-bff, sub, intent_hash, method, jti, exp <= 120 s) signed with the concierge key (JWKS already read by the BFF);
    does not move the session's `auth_time`. Passkey with explicit `rpId: "evinvest.ltd"` (valid from sa./invest. as the parent registrable domain);
    existing "passkeys created before the session" rule stays. Email code: the mail carries the operation summary and the host. Password: only for PUBLIC_ORIGIN.
    CORS + exact origin allowlist (flag `STEPUP_PROOF_ORIGINS`, empty = off). The bank accepts a proof only for the intent_hash it stored itself.
(d) Intent: `POST /api/wallet/intents {kind, params}` -> server-computed `{intent_id, summary{amount, fee, net, recipient_full, network, scope, host_origin},
    expires_at (120 s), stepup{required, methods}, new_recipient, release_at}`; `POST /api/wallet/intents/{id}/confirm {proof?}` executes with
    `idempotency_key = intent_id`. Intent = signed token (sub, origin, kind, params, jti, exp); statuses created -> authorized -> executed|expired|cancelled
    (CAS); confirm from another Origin or sub is refused. pay/mandate read the invoice / mandate request from the server and check its scope against the
    origin's scope (confused deputy). The server ignores `scope` from the DOM.
(e) Limits and velocity in the hub per the owner profile above, per origin; address book with `release_at` honoured by the dispatcher
    (today withdrawals go `Opening::Immediate`); cooling-off after credential changes; approvals above the host threshold.
(f) Audit + notifications: add `origin, client_ip, user_agent, intent_id, stepup_method` to RequestWithdrawal / MoveFunds / PayInvoice / GrantMandate /
    RevokeMandate; client IP from the trusted Traefik hop, never the leftmost x-forwarded-for; email per withdrawal / mandate / new address with a
    "this wasn't me" link (cancel from Queued + sign out everywhere via `token_version`).
(g) Host integrity: SA panel CSP without `'unsafe-inline'` in `script-src` (sha256 of each inline script computed at startup, whole origin, not only
    the wallet page); bundle integrity (pins on sa., registry SRI on invest.); host CSP directories per the SDK helper `walletCsp()`; Trusted Types in
    report-only first; wallet forms in a closed shadow root + `data-sentry-block` / `data-ph-no-capture` + `maskAllInputs` on invest (Replay/PostHog).
(h) Risk classes: A read-only (balance, history, mandate list, invoice) as today; B intent without step-up below threshold (move <= 100 USDT and
    <= 300 USDT/24 h, cancel withdrawal from Queued, revoke mandate that only lowers limits); C step-up for THIS operation (withdraw, grant mandate,
    pay, move above threshold); D delayed + cancellable (new address, large withdrawal). Adding a new withdrawal address and changing sign-in methods stay
    on evinvest.ltd first-party (minimal, not default for the rest).

## PR sequence (each reversible; flags off by default)
PR0 banking backend hardening of today's gaps (no new host access). PR1 concierge step-up bound to an operation. PR2 banking intents + trusted origin
(+ hub: address book, release_at, limits, audit fields, mails). PR3 wallet MFE (in-page forms, passkey with rpId, shadow root, popup as fallback).
PR4 SDK @evinvest/wallet 0.2.0 (apiVersion 3, inline mode, walletCsp(), integrity helper; npm publish by the owner). PR5 hosts: invest, SA panel
(strict CSP first, then pins, then enable), admin unchanged.
Enable order with observation after each step: move on invest -> pay + mandate on invest -> move/pay/mandate on SA (after the strict CSP) -> withdrawal on invest
with passkey -> withdrawal on SA.
Negative tests are part of every PR: foreign origins (rogue.evinvest.ltd, evinvest.ltd.evil.example, null, empty, with port/path/trailing slash), missing or
cross-site Sec-Fetch-Site, text/plain, replay of intent_id and proof jti, tampered amount/recipient, expired step-up/intent, scope confusion between hosts,
over-limit, new address before release_at, within 6 h of a credential change, empty WALLET_ACTION_ORIGINS closes everything, password on /auth/stepup from a non-PUBLIC_ORIGIN.
