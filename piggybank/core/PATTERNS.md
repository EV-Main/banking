# Money plane — balance, compartments & withdrawals

How the hub ("piggybank") stores and accounts for its money. Cross-cutting context
is in [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md); this file is the per-area
reference. **Working with money — read this before touching the ledger.**

## Two stores, never joined in one transaction

- **TigerBeetle = data plane** — authoritative amounts (balances, transfers). Never
  re-bookkept in Postgres.
- **Postgres = control plane** — ids, state, the event log + outbox, projections, and
  the `tb_accounts` UUID→`u128` account-id map. Holds **zero** amounts as numbers it
  reasons about (deposit amounts are stored as exact base-unit TEXT, never
  summed authoritatively in SQL).

## Units

One canonical internal unit: **18-decimal USDT base units** (`domain::money::Usdt`, a
checked `u128`). On-chain decimals differ (BEP20 = 18, TRC20/TON/**Polygon** = 6); the custody
edge ([`Usdt::from_onchain`]/[`to_onchain`]) scales by `10^12` and rejects sub-precision
dust. Amounts cross gRPC and the event log as **strings** (proto3 has no `u128`;
`serde_json` has no `u128`) — lossless and JS-safe.

**Rails.** Two chain families back the same fungible USDT pool. **EVM** — BEP20 (BSC) and
**Polygon (PoS)** — share one parameterized adapter set (`evm_rpc` + `ChainCustody`/`DepositWatcher`/
`WithdrawalWatcher`/`Sweep`, one instance per rail keyed by `EvmConfig::network`): identical
`eth_getLogs`/receipt/nonce/legacy-signing machinery, differing only in chain-id, USDT contract,
token decimals (BEP20 18-dp vs Polygon 6-dp), and native gas coin (BNB vs POL). Each EVM rail's
`withdrawal_broadcasts` nonce sequence is SQL-scoped by `network`, so the two never collide. **Non-EVM**
— TRON (TRC20) and TON (jetton) — keep their own per-rail files (`tron_*`, `ton_*`) because their
curves/tx formats/RPC shapes can't share the EVM path. A rail runs only when its endpoint env is set
(`POLYGON_RPC_URL` for Polygon); its treasury + gas-station are per-network wallets the operator funds
separately (USDT + native coin).

## Chart of accounts (`domain::balance`)

**Two layers** on one USDT value ledger (`ledger = 1`); the bank mock is a separate USD
ledger (`= 2`):

| Layer | Accounts (`code`) | Normal | Non-negative flag | Network? |
| --- | --- | --- | --- | --- |
| **Treasury / custody** (assets) | `wallet:<net>` (10), `bank` (11) | debit (`debits − credits`) | `CreditsMustNotExceedDebits` | **per-rail** |
| **Claims** (equity/liab) | `user:<uuid>` (20), `scope_claim:<scope>:<uuid>` (21 — a user's compartment in a scope), `scope_account:<scope>:<label>` (31 — a scope's own account, banking's treasury `scope_account:main:treasury` included), `clearing` (50), `hold_clearing` (51 — pending only) | credit (`credits − debits`) | `DebitsMustNotExceedCredits` | **network-agnostic** |
| **Retired** | `AccountCode::RETIRED` (1, 30, 40, 60–65) | — | — | the accounts stay in TigerBeetle and their rows in `tb_accounts`; scans skip them by code before parsing; never reused |

A deposit is one balanced transfer **`Dr wallet:<net> / Cr <claim>`** (textbook Dr Cash
/ Cr customer-deposit) — there is no "external world" account. The flags are set **once
at account create** (immutable in TB) and are the last-line backstop against an
over-spent claim or negative custody.

**Every claim has a holder.** A claim is a person's (`user:`, `scope_claim:`) or a scope's
own account (`scope_account:`), banking's treasury `(main, treasury)` among them (D17). On
the wire and in every column a `Party` is `user | treasury`. The cash scan classifies a
retired code as `CashSide::RetiredClaims`, and value on one fails reconciliation
([§ Reconciliation](#reconciliation--reaper--dispatcher-recovery-jobs), 1). A transfer kind
the hub no longer posts is a number in `TransferCode::RETIRED`, never handed to a new kind.

**Two layers, network only at the edges (load-bearing).** USDT is one fungible pool, so a
user has **one** claim per compartment, not one per chain — network lives **only** in the
treasury (`wallet:<net>`) and on deposit/withdrawal *transactions*. The invariant is
therefore **global**: `sum(custody) == sum(claims)` (a deposit grows both sides; a move or
a payment is claim→claim, net-zero on each sum; a withdrawal drops both by `net`).
Per-rail backing is a **treasury** concern, not a ledger one — a withdrawal on a rail short
of liquidity is *accepted and queued* (see below), never refused. `clearing`
(`WithdrawalClearing`) holds a queued/in-flight withdrawal's reserved gross, decoupled from
any rail so acceptance never depends on rail liquidity.

## User wallet — deposit & withdraw (`domain::withdrawals`, `WalletService`)

A user's money is **one** network-agnostic claim (`user:<uuid>`) plus their compartments.
`GetWallet` presents it segmented by lifecycle — `available` (`posted − locked`),
`pending_withdrawal` (the claim's `locked`: queued/in-flight withdrawals), `in_compartments`
(Σ the user's `scope_claim` posted, still theirs), and `total = available +
pending_withdrawal + in_compartments`. The claim's terms come off one posted balance (its
`posted` is `available + pending_withdrawal` by construction) — plus a
per-rail deposit address and a per-rail **withdrawable** view (`instant = min(available,
rail liquidity)`, the accept-and-queue hint — it discloses a rail's liquidity only up to
the user's own balance; bucket/round it if that must stay private).

**Deposit (chain → claim).** `GetDepositAddress` hands a **verified** user (`kyc_level ≥ 1`
— see [Authorization](#authorization-defense-in-depth)) a stable per-(user, network)
address from the [`DepositAddresses`] port — a stub HD-derivation cached into
`user_deposit_addresses`; the real xpub service is a follow-up. Crediting flows through the
admin `RecordDeposit` gate (idempotent by `tx_ref`), the stand-in for a chain watcher;
`Dr wallet:<net> / Cr user:<uuid>` credits the **unified** claim regardless of rail.

**Withdraw (claim → chain) — the dangerous direction.** A [`Withdrawal`] is a queued saga.
`RequestWithdrawal` Read-First checks the user's **available** unified claim covers the
gross (user solvency) and gates on the account being active (KYC/freeze), then records the
aggregate. It starts **`Queued`**, with the gross reserved as a pending `Dr user:<uuid> /
Cr clearing` — independent of any rail. The chosen rail's liquidity is the **treasury's**
job, and the dispatch gate is **`min(TB rail, on-chain treasury)`**: the TB `wallet:<net>`
balance alone over-counts (it includes confirmed deposits still on users' derived
addresses, which the treasury hot wallet cannot spend), so the gate also reads the custody
adapter's real on-chain treasury USDT (`Custody::treasury_liquidity`; `None` = no chain
view = TB-only, the stub rails). If the effective liquidity covers the net the withdrawal
is dispatched immediately, otherwise it stays `Queued` until the rail is topped up
(accept-and-queue). A treasury read **failure degrades to `Queued`, never a refusal** —
acceptance and the clearing reserve must not depend on a flaky chain node.

| event | → state | relay ops |
| --- | --- | --- |
| `Requested` | `Queued` | reserve `Dr user:<uuid> / Cr clearing` (gross) — no rail touched |
| `Dispatched` | `Processing` | `Custody::broadcast` the net on-chain |
| `Settled` (N confs) | `Completed` | **post** the clearing pending, then `Dr clearing / Cr wallet:<net>` (net) + `Dr clearing / Cr scope_account:main:treasury` (the retained fee) |
| `Failed` (never landed) | `Failed` | **void** the clearing pending — refund in full |
| `Cancelled` (still queued) | `Cancelled` | **void** the clearing pending — refund in full |

The treasury worker is the [`dispatcher`](src/infrastructure/dispatcher.rs) (see
[Recovery jobs](#reconciliation--reaper--dispatcher-recovery-jobs)); `DispatchWithdrawal`
is its manual override — it refuses (leaving the withdrawal `Queued`, still cancellable)
when the rail treasury provably lacks the net on-chain, or when the **outflow policy** no
longer permits the payout. That policy — the read-only kill-switch, the owner's
cross-plane freeze, and the tier-1 floor for a `WithdrawalSource::User` — lives inside
`dispatch_withdrawal` itself, not in its callers: admission is not the last word, because
a withdrawal can sit `Queued` for hours and a pause, an AML hold or a revoked
verification landing in that window must still stop the money. Every arm fails **closed**
(a missing owner row, an unreadable flag and a corrupt tier all refuse), and both the
sweep and the operator RPC inherit it from the one place rather than each carrying a copy.
The operator handle has **no `force` override**: the override for a pause is
`SetOperationsMode`, which is itself permissioned and leaves one auditable record of who
reopened outflows, instead of a per-payout flag indistinguishable from a routine
dispatch. `SettleWithdrawal` and
`FailWithdrawal` are operator/watcher-driven **admin** RPCs on `BalanceService`;
`CancelWithdrawal` is the user's own (a queued withdrawal only). The cardinal rule —
**never void once the broadcast may have reached the chain** (that double-pays) — is why
`Fail` is legal only from `Processing` and `Cancel` only from `Queued`. The incident
runbook for a stuck/parked withdrawal is
[`docs/RUNBOOK-withdrawals.md`](../../docs/RUNBOOK-withdrawals.md). The fee leg is omitted when the fee is zero (TB rejects a
zero-amount transfer); the policy enforces `min_withdrawal > fee` and the net must be
representable at the chain's precision (no dust leaves). The global invariant holds at
settle: `user` falls by `gross`, `wallet:<net>` by `net`, the treasury rises by `fee`,
and `clearing` nets back to zero — so `sum(custody)` and `sum(claims)` both fall by
exactly `net`.

**Custody is a separate trust domain.** [`Custody`] is the hub's only ask of the signing
service ("broadcast this *already-reserved* withdrawal, idempotently by id"); the hub never
holds keys. [`StubCustody`] no-ops until the real MPC/HSM service exists — the saga, the
ledger, and the RPCs are complete and unchanged when it lands. The **signer** applies its own
spend policy as an independent second gate (holds even if the hub is compromised), always on
and per wallet class (`piggybank/signer/src/policy.rs`): a **treasury** payout is held to a
per-transfer USDT cap (`SIGNER_MAX_TRANSFER_USDT`, default 100), a per-hour USDT window
(`SIGNER_MAX_TREASURY_USDT_PER_HOUR`, default 1000), the pinned USDT contract
(`SIGNER_USDT_CONTRACT_{BEP20,POLYGON,TRC20}`, mainnet by default) and an optional destination
allowlist (`SIGNER_DESTINATION_ALLOWLIST`); treasury-native transfers are refused unless
`SIGNER_ALLOW_TREASURY_NATIVE` plus `SIGNER_TREASURY_NATIVE_ALLOWLIST` and `SIGNER_MAX_TREASURY_NATIVE_*` opt in (a window refusal parks the withdrawal — an alert, not a retry). A
**sweep** (from a deposit address) may only go to the treasury address the signer holds for
that network; a **gas top-up** (from the gas station) may only be native, only to an address
the signer holds a key for, and at most `SIGNER_MAX_GAS_TOPUP_*`. Every wallet also has a
sliding one-hour native-spend window (`SIGNER_MAX_NATIVE_SPEND_PER_HOUR_*`, fee + native
value, counted in the signer's own database before the key is touched). A TON jetton
transfer names two more Toncoin sinks: `response_destination` must be a wallet the signer
holds (the sender, the treasury or the gas station), and `our_jetton_wallet` is pinned on
first use per wallet (`jetton_wallets`), or for the treasury by
`SIGNER_TON_TREASURY_JETTON_WALLET` when set (#377 derives it offline instead). Plus an
always-on **fee budget** on every signed transaction from every
wallet (`SIGNER_MAX_GAS_LIMIT`, `SIGNER_MAX_GAS_PRICE_GWEI_{BEP20,POLYGON}`,
`SIGNER_MAX_TRON_FEE_LIMIT_SUN`, `SIGNER_MAX_TON_{MSG_VALUE,FORWARD}_NANO`), so a forged
1 USDT transfer cannot burn the native balance as gas. Tron signing is off until
`SIGNER_TRON_SIGNING_ENABLED=true` (the rail is frozen on the hub side, #31). The
environment is the unliftable ceiling; the operator's **spend brake** — one `spend_brake`
row in the signer's own database (`piggybank/signer/migrations/0009_spend_brake.sql`),
read before every signature, no RPC to it by design — can tighten the per-transfer cap,
the USDT window and the native windows to `min(env, brake)` or halt every signature
without a restart, and a halt parks the withdrawal exactly like a window refusal
(operator SQL in [`docs/RUNBOOK-withdrawals.md`](../../docs/RUNBOOK-withdrawals.md)
§Spend brake).

### Treasury spending (D17, D24)

The **retained fee** on every user withdrawal is banking's own and lands in banking's
treasury, `scope_account:main:treasury` (D17), and so do the fees of every scope but invest,
swept from their `(<scope>, fee)` collectors (`scope/v1/ledger.proto`, the fee route;
invest's go to `(invest, alloc:fee)`). Nothing leaves the treasury until its spending is
decided by concierge's scope approvals (`bank:treasury_surplus`, D24): `payments::open`
refuses a payment order out of it (`Party::Treasury`) with `treasury spending waits for scope
approvals`, and `execute` fails one approved before the close. The machinery it will use
stays — a payment order, internal or on a rail through a treasury withdrawal
(`WithdrawalSource::Treasury { payment }`, no fee, no owner standing to read). A treasury
withdrawal names the order it executes — on the source, and in `withdrawals.payment_id`
under a CHECK that only a treasury withdrawal has one — and reconciliation (4) holds every
treasury debit to an approved order. `require_dispatchable` (`application/withdrawals.rs`)
reads the owner of every person's withdrawal. Old outbox payloads spell the source field
`user` and hold a bare UUID; `WithdrawalSource`'s string form and the `alias = "user"` on
the events read those unchanged.

## Compartments by scope (`domain::scopes`, `CompartmentService`)

A user's wallet is split by **scope** (D9): `main` is their `user:<uuid>` claim — deposits
land there and withdrawals leave from there — and every scope they enter holds a
compartment, `scope_claim:<scope>:<uuid>`. Only the user moves money `main ⇄ scope`
(`MoveFunds`), instantly and without a fee, never scope to scope.

**A move IN** passes the scope's state (`scope_frozen` / `scope_closed`), the account's
freeze (`user_frozen`), the outflow pause and the scope's policy. **A move OUT** — back to
`main` — passes none of them: the money never leaves the platform, so a frozen user in a
paused system still gets it back. Both need the source to cover the amount, at least
`MIN_MOVE` (0.01 USDT, `amount_below_minimum`), and at most `MAX_MOVES_PER_MINUTE` (10)
moves by the user in a rolling minute (`rate_limited`): every move is a transfer through
the platform's one relay, and dust or a burst from one user is everyone's latency.

**Checked under the locks, net of what is in flight.** One Postgres transaction takes an
advisory lock on `(user, idempotency_key)` first — two requests under one key may name
different sources — and answers a reused key with the stored move (a racer that still
reaches the unique index gets ALREADY_EXISTS). Then a lock on the user, under which the
per-minute count is taken, then the source claim's write lock
(the one a withdrawal takes) and, for a move in, the target's, always in that
order, and only then runs every check: for a move in, the scope's state, the account's
freeze and the policy — the target's `posted + pending` plus the `pending` moves into it
against `max_balance`, the `pending` + `posted` moves into the scope over 24 h against
`max_moved_in_24h` — and for both directions what the source may spend. The loser of two
racing moves is refused at once. A refusal writes nothing; a pass records the
`compartment_moves` row and its event together.

**One answer to "what may the user still spend".** `Ledger::spendable(key)` is the
ledger's `available` less every debit of `key` the control plane recorded and the relay
has not applied yet (`claims::unapplied_debits`): the owner's `pending` moves out of that
compartment, and every outbox event neither dispatched nor parked whose relay plan posts or
reserves a debit of `key` — a withdrawal's reserve, a payment's reserve, a scope's debit. The unapplied debits are read before the ledger, because the
relay applies first and marks dispatched after: the order can count a debit twice for a
moment (a refusal that clears on retry), never miss it. Every Read-First on a user's money
asks it: a withdrawal (`require_solvent`), a payment, a move. The withdrawal repository
and the move store ask it twice: in the
use case, and again through a `SpendGuard` (`ClaimCovers`) or the move's admission after
they take the claim's lock and before they write — so two spends of one balance cannot
both be recorded, whatever the relay has applied yet. Under the lock every control-plane
read goes through `LockedReads` on the connection holding the lock (`claims::TxReads`):
each waiter on the lock holds a pooled connection, and a holder asking the pool for a
second one stalled the hub until the acquire timeout (#507). A parked event is not counted
— the relay will not apply it on its own; unparked, it meets the ledger's non-negative
flag. What stays advisory: a payment order's open-time check — the money is reserved at
approval, by the relay.

**A move is never lost and never runs twice.** The row is `pending` until the relay posts
`Dr <from> / Cr <to>` (`TransferCode::CompartmentMove`, id `uuid_v5(move_id,
"compartment:move")`) and stamps it `posted`. If the ledger refuses it — another flow's
reservation landed first — the relay stamps it `failed` (`failure_reason`), parks nothing,
and never carries it out later. Any other park of a move's event (a ledger conflict, an
unplannable payload, an exhausted retry) stamps it `failed` too, as `parked:<kind>`, which
releases the money it promised; an unparked or replayed event of a `failed` move is a no-op.
A `pending` move whose event a relay dispatched without posting (an older build that did
not know the kind, in a rolling deploy or after a rollback) is re-enqueued by the
[`reaper`](src/infrastructure/reaper.rs) after five minutes; the replay finds the transfer
id already on the ledger if it was, and only stamps the row. A row the reaper cannot replay
is logged and sent to the back of the queue; the others are not held up by it.

The registry is `scopes` (seeded `invest`, `sa`; since 0053 kept by concierge's scope stream
— [`scope_events`](src/infrastructure/scope_events.rs), one consumer under an advisory lock,
which stops and alerts on a rewound stream or a malformed page; reconciliation alerts when
no well-formed page has arrived for 5 minutes, and only warns while concierge answers
UNIMPLEMENTED; a scope it registers is closed to moves in until an operator sets its
policy — [`docs/RUNBOOK-scope-policy.md`](../../docs/RUNBOOK-scope-policy.md)) and
`scope_policies` (`min_kyc_level` against the bridge's mirrored tier; `max_per_move`,
`max_moved_in_24h`, `max_balance` on moves IN only; the automatic ceilings
`auto_payout_per_op` / `auto_payout_24h` / `auto_transfer_per_op` / `auto_transfer_24h`,
defaults 1000 / 5000 / 1000 / 5000 USDT, which the [scope ledger](#scope-ledger--the-service-facing-money-surface-domainscope_ledger-scopeledgerservice)
enforces). `scope_accounts` records who owns each scope account: banking owns `(main,
treasury)` — where its own fees land (D17) — and `(<scope>, fee)`; every other row is a
client's, written by `OpenAccount`.
The scope list behind listings and totals is cached per process for 30 s; a move reads the
scope row itself. `GetWallet` (`in_compartments`, inside `total`) and `GetBalance` / `GetUserBalance`
(`amount = main + in_compartments`) count the compartments; `GetTreasury` shows
`held_in_compartments` and `banking_treasury`. Mandates are not served yet
(`GrantMandate`/`RevokeMandate` UNIMPLEMENTED, `ListMandates` empty): the per-client grants
they need exist now (below), the mandates themselves come with the scope ledger's next part.

## Scope ledger — the service-facing money surface (`domain::scope_ledger`, `ScopeLedgerService`)

`scope/v1/ledger.proto` is how a scope's own service (invest, the SA panel) moves money in
its scope. Served: `OpenAccount`, `GetBalances`, `Transfer`, the holds, `Payout`, the requested
operations and `MoneyEvents.Pull`; `Charge` is UNIMPLEMENTED until users can grant mandates.

**Its own listener, mTLS only** (`services::scope_ledger`, `config::ScopeLedgerSettings`):
`SCOPE_LEDGER_BIND` turns it on, and then the server certificate, the client CA and the
grants file are all required and checked at boot — the TLS material is parsed into the very
rustls server config the listener will run, so a broken key or a certificate that is not
the key's exits 78 before the hub says it is listening. `main` installs the process-level
rustls provider (`ring`, `crypto.rs`) before anything else: the graph carries both `ring`
and `aws-lc-rs`, and a rustls server refuses to pick — the first listener panicked on its
first handshake until it did (`tests/scope_ledger_boot.rs`, a binary of its own, guards it).

HTTP/2 only: ALPN offers only h2, so an HTTP/1.1 client fails the handshake, and a gRPC-Web
request sent over h2 is refused by a content-type gate (tonic itself does not look and
would decode it as gRPC). Two layers stand in front of the service, both mounted in
`serve_with` and nowhere else, the certificate outside the token: `ClientIdentityLayer`
maps the presented certificate (SHA-256 of its DER, the grants file's `cert_sha256`) to a
`ClientId`, the **only** source of the caller's identity; inside it, the auth layer on
`TokenClass::Service` (`aud=banking-scope`, `typ=service`). The token is then bound to
the certificate: its `sub` must be that client's id, or the call is PERMISSION_DENIED
`token is not this client's` — a token copied off one client is useless on another's
connection. `tests/scope_ledger_listener.rs` is the guard that the service is never
reachable without them.

**Getting the token: `ScopeAuthService.MintServiceToken`** (`scope/v1/service_auth.proto`) is
served on this same listener and is the ONE service outside the token layer — a client with
no token yet has to be able to ask for one. It sits behind `ClientIdentityLayer` only: the
certificate must map to a client in the grants file (else UNAUTHENTICATED), the content-type
gate applies, and the request is empty, so nothing the caller sends chooses the subject. The
token is signed by the hub's own key (a concierge token is refused by `assert_plane`, which
is why this is not concierge's to mint) with `sub` = the client id, `aud=banking-scope` (`AUTH_SCOPE_AUDIENCE`),
`typ=service`, and lives `AUTH_SCOPE_TOKEN_TTL_SECS`; there is no `cnf`/`x5t#S256` — the `sub`
binding above already ties it to the certificate. The audience is NOT `banking-services`: the signer accepts that one, so a scope client's token must not open it (and the hub's own bearer is no scope client's token) — two trust domains, two audiences, told apart at the verifier. Neither `banking` nor any `piggybank*` name can be a client, and a client with no grant at all is refused `not_granted`. A client mints at most once per 5 s (RESOURCE_EXHAUSTED, "reuse the token until expires_at"); the listener also has a 30 s request timeout and a per-connection concurrency cap. core reaches
the signer over the same in-process channel as authorize (`MintService` on `Authorizer`); a
user's access token is neither needed nor looked at on this service. Logged: client and `exp`,
never the token. `tests/scope_ledger_listener.rs` pins it, including
`the_mint_is_the_only_service_outside_the_token_layer`, which reads every `service` in
`scope/v1/*.proto` and fails if a new one is mounted without the token layer.

**Revoking a service client** is done on the certificate side, not the token: drop its
fingerprint from the grants file (or its grants) and roll the hub — the next call is
UNAUTHENTICATED (or `not_granted`) whatever token it carries, since every call needs both.
A service token carries no `token_version` worth checking (minted at 0, nothing stores a
floor for a service) and lives `AUTH_SCOPE_TOKEN_TTL_SECS` (300 s by default, 30..=900); a stolen token
without the client's private key is already useless by the binding above. A stolen key is
a certificate revocation: re-issue the client's certificate and replace the fingerprint
(two may be listed during the swap).

**Grants are banking's configuration** (`SCOPE_LEDGER_GRANTS_FILE`): `(client_id, scope,
label_prefix, compartments)`. The load refuses `client_id = "banking"` (banking owns `fee`
and `treasury`), `scope = main`, an empty prefix (it would cover `fee`), two prefixes in one
scope where one starts with the other, and a grant to a client no certificate maps to. At
call time `main` and a scope the client holds no grant in are the same PERMISSION_DENIED
`not_granted` — a client cannot probe which scopes exist — and only a granted scope banking
has not heard of from concierge is NOT_FOUND.

**Ownership.** `OpenAccount` writes `scope_accounts (scope, label, owner_client)` for a label
under the caller's prefix; `fee` and `treasury` are refused **by the label**
(`ScopeAccountLabel::is_reserved`) whatever a prefix covers, a label another client owns is
ALREADY_EXISTS, and the owner opening its label again gets the existing account. Only the
owner debits an account; a debit of `fee`/`treasury`, of someone else's account or of one
that does not exist is the same PERMISSION_DENIED `not_account_owner`. A debit also needs
the label inside one of the client's grants as configured **now** — a prefix taken away
takes away the debit of the accounts opened under it (`not_granted`), while the money stays
readable; a repeat of a key used before the prefix went still gets its original result
(only losing every grant in the scope refuses a repeat). `GetBalances` answers only the caller's accounts (`fee` included in "not yours":
NOT_FOUND), at most 256 labels per call, repeats answered once.

**Transfer** is a client's automatic fee sweep: from an account it owns into `(scope, fee)`,
`reason = FEE_SWEEP`; any other destination is `approval_required` (the client asks with
`RequestTransfer`). One Postgres transaction takes, in order, the key lock
`(client, scope, rpc, key)`, the ceiling lock `(client, scope)`, the scope row `FOR SHARE`
(a freeze cannot commit between the check and the record) and the source account's claim
lock, then checks: owner, `scope_frozen` (a closed scope's accounts can still be swept),
the automatic ceilings — `auto_transfer_per_op` and `auto_transfer_24h` over the client's
`pending` + `posted` transfers in the scope over a rolling 24 h, across every account it
owns there — and `spendable_locked` of the source, which counts `pending` transfers out of
it from `scope_transfers`. A scope without banking's fee collector is banking's broken
invariant, INTERNAL, never the caller's to retry. A refusal writes nothing; a pass records the row, the key and the
outbox event together. The relay posts `Dr scope_account:<s>:<from> / Cr
scope_account:<s>:fee` (`TransferCode::ScopeTransfer`, id `uuid_v5(transfer_id,
"scope:transfer")`) and stamps `posted`, or `failed` when the ledger refuses it — the same
single-leg discipline as a compartment move (`relay::Posting`), the same reaper replay of a
`pending` row an older relay dispatched without posting.

**The fee route** empties every `fee` collector: `scope_ledger::mark_posted` — the relay's
`posted` stamp of a `fee_sweep` — inserts, in the SAME transaction, a `scope_transfers` row of
reason `fee_route` (client `banking`, `from fee`, `fee_route_of` = the sweep, unique) and its
outbox event; invest's goes to `(invest, alloc:fee)`, every other scope's to `(main, treasury)`
(`domain::scope_ledger::fee_route`). The relay posts it as any scope transfer, so it is one
ledger leg: atomic, replay-safe (`ON CONFLICT (fee_route_of) DO NOTHING`; the ledger id derives
from the row's id), with the usual money events. No admission, ceiling or mandate applies — it
is banking's, not a client's. `Transfer` refuses a sweep (`fee_route_unavailable`) while the route's destination is not open,
read under the same locks. Should it vanish after admission, the route is inserted already
`failed` (`destination_missing`) with no event, so a missing account cannot wedge the relay;
the reaper (`redrive_fee_routes`) revives it when the account exists and also routes posted
sweeps that have no route (newer than migration 0059). A route that cannot be recorded does
not block the sweep's `posted` stamp (a savepoint rolls back only the route, `error!`). Fees swept before migration 0059 (or by an older relay) are not routed
retroactively.

**Idempotency** (`scope_ledger_requests`): a key (1..128 characters, no control characters)
is unique per `(client, scope, rpc)` and
stored with the canonical rendering of every term the call named (length-prefixed fields:
from, to, amount in base units, reason, `subject_ref`, memo). A repeat with the same terms
returns the original result, even after a freeze; other terms are ALREADY_EXISTS. The key
lock makes a second insert unreachable; should the unique index still fire (23505), the call
rolls back and answers with what the winner recorded.

**Reconciliation** counts scope accounts in the cash invariant by code (since #507) and,
since part 1, alerts on value on a scope account no `scope_accounts` row owns
(`unowned_scope_accounts`, part of `clean()`).

**Holds** (`Reserve` / `Capture` / `Void` / `GetHold`, `scope_holds`, part 2a). A hold is
ONE pending TigerBeetle transfer at a time, `Dr scope_claim:<s>:<user> / Cr hold_clearing`
(`AccountCode::HoldClearing` = 51, `TransferCode::ScopeHold` = 57), for exactly what is
left. A capture is a linked chain (`Ledger::apply_linked`): void the standing reservation,
post the captured amount out of the compartment to its destination, reserve the rest anew —
TigerBeetle posts a pending only once, so a partial capture cannot be a partial post, and
the chain keeps the user's money from being free in between. A release is the void alone.
Ids derive from the hold id and the version of the event (`scope:hold:{pending,void,capture}:<v>`,
FROZEN); the row's `pending_version` names the reservation standing now. Nothing is ever
posted to `hold_clearing`. `Reserve` takes the key lock and the compartment's claim lock
and checks, in order: the `compartments` grant flag, `scope_frozen`, the user (unknown →
`mandate_required`, indistinguishable from no consent), the mandate, `user_frozen`, and
`spendable_locked` of the compartment (an undispatched hold counts through the outbox scan;
a capture's chain counts net of the void it carries, `relay::chain_debit`). `Capture` /
`Void` lock the hold's row (another client's hold is NOT_FOUND). Into an account: only the
caller's own, never banking's. Into another user: the `compartments` flag, `scope_frozen`,
BOOK_FILL on the hold's own mandate, and the holder not frozen. A due hold is not capturable;
voiding it expires it; the reaper releases due holds (`scope_holds::expire_due_holds`).
**No user can grant a mandate yet**: production wires `NoMandates`, so every `Reserve` is
`mandate_required` until `GrantMandate` lands — the caps (`cap_per_op`, `cap_24h`,
`cap_outstanding`) are that part's, under the mandate row's lock. A reservation the ledger
refused parks like any refused reserve; the relay parks a later capture or release of that
hold at once rather than retrying a void of nothing.

`ListHolds` is the saga's recovery read for a `Reserve` whose answer was lost: the caller's
own `active` holds (the store filters by `client_id`, so a neighbour's are never read), by
scope and `subject_ref` prefix (`starts_with`, no `LIKE` escaping), keyset-paged by hold id
(`scope_holds_client_active_idx`). A page asks for one row more than it returns, so the last
page needs no second query. Ordering by id, not by time, is deliberate: a time cursor would
need microsecond tokens, and the reaper that walks it runs again anyway.

**Money events** (`MoneyEvents.Pull`, `money_events` + `money_event_deliveries`). Written
once: an account's opening in its own transaction, a movement by the relay right after the
ledger took it (single writer, so the balances read then are the balances right after the
event), with an id derived from what it is about so a redelivered row records nothing new.
Who receives an event is decided at write time — the initiating client and the owners of
the scope accounts it touched, never banking — and `balances` carries each account with its
owner; `Pull` returns only the receiving client's own. Appends serialize on one advisory
lock held to commit, so a cursor never passes a position that commits later. `Pull` with a
scope outside the caller's grants (`main` included) is `not_granted`; an empty scope means
every scope it is granted in now. Kinds so far: ACCOUNT_OPENED, HOLD_PLACED / CAPTURED /
VOIDED / EXPIRED, TRANSFERRED, PAID_OUT, OPERATION_REQUESTED / REJECTED / FAILED, INVOICE_CREATED /
PAID / CANCELLED / EXPIRED.

**Payouts** (`Payout`, `scope_payouts` + `scope_payout_lines`, part 2b). A payout is ONE
linked TigerBeetle chain, one posted leg per line, `Dr scope_account:<s>:<from> / Cr
scope_claim:<s>:<user>` (`TransferCode::ScopePayout` = 59, ids `uuid_v5(payout_id,
"scope:payout:<line_no>")`, FROZEN): every line lands or none does, and a source that cannot
cover it after all fails the whole payout (`relay::Posting::ScopePayout`, the same single
posting discipline and reaper replay as a transfer). One transaction takes the key lock, the
scope's **budget lock** (`scope_ledger_budget:<scope>`), the scope row `FOR SHARE` and the
source's claim lock, then checks: the `compartments` flag and the label grant, owner,
`scope_frozen`, every user known (NOT_FOUND), the ceilings, `spendable_locked`. Ceilings:
`auto_payout_per_op` on the total, `auto_payout_24h` over the client's automatic payouts in
the scope, and the scope's ONE daily budget (`scope_policies.scope_budget_24h`, D24(4)) over
every client's automatic payouts plus the scope-level payout decisions still live or executed
in 24 h. Under the budget lock concurrent payouts cannot both count the same room.

**Requested operations** (`RequestPayout` / `RequestTransfer` / `GetPendingOperation`,
`scope_operations` + `scope_operation_lines`). A client may have at most 20 operations
awaiting approval per scope (RESOURCE_EXHAUSTED, counted under a per-(client, scope) lock),
the source must cover the request now (checked under its claim lock, not reserved), a payout
never names a frozen or disabled user (`user_frozen`, D24(7)), and a transfer into another
scope needs that scope active. The approval names the requesting `service` (in its
`payload_hash`); its reason reads `Service <client>: <memo>`. The request is stored whole with its
`lines_digest` and `payload_hash` (concierge's encoding, `scope_operations::approval_payload`),
its source owned by the caller (a label grant is not needed — the approvers decide), a
transfer's destination the caller's own too. A requested payout carries `over_scope_budget`,
decided under the budget lock; its `max_line` field feeds the catalog's `per_line` escalation
(1000 USDT), and an automatic `Payout` with a line past `MAX_PAYOUT_LINE` is `approval_required`
(banking#525). Nothing is opened on the request path: the **approval
executor** (`application::scope_operations::ApprovalExecutor`, every 30 s and on a nudge, only
when the listener is on) first publishes banking's `bank:*` catalog
(`scope_operations::BANK_CATALOG`, versioned by `BANK_CATALOG_VERSION` — bump it with every
change; the same version again is a no-op in concierge, a transient failure is retried with
backoff and holds proposals back; a refusal holds them too, retried hourly, counted and
alerted; a test pins the catalog's digest to its version), then proposes over the bridge (`infrastructure::approvals`, the bridge
token = namespace `bank`, `dedupe_key = subject_ref = pending_operation:<id>`), pulls passed
decisions, RE-READS each by id right before moving money, checks kind, scope, subject_ref,
`lines_digest` and `payload_hash` against the stored row, and executes in one transaction under
the operation's `FOR UPDATE` — an approved payout or transfer (reason `approved`, naming the
operation) or FAILED with the reason; then reports the outcome (a lost report is repeated on
the next pull, never the execution). A decision that does not match is FAILED
`approval_mismatch` and reported VOID; a rejected / vetoed / expired / cancelled / void one
ends REJECTED `approval_<state>`; a proposal concierge refuses ends REJECTED
`proposal_refused`, one not opened in 72 h `proposal_not_opened`; concierge away only puts it
off (backoff to 15 min), and its RESOURCE_EXHAUSTED puts off only that one. The execution
re-reads the `compartments` grant, the users' freeze (rows `FOR SHARE`) and the destination
scope's state. An operation the executor cannot read or step is set aside for an hour and
the round goes on. The client sees
OPERATION_REQUESTED / _REJECTED / _FAILED, and PAID_OUT / TRANSFERRED under its
`subject_ref` when executed; an approved transfer between scopes shows once per side.

**Payout review** (`PayoutReviewService`, `infrastructure::payout_review`, migration 0063; banking#529).
The read behind the approvers' lines page: `GetPayoutApproval` hands the BFF the operation's
`approval_id` (no line, no name) so it can ask concierge — with the caller's own token — whether
the caller is a voter on that roster BEFORE `ListPayoutLines` reads anything; the lines come from
the stored `scope_operation_lines`, bound to that approval id and re-hashed against the
stored `lines_digest` (a mismatch refuses the page; the BFF also compares the digest to the one in
the approval's fields and tells the page via `digest_matches`). A recipient is "without history"
unless banking has a POSTED, `approved`-reason payout to them in the same scope — pending, failed,
automatic (redemption/book/fee_rebate) and the operation's own payouts do not count. Every refusal is one `NOT_FOUND`.
This service cannot see the roster itself (a service reads concierge's approvals without
addresses), so it checks only revocation and the id binding: the roster gate is the BFF's
(`wallet/backend/src/routes/payout_review.rs`). Do not expose it to a caller that skips the BFF.

**Reconciliation** (7) gauges scope transfers and payouts recorded and unposted for over 15
min (`telemetry::note_stale_scope_postings`), outside `clean()`.


**Charges** (`ScopeLedgerService.Charge`; `domain::scope_charges`, `scope_charges`, migration
0060). A one-off debit of a user's compartment into an account the client owns, under the user's
live CHARGE mandate: ONE posted transfer `Dr scope_claim:<s>:<user> / Cr scope_account:<s>:<label>`
(`TransferCode::ScopeCharge` = 60, id `uuid_v5(charge_id, "scope:charge")`, FROZEN). The store
takes the key lock, the compartment's claim lock, then the mandate row `FOR UPDATE` (a Reserve's
order), and the admission checks in order: the `compartments` grant, the credited account's owner
(`not_account_owner`; `fee`/`treasury` are refused by label first), the scope's state
(`scope_frozen` / `scope_closed`), the mandate (`admit_draw`: liveness, CHARGE listed, `cap_per_op`,
`cap_24h` — `cap_outstanding` is a Reserve-only cap, a charge holds nothing — all
`mandate_required`), `user_frozen`, and `spendable_locked` of the compartment. The row is
`pending` until the relay posts it; a `pending` charge is counted from the row in
`unapplied_debits` (the `scope_charges` kind is left out of the outbox scan) and, with every
`posted` one, in the mandate's rolling 24 h usage; a `failed` one (ledger refusal or park) counts
for neither. KIND_CHARGED is written by the relay after the posting, for the charging client
only. The reaper replays a `pending` charge whose event was lost.
A charge not on the ledger an hour after it was recorded is failed (`expired`) by the relay
before it posts, so an outage or a late unpark never carries out a stale consent; a revoke does
not cancel a charge already recorded. The smallest charge is 0.000001 USDT. A Closed scope
refuses a Charge (`scope_closed`); Reserve admits one.
RUNBOOK — a parked `scope_charges` row whose park is an `applied check` or `retry exhausted`
is deliberately left `pending` (the leg may have posted; its amount keeps counting against the
compartment). Settle it by hand: look the transfer id (`uuid_v5(charge_id, "scope:charge")`) up
on the ledger — present: `UPDATE scope_charges SET state='posted'` and unpark; absent: unpark
(it posts, or expires after the hour) or `UPDATE ... SET state='failed', failure_reason='manual'`.

**Invoices** (`scope.v1.InvoiceService` on the scope listener, `banking.v1.InvoicePaymentService`
on the client one; `domain::scope_invoices`, `scope_invoices`, migration 0056). A client issues
an invoice into an account it owns under a current grant (`CreateInvoice` keys are the scope
ledger's, rpc `create_invoice`; the canonical terms are every field); never into `fee` or
`treasury`; a frozen scope issues none. A payment is ONE posted transfer `Dr <payer's main or
scope_claim:<s>:<payer>> / Cr scope_account:<s>:<label>` (`TransferCode::InvoicePayment` = 58,
id `uuid_v5(invoice_id, "scope:invoice:pay:<version of the Paid event>")`, FROZEN) — no move
between compartments first.
`PayInvoice` takes the payer's key lock (`invoice_pay_key:<user>:<key>`), then the invoice row
`FOR UPDATE`, then the source's claim lock — nothing holding a claim lock ever waits on an
invoice row — and checks, in order: open and not past `expires_at` (`invoice_not_open`), the
scope's state (`scope_frozen` / `scope_closed`), `user_frozen`, the RECEIVING scope's
`min_kyc_level` (`kyc_level`, from either source), and `spendable_locked` of the source (an
undispatched payment counts through the outbox scan). The scope's limits on moves in do not
apply (D24 (6)). The row is stamped `paid` with `ledger_state = pending` in the same
transaction, so a second payer waiting on the row finds it taken; the payer's key is unique per
user on the row (`scope_invoices_pay_key_idx`). **Paid means posted**: the payment follows the
relay's `Posting` discipline like a move — counted from the row in `unapplied_debits` (the
`scope_invoices` kind is left out of the outbox scan), stamped `posted` by the relay, and only
then shown as PAID (to the issuer and the payer alike) and sent as INVOICE_PAID. Until then every read shows PROCESSING (a view, never stored: the row is `paid` with `ledger_state = pending`), with no payer named; `CancelInvoice` in that window is `invoice_processing`, not `invoice_not_open`. A refused or
parked leg reopens the invoice (`scope_invoices::fail_payment`: `open`, payment columns and key
cleared, version + 1, `pay_failure_reason`, a `PaymentFailed` fact in the event log) — the
issuer, never told of the payment, hears nothing. The stamps match the attempt's version, so a
replayed refused event never touches a later payment; the reaper replays a `pending` payment
whose event a relay dispatched without posting (`redrive_stale_payments`). Who paid, when and
from where is shown to the payer alone. An invoice restricted to another payer is the same NOT_FOUND as
an unknown id. Only the payment reaches the outbox; creation, cancellation and expiry write
their money event in their own transaction. **Expiry** is both: every read shows an open
invoice past `expires_at` as EXPIRED and a payment or cancel is refused under the row lock
(so the sweep's cadence never decides a late payment), and the reaper records it
(`scope_invoices::expire_due_invoices`), which is what puts INVOICE_EXPIRED in the issuer's
stream whether or not anyone reads the invoice again. `CancelInvoice` carries no key: it asks
for one end state, so a repeat returns the cancelled invoice unchanged. `pay_url` is
`@evinvest/wallet`'s pay page, `https://evinvest.ltd/wallet/pay?invoice=<id>`. The clock of
every expiry decision is the statement's (`clock_timestamp()`), not the transaction's start.

## Operations — the activity timeline (`OperationsService`)

The first surface here that is **only** a read model. A user's deposits and withdrawals
are separate aggregates, each already listing its own history. What neither can answer is
"what happened, in order": interleaving client-side lists needs a timestamp they all agree
on, and a per-source `LIMIT` cannot produce the newest N *overall* (a long deposit history
would push out a recent withdrawal). `ListOperations` is that join, done once in the hub as a single `UNION ALL`
over the projections the write side already maintains — so it introduces no new table, no
new event, and no new write path.

It owns nothing, so it gets a **plain driven port** ([`ports::operations`]) rather than a
`Repository`: there is no aggregate to hang the marker on, the same shape [`Deposits`]
takes. In Rust the row is a **sum type** (`Operation::{Deposit, Withdrawal}`) so a deposit
cannot carry a destination; on the wire it flattens to one message with
a `kind` discriminator, because the whole surface is projected to TypeScript through
OpenAPI and a flat shape survives that pipeline unambiguously. The service layer is where
that trade is paid, once.

Paging is a `limit` (default 100, capped at 200) with no cursor — the timeline is a
recent-activity surface, not an export. The adapter over-fetches one row to answer
`truncated` honestly rather than running a second `COUNT` over the same union. Ordering is
`created_at DESC, id`: on the `timestamptz`, not on truncated epoch seconds, so a
deposit and a withdrawal made in the same second keep the order they happened in.

## Write path (Write-Last, Read-First)

A command opens one Postgres tx (the **only** ACID point), mutates the aggregate under
a row lock, and drains its events to `event_log` + `outbox` in that tx. The
single-worker **relay** ([`infrastructure::relay`]) then drains the outbox in strict
`seq` order and issues the TigerBeetle transfer **after** the commit. Existence/balance
checks read TigerBeetle **first** (it's authoritative). Reservations are two-phase TB
pending transfers (`timeout = 0` — the saga owns the lifecycle, never TB's clock).

### Idempotency (at-least-once delivery)

- The stable **`event_id` UUID** (not the delivery cursor `seq`) is the key. A
  single-transfer event's TB id **is** the event id; a reservation's pending uses an
  **aggregate-derived** id (`tid(aggregate_id, <salt>)` — `CLEARING_RESERVE` for a withdrawal) so its settle/cancel can recompute the same
  `pending_id`.
- The gateway treats `Exists | AlreadyPosted | AlreadyVoided` as success; a post racing
  its pending is `Retryable` (can't happen under strict `seq`, but handled).
  `InsufficientFunds`/`Conflict` are **parked** into a distinct `outbox.parked_at`
  terminal state (NOT `dispatched_at`) so the row stays queryable yet is excluded from
  the drain (`WHERE dispatched_at IS NULL AND parked_at IS NULL`) — one bad event can't
  wedge the queue, and nothing is silently dropped. A park *after* an earlier leg of a
  multi-leg event posted is flagged half-applied (`compensated_at`); the
  [`reconciliation`](src/infrastructure/reconciliation.rs) job surfaces every parked row
  for intervention (TB-reversal of the applied legs is still a follow-up). Parked rows
  are operator-unparkable once the cause is fixed (`BalanceService.UnparkEvent`:
  `parked_at` cleared **and** `attempts` reset — a retry-exhausted row would otherwise
  re-park on first redelivery — then the relay is notified); a **compensated** row is
  refused, since its recovery event already applied and re-driving would double-apply.
- Deposits are idempotent by the `deposits.tx_ref` **gate** (`ON CONFLICT DO NOTHING` →
  emit the event only if newly inserted), so a re-record never double-credits.
- A withdrawal's clearing reservation gets a **withdrawal-derived** id
  (`uuid_v5(withdrawal_id, "withdraw:clearing")`) so settle/fail/cancel recompute the same
  `pending_id`; the settle disbursement + fee posts and the fail/cancel void completions use
  distinct salts; and `Custody::broadcast` must be idempotent by `withdrawal_id` so an
  at-least-once relay retry never double-sends.
- Amounts are **explicit** in every transfer (never TB balancing flags), so a retry
  moves the exact amount frozen into the event.

### Relay safety (single-worker guarantees)

- **Atomic settle.** A withdrawal settle is three legs — post the clearing pending, then
  `Dr clearing / Cr wallet:<net>` (the rail-liquidity backstop), then the fee. Applied
  naïvely these can half-commit: if the rail can't cover the net *after* the pending is
  posted, the user is debited the gross with funds stranded in clearing. Because the relay
  is **single-worker and sequential**, the disburse op is **Read-First rail-checked before
  any leg is applied**: a short rail parks the whole settle atomically (nothing applied),
  recoverable by a rail top-up + reconciliation. (Concurrent withdrawals on one rail are
  the realistic trigger — each is dispatched against the same liquidity, since reserves go
  to `clearing`, not the rail.) The guard is **idempotent under redelivery**: legs apply as
  separate TB calls, so a transient failure after the disburse (the fee leg, or its own
  saga-step insert) redelivers an event whose re-read rail balance already reflects the
  disburse's own outflow — re-checking it would double-count and spuriously park a
  half-applied settle. The pre-check therefore asks TB (`Ledger::transfer_exists`, the
  authority — the saga step may have failed to record) whether the guarded leg's
  deterministic id already applied, and skips its guard if so; the re-apply is `Exists`.
- **No shutdown cascade on a DB blip.** The composition root cancels every sibling task when
  any branch returns, so the relay must not exit on a transient DB failure. `Relay::run`
  re-acquires the outbox advisory lock with capped backoff when the lock connection drops (or
  initial acquisition errors) instead of returning — a Postgres hiccup pauses the drain, it
  doesn't tear down the money plane. Cancellation is still observed at every wait point.
- **Bounded retryable.** Infra outages (`Unavailable`) retry **unbounded**; a *retryable
  ledger state* (`PendingTransferNotFound`) retries **bounded** (`MAX_RETRYABLE_ATTEMPTS`)
  then parks — so a completion whose pending can never appear (its reserve was itself
  parked) cannot wedge the single, globally-ordered queue forever.
- **Parked opening reserve.** If a withdrawal's *first* op — the `clearing` reserve —
  parks (e.g. two concurrent same-user requests both pass the optimistic Read-First, but
  the second violates the user-claim non-negative flag), the control-plane row stays
  `Queued` with nothing reserved. Its later cancel/settle then parks (bounded, no wedge),
  and the [`reconciliation`](src/infrastructure/reconciliation.rs) clearing check (ledger
  reserve vs the gross of in-flight withdrawals) catches the mismatch; the
  [`reaper`](src/infrastructure/reaper.rs) auto-cancels the abandoned `queued` row. A real
  custody broadcast of such a withdrawal is **refused**: the `Dispatched` event's broadcast
  op is the one op with no TB leg whose flags could reject it (it is an external side
  effect), so the relay guards it with a Read-First on the reserve having actually applied —
  its `CLEARING_RESERVE` transfer id recorded in `saga_steps` by the strictly-earlier
  Requested leg. No step ⇒ the reserve parked ⇒ the broadcast is parked, never sent. This
  closes the over-withdrawal race where a double-submit (TB lags the committed-but-undrained
  reserve, so the second request passes the optimistic Read-First) would otherwise broadcast
  real money with nothing locked. The `saga_steps` insert is therefore load-bearing, not
  best-effort: a failed insert retries the whole (idempotent) event. Beside it sits the
  **broadcast-state guard**: the withdrawal row must still be `processing`, or the
  `Dispatched` event is parked (never sent). This makes unparking a `Dispatched` event
  *after* the withdrawal was failed/cancelled — a broadcast against a voided reservation,
  the unpark-after-fail double-pay hazard — structurally impossible rather than a runbook
  discipline.
- **On-chain treasury Read-First (custody).** The dispatch gate already min-s the TB
  accounting balance with the adapter's on-chain treasury read (`treasury_liquidity`,
  USDT-only), so an underfunded rail normally queues instead of ever reaching custody. But
  the gate is check-then-act — the on-chain balance can still drop between the dispatch-time
  read and the broadcast (a parallel withdrawal, an out-of-band spend, a gas-only
  shortfall). So each custody adapter **also** Read-Firsts the **real** treasury balance
  (USDT to send + native gas) before it allocates a nonce/seqno or signs — the last-line
  backstop behind the gate; a shortfall **parks** (`Rejected`) rather than retrying, so an
  underfunded rail can't wedge the single-worker drain. That residual park is rare,
  operator-visible (reconciliation), and recovered via
  [`docs/RUNBOOK-withdrawals.md`](../../docs/RUNBOOK-withdrawals.md). A **gas-price spike**
  is the one market condition checked *before* that read: an EVM rail compares the node's
  `eth_gasPrice` quote against the hub's own ceiling (`BSC_MAX_GAS_PRICE_GWEI` /
  `POLYGON_MAX_GAS_PRICE_GWEI`, defaulting to the signer's fee-budget values) and, above it,
  reports `Unavailable` — retried from the same `seq` on the relay's next pass, so the
  withdrawal resumes by itself when the spike passes. Only a quote under the hub's ceiling
  reaches the signer; its own `PermissionDenied` is a policy verdict and still parks, which
  is why the hub's ceiling must stay ≤ the signer's. On BSC and TON a node
  rejection of a *first-ever* send additionally frees its stored nonce/seqno (`discard_tx`)
  so the sequence never gaps at a slot nothing will fill.
- **Provable death before re-sign (TRON/TON).** A nonce-free rail (TRON, TON) can only
  re-sign a stuck send once it is *provably* dead, never merely past its local-clock
  expiration: TRON waits until the **solidified** head's timestamp is past the tx expiration
  (+ margin) with no receipt; TON re-signs at the **same seqno** with a fresh window (only one
  message per seqno can ever be accepted, and the replaced one is expired) — which also
  unfreezes the strictly-sequential seqno pipeline when a send expired before its turn. A
  wall-clock-only re-sign would double-pay a tx that later lands.
- **Settlement proof, not seqno advance (TON).** A treasury seqno advance only proves the
  wallet processed *an* external message — a bounced jetton transfer advances it too, with the
  USDT returned. The TON watcher therefore settles only on a matching non-aborted **outgoing**
  jetton transfer from the indexer (the mirror of the deposit path), recording that transfer's
  real tx hash; a seqno advance with no such transfer leaves the withdrawal `processing`
  (reserve held, operator/reaper-recoverable) rather than settling a phantom disbursement.
- **Cross-flow claim contention (shared per-claim lock).** Every flow that spends a claim
  — a withdrawal, a payment, a compartment move, an invoice payment, a scope's debit —
  writes its own table, so a per-table `FOR UPDATE` does not serialize one against another.
  Each takes one **shared** lock first: `pg_advisory_xact_lock` keyed on the claim
  ([`outbox::lock_claim`], `lock_user` its named case), held to commit. The user claim's key
  is **frozen** at what every released binary computes — the raw user id — so a rolling
  deploy cannot leave old and new writers serializing on different targets;
  `the_generalized_claim_lock_keeps_the_keys_the_old_helpers_computed` pins it. Under the
  lock the spend's last check is `spendable_locked`, net of every debit recorded and not yet
  applied ([§ Compartments](#compartments-by-scope-domainscopes-compartmentservice)), and a
  raced over-commit that still reaches the ledger parks on TB's non-negative flag.

## Authorization (defense in depth)

Boundary (gRPC) does the cheap stateless check; the stateful rule lives in the
aggregate, applied under the row lock; the TB non-negative flag is the ledger backstop.

| RPC | Who | Boundary | In-tx invariant |
| --- | --- | --- | --- |
| `GetTreasury` / `RecordDeposit` | operator | `require_permission` | chain-proven arrival (amount + party read off the chain) ∧ `tx_ref` gate |
| `GetWallet` / `ListWithdrawals` | the user | `sub == user` | — (`GetWallet` serves an address only at `kyc_level ≥ 1`) |
| `GetDepositAddress` | the user | `sub == user` | `kyc_level ≥ 1` (else `permission_denied`) |
| `RequestWithdrawal` | the user | `sub == user`, `is_access`, **not revoked, not paused, not frozen** | owner not frozen (`frozen ∨ disabled`, via `OutflowPolicy::standing`) ∧ `kyc_level ≥ 1` ∧ available claim ≥ gross (TB flag backstop) |
| `CancelWithdrawal` | the user | `sub == user`, `is_access` | owns it ∧ state is `queued` (idempotent) |
| `DispatchWithdrawal` | operator (treasury) | `require_permission` | state is `queued` (idempotent) ∧ **not read-only** ∧ (user source) owner not frozen ∧ `kyc_level ≥ 1` — fail-closed, no `force` |
| `SettleWithdrawal` / `FailWithdrawal` | operator | `require_permission` | state is `processing` (idempotent) |
| `OpenPayment` / `ListPayments` / `GetPayment` / `CancelPayment` | operator (`bank:payment:open`) | `require_permission` | (open) source not the treasury (D24) ∧ executable now — the source covers it, an external shape is possible on the rail; seated on the source's one requirement (`PaymentTerms::requirement`) |
| `ListPaymentConfirmations` / `GetPaymentConfirmation` | the user | `sub == subject` | seated on the caller (else `NOT_FOUND`) |
| `AnswerPaymentConfirmation` | the user | `sub == subject`; approve: **not revoked, not paused, not frozen**; decline: ungated | pending ∧ seated on the caller ∧ the pins unmoved |
| `GetUserBalance` | operator | `require_permission`; resolves the CONCIERGE id first via the bridge mirror (`users.concierge_user_id`), then the banking id; unknown ⇒ `NOT_FOUND` | — |
| `ListParkedEvents` | operator | `require_permission` | — |
| `UnparkEvent` | admin (`OutboxManage`) | `require_permission` | parked ∧ not dispatched ∧ **not compensated** (the double-apply guard) |
| `ScopeLedgerService.OpenAccount` | a service client | own listener: mTLS client certificate → `ClientId`, `typ=service` token whose `sub` is that client | grant in the scope (not `main`) ∧ label under the client's prefix ∧ not `fee`/`treasury` ∧ not another client's; idempotent by `(client, scope, rpc, key)` |
| `ScopeLedgerService.GetBalances` | a service client | as above | grant in the scope; only accounts the client owns |
| `ScopeLedgerService.Transfer` | a service client | as above | grant ∧ a current grant covers the source's label ∧ owns the source ∧ source not `fee`/`treasury` ∧ destination `fee` ∧ scope not frozen ∧ automatic ceilings per `(client, scope)` ∧ spendable ≥ amount (TB flag backstop); idempotent by `(client, scope, rpc, key)` |

The `kyc_level ≥ 1` arms above are the **enforced** position of the deployment switch
(`KYC_GATE_ENABLED`, enforced unless explicitly lifted — see **The switch** under the
verification gate below); every other arm in this matrix is unconditional.

`require_permission` (`services::support`) is `is_access` + a `bank:*` permission
(`concierge_domain::authz::bank`) looked up in the caller's bridge-mirrored
`users.permissions` — concierge decides what a seat holds — **after** the account gates, in
the money-path gate's order: a stale `token_version` is `unauthenticated` first, then a
`disabled` (or frozen) operator is `permission_denied`. The mirrored column is the **only**
source — there is no environment-driven override, so a caller with no local row, or one
concierge has not told us about yet (NULL), holds nothing and a non-UUID subject is refused
outright.

**Money-path gate** (`services::support::unfrozen_caller`): the value-leaving RPCs above
(`RequestWithdrawal`, an approving `AnswerPaymentConfirmation`; `MoveFunds`, `PayInvoice` and
the mandate RPCs take their own `MoneyGate` through `gated_caller`) run one `IssuanceTarget` resolve and one
kill-switch read per call and refuse, in this order: `unauthenticated` when the token's
`token_version` is below the folded revoke floor (the same floor `require_permission` holds
operators to — answered first, so a revoked caller learns nothing about the platform's
state); `failed_precondition` when outflows are paused; `failed_precondition` when the
caller's banking row is `frozen` or `disabled`, or has no row at all (fail-closed). The
decision is the pure `money_caller_gate`, unit-tested for the ordering. `frozen` is set
by the one-way concierge→banking lifecycle bridge consumer (`infrastructure::bridge`),
which PULLS `UserLifecycleEvent`s from the
concierge plane (`UserEvents.PullUserLifecycle`, `BRIDGE_SERVICE_TOKEN`) and mirrors
SUSPENDED→frozen / REINSTATED→unfrozen, KYC, and the revoke floor onto `users`, keyed by
`auth_subject` and dedup/ordered by per-user `sequence`. Identity stays owned by concierge;
banking only mirrors the gating slice. The gate fails CLOSED (UNAVAILABLE) if the flag can't
be read. Cancel/read RPCs are intentionally NOT gated, so a frozen user can still cancel a
queued withdrawal or decline a payment.

**Nothing is consumed without being applied.** `bridge_cursor` is a single global position
and the concierge only re-delivers *ahead* of it (`WHERE position > after_position`), so an
event the consumer walks past is gone for good — there is no dead-letter to recover it from.
Two things can stop an event from applying, and they get opposite treatments because they
are unblocked by opposite things. **A subject with no local row** (never signed in here, or
a CREATED that aged out of the outbox before banking was deployed) is parked in
`bridge_deferred_event` and replayed by the consumer's own sweep the moment the row appears,
from either direction — a later CREATED or a first sign-in. The cursor moves on, so one
orphan subject cannot wedge the mirror for every other user; the price is a table whose
columns must carry every field `apply` reads. **A `kind` this build cannot name** is
different: the concierge ships ahead of banking, so an unknown kind is the ordinary shape of
a mid-rollout event, and *nothing local can ever interpret it* — only a newer binary can.
The cursor stops on it (head-of-line, and the rest of the batch is left unapplied so a later
event can't advance that subject's guard past it), leaving the event in the concierge outbox
at full fidelity until the upgrade lands. Marking it applied — which is what bumping
`last_lifecycle_sequence` "so it isn't re-fetched forever" did — is how a freeze or a tier
revocation gets swallowed while the money plane keeps trading under withdrawn rules.

**What the bridge trusts about its peer.** The channel is built once at boot from
`CONCIERGE_BRIDGE_ADDR` (`infrastructure::bridge::endpoint`), and the lifecycle consumer
the scope stream, the approvals and the scope notices share it — one address, one trust relationship, one place it
is configured. The only credential on the wire is the outbound `BRIDGE_SERVICE_TOKEN`
bearer, and it points the wrong way for this question: it proves banking to concierge,
never concierge to banking. The transport is the whole of the reverse proof. In production
the hub therefore **refuses to boot** (`config::ensure_bridge_is_authenticated`) unless the
address is `https://` **and** `BRIDGE_TLS_CA_PEM_FILE` pins concierge's CA — or the peer is
loopback, the single-host exception the signer seam also makes. Cleartext to a cluster name
is refused because anything that can answer to that name could hand the mirror a freeze, a
revoke floor or a `KYC_CHANGED`; `https://` without a pin is refused because the public
roots vouch for no cluster-internal name, so an unpinned root store is not proof of which
concierge answered. The pin is explicit: the server's certificate must carry the host of
the address (`concierge`, on the TLS listener at `:55672`), never a name service discovery
happened to resolve. `BRIDGE_TLS_CLIENT_CERT_PEM_FILE` + `…KEY…` add the hub's identity,
so the seam becomes mTLS the moment concierge's listener asks for a client certificate,
with no code change here. Outside production the seam is whatever the address says: dev
dials loopback in cleartext. Phase 1 of #199 only WARNed here, because production ran h2c
on `:55670`; phase 2 moved the deploy contract in `flake.nix` to the TLS listener and the
pinned CA in the same release as the refusal — a rollout that ships this hub before the
listener, the Secret and the Service exist gets a hub that will not start, by design, and
that is the correct outcome for a mirror that would otherwise trust a name. What this does
not close is the database path (`pg_hba` `trust` for the cluster subnet, rpi5.nix#18):
a pod that can `INSERT` into `concierge.user_outbox` never touches this channel.

**Verification gate (`kyc_level`).** The mirrored tier is not just stored, it *gates* —
and what it gates is **provisioning and payout, not the arrival of money**.
`domain::users::KYC_LEVEL_VERIFIED` (= 1) is the floor for three calls and three only:
`GetDepositAddress`, which provisions the address; `RequestWithdrawal`, which accepts the
payout; and `dispatch_withdrawal`, which checks again at **dispatch**, since acceptance and
payout can be hours apart and a `KYC_CHANGED{kyc_level: 0}` sets no freeze for the freeze
gate to catch. The ladder is written down on `banking.v1.UserProfile.kyc_level`: **0**
registered (confirmed email, nothing verified), **1** verified (document + liveness + face
match + a passed sanctions/PEP screen), **2** enhanced (proof of address + source of funds,
raised limits), **3** elevated (EDD, human-set only). Concierge owns the value and an admin
sets it there; banking has no transition that writes it, and the `users` UPDATE
deliberately omits the column so a profile save can never race a `KYC_CHANGED` back to an
older tier.

**Crediting is not gated, and that is deliberate (#179).** Once an address has been
provisioned it stays live, and the rail watcher keeps crediting whatever arrives on it
whatever the owner's tier is today — a user verified in the past and revoked since keeps a
working deposit address. Nothing leaks: the same revocation closes `RequestWithdrawal` and
`dispatch_withdrawal`, so an incoming transfer lands on a claim its owner cannot move and
sits there until the tier is restored or an operator settles it by hand. The invariant is
therefore a gate on **provisioning**, not a claim that an unverified user cannot be sent
money: a transfer to an address that already exists on chain is not ours to refuse, and the
alternative — holding the credit off the ledger — is custody the books do not show. Two
consequences that are easy to get wrong:

- **Copy must not promise what the gate does not do.** The cabinet may say that no deposit
  address will be issued and that withdrawal is closed; it must never say "you cannot
  receive funds" or "incoming transfers are blocked until you verify".
- **A revoked tier is not a quarantine.** Should "credited to a user whose verification was
  withdrawn" ever need its own handling — a quarantine claim and a manual review path —
  that is new work, not something the existing gate is quietly assumed to be doing.

Both gates sit **above** the [`DepositAddresses`] port for the same reason the rail gate
does: the first `address` call provisions a signer keypair, and a key minted for an
unverified user is an address the fund must watch, sweep and account for forever. The two
refusals stay distinguishable at the wire — an unconfigured rail is `Ok(None)` ("this rail
cannot fund you"), an unverified caller is `Forbidden`/`permission_denied` ("finish
verification") — because the cabinet has to pick a different screen for each. `GetWallet`
stays fully readable at tier 0 (a user's own balance is never hidden from them) but serves
no address on any rail. A **payment is gated on its source**, never its destination or its
initiator: a tier-0 investor may be paid, and the floor applies when they try to take the
money out, not when it arrives (`tests/kyc_gating.rs`).

**The switch (`KYC_GATE_ENABLED`).** The verification floor is the one of the two deposit
gates that can be turned off — it guards a rule the platform chose, where the rail gate
guards a fact about the chain — so it is a deployment switch, `config::KycGate`, read from
the environment **once at boot** and carried on `AppState` rather than re-read per call.

| `KYC_GATE_ENABLED` | gate | effect |
| --- | --- | --- |
| unset / empty / `true` / `1` | **enforced** (the default, and what production runs) | tier 0 gets no deposit address and cannot withdraw |
| `false` / `0` | lifted | every tier is issued an address, admitted and paid out — the pre-switch behaviour |
| anything else | **enforced**, with a WARN naming the value | a typo never opens the gate |

Every uncertain reading resolves to *enforced*: this decides whether unverified money
crosses the platform boundary, so it opens only on an explicit, unambiguous word. That is
also why it is not `bool_env`, which reads anything it does not recognise as `false` — the
right default for an opt-in sweep, the wrong one here. An unparseable value warns rather
than refusing the boot, because the reading that cannot lose money is already available
without stopping the hub. Boot logs the position either way (`warn!` when lifted), so a gate
that is off is visible in a pod's first lines and not only in someone's memory.

The same value **must** reach all three gates — address, admission, dispatch. Lifting it at
admission only would accept an unverified user's withdrawal and then leave it queued
forever, since `dispatch_withdrawal` re-checks the tier when the money actually leaves;
that is worse than having no switch at all, and
`tests/kyc_gating.rs::a_lifted_gate_both_admits_and_dispatches_an_unverified_withdrawal`
exercises one withdrawal across both points so the two cannot be wired apart again.

## Reconciliation + reaper + dispatcher (recovery jobs)

TB always wins; the jobs run as `join!` branches of the composition root next to the
relay, on the relay's dedicated pool.

[`reconciliation`](src/infrastructure/reconciliation.rs) (`Reconciliation::scan`) asserts
and **alerts** (Sentry-shipped `error!`, no auto-write); the full list is its module header:

1. the **global** posted `sum(custody) == sum(claims)` on the USDT ledger, read straight
   from TB via `Ledger::cash_invariant` with the claims broken down by whose they are
   (`CashSide`: `Σ user + Σ scope_claim + Σ scope_account + clearing + retired == Σ
   custody`). `claims` is the whole credit side, so the check cannot be fooled by an account
   the breakdown does not know: what the named parts leave over
   (`CashInvariant::unclassified`) is its own alert. A retired code
   (`AccountCode::RETIRED`) counts as `CashSide::RetiredClaims` (`ports/ledger.rs`), never
   unclassified, and any value on one is a finding;
2. `clearing`'s reserved (pending + posted) balance vs the gross of every
   `queued`/`processing` withdrawal in Postgres;
3. a scan of every `outbox.parked_at` row (with its `last_error` and `compensated_at`).
   The `last_error` column on a parked row is the first place to look when money didn't
   move;
4. **banking's treasury debits vs what the owners approved** (D17): `Ledger::total_debits`
   of `scope_account:main:treasury` (posted + pending) against Σ internal treasury orders
   `approved`/`executed` plus Σ treasury withdrawals `queued`/`processing`/`completed`;
   re-read once before it is alerted. A debit no approval accounts for is the alert;
5. – 8. the scope stream's freshness and the scope ledger's checks (§ Scope ledger).

`ReconReport::clean` is (1) balanced, fully classified and nothing retired, (2) equal, (3)
no parked rows, (4) equal, plus the scope-account owners and `hold_clearing`.

[`treasury_drift`](src/infrastructure/treasury_drift.rs) (`TreasuryDrift::scan_once`) is the
**per-rail** counterpart, hourly and alert-only. The invariant above relates two TigerBeetle
accounts, so it stays green when the ledger and the CHAIN disagree; this compares
`wallet:<net>` against the wallets it claims to describe — `treasury_liquidity` **plus**
`deposit_address_liquidity`, because the ledger counts un-swept deposit-address funds too. A
surplus means USDT arrived without a ledger fact (it is also unspendable, since the dispatch
gate mins the two); a shortfall means claims aren't backed on-chain. A divergence must survive
two consecutive scans to be reported, so a transfer landing between the two reads is not an
alert. Rails with no chain view are skipped.

The same job watches **native-coin gas**, a separate failure — a rail can hold exactly the
USDT it should and still be unable to move any of it. `Custody::treasury_gas_runway` reports
how many withdrawals the treasury can still pay for (an EVM rail prices `gas_limit × gas_price`
live; TON divides by its fixed `msg_value`), so one threshold means the same thing on every
rail and `0` is precisely where `ensure_treasury_funded` starts parking. Exhausted is an
`error!`, thin-but-working a `warn!`. Nothing else looked at the native balance before this:
the first symptom of an empty treasury was a user's withdrawal parking.

Out-of-band arrivals on a rail's **treasury** are **not credited**. Each deposit
watcher also watches its rail's treasury hot wallet, and a USDT transfer that lands there
from outside every wallet we control (the sweep from a derived address is money already in
`wallet:<net>`, so it is excluded — `recipient_of` in `deposit_watcher`,
`report_treasury_arrival` in `ton_deposit_watcher`) is reported, not booked: a claim needs
a holder and the chain cannot say whose it is, so the watcher fires a Sentry-shipped `error!` with
rail, hash, sender and amount and bumps the process-lifetime counter
`unattributed_treasury_inflows` (`telemetry::note_unattributed_treasury_inflow`); the EVM
scan advances its cursor like a credit (re-reading the chunk forever would only repeat the
report), the TON scan dedupes by hash across its lookback. The `treasury_drift` watch
reports the same dollar as a **surplus** on every scan until it is attributed, so the
counter is the number to alert on. `record_deposit` / `Deposits::record` take a `UserId`
(`application/balance.rs`, `ports/deposits.rs`), so a deposit can only land on a person.

`RecordDeposit` (admin, `CapitalManage`) is the manual path for a **user's** arrival the
watchers could not see: the reference is read back from the chain and the amount and the
credited party are taken from what is there (`record_verified_arrival`); a transfer that
landed on the treasury is refused, since the chain cannot say whose deposit it is. It never
accepts an amount from the caller as a credit.

The EVM deposit scan ([`deposit_watcher`](src/infrastructure/deposit_watcher.rs)) degrades
rather than wedges when its `eth_getLogs` endpoint refuses it. A window refused for **width**
is halved down to a floor (the cap is static, so the window never widens again). A window
refused because the provider has **pruned** that history (`-32701`, "pruned" — shared nodes
keep a bounded suffix of the chain) can never succeed at any width, so the live scan bisects
for the oldest block still served, jumps the cursor **past** it, and files the skipped range
as a Sentry-shipped `error!` with its `from`/`to` — those deposits are **not credited** and
the operator reconciles the window by hand with `RecordDeposit`. Retrying the pruned window
instead (the pre-#309 behaviour: cycle backoff to 300 s, forever) loses every deposit after
the gap as well. "Past", not "to": the boundary is an edge moving with the chain, and a scan
resumed exactly on it is refused again before its first chunk lands — v0.16.0 livelocked on
BSC that way, clamping 10–40 blocks every cycle with the lag frozen at the provider's window.
The jump adds a slack (`PRUNE_SLACK_BLOCKS`, 256 blocks, doubled per further clamp in one
scan) that the incident's range includes. The skip is persisted only on a boundary the provider **confirmed**: the
refused block is re-probed (a refusal that does not repeat is one backend's opinion), the
head is probed rather than assumed, and the block under the boundary is probed once more —
a provider refusing everything (index off) or a keyed pool answering the same block both
ways yields no boundary, and the cycle holds its cursor and backs off as before. A backfill
(`CursorPolicy::Leave`) never clamps — the operator chose that window, so it gets the
refusal back.

[`reaper`](src/infrastructure/reaper.rs) (`Reaper::sweep`) owns the timeout for abandoned
sagas (TB pendings are `timeout = 0`, so nothing auto-voids). Split by safety per the
cardinal withdrawal rule: a **`processing` withdrawal** past the max age is **alert-only**
(its broadcast may have landed — voiding would double-pay; only a confirmed not-broadcast
signal may fail it); a **`queued` withdrawal** is **auto-cancelled** (never broadcast →
safe full refund). Max age is 24h (config seam: `Reaper::with_max_age`).

[`dispatcher`](src/infrastructure/dispatcher.rs) (`Dispatcher::sweep`, every 30s) is the
treasury worker: it re-checks every `queued` withdrawal against **both** liquidity gates —
the TB rail balance and `Custody::treasury_liquidity` — and dispatches the covered ones
(idempotently, via the same row-locked command as the admin RPC, which is also where the
outflow policy is enforced), so a rail top-up self-heals the queue within one interval.
The sweep keeps exactly one policy check of its own — a whole-cycle short-circuit on the
kill-switch, through that same gate function — so a paused platform costs one read per
interval rather than one refusal per queued row. A treasury read `Err` skips that cycle (the
automatic path stays conservative; the operator RPC may still exercise judgment). Together
with the reaper this brackets accept-and-queue: dispatched within ~30s of a top-up, or
auto-cancelled (refunded) at 24h — the de-facto rail top-up SLA.

## Tests

### Bring-up

Every suite under `piggybank/core/tests` runs against a **real** Postgres and TigerBeetle
through `tests/common/mod.rs` — no suite opens its own connection. `DATABASE_URL` names the
*base* database; the harness never migrates it. On a binary's first `pool()` it takes a
session advisory lock on the base, ensures `<base>_template` exists and carries exactly this
build's migrations, then `DROP … IF EXISTS` / `CREATE DATABASE <base>_<binary> TEMPLATE
<base>_template` — one database per test binary (`banking_wallet_withdrawals`,
`banking_relay_shutdown`, …), so the outbox relay's per-database advisory lock is never
shared between binaries and a parallel runner (nextest, two `cargo test --test …` side by
side) cannot race for it. `pool()` hands each test its own pool on that clone;
`database_url()` returns the clone's URL for the two suites that size their own pool;
`seeded_ledger()` connects to `TIGERBEETLE_ADDRESS` / `TIGERBEETLE_CLUSTER_ID` (default
`127.0.0.1:3033`, cluster `0`) and seeds the singleton accounts (`ledger::seed_singletons`:
the per-rail wallets, `clearing`, `bank`). Clones are not dropped at
exit (a test binary has no end-of-process hook); the next run of the same binary replaces
its clone. Locally:

```bash
nix run .#db          # the shared postgres cluster; ensures the `banking` database
nix run .#tb          # a single-replica ledger on :3033
DATABASE_URL=postgres://postgres@localhost:5432/banking cargo test -p piggybank-core --tests
```

A template migrated by another build — a migration file edited on this branch
(`VersionMismatch`) or a version this build does not know after switching branches
(`VersionMissing`) — is recreated from scratch with a notice on stderr (`--nocapture` to
see it), instead of failing every suite with `migration N was previously applied but has
been modified`. The base database itself is never touched, so it can no longer go stale
through the tests.

A missing service is a **skip locally and a failure under CI**. Without `DATABASE_URL`
(or with a replica that does not answer) a suite prints a notice and returns early — the
counters still read `N passed`, so the signature of a run that executed nothing is
`finished in 0.00s`, not a zero. Under `CI=true` — GitHub Actions exports it, and
`nix run .#check-rust` exports it too — the same condition panics with
`integration services required in CI`, so a green tick means the money plane was actually
exercised (#259). `check-rust` brings up its own throwaway Postgres (`:54329`) and
TigerBeetle (`:3039`) from the flake apps, runs the workspace against them and tears them
down on exit; move the ports with `CHECK_POSTGRES_PORT` / `CHECK_TIGERBEETLE_PORT`.

Your own base database, when several checkouts share the cluster:
`PGDATABASES="banking_<name>" nix run .#db` creates it; point `DATABASE_URL` at it and the
template and clones follow as `banking_<name>_template`, `banking_<name>_<binary>`. The base
name must be plain `[a-z0-9_]` — it is spliced into DDL. A second ledger next to the default
one: `TIGERBEETLE_PORT=3034 TBCLUSTER=1 nix run .#tb`, then
`TIGERBEETLE_ADDRESS=127.0.0.1:3034 TIGERBEETLE_CLUSTER_ID=1`.

`domain` unit tests cover the money math, the chart of accounts (every key round-trips, every
live and retired code is unique) and the withdrawal and payment transitions.
`piggybank/core/tests/wallet_withdrawals.rs` hits **real** Postgres + TigerBeetle (the
withdrawal reserve→settle with fee, fail→refund, short-rail queue→dispatch→settle, queued
cancel→refund; the on-chain dispatch gate's three arms — short treasury queues despite a
liquid TB rail, liquid treasury dispatches, read failure degrades to queued — plus the
refused admin dispatch and the `Dispatcher::sweep` both-gates flow, driven by a test
`Custody` adapter with a configurable treasury view; plus the dispatch-time outflow
policy — a tier revoked after acceptance stops the sweep, the admin dispatch is refused
under read-only, under a freeze and at tier 0, and a withdrawal whose owner row is gone
fails closed). `piggybank/core/tests/kyc_gating.rs` covers the verification floor against
real Postgres + TigerBeetle: tier 0 is refused a deposit address **without the address
gateway being reached at all** (a call counter, since a gate placed after the port would
look identical from the return value while having already minted the key), tier 0 cannot
withdraw *with a funded claim* (so the refusal is the gate, not insolvency), tier 1 does
both, the unconfigured-rail `None` stays distinguishable from the unverified `Forbidden`,
and a payment to a tier-0 investor is gated on its source, not on them.
`piggybank/core/tests/treasury.rs` (`held_by_users` a sum of user claims, not a remainder;
banking's treasury on a line of its own) and `piggybank/core/tests/retired_claims.rs` (value
on a retired code fails reconciliation and is never unclassified) hit real Postgres +
TigerBeetle too.
`piggybank/core/tests/relay_recovery.rs`
proves a parked event lands in the distinct `parked_at` state (never marked dispatched),
stays queryable, and is surfaced by `Reconciliation::scan`; that `Reaper::sweep` alerts on
a stuck `processing` withdrawal (never auto-voids it) while auto-cancelling an abandoned
`queued` one; and that an unparked `Dispatched` event for a failed withdrawal is re-parked
by the broadcast-state guard, never sent. They skip when `DATABASE_URL` is unset or TB is
unreachable. Drive the relay deterministically with `Relay::drain`, and the recovery jobs
with `Reconciliation::scan` / `Reaper::sweep` / `Dispatcher::sweep`.
