# Money plane — balance, fund shares & withdrawals

How the fund ("piggybank") stores and accounts for its money. Cross-cutting context
is in [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md); this file is the per-area
reference. **Working with money — read this before touching the ledger.**

## Two stores, never joined in one transaction

- **TigerBeetle = data plane** — authoritative amounts (balances, transfers). Never
  re-bookkept in Postgres.
- **Postgres = control plane** — ids, state, the event log + outbox, projections, and
  the `tb_accounts` UUID→`u128` account-id map. Holds **zero** amounts as numbers it
  reasons about (allocation/deposit amounts are stored as exact base-unit TEXT, never
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
| **Claims** (equity/liab) | `user:<uuid>` (20), `scope_claim:<scope>:<uuid>` (21 — a user's compartment in a scope), `service:<id>` (30 — a product's, or the reserved `service:fee` / `service:fund`), `scope_account:<scope>:<label>` (31 — a scope's own account, banking's treasury `scope_account:main:treasury` included), `clearing` (50), `book_cash:<uuid>` (65) | credit (`credits − debits`) | `DebitsMustNotExceedCredits` | **network-agnostic** |
| **Retired claims** (#245) | `fund` (1), `fee` (40) | credit | as above | history: the rows stay in `tb_accounts` at zero, no live key names them (the cash scan still counts them by code); the codes are never reused |

A deposit is one balanced transfer **`Dr wallet:<net> / Cr <claim>`** (textbook Dr Cash
/ Cr customer-deposit) — there is no "external world" account. The flags are set **once
at account create** (immutable in TB) and are the last-line backstop against an
over-spent claim or negative custody.

**Every claim has a holder (#245).** There is no platform-owned money: a claim is a
person's (`user:<uuid>`), or an allocation's (`service:<id>`) whose units people hold. The
platform's own capital is the reserved **`fund`** allocation and its earnings the reserved
**`fee`** allocation (`ServiceId::fee()` / `ServiceId::fund()`, `domain/src/balance.rs`;
registry rows from migration `0045`, `hidden`, `open`, `cash`) — see
[Reserved allocations](#reserved-allocations--fee-and-fund-245). The old singletons
— the `piggybank` / `revenue` parties, the `fund` / `fee` claims, the company
holder and its stake account, the seed and stake-hand-over events, the revenue payout —
were removed with the contract step (C-9, migration `0047`) after the one-off data
migration emptied them in production (2026-09-27). What stays is history: the TigerBeetle
accounts (codes 1, 40, 63, reserved forever, `AccountCode::is_retired`) that scans step
over by code; the seed's `deposits` rows and the company stake's two `unit_issuances`
rows, left in place under `NOT VALID` CHECKs and read at the persistence boundary as
history (`ports::issuance::StoredIssuance::RetiredCompany`); and the transfer codes 1, 52,
54 (`TransferCode::RETIRED`). On the wire and in every column a `Party` is
`user | service` only.

A **third ledger** holds the **service currency** — fund units (`Ledger::Share`, `= 3`),
see [Fund shares](#fund-shares--the-service-currency). It is independent: a unit transfer
can never touch a cash account, so the two planes can't imbalance each other.

**Two layers, network only at the edges (load-bearing).** USDT is one fungible pool, so a
user or an allocation has **one** claim, not one per chain — network lives **only** in the
treasury (`wallet:<net>`) and on deposit/withdrawal *transactions*. The invariant is
therefore **global**: `sum(custody) == sum(claims)` (a deposit grows both sides;
allocate/revoke are claim→claim, net-zero on each sum; a withdrawal drops both by `net`).
Per-rail backing is a **treasury** concern, not a ledger one — a withdrawal on a rail short
of liquidity is *accepted and queued* (see below), never refused. `clearing`
(`WithdrawalClearing`) holds a queued/in-flight withdrawal's reserved gross, decoupled from
any rail so acceptance never depends on rail liquidity. *(This supersedes the original
per-network `sum(custody:N)==sum(claims:N)` model: claims were unified once a single
fungible balance — not three — became the product requirement.)*

## Allocations — the registry of investable products (`domain::allocations`, `AllocationsService`)

An **allocation** is the control-plane record that makes a `ServiceId` investable. It is
the *only* way a fund comes into existence.

**The hole it closed.** `Subscribe` used to validate only the slug's *shape*
(1..64 `[A-Za-z0-9_-]`), and `dealing_nav` bootstraps a service with no valuation at the
**seed NAV 1.0** — so any investor could mint a fund by typing an unregistered id, moving
real money into a `service:<typo>` claim with no operator behind it. `PostFundValuation`
was the second such door. Both now resolve the registry first.

**States.** `draft` (registered, accepts no money, hidden from the default catalog) ->
`open` (subscribe + redeem) -> `closed` (**redeem only**). `closed -> open` re-opens;
`draft` is entered only by registration, so a product that has taken money can never
travel back to "never opened". Transitions are idempotent — re-opening an open allocation
raises no event.

**The asymmetric gate is the point.** `require_subscribable` admits only `open`;
`require_redeemable` admits `open` *and* `closed`. Winding a product down must never trap
an investor's units inside it, so only the *new money* direction is ever blocked.

**Access — closed by default (migration `0033`).** A second axis, orthogonal to the
lifecycle: `state` says whether the product deals, `access` says *with whom*. Each
allocation carries a default level `hidden < view < invest` (`AllocationAccess`, ranked —
the derived `Ord` is the policy), and `allocation_access_grants` raises individual
investors above it (`view` | `invest`, never `hidden`: a grant only adds). The level an
investor effectively holds is `max(default, grant)` — **computed, never stored**: the read
port `LEFT JOIN`s the caller's grant and the domain folds the two. A registration lands on
`view` (listed once open, locked); `0033` backfilled every `open` row to `invest` so the
release changed nothing for live products. `ListAllocations` shows an investor `open`
products at `view` or above; `GetAllocation` answers NOT_FOUND for a `hidden` one (as for
an unregistered slug); `Subscribe` refuses below `invest` with `DomainError::Precondition`
→ FAILED_PRECONDITION, distinguishable from "not open" and the cap (both `Validation`).
`require_redeemable` never consults access — a locked product still lets holders out.
`AllocationManage` reads everything, but `caller_access` stays honest for a manager too:
the permission reads the product, it does not invest in it. Grant/revoke facts land in
`event_log` under the allocation aggregate (`relay = false`, like every registry event).

**Authorization.** Registration and every transition need `bank:allocation:manage`
— Admin/Owner, alongside `ValuationPost`, never Operator: bringing a fund into existence
is not a read. Reads are open to any authenticated user; `include_unlisted` (drafts +
closed) is behind the same permission and **refuses** rather than silently downgrading to
the open-only list.

**No money, so no relay.** An allocation moves no value: its events are drained with
`relay = false` — audit facts in `event_log`, never in the `outbox` (the relay has no
ledger op for the kind and would park the row). Identity is the `ServiceId` slug; the
table's UUID `id` is a surrogate carried only because `event_log.aggregate_id` is a UUID.

**Wire contract.** `contracts/proto/banking/v1/allocations.proto` plus the hand-written
`evbanking_contracts::allocation` facade, which gathers the stubs and pins the state
vocabulary consumer repos match on. `domain::allocations::AllocationState::as_str` must
stay byte-identical with it (guarded by `allocation_state_strings_are_canonical` and
`domain_states_match_the_wire_contract`).

**Migration `0021`** drops the dead `allocations` table from `0002` (the write store of
the abandoned per-service stake aggregate, unread by any code since `0005`) and backfills
every service already carrying subscriptions/redemptions/positions/valuations as `open` —
without that backfill the gate would retroactively lock existing investors out of
`Redeem`.

### Reserved allocations — `fee` and `fund` (#245)

The platform's own money is two ordinary allocations that people hold through units.
`ServiceId::fee()` / `ServiceId::fund()` (`domain/src/balance.rs`) are reserved slugs:
`Allocation::register` refuses them, migration `0045` wrote the two rows (fixed ids
`FEE_ALLOCATION_ID` / `FUND_ALLOCATION_ID`, `open`, `hidden`, `cash`), and
`ServiceId::is_reserved` is the one predicate every gate below reads.

- **Not an operator's to manage.** Every registry write — details, cap, open/close,
  access, backing, grant/revoke — runs `allocations_app::refuse_on_reserved` first
  (`Forbidden`): a single `AllocationManage` holder can neither let themselves in nor lock
  the holders out. `IssueUnits` / `RetireUnits` refuse a reserved `service` (`Forbidden`),
  and `Subscribe` refuses it too — units of `fee`/`fund` are never bought.
- **Holders are seated by the owners.** `issuance::grant_units` is the only door into a
  reserved supply, reached only by an executed `holder_grant` consilium
  (`ConsiliumService.OpenHolderGrant`, [`docs/CONSILIUM.md`](../../docs/CONSILIUM.md)
  § "Holder grant and seed capital"); a holder must be an
  active, mirrored user. The mint does **not** flip the backing to `in_kind` — a reserved
  allocation's units are backed by the cash on its own claim, and flipping it would refuse
  the one exit its holders have.
- **The holder graph is `reserved → product`, one hop** (`UnitHolder::ensure_may_hold`,
  `domain/src/issuance.rs`): a user may hold anything; an allocation may hold units only
  when it is reserved and the product is not, so nothing holds a reserved allocation but
  people, and every unit bottoms out at a person within two hops. In phase 1 `fee` is the
  only allocation holder — its holding of a product **is** the product's fee class, the
  physical `FeeShares(svc)` account (`UnitHolder::Allocation(fee).shares_key`); `fund`
  holds no product units (its capital is cash on `service:fund`), so an issuance naming it
  as holder is refused rather than aliased. `IssueUnits` refuses an allocation holder
  outright: a fee class is minted by the fee accrual, never by hand.
- **NAV is derived, never posted** (`application/funds.rs::nav_of`): for a reserved
  allocation the price is `(cash on its claim + Σ product units it holds × that product's
  NAV) / its own supply`, the seed NAV while the supply is zero; `posted_at` is the
  **oldest** mark among the products it holds (as fresh as its stalest input). `PostFundValuation`,
  `OpenValuationOverride` and `ScheduleFeePolicy` refuse a reserved slug
  (`refuse_mark_on_reserved`; a reserved allocation charges no fee).
- **A holder leaves by redemption at NAV**, priced by `nav_of` and paid out of the
  allocation's own claim like any product — but a shortfall is **refused, never queued**
  (`request_redemption`): nobody tops a reserved allocation up on request, its cash grows
  only as products settle their fee classes or a seed lands. The redeem cooldown on a
  reserved allocation is the cooldown on **every product it holds** (`refuse_recent_poster`
  walks `share_holdings(Holder(Allocation(svc)))`): a `fee` holder who marks a product up
  cannot cash `fee` out at that mark for `VALUATION_REDEEM_COOLDOWN_SECS`.
- **Cash leaves a reserved claim two ways only:** a holder's redemption, or a payment order
  out of `service:fee` / `service:fund` that the owners' consilium approved
  (`PaymentsService.OpenPayment`, L2/L3 — an external destination is refused for any
  allocation source). Holders read their position through `allocation_for_holder`: the
  catalog never lists a hidden allocation, but whoever holds units of one reads its title
  and price like any other position.
- **Seed capital** is a chain-proven arrival on a rail's treasury booked as the
  **depositor's** deposit (`Dr wallet:<net> / Cr user:<depositor>`) followed by their
  subscription into `fund` at its NAV (`application/balance.rs::seed_fund_capital`, priced
  and gated by `funds::price_fund_seed` — `require_subscribable`'s access gate and the
  Read-First balance check are the two gates deliberately absent). The RPC `SeedCapital`
  writes nothing: it opens a `seed_capital` consilium and only its execution reaches
  `seed_fund_capital`. The subscription id is `seed_subscription_id(tx_ref)`, so the
  deposit gate and the mint share one idempotency key.
- **The treasury read** (`application/balance.rs::treasury`, `GetTreasury`) is
  `Treasury { rails, bank, total_custody, held_by_users, allocations[],
  reserved_for_withdrawals }`: `held_by_users` is Σ `user:<id>` read through the same
  `cash_invariant` scan reconciliation asserts with, `allocations[]` is every registry row
  (the hidden `fee`/`fund` included) as `AllocationTreasury { service, title, access,
  claim {posted, reserved, available}, units_outstanding, nav, holders[] }`, and nothing is
  a remainder — the pre-#245 `held_for_clients = custody − fund − fee` is gone (wire fields
  4..6 reserved). `GetFundRevenue` is the same `AllocationTreasury` for the one
  slug `fee` (`fee_allocation`), not a second computation.
- **Migration path.** Expand (`0045` registry rows + `holder_service`, `0046` consilium
  kinds) → the one-off data command `piggybank migrate-ownership` moving the retired
  `fund`/`fee` claims onto `service:fund`/`service:fee` and minting the owners' holder table
  in one linked chain per allocation
  ([`docs/RUNBOOK-ownership-migration.md`](../../docs/RUNBOOK-ownership-migration.md), run
  2026-09-27, command since removed) → contract (C-9, `0047_ownership_contract.sql`: the
  CHECKs narrowed to the live vocabulary, `NOT VALID` on `deposits` and `unit_issuances`
  so the history stays; the retired variants, keys and replay branches removed).

## Fund shares — the service currency (`domain::subscriptions`, `domain::redemptions`, `FundsService`)

A client invests by **subscribing** cash into a fund (a `ServiceId`) and receiving
**units** of the service currency, priced at **NAV per share**. A holding's value is
`units × NAV`, so profit comes from a rising NAV, **not** from extra units (standard
NAV/unit accounting). This **replaced** the original flat per-service *stake* aggregate
(confusingly also called an "allocation" then — the name now belongs to the registry
above, which is a different thing entirely: a product's listing, not a holding).

**Units ledger (`Ledger::Share`, `= 3`).** `UserShares(service, user)` (60, debit-normal,
`shares:<svc>:<uuid>`) is a holder's units; `SharesOutstanding(service)` (61, credit-normal,
`shares_outstanding:<svc>`) is the fund's units in circulation. Per-service invariant
`SharesOutstanding(svc) == Σ_user UserShares(svc, user) + Σ_user BookShares(svc, user) + FeeShares(svc)`,
by construction (the extra holders are introduced under [Fees](#fees--2-and-20-domainfees-feesservice-feesweeper)
and [The book](#the-book--holders-trading-units-with-each-other-domainbook-bookservice);
`FeeShares` is the `fee` allocation's holding, so every term is a person's or an
allocation's whose holders are people). The retired company stake (`shares_company:<svc>`,
code 63, `AccountCode::RetiredCompanyStake`) still exists in TigerBeetle at zero and is not
a term: `share_holdings` steps over it by code (`infrastructure/ledger.rs`) — see
[In-kind issuance](#in-kind-issuance--units-with-no-cash-leg-domainissuance-allocationsserviceissueunits). **Mint**
`Dr UserShares / Cr SharesOutstanding`; **burn** `Dr SharesOutstanding / Cr UserShares`.
A burn that exceeds the holder's minted units is rejected **by TigerBeetle's flag — even as
a pending reserve** (this is the over-redeem backstop; the PG row-lock only serializes).
`Shares`/`Nav` are 18-dp `u128` newtypes; `Shares::from_cash`/`Nav::value`/`Nav::from_aum`
use an overflow-safe 128×128→256 `mul_div` (a naïve `u128` mul overflows at ~340 USDT).

**NAV is derived, not posted.** An operator posts a fund's **AUM**; the handler reads
`units_outstanding` live from TB and stores `NAV = AUM / units_outstanding` in
`fund_valuations` (append-only marks; the latest is the price, **frozen** between marks).
The first subscription bootstraps at **seed NAV 1.0**. Dealing on a frozen mark is
*backward pricing* — guarded by a **staleness** check (`MAX_NAV_AGE_SECS`); the AUM post is
guarded by a **move** check (`MAX_NAV_MOVE_PCT`) because the AUM input is the most
dangerous seam ("trusted" ≠ "safe"). NAV is a price, never a TB balance. There is
**no cross-ledger invariant** tying units to USDT — units float; cash stays exact.

**The move check has no override, and it is measured twice** (banking#232). The cap is
checked against the previous mark AND against the mark anchoring a rolling
`NAV_MOVE_WINDOW_SECS` window (the newest mark at least that old, or the fund's first mark
while it is younger than the window) — so +49% seventeen times in an afternoon is refused
on the second post, not compounded into a 900× price. A move the guard refuses is a
**valuation-override consilium** (`docs/CONSILIUM.md` § Valuation override): the poster
proposes, the owners vote, and executing it records the mark through the same writer
(`funds::record_valuation`) with the guard simply not consulted. A subject who posted a
mark for a fund — directly or by proposing the override — **cannot redeem from that fund
for `VALUATION_REDEEM_COOLDOWN_SECS`**, checked at request AND at settle, so the person
who moved the price is never the person cashing out at it. A hard ceiling "AUM ≤ cash in
the fund's claim" was considered and rejected: an in-kind product (§ In-kind issuance) is
worth its asset with zero cash in its claim, and that invariant would make it unmarkable.

**Subscribe (cash → units, synchronous).** Read-First on the unified claim + a fresh NAV;
the relay posts two legs, **cash-first**: `Dr user / Cr service` (the cash pools in the
fund), then mint `Dr UserShares / Cr SharesOutstanding`. Cash-first means an insufficient
claim parks before any units mint — never units without cash. The `fund_positions`
**cost-basis projection is written by the relay**, after the cash leg posts — *not* on the
synchronous `open` path. Were it written at `open` (as it once was), a cash leg that later
parks (a raced over-subscribe) would strand a **phantom position**: cost basis with no units
and no cash debited, fabricating a P&L loss. Writing it relay-side, after the leg lands,
keeps the projection from ever leading the ledger. The relay's add is `cost_basis += cash`,
made idempotent under at-least-once delivery by a per-event `saga_steps` marker (`leg = 100`,
`role = 'subscribe_position'`) committed in the same transaction as the add — a redelivery
re-applies the TB legs (`Exists`) but the marker gates the relative add to exactly once.

**Redeem (units → cash, accept-and-queue, settle-time priced).** Units are reserved now;
the cash is **priced and paid at settle** (settle-time NAV, so a queue that drains after a
NAV drop doesn't overpay the redeemer). States `Queued → Completed | Cancelled | Failed`.

| event | → state | relay ops |
| --- | --- | --- |
| `Requested` | `Queued` | reserve a **pending burn** `Dr SharesOutstanding / Cr UserShares` (locks the units) |
| `Settled` | `Completed` | **burn-first**: post the pending burn, then pay `Dr service / Cr user` (`units × settle-NAV`) |
| `Failed` / `Cancelled` | `Failed`/`Cancelled` | **void** the pending burn — units returned |

`request_redemption` settles immediately (via a **separate** command — never co-emitting
`Requested`+`Settled`, which would race the reserve) when the fund claim covers the payout,
else leaves it `Queued` for the operator `SettleRedemption` after the fund tops up (a
deposit to its `service:<id>` claim). The settle pre-check guards the **payout's debit**
(`service` claim `available()`); ordering is **burn-first** so a short fund parks before any
leg, and a raced over-redeem (parked reserve) fails the burn-post **before** any cash leaves
— neither half-applies. Cost basis (average cost) is tracked in `fund_positions` for P&L,
alongside the position's **remaining units** (set on the subscribe mint, decremented per
settle). At settle the basis is reduced *proportionally* — `cost_basis ← cost_basis ×
(units − redeemed) / units` — dividing by those **projection-tracked** units under the
`fund_positions` row lock (re-taken inside `settle`, not just `open`), **never** a live
TigerBeetle holding: the unit burn is posted by the relay *after* the settle tx commits, so a
TB read lags it and back-to-back settles would each divide by the same gross pre-burn balance
(under-reducing the basis). Tracking units on the projection makes concurrent/back-to-back
settles compound deterministically. The reduction applies **exactly once** per redemption: a
repeat `SettleRedemption` (documented idempotent) short-circuits on the already-`Completed`
row before reducing, and a settle that outruns the relay's subscribe projection (the redeem
is admitted off the TB balance, which the projection lags) **rolls back** with `Conflict` —
redemption still `Queued`, settleable by the operator once the projection lands — rather
than silently skip a reduction the projection would then overwrite into a permanently
overstated basis. The auto-settle inside `Redeem` degrades that `Conflict` to re-reading and
returning the redemption's actual state (queued, or a raced terminal) instead of an error. The
per-investor `high_water_mark` column reserved here is now live — see [Fees](#fees--2-and-20-domainfees-feesservice-feesweeper).

## In-kind issuance — units with no cash leg (`domain::issuance`, `AllocationsService.IssueUnits`)

A subscription is cash-for-units. It has no honest shape for a product registered against
an asset that **already has owners**: `service_arb` is valued at $16 250 with its units
held by named people who own the asset, and nobody wires cash into a fund claim to make
that true. `IssueUnits` (`AllocationManage`) is the second supply path: a mint **with no
cash leg**, to a `UnitHolder` that is a **user** (the only holder an operator may name —
see below), at a cost basis the operator states (`units × NAV` when they do not — the same
figure a subscription for those units would have cost at the mark). It lives on the
registry service, not `FundsService`, because it is a decision about who holds what, made
by the operator who sizes the product, not an investor dealing at NAV.

**Who holds (#245).** `UnitHolder` is `User(id)` or `Allocation(svc)` — a reserved
allocation holding a product's units, which in phase 1 is `fee` holding the product's fee
class (`FeeShares(svc)`, 62). `IssueUnits` refuses an allocation holder (`Forbidden`: a
fee class is minted by the fee accrual, never by hand) and refuses a reserved `service`
(its units come only through a holder-grant consilium, `issuance::grant_units`). The
**company is no holder** — retired in #245: there is no company variant of `UnitHolder` or
`IssuanceSource` (`domain/src/issuance.rs`; `from_parts` / `parse` refuse `company`) and
no live key for its stake account (see "Retired: the company's stake" below).
`ListUnitHolders` is the cap table read straight from the Share
ledger (`ownership_app::allocation_ownership`): `units_outstanding` and `holders[]` —
`{holder: {kind: user | allocation, id}, units}`, largest first, a person's line being
their free units plus the units resting in their sell orders, the `fee` line the product's
fee class — plus
`queued_units`, the `mint` rows still `queued` in `unit_issuances`, because
`ensure_capacity` reads the settled supply and an operator pinning the cap to it while a
mint is in flight pins it below where the supply is about to land. (The pre-#245
three-class summary `company_units` / `fee_units` / `investor_units` and
`FundNav.company_units` are reserved fields on the wire.) The mint posts under its own
`TransferCode::UnitIssue` (47), not `ShareMint`, so supply growth the fund's cash never
paid for is distinguishable from a subscription's on the Share ledger alone.

**Same path as a subscription, minus the cash.** The use case (`application::issuance`)
resolves the allocation (registered in **any** state — a product is normally seeded before
it opens, and a closed one may still need its cap table corrected; access is not consulted,
this is an operator command), requires a user holder to **exist and be active**
(`require_holder`: units minted to a UUID nobody can sign in as, or to a frozen account,
are units nobody can redeem — the DB reference on `unit_issuances.holder_id` backs the
first half), prices at the fresh dealing NAV (the mark
is recorded on the row and blended into the holder's high-water mark, so a stale one is
refused as it is for a subscription), and runs `ensure_capacity` against the ledger's
issued supply — an operator sizing a product below what they mean to issue raises the cap
first. The control-plane row (`unit_issuances`, migration `0031`) and the `Issued` event
commit together; the relay posts `Dr <holder shares> / Cr SharesOutstanding` with a
transfer id derived from the **issuance id** (`tid(issuance, "issue:mint")`), then — after
the leg lands, never on the open path — stamps the row `queued → applied` and, for a user
holder, adds the cost basis to `fund_positions` under the same per-event `saga_steps`
marker discipline as the subscribe projection (`leg = 100`, `role = 'issue_applied'`). A
parked mint therefore leaves a `queued` row and no basis, never a row claiming units it
did not get.

**Idempotent by an operator-supplied key**, unique per service (`1..64` chars): the
console generates one per form submission and re-sends the same one on a timeout, so a
double click lands one mint. The use case reads the key **before** pricing (a retry must
succeed after the mark went stale); the adapter's `INSERT … ON CONFLICT DO NOTHING` settles
the concurrent case and hands the loser the winner's row, and the event is drained only
when the insert landed. A repeat asking for the same holder and units returns the row as it
stands now; the same key for a **different** request is `Conflict` → ALREADY_EXISTS — the
retry is what the key exists for, the reuse is the mistake it has to catch.

**The service_arb recipe.** Register → `IssueUnits` to each named holder at the seed NAV
with the agreed bases → `PostFundValuation` at the asset's value (NAV needs units
outstanding, so the issuance comes first) → `SetAllocationUnitCap` to exactly the issued
supply (`remaining_capacity == 0`, so no subscription and no further issuance fits) →
open. The investor can still redeem: every gate here keeps the exit open.

**Retired: the company's stake and `TransferCompanyStake` (#245).** Until #245 the
company itself was a holder (`CompanyShares(svc)`, 63) and `TransferCompanyStake` handed
part of its stake to a named person as a `unit_issuances` row with `source = 'company'`
(migration `0038`), posted `Dr UserShares / Cr CompanyShares` under
transfer code 52 — a move *between holders*, supply and NAV unchanged. All of it is
removed: the RPC, the holder and source variants, the account key and the transfer kind.
What stays is history. The stake's account (code 63, `AccountCode::RetiredCompanyStake`)
exists in TigerBeetle at zero, and code 52 is reserved in `TransferCode::RETIRED`
(`domain/src/balance.rs`); neither is reused. Production's two company rows in
`unit_issuances` (the 13 000 `service_arb` units minted to the stake, and their hand-over
to a person) stay under `NOT VALID` CHECKs (`0047_ownership_contract.sql`) and are read at
the persistence boundary as `ports::issuance::StoredIssuance::RetiredCompany`
(`infrastructure/issuance.rs`), never as a live issuance. Their idempotency keys stay
taken: reusing one is `Conflict` (`application/issuance.rs`). A product's own stake goes to
people (`IssueUnits`) or is the `fee` allocation's fee class.

**Retiring units (`RetireUnits`).** The mint's mirror: units burnt out of a holder's
account — a user's, or the `fee` allocation's fee class in a product — with no cash leg,
for units that should never have been minted or that stand for an asset the holder no
longer owns. A reserved `service` is refused (`Forbidden`): its holders leave by
redemption at NAV, never by an operator's burn. Same `unit_issuances`
row with `source = 'retire'` (migration `0039` widens the CHECK), `units` **positive** —
every row carries the magnitude and the source carries the direction, so the 0034 digit
CHECK never learns a sign and the console reads one history of mints, hand-overs and
retirements. Same key contract, same key space (a mint's key reused for a retirement is
`Conflict`). Gates: key first; the allocation registered **and `closed`** — burning a
holder's units out of a live product is a decision that deserves a closed door first,
and `force` is the operator's explicit override (`Precondition` → FAILED_PRECONDITION
without it); a user holder exists; fresh NAV (recorded, default basis `units × NAV` — the
book value written off, not cash that moves); and a Read-First that the holder's
`shares_key(svc).available() ≥ units` — units resting on the book or reserved by a queued
redemption are spoken for and stay (`Validation`). No cap check: supply only shrinks. The
relay posts `Dr SharesOutstanding / Cr <holder shares>` under `TransferCode::UnitRetire`
(53, its own code beside `ShareBurn` for the reason `UnitIssue` sits beside `ShareMint`: a
`ShareBurn` is always paired with a `Redeem` payout, a retirement never is) with its own
transfer id (`tid(issuance, "issue:retire")`); the holder's debit-normal account with the
non-negative flag parks an over-retire that races the read. The `applied` stamp is the
same; for a user holder the projection runs the **seller's** side of a trade — `units −=`,
`cost_basis` shed pro rata and clamped at zero — and the high-water mark is untouched,
because nothing was realised at any price. An allocation holder has no projection to reduce.

**Backing — cash vs in_kind.** Every unit a subscription mints has cash behind it: the
investor's claim moved into the fund's, and a redemption pays that cash back out. Units
minted in kind have **none**, so a redemption on such a product asks the fund to pay cash
it does not hold — it queues forever or, once the fund holds cash for some other reason,
pays out money that belongs to someone else. `Allocation.backing` (`domain::allocations::
AllocationBacking`, migration `0039`: `cash` default, `in_kind`) names the distinction,
orthogonal to state and access. The **first in-kind mint flips a `cash` product to
`in_kind`** inside `issue_units`, after every gate and before the row is written
(idempotent; the order is deliberate — a flip with no mint behind it is one operator
command to undo, a mint on a product still `cash` lets the next redemption price units
the fund cannot pay for); `RetireUnits` leaves it alone, and so does a holder grant into a
reserved allocation — `fee` / `fund` stay `cash`, because their units are backed by the
cash on their own claim and a holder is paid out of exactly that (`issuance::mint`).
Nothing automatic ever flips it back: an operator declares the fund holds cash for the
units with `SetAllocationBacking` (`AllocationManage`, idempotent, either value, audited
as `BackingChanged`). The redeem gate (`allocations_app::require_redeemable`) now runs
both checks off one load — state first, so a draft answers "never open", then
`Allocation::ensure_cash_backed`, which refuses an `in_kind` product as a `Precondition`
(FAILED_PRECONDITION, "units of '<svc>' are not backed by fund cash — sell them on the
book instead of redeeming"). Access is still never consulted on the way out. The
migration backfills by data, not by name: a product with at least one `source = 'mint'`
row is `in_kind` (in production `service_arb` and `test_book`); a pod that predates the
column inserts without it and lands on `cash`.

## The book — holders trading units with each other (`domain::book`, `BookService`)

A subscription is a dealing *with the fund* at NAV; a redemption is the reverse. The
**book** is where holders deal **with each other**: one central limit order book per
allocation, price-time priority, in the shape of a spot exchange (limit and market
orders; `gtc` / `ioc` / `alo` post-only; partial fills; cancel). NAV stays the accounting
price — positions, P&L and the fee high-water mark are still measured at it — and the
book's last trade is a second, market quote shown beside it (`BookSnapshot.nav`). The
book never mints or burns: every unit that changes hands already existed, so supply and
NAV are untouched by a trade. Issue [`#218`](https://github.com/EV-invest/banking/issues/218).

**Who trades.** The registry's *access* axis in full — `require_tradable` refuses a caller
below `invest` (`Precondition`) and answers `NotFound` for a `hidden` product — but **not
its lifecycle**: a `closed` product's holders may still trade among themselves. A book
closes only by its own policy (`book_policies.book_open`, opt-in per product like the fee:
no row is a closed book). Placing is also refused under the read-only kill-switch and for a
frozen owner (the [`OutflowPolicy`] facts, checked in the use case as they are at dispatch);
cancelling never is. The policy — `book_open`, `taker_fee_bps`, `price_tick` (default
`0.01`), `lot_size` (default `0.0001`), `market_slippage_bps` (default 500),
`allow_unbacked_trading` (default `false`) — is set by `AllocationManage`.

**Unbacked trading acknowledgement.** On an `in_kind` product (see
[In-kind issuance](#in-kind-issuance--units-with-no-cash-leg-domainissuance-allocationsserviceissueunits))
a redemption is refused and the book is the holders' only exit — which makes the book the
place a buyer pays cash for a claim on an asset the fund holds no cash for, one they cannot
redeem. That is a decision an operator makes knowingly, so the policy carries
`allow_unbacked_trading` and one domain check, `BookPolicy::ensure_tradable_for(service,
backing)`, runs at **two** gates: `set_policy` refuses to open the book on an `in_kind`
product without the flag (`Precondition`, nothing written — the console's early, legible
refusal), and `place_order` refuses every order on an `in_kind` product's open book without
it (the backstop). The second gate is not redundant: the backing flips `cash → in_kind` on
the first in-kind mint, which can land *after* the book opened, and a book that kept
trading would then sell unbacked units nobody acknowledged — so the order is refused with
the reason rather than filled quietly. The flag is harmless on a `cash` product (it
acknowledges in advance) and the terminal shows buyers a notice when it is set. Migration
`0040` backfills it `true` for every book that was open on an `in_kind` product at the
deploy (`service_arb` in production): those were trading unbacked units with the operator's
knowledge, and a deploy must not close a book nobody decided to close. A pre-0040 pod that
UPSERTs a policy mid-rollout resets the flag; the new pod then refuses that product's orders
with the reason and the operator sets it again — short and recoverable.

**An order is an escrow in the ledger.** Two per-user accounts hold what an order has
committed: `BookShares(service, user)` (64, debit-normal, Share ledger,
`book_shares:<svc>:<uuid>`) and `BookCash(user)` (65, credit-normal, USDT ledger,
`book_cash:<uuid>`). A sell locks its size `Dr BookShares / Cr UserShares`
(`TransferCode::BookLock`, the holding's non-negative flag refuses an over-lock); a buy
locks its **worst case** — notional at the limit plus the taker fee on it, `Dr UserClaim
/ Cr BookCash` (the claim's flag does the same). `BookCash` is a claim like any other, so
the global `sum(custody) == sum(claims)` does not move on a lock; the Share-ledger
invariant becomes `SharesOutstanding(svc) == Σ UserShares + Σ BookShares + FeeShares`. A holder's position reports the escrowed units as `units_in_orders`
(still theirs, still valued, not free to redeem or sell twice), and the wallet reports the
escrowed cash as `in_orders` — `BookCash` is a second account of the same user, so
without that figure `available` would simply drop by the reserve with nothing on the
wallet explaining where it went. Both escrows stay inside `total` (see
[User wallet](#user-wallet--deposit--withdraw-domainwithdrawals-walletservice)): an order
moves money between two of its terms, never out of it.

**A fill is delivery versus payment or nothing.** Each trade is ONE linked TigerBeetle
chain ([`Ledger::post_linked`], `LedgerAction::PostLinked`): `Dr UserShares(buyer) / Cr
BookShares(seller)` for the units, `Dr BookCash(buyer) / Cr UserClaim(seller)` for the
cash (`BookFill`), and — when the taker owes one — the fee out of the taker's side into
the event's `payee`, the `fee` allocation's claim `service:fee` (`BookFee`;
`Party::fee_payee()`, carried on the event as a required `payee` field, `domain/src/book.rs`;
a taking seller pays it out of the claim
the cash leg just credited, because linked legs see each other's effect). The chain is idempotent on its
first leg's id: it applies atomically, so the first id existing means every leg did. Fills
land at the **maker's** price; whatever the escrow did not spend when the order reaches a
terminal state — a buy filled below its limit, an IOC remainder, a maker's unused fee
reserve, a cancel — comes back in one `BookRelease` (`Dr BookCash / Cr UserClaim` or `Dr
UserShares / Cr BookShares`). Every id is deterministic: the lock and the release from the
**order** id (`book:lock`, `book:release`), the three legs from the **trade** id
(`book:fill:units|cash|fee`).

**A cancelled order says why.** `cancelled` is reached three ways — the owner's
`CancelOrder`, an IOC limit whose remainder could not fill at once, a market order whose
priced limit ran out of depth — and the last two end with `filled > 0` exactly the way a
user's cancel after a partial fill does. So every cancel carries a
[`CancelReason`](../../domain/src/book.rs) (`user` | `ioc_remainder` | `market_remainder`;
wire vocabulary in `evbanking_contracts::book::cancel_reason`), set exactly when the state
is `cancelled` (`book_orders` checks the pair the way it checks `rejected` ↔
`reject_reason`), and the first reason stands on the idempotent repeat cancel because it is
the one that actually ended the order. The two reason columns are disjoint by construction:
`reject_reason` is the ledger's free text on a `rejected` order, `cancel_reason` a closed
vocabulary on a `cancelled` one; a refused post-only or self-trade order is never recorded
at all, so it has neither.

**Single writer, no in-memory book.** `PgBook::place` is one transaction under
`pg_advisory_xact_lock(hashtext(service))` (a buy also takes the shared per-user claim
lock, a sell the position row): it reads the opposite side best-first (the partial index
`book_orders_resting_idx`, sorted on `price::numeric` — the digit strings do not sort —
then `seq`, the identity column assigned under the lock; `created_at` is the transaction
START and two queued transactions can carry it in the wrong order), runs the pure
[`MatchingEngine`] (`PriceTimeEngine`; the trait is the seam for a different rule), and
records the answer: the taker's row, a `book_trades` row per fill, every maker's new
state, the bumped `book_revisions` counter, and the [`BookEvent`]s drained to the outbox
**in the order the relay must apply them** — `OrderPlaced` (the lock) first, then a
`TradeExecuted` per fill, then an `OrderReleased` for whatever ended. The engine refuses a
whole order rather than filling part of it for a self-trade (the incoming order would hit
the caller's own resting one — skipping it would let them jump their own queue) and for a
post-only order that would take; a market order is priced upstream from the best opposite
quote ± slippage, rounded *outward* to the tick, and then run as an IOC limit (an empty
opposite side is `Precondition`). The depth snapshot, the tape and the candles are
aggregating SQL over the rows; there is nothing to warm up or lose on a restart.

**Read-First, ledger backstop, and the one park that changes state.** The use case checks
the free holding / claim before writing (the same optimistic read as Subscribe, serialized
by the same locks). Two placements can still both pass it before the relay moves the first
— and then the second lock parks on the non-negative flag. Unlike every other park, this
one **must** change control-plane state: an order standing on the book with nothing behind
it would keep matching, and every fill against it would park too. So the relay, after
parking an `OrderPlaced`, marks the order `rejected` (`reject_reason` set) and bumps the
book revision (`infrastructure::book::mark_rejected`) — the order comes off the book and
the caller sees why. Its trades, if any raced in between, park under reconciliation like
any other.

**Cost basis moves on both sides, relay-side.** After the chain posts, `project_trade`
(marker `saga_steps` leg 100, role `book_trade_position`, once per event) adds what the
buyer paid — notional plus the fee when they took — to their `fund_positions` basis and
units, blending the high-water mark at the fund's NAV of the moment (carried on the event:
performance is measured against NAV, so the mark has to be one), and reduces the seller's
basis pro rata as a redemption settle does — **clamped at zero rather than refused**: a
refusal here would be a `Retry` that wedges the single-worker relay, and every unit a
seller can lock came through this same relay in an earlier `seq`. Both writes settle the
accrued management fee first (`fee_accrual::carry_accrual`).

**Idempotent by `client_order_id`**, unique per caller (`1..64`), read before pricing and
again under the lock: a retry lands one order; the same id for a different order is
`Conflict`.

**The live feed** (`WatchBook`) is an in-process `tokio::sync::watch` per book
([`application::book::BookFeed`]), told the revision after every commit: a subscriber
whose wait ends re-reads the book and frames the top-N snapshot, the latest public trades
and `orders_revision` (the revision at which the CALLER's own orders last changed, from
`book_orders.revision`, so a client refetches `ListOpenOrders` only when that moves).
`watch` rather than `broadcast` so a slow socket coalesces to the latest state instead
of building a backlog; no `LISTEN`/`NOTIFY`, because the hub is one instance. Nobody
else's orders and no balances ride on a frame.

## Fees — "2 and 20" (`domain::fees`, `FeesService`, `FeeSweeper`)

A fund charges two things, and they answer different questions. **Management** (2% p.a.)
is rent on parked capital: it accrues continuously with elapsed time and is charged
whether the fund made money or not. **Performance** (20%) is a share of profit above a
**high-water mark**, crystallized at the end of a period. A `ServiceId` with no
`fee_policies` row charges **nothing** — the fee is opt-in per product, so it can never
appear on a fund whose prospectus did not promise it. `FeePolicy::HOUSE` is the default
shape (200 bps / 2000 bps / no hurdle / invested capital / annual).

**The management base is the investor's *invested capital*, not the market value** —
`ManagementBasis::InvestedCapital`, the position's `cost_basis`. Market value is
available (`MarketValue`, the hedge-fund convention) but is not the default, and the
reason is a trust seam: the AUM the operator posts is what sets NAV, so a
market-value base would let an operator raise their own fee with the same input the
NAV-move guard already treats as "the most dangerous seam in the system". Invested
capital is the number the investor agreed to, and no operator input moves it.

**The mark is per investor, on `fund_positions.high_water_mark`** — reserved by `0005`
"so adding a performance fee later isn't a painful backfill", and this is that
follow-up. A *fund-level* mark (what a tokenized vault is forced into, because a share
token cannot remember who bought when) mutualizes the fee: someone who subscribes after
a drawdown rides the recovery fee-free while someone who subscribed at the top pays on
gains that only restored their own loss. The industry's fixes are *series accounting* (a
share class per dealing day) and *equalization* (per-investor credits against one
class); the position projection already exists per `(user, service)`, so the third road
is simply to put the mark on it. The mark is maintained by the subscribe projection as
`GREATEST(existing, subscription NAV)` — deliberately the **investor-favourable** blend
on both sides (a top-up above the mark raises it for the whole position; one below
leaves it), where exact per-lot fairness would need series accounting. `0023` backfills
pre-existing positions to `cost_basis / units`, their own average entry price: a mark
left at the reserved `'0'` would treat an investor's whole NAV as profit and charge 20%
of their **capital** on the first crystallization.

### The charge moves units, never cash — and that is the whole design

A charge is one posted transfer on the **Share** ledger: `Dr FeeShares(svc) / Cr
UserShares(svc, user)` (`TransferCode::FeeClawback`). `FeeShares` (code 62,
debit-normal, `shares_fee:<svc>`) is a holder of units exactly like a user — it is the
product's **fee class**, the units the reserved `fee` allocation holds in that product
(`UnitHolder::Allocation(fee).shares_key(svc)`, #245), so the `fee` allocation's own
holders own the charge through their `fee` units and its NAV carries the class at the
product's mark before a single dollar settles. Three
properties follow, and each is pinned by a test in
[`tests/fee_policy.rs`](tests/fee_policy.rs):

- **No chain fee, ever.** Nothing leaves custody when a fee is charged, so the fund pays
  no gas per investor per period. The manager converts an accumulated unit balance to
  cash *once*, in bulk (below), instead of N times.
- **An investor cannot be pushed negative.** The cash claim is not touched *at all*, and
  the clawback is capped by the units actually held — with TigerBeetle's
  `credits_must_not_exceed_debits` flag on `UserShares` as the ledger backstop under the
  application's own cap.
- **Nobody else pays.** `SharesOutstanding` does not move (a transfer *between holders*,
  not a mint), so NAV per unit is unchanged. This is what makes the per-investor mark
  honest rather than a dilution everyone shares. The Share-ledger invariant becomes
  `SharesOutstanding(svc) == Σ_user UserShares(svc, user) + Σ_user BookShares(svc, user) +
  FeeShares(svc)`.

What cannot be collected — the holder's units are locked by a queued redemption or
escrowed by a resting sell order on the book, or the charge floors below one base unit of
share — is carried as `fund_positions.fee_debt` and taken on the next assessment. It is
never written off and never becomes a negative balance. Only a charge that is not **owed**
at all (a zero policy, a clock that did not run, a residue flooring to nothing) persists
nothing and leaves both clocks where they are, so the accrual simply continues into the
next sweep (`FeeCharge::is_empty` is `due.is_zero()`, not "nothing collected").

**The fee is owed on the whole position and collected from the free holding** (#255).
`PositionSnapshot` carries two unit figures: `units`, the position the fee is measured on
— `UserShares.posted + BookShares(svc, user).posted`, so a resting sell's escrow and a
queued redemption's reserve both count, because an order or a pending exit changes where
the units sit and not who owns them (and `MarketValue` management is priced on all of
them) — and `collectable`, the holding's **available** balance, which caps the clawback.
Both escrows fall outside the cap: a queued redemption is a pending debit (`locked`), a
resting sell has moved its units into `BookShares` outright. So an order changes *which*
road the fee takes but not the amount — with some units free, the charge takes those and
carries the rest as debt; with every unit in an order, the charge is still recorded, with
`charged_units = 0` (`0041` widened the audit row's CHECK for exactly this), the whole
amount lands in `fee_debt`, the clocks move, and the next assessment after the order ends
collects the debt plus only the seconds since. `FeeAssessment::record` raises `Charged`
only when `charged_units > 0`, so the relay never sees a zero transfer; the audit row, the
debt and the clocks commit either way. The escrow itself is never drawn on — it belongs to
the order until the book releases it — and the holding cannot go negative, because the cap
floors at what it has. Pinned by the three escrow tests in
[`tests/fee_policy.rs`](tests/fee_policy.rs).

The alternative — pausing the clock while the units are escrowed, and telling the holder
— was rejected because it *is* the hole this closes: a holder who keeps an ask resting
defers their fee for as long as they like and is billed the whole stretch in one blow the
day it comes off, while the fund carried their capital the entire time. The debt road
already existed (`0023`, [`fee_accrual`](src/infrastructure/fee_accrual.rs)) and adds no
new state.

The residual window is the few milliseconds between a placement being recorded and the
relay applying its lock, during which an assessment still sees the units as free: a
clawback landing there makes the lock park on the non-negative flag and the order is
marked `rejected` — the designed backstop, not a negative balance.

### Ordering, clocks, and the atomic write

`assess` charges **management first, performance on what is left**: charging both
against the same pre-fee unit count would take a performance cut of capital the
management fee has already claimed. Two clocks are tracked separately because they mean
different things — `fees_accrued_at` moves on **every** charge (management is
continuous), `crystallized_at` only when the performance fee actually crystallized, so a
mid-period charge cannot silently restart the period.

`PgFeeAssessments::charge` commits four things in one transaction under the
`fund_positions` row lock: the `fee_assessments` audit row, the new debt/mark/clocks,
the projection's `units` **decremented by the clawback**, and the `Charged` event into
`event_log` + `outbox` (absent when nothing was collected — the first three still
commit). The `units` decrement is load-bearing and easy to miss: the
redemption settle reduces cost basis by `(units − redeemed) / units` against the
*projection's* count (`0010`), so a clawback that took units without telling the
projection would leave that denominator permanently too large and every later settle
would under-reduce the basis. The row lock is the same target the settle takes, which is
what keeps the two from interleaving.

**A fee is priced at the dealing NAV, staleness guard included.** That is a safety
property: if an operator stops posting marks, fees stop accruing rather than accruing
against a price nobody has confirmed. The fee is the one charge the operator sets,
prices and collects, so it gets the same guard as an investor's own dealing.

### Settlement — the one moment a fee becomes cash

`SettleFeeShares` (operator, `AllocationManage`) converts a fund's accumulated fee units
in one bulk operation, priced at the **product's dealing NAV of the day**. Two posted
legs, **burn-first** like a redemption settle: post `Dr SharesOutstanding / Cr FeeShares`
(`ShareBurn`), then `Dr ServiceClaim(svc) / Cr <payee>` (`FeeSettle`) — the payee is the
`fee` allocation's claim `service:fee` (`Party::fee_payee()`, carried on the
`SharesSettled` event as a required field, `domain/src/fees.rs`, so a redelivery recomputes
the same legs). The product buys its fee class back from the `fee`
allocation: the class shrinks, `fee`'s cash grows by the same value, and its NAV does not
move — the settle only changes *what* stands behind the `fee` units, not how much. The
payout leg joins `redeem_payout` in the relay's settle-time liquidity pre-check, so a
fund short of cash parks the whole event with nothing applied rather than burning units
it cannot pay for. The application layer **refuses** rather than queueing when the claim
is short — after reserving what the product's queued redemptions would cost at the same
NAV (`queued_redemption_cash`) — because unlike an investor's redemption nobody is waiting
on this, and unconverted fee units keep accumulating at no cost. The settler is bound by
the redeem cooldown (`refuse_recent_poster`, `application/fees.rs::settle_fee_shares`):
within `VALUATION_REDEEM_COOLDOWN_SECS` of their own mark on the product the settlement is
refused, because converting one's own mark into the `fee` holders' cash is the same move
the cooldown stops one step later. The same fee is retained on every user withdrawal
(`WithdrawFee`) and charged to a taker on the book (`BookFee`) — every fee producer
credits `service:fee`, and the only ways cash leaves it are a `fee` holder's redemption at
`fee`'s derived NAV and an owners' payment order out of it (§ Reserved allocations).

### The sweeper

[`fee_sweeper`](src/infrastructure/fee_sweeper.rs) is a `join!` branch beside the
recovery jobs — the only one that *charges* rather than repairs. Hourly, it takes up to
500 unit-holding positions whose last accrual is over 24h old, oldest first, and
assesses each. The cadence is an efficiency knob only: the fee owed is a function of
elapsed seconds, so missing a week bills that week on the next pass with no drift and no
double-charge. Per-position failures warn and continue — one investor's stale NAV must
not stop everyone else's fee from being collected.

**Crystallization frequency is a price, not a detail.** Moving from annual to quarterly
measurably raises what an investor pays over a fund's life, because each reset locks in
gains a later loss can no longer claw back. Annual is the default for exactly that
reason.

### Known gap — exit crystallization

`Trigger::Redemption` (crystallize only the units leaving, at the redemption-day NAV,
leaving the mark for the units that stay) is modelled and unit-tested but **not wired**
in v1: only `Trigger::Period` runs. Wiring it at *request* time would double-charge the
same gain if the redemption were cancelled and re-requested, and over-charging an
investor is the worst failure this feature has. The correct hook is the redemption
**settle**, netting the fee from the payout (`Dr service / Cr service:fee` beside the existing
`Dr service / Cr user`, mirroring the withdrawal settle's fee leg) — where it is
terminal, idempotent, and cannot be replayed. Until then an investor who redeems shortly
before a period end escapes the performance fee on that period's gain.

## User wallet — deposit & withdraw (`domain::withdrawals`, `WalletService`)

A user's money is **one** network-agnostic claim (`user:<uuid>`). `GetWallet` presents it
segmented by lifecycle — `available` (`posted − locked`), `in_orders` (the cash the
book holds for the user's resting buy orders, `BookCash` posted — still theirs, handed
back as the orders fill or are cancelled), `invested` (the value of the user's fund
positions, `Σ (units + units_in_orders) × current NAV` — the units a resting sell escrowed
are still the holder's and still valued, exactly as the position reports them),
`pending_withdrawal` (the claim's `locked`: queued/in-flight withdrawals), and `total =
available + in_orders + invested + pending_withdrawal`. The cash terms come off two
posted balances read in one pass (the claim's `posted` is `available + pending_withdrawal`
by construction, `BookCash` is the third term), so placing or cancelling an order moves
money between terms without moving the sum — plus a
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
| `Settled` (N confs) | `Completed` | **post** the clearing pending, then `Dr clearing / Cr wallet:<net>` (net) + `Dr clearing / Cr scope_account:main:treasury` (the retained fee, to the event's `payee` — banking's treasury since D17; an event raised before names `service:fee` and still pays it) |
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

### Revenue payout — retired (#245); how earnings leave now

The allocations earn money in two places — the book's **taker fee** and the settled
2-and-20 (§Fees → *Settlement*) — and both land on one claim, the `fee` allocation's
`service:fee`, held by people through `fee` units. The **retained fee** on every user
withdrawal is banking's own and lands in banking's treasury, `scope_account:main:treasury`
(D17); money leaves the treasury only by a payment order out of it (`Party::Treasury`) that
the owners' consilium approved — internally, or on a rail through a treasury withdrawal
(`WithdrawalSource::Treasury`, no fee, no owner standing to read). There is no "pay the fund out" path any more. Cash leaves `service:fee` in exactly
two ways: a **holder's redemption** of `fee` units at `fee`'s derived NAV onto their own
`user:<uuid>` claim (from where they withdraw under their own gates), or a **payment
order** out of `service:fee` that the owners' consilium approved (`PaymentsService`, L2/L3
only — an external destination from any allocation source is refused at open,
`application/payments.rs`). A payment out of the `fee` allocation is not KYC-gated: there
is no user behind `Party::Service(fee)`, and the floor applies when the recipient tries to
take the money out.

**Retired in #245: the revenue payout.** Until #245 `RequestRevenuePayout` /
`OpenRevenuePayout` moved the retired `fee` claim (code 40) to an external wallet through
the withdrawal saga, under its own withdrawal source, consilium kind (`revenue_payout`) and
approval mail (`payout_approval`). All of it is removed, `CancelRevenuePayout` /
`ListRevenuePayouts` included, and `0047_ownership_contract.sql` narrows
`withdrawals.source` to `user` and drops `revenue_payout` / `payout_approval` from the
`consilium` / `consilium_mail` CHECKs (production held no such row when it ran). A
withdrawal is a person's (`WithdrawalSource::User`) or, since D17, the treasury's
(`WithdrawalSource::Treasury`, only from an approved payment order), and
`require_dispatchable` (`application/withdrawals.rs`) reads the owner of every person's row. Old outbox payloads spell the field `user` and hold a bare
UUID. `WithdrawalSource`'s string form and the `alias = "user"` on the events read those
unchanged, so a row parked before the source existed still drains after it.

## Compartments by scope (`domain::scopes`, `CompartmentService`)

A user's wallet is split by **scope** (D9): `main` is their `user:<uuid>` claim — deposits
land there and withdrawals leave from there — and every scope they enter holds a
compartment, `scope_claim:<scope>:<uuid>`. Only the user moves money `main ⇄ scope`
(`MoveFunds`), instantly and without a fee, never scope to scope.

**A move IN** passes the scope's state (`scope_frozen` / `scope_closed`), the account's
freeze (`user_frozen`), the outflow pause and the scope's policy. **A move OUT** — back to
`main` — passes none of them: the money never leaves the platform, so a frozen user in a
paused system still gets it back. Both need the source to cover the amount.

**Checked under the locks, net of what is in flight.** One Postgres transaction takes an
advisory lock on `(user, idempotency_key)` first — two requests under one key may name
different sources — and answers a reused key with the stored move (a racer that still
reaches the unique index gets ALREADY_EXISTS). Then it takes the source claim's write lock
(the one withdraw and subscribe take) and, for a move in, the target's, always in that
order, and only then runs every check: for a move in, the scope's state, the account's
freeze and the policy — the target's `posted + pending` plus the `pending` moves into it
against `max_balance`, the `pending` + `posted` moves into the scope over 24 h against
`max_moved_in_24h` — and for both directions what the source may spend. The loser of two
racing moves is refused at once. A refusal writes nothing; a pass records the
`compartment_moves` row and its event together.

**One answer to "what may the user still spend".** `Ledger::spendable(key)` is the
ledger's `available` less the owner's `pending` moves out of that compartment — moves the
relay has not posted yet. Every Read-First on a user's money asks it: a withdrawal
(`require_solvent`), a subscription, a buy order, a payment, a move. So a move recorded a
moment ago is already spent for a withdrawal that follows it, refused at once instead of
parking in the relay. The other direction stays optimistic as before: a withdrawal's or a
subscription's reservation is not in the ledger until the relay applies it, and a move
that overtakes it fails in the relay (`failed`, `insufficient_funds`).

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
id already on the ledger if it was, and only stamps the row.

The registry is `scopes` (seeded `invest`, `sa`; concierge's scope stream will own it) and
`scope_policies` (`min_kyc_level` against the bridge's mirrored tier; `max_per_move`,
`max_moved_in_24h`, `max_balance` on moves IN only; the automatic ceilings
`auto_payout_per_op` / `auto_payout_24h` / `auto_transfer_24h`, defaults 1000 / 5000 / 5000
USDT, stored for the scope ledger). `scope_accounts` records who owns each scope account:
banking owns `(main, treasury)` — where its own fees land (D17) — and `(<scope>, fee)`.
The scope list behind listings and totals is cached per process for 30 s; a move reads the
scope row itself. `GetWallet` (`in_compartments`, inside `total`) and `GetBalance` / `GetUserBalance`
(`amount = main + in_compartments`) count the compartments; `GetTreasury` shows
`held_in_compartments` and `banking_treasury`. Mandates are not served yet
(`GrantMandate`/`RevokeMandate` UNIMPLEMENTED, `ListMandates` empty): they need the
per-client grants the scope ledger brings. The existing `service:fee` / `service:fund` /
`service:<slug>` claims are untouched until the migration window moves them into scope
accounts.

## Operations — the activity timeline (`OperationsService`)

The first surface here that is **only** a read model. The four things a user does —
deposit, withdraw, subscribe, redeem — are separate aggregates, each already listing its
own history. What none of them can answer is "what happened, in order": interleaving four
client-side lists needs a timestamp they all agree on, and a per-source `LIMIT` cannot
produce the newest N *overall* (a long deposit history would push out a recent
redemption). `ListOperations` is that join, done once in the hub as a single `UNION ALL`
over the projections the write side already maintains — so it introduces no new table, no
new event, and no new write path.

It owns nothing, so it gets a **plain driven port** ([`ports::operations`]) rather than a
`Repository`: there is no aggregate to hang the marker on, the same shape [`Deposits`]
takes. In Rust the row is a **sum type** (`Operation::{Deposit, Withdrawal, Subscription,
Redemption}`) so a deposit cannot carry a NAV; on the wire it flattens to one message with
a `kind` discriminator, because the whole surface is projected to TypeScript through
OpenAPI and a flat shape survives that pipeline unambiguously. The service layer is where
that trade is paid, once.

Paging is a `limit` (default 100, capped at 200) with no cursor — the timeline is a
recent-activity surface, not an export. The adapter over-fetches one row to answer
`truncated` honestly rather than running a second `COUNT` over the same union. Ordering is
`created_at DESC, id`: on the `timestamptz`, not on truncated epoch seconds, so a
subscribe/redeem pair made in the same second keeps the order the user made it in.

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
  **aggregate-derived** id (`tid(aggregate_id, <salt>)` — `BURN_RESERVE` for a redemption,
  `CLEARING_RESERVE` for a withdrawal) so its settle/cancel can recompute the same
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
- **Cross-flow claim contention (shared per-user lock).** Withdraw and subscribe both spend
  the **same** `UserClaim`, yet live in different tables — so a per-table `FOR UPDATE` (the
  redemptions' `fund_positions` lock) does **not** serialize a withdraw against a subscribe.
  Both `PgWithdrawals::open` and `PgSubscriptions::open` therefore take one **shared** lock
  first: `pg_advisory_xact_lock` keyed on the claim ([`outbox::lock_claim`], of which
  `lock_user` and `lock_revenue_claim` are the two named cases), held to commit. The key for
  those two is **frozen** at what every released binary computes — the raw user id, and a
  fixed v5 name for `fee` — so a rolling deploy cannot leave old and new writers serializing
  on different targets; `the_generalized_claim_lock_keeps_the_keys_the_old_helpers_computed`
  pins it.
  This serializes the two `open` transactions on a single target (an advisory lock needs no
  `users` row and no FK, so it engages unconditionally). It shrinks but does not erase the
  optimistic-Read-First window: the reservation is applied by the relay **after** commit, so a
  fully race-free read-and-reserve would need a PG-side reserved counter (a deliberate
  follow-up). What the lock + relay **do** guarantee today is no silent divergence — a raced
  over-commit parks (TB's non-negative flag), recoverable via reconciliation, and the
  combined fix never leaves a phantom: a parked subscribe writes **no** cost_basis (see
  Subscribe, above), and a parked withdrawal reserve leaves nothing reserved (above).

## Authorization (defense in depth)

Boundary (gRPC) does the cheap stateless check; the stateful rule lives in the
aggregate, applied under the row lock; the TB non-negative flag is the ledger backstop.

| RPC | Who | Boundary | In-tx invariant |
| --- | --- | --- | --- |
| `GetTreasury` / `RecordDeposit` | operator | `require_permission` | chain-proven arrival (amount + party read off the chain) ∧ `tx_ref` gate |
| `SeedCapital` | an owner (`CapitalManage` admits to the door; `Consilium::open` refuses a non-owner) | `require_permission` + `expected_amount` required | opens a `seed_capital` consilium: chain-proven treasury arrival at exactly that amount, reference not yet booked, depositor active, `fund` open and fresh — the deposit + `fund` subscription are booked only when the quorum executes |
| `Subscribe` | the user | `sub == user`, `is_access`, **not revoked, not paused, not frozen** | available claim ≥ cash ∧ fresh NAV (TB flag backstop) |
| `Redeem` | the user | `sub == user`, `is_access`, **not revoked, not paused, not frozen** | available units ≥ amount ∧ fresh NAV (TB flag backstop) |
| `CancelRedemption` | the user | `sub == user`, `is_access` | owns it ∧ state is `queued` (idempotent) |
| `GetPosition` / `ListPositions` / `ListRedemptions` / `GetFundNav` | the user | `sub == user` | — |
| `GetWallet` / `ListWithdrawals` | the user | `sub == user` | — (`GetWallet` serves an address only at `kyc_level ≥ 1`) |
| `GetDepositAddress` | the user | `sub == user` | `kyc_level ≥ 1` (else `permission_denied`) |
| `RequestWithdrawal` | the user | `sub == user`, `is_access`, **not revoked, not paused, not frozen** | owner not frozen (`frozen ∨ disabled`, via `OutflowPolicy::standing`) ∧ `kyc_level ≥ 1` ∧ available claim ≥ gross (TB flag backstop) |
| `CancelWithdrawal` | the user | `sub == user`, `is_access` | owns it ∧ state is `queued` (idempotent) |
| `DispatchWithdrawal` | operator (treasury) | `require_permission` | state is `queued` (idempotent) ∧ **not read-only** ∧ (user source) owner not frozen ∧ `kyc_level ≥ 1` — fail-closed, no `force` |
| `SettleWithdrawal` / `FailWithdrawal` | operator | `require_permission` | state is `processing` (idempotent) |
| `PostFundValuation` | operator | `require_permission` | allocation registered ∧ units outstanding > 0 ∧ NAV move ≤ threshold vs the previous mark ∧ vs the rolling-window anchor — **no override**; beyond it: `ConsiliumService.OpenValuationOverride` (same `ValuationPost` permission, initiator must hold an owner seat, owners' quorum executes the mark) |
| `Redeem` / `SettleRedemption` (cooldown) | the user / operator | as above | the redeeming user posted **no** mark for this fund within `VALUATION_REDEEM_COOLDOWN_SECS` (`failed_precondition` otherwise; checked at request and again at settle) |
| `IssueUnits` / `RetireUnits` / `ListUnitHolders` | admin (`AllocationManage`) | `require_permission` | `service` not reserved ∧ holder is a user (`Forbidden` otherwise) ∧ allocation registered (any state) ∧ user holder exists and is active ∧ fresh NAV ∧ (issue) issued + units ≤ cap / (retire) `closed` unless `force` ∧ holder's available units ≥ units; idempotent by `(service, idempotency_key)`, one key space for mints and retirements |
| `RegisterAllocation` / `UpdateAllocation` / `SetAllocationState` / `SetAllocationUnitCap` / `SetAllocationAccess` / `SetAllocationBacking` / `Grant`/`RevokeAllocationAccess` | admin (`AllocationManage`) | `require_permission` | `service` not reserved (`refuse_on_reserved` → `Forbidden`; a reserved slug cannot be registered either) |
| `OpenHolderGrant` | an owner (`ConsiliumManage` — the owner-surface permission — admits to the door; `Consilium::open` refuses a non-owner) | `require_permission` | terms name a reserved allocation ∧ units > 0 ∧ grantee is an active mirrored user ∧ `fee`/`fund` prices fresh; executed by the owners' quorum through `issuance::grant_units` (cap ∧ fresh NAV re-checked at execution, idempotent by `holder-grant:<consilium>`) |
| `SettleFeeShares` | operator (`AllocationManage`) | `require_permission` | `0 < units ≤ FeeShares.available` ∧ the settler posted **no** mark for the product within `VALUATION_REDEEM_COOLDOWN_SECS` ∧ fresh NAV ∧ `service:<svc>.available ≥ cash + queued redemptions at that NAV` |
| `PlaceOrder` | the user | `sub == user`, `is_access`, **not frozen**, **not read-only** | allocation visible ∧ `invest` (state ignored) ∧ `book_open` ∧ on tick/lot ∧ free units / claim ≥ escrow (TB flag backstop → `rejected`); idempotent by `client_order_id` |
| `CancelOrder` | the user | `sub == user`, `is_access` | owns it ∧ state is resting (idempotent on cancelled) |
| `ListOpenOrders` / `ListOrderHistory` / `ListUserTrades` | the user | `sub == user` | — |
| `GetBook` / `ListTrades` / `ListCandles` / `WatchBook` / `GetBookPolicy` | the user | `sub == user`; allocation visible to the caller (`AllocationManage` sees all) | — |
| `SetBookPolicy` | admin (`AllocationManage`) | `require_permission` | allocation registered; bps ≤ 10000, tick and lot > 0 |
| `SettleRedemption` / `FailRedemption` | operator (treasury) | `require_permission` | state is `queued` (idempotent) ∧ (settle) position projection tracks ≥ the redeemed units |
| `GetUserBalance` | operator | `require_permission`; resolves the CONCIERGE id first via the bridge mirror (`users.concierge_user_id`), then the banking id; unknown ⇒ `NOT_FOUND` | — |
| `ListParkedEvents` | operator | `require_permission` | — |
| `UnparkEvent` | admin (`OutboxManage`) | `require_permission` | parked ∧ not dispatched ∧ **not compensated** (the double-apply guard) |

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
(`Subscribe`/`Redeem`/`RequestWithdrawal`) run one `IssuanceTarget` resolve and one
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
be read. Cancel/read RPCs are intentionally NOT gated, so a frozen user can still unwind
queued positions.

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
and the governance-mail relay share it — one address, one trust relationship, one place it
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
no address on any rail. A **payment out of the `fee` allocation is not gated**: there is no
user behind `Party::Service(fee)` to verify (its holders own it through units), the owners'
quorum stands in front of it instead, and the floor applies to the recipient when they try
to take the money out, not when it arrives (`tests/kyc_gating.rs`).

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
and **alerts** (Sentry-shipped `error!`, no auto-write) on four things:

1. the **global** posted `sum(custody) == sum(claims)` on the USDT ledger, read straight
   from TB via `Ledger::cash_invariant` with the claims broken down by whose they are
   (`CashSide`: `Σ user + Σ service + clearing + Σ book_cash + retired == Σ custody`).
   `claims` is the whole credit side, so the check cannot be fooled by an account the
   breakdown does not know: what the named parts leave over (`CashInvariant::unclassified`)
   is its own alert. The retired `fund`/`fee` singletons (codes 1 and 40, zero since the
   2026-09-27 data migration) are classified by code as `CashSide::RetiredClaims`
   (`ports/ledger.rs`) and count as claims, so they never show up as unclassified;
2. `clearing`'s reserved (pending + posted) balance vs the gross of every
   `queued`/`processing` withdrawal in Postgres;
3. **every unit at a holder, per allocation** (#245): for each registry row, the hidden
   `fee`/`fund` included, `Σ holder units == SharesOutstanding` off the Share ledger
   (`ownership_app::allocation_ownership`; a mismatch is re-read once before it is
   alerted, so a mint landing mid-scan is not a finding). Beside it, **value without a
   holder** — cash or product units on an allocation with no units outstanding
   (`AllocationOwnership::is_unheld`) — is a `warn!` plus a counter
   (`telemetry::note_unheld_allocation_value`), **not** an alert: nothing is lost, the
   value is simply nobody's yet. It was the expected state of `fee` until the 2026-09-27
   data migration seated its holders, and should be zero everywhere since;
4. a scan of every `outbox.parked_at` row (with its `last_error` and `compensated_at`).
   The `last_error` column on a parked row is the first place to look when money didn't
   move.

`ReconReport::clean` is (1) balanced and fully classified, (2) equal, (3) no drift, (4) no
parked rows; unheld value does not make a report unclean.

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

Out-of-band arrivals on a rail's **treasury** are **not credited** (#245). Each deposit
watcher also watches its rail's treasury hot wallet, and a USDT transfer that lands there
from outside every wallet we control (the sweep from a derived address is money already in
`wallet:<net>`, so it is excluded — `recipient_of` in `deposit_watcher`,
`report_treasury_arrival` in `ton_deposit_watcher`) is reported, not booked: a claim needs
a holder and the treasury is nobody's, so the watcher fires a Sentry-shipped `error!` with
rail, hash, sender and amount and bumps the process-lifetime counter
`unattributed_treasury_inflows` (`telemetry::note_unattributed_treasury_inflow`); the EVM
scan advances its cursor like a credit (re-reading the chunk forever would only repeat the
report), the TON scan dedupes by hash across its lookback. The `treasury_drift` watch
reports the same dollar as a **surplus** on every scan until it is attributed, so the
counter is the number to alert on and each increment is a pending `SeedCapital`. The
pre-#245 credit onto the platform's own `fund` claim (`Dr wallet:<net> / Cr fund`) is
retired: `record_deposit` / `Deposits::record` take a `UserId` (`application/balance.rs`,
`ports/deposits.rs`), so a deposit can only land on a person. The seed's three historical
`deposits` rows (`party_kind = 'piggybank'`) stay under the `NOT VALID` CHECK of
`0047_ownership_contract.sql`; every read filters `party_kind = 'user'` or matches by
`tx_ref`, so none of them is parsed as a party.

`RecordDeposit` (admin, `CapitalManage`) is the manual path for a **user's** arrival the
watchers could not see: the reference is read back from the chain and the amount and the
credited party are taken from what is there (`record_verified_arrival`); a transfer that
landed on the treasury is refused and pointed at `SeedCapital`. `SeedCapital` (admin,
`CapitalManage` admits to the door; the caller must hold an owner seat) is the opposite
verdict on the same evidence (`verify_arrival`): the transfer must have reached the
treasury from outside, one that landed on a user's deposit address is that user's deposit
and is refused, `expected_amount` is **required** and asserted against the chain, and the
RPC books **nothing** — it opens a `seed_capital` consilium ([`docs/CONSILIUM.md`](../../docs/CONSILIUM.md)
§ "Holder grant and seed capital") over `{tx_ref, network, amount, depositor}` and only
its execution writes the depositor's deposit and their `fund` subscription
(`balance::seed_fund_capital`, idempotent by `tx_ref`). Neither accepts an amount from the
caller as a credit (the free-amount, no-dedup `SeedCapital` was removed in #234).

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
safe full refund); a **`queued` redemption** is **auto-failed** (internal claim→claim →
safe). Max age is 24h (config seam: `Reaper::with_max_age`).

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
<base>_template` — one database per test binary (`banking_allocation_registry`,
`banking_relay_shutdown`, …), so the outbox relay's per-database advisory lock is never
shared between binaries and a parallel runner (nextest, two `cargo test --test …` side by
side) cannot race for it. `pool()` hands each test its own pool on that clone;
`database_url()` returns the clone's URL for the two suites that size their own pool;
`seeded_ledger()` connects to `TIGERBEETLE_ADDRESS` / `TIGERBEETLE_CLUSTER_ID` (default
`127.0.0.1:3033`, cluster `0`) and seeds the singleton accounts (`ledger::seed_singletons`:
the per-rail wallets, `clearing`, `bank` — not the retired `fund`/`fee` claims, which boot
no longer creates either). Clones are not dropped at
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

`domain` unit tests cover the money + NAV math (incl. the `mul_div` overflow bound and the
share-key ledger sides), the subscription/redemption aggregates, the withdrawal
transitions, the allocation registry's state machine, and the fee arithmetic (the flat-year
2%, the 20% taken net of management, the mark refusing to charge a mere recovery, the
hurdle, the deferral of an uncollectable charge into debt — recorded, not skipped, and
raising no clawback event — and a backwards clock charging nothing);
`piggybank/core/tests/allocation_registry.rs` covers the gate against real Postgres +
TigerBeetle (an unregistered service refused *before* any money moves, a draft taking
nothing, a closed allocation still redeeming, double registration as a conflict, the
catalog's listed/unlisted split, and allocation events staying out of the outbox), and the
in-kind issuance (units landing on a user with no cash leg and `ListUnitHolders` reporting
the cap table, the idempotency key returning the same row and minting once while refusing
a different request, the defaulted `units × NAV` basis, the registry/holder/cap gates, the
recipe ending at `remaining_capacity == 0` with the investor still able to redeem, and the
event reaching the relay as its own kind; the retired company rows are pinned in
`tests/ownership_contract.rs`, below), the reserved allocations (not an operator's to
manage or buy into; `fee` holding a product's fee class and nothing but people holding a
reserved one), the retirement (units burnt out of an investor and out of the asset's
owner — both people — on a closed product with the supply, `ListUnitHolders` and the investor's
`fund_positions` units and basis shrinking and the mark untouched, a live product refusing
without `force` and burning with it, the shared key space returning a repeat and refusing
a mint's key, more than the holder has **available** refused before anything is written —
including units a queued redemption has reserved — and the widened `source` CHECK), and
the backing (a registration landing on `cash`, the first mint flipping it to `in_kind`
with one `BackingChanged` fact and a second mint leaving none, the
operator's `set_backing` idempotent and the next mint flipping again, `Redeem` refused as
a precondition on an `in_kind` product before any redemption is recorded and passing
once the operator declares cash — on a closed product too — and a row written by the
pre-0039 INSERT reading as `cash` with the column CHECK refusing anything else);
`piggybank/core/tests/balance_allocations.rs` and
`piggybank/core/tests/wallet_withdrawals.rs` hit **real** Postgres + TigerBeetle
(deposit idempotency, the non-negative backstop, transfer-id idempotency; the Share-ledger
mint/burn + **over-redeem reject by the TB flag**; NAV derivation + the fat-finger guard;
subscribe minting at seed + fractional NAV pricing + staleness/Read-First; redeem
**auto-settle when liquid**, **short fund → queue → top-up → settle at settle-NAV (profit)**,
**short settle parks without burning or paying**, cancel returns the units; and the
withdrawal reserve→settle with fee, fail→refund, short-rail queue→dispatch→settle, queued
cancel→refund; the on-chain dispatch gate's three arms — short treasury queues despite a
liquid TB rail, liquid treasury dispatches, read failure degrades to queued — plus the
refused admin dispatch and the `Dispatcher::sweep` both-gates flow, driven by a test
`Custody` adapter with a configurable treasury view; plus the dispatch-time outflow
policy — a tier revoked after acceptance stops the sweep, the admin dispatch is refused
under read-only, under a freeze and at tier 0, and a withdrawal whose owner row is gone
fails closed). `piggybank/core/tests/book.rs` hits real Postgres + TigerBeetle for the book: a crossing
limit buy settling delivery-versus-payment with the taker's fee on `service:fee`, both cost
bases and `units_in_orders` moving, supply untouched; a buy below its limit getting the
price improvement back; a partial fill leaving the maker resting; an IOC releasing its
remainder; post-only and self-trade refused with nothing written; a market order priced off
the best quote and refused on an empty side; cancel returning the escrow, idempotently; the
gates (a closed book, `view`, `hidden`, a closed allocation still trading, read-only, a
frozen owner); tick/lot/balance refusals before any write; `client_order_id` retry vs
reuse; a raced over-lock parked by the ledger and the order marked `rejected`; candles and
the 24h change; the feed framing per change with the caller's `orders_revision`; the
policy's defaults and bounds; the wallet showing a resting buy's reserve as `in_orders`
(available down by exactly that, `total` unmoved, a resting sell still inside `invested`)
and every figure back after the cancel; and the three `cancel_reason`s landing on the
row — `ioc_remainder`, `market_remainder`, `user`.
`piggybank/core/tests/fee_policy.rs` hits real Postgres + TigerBeetle for the fee plane's
three load-bearing properties — a charge moves **units** and leaves every cash account
untouched, `SharesOutstanding` is unchanged so no other holder pays, and two investors at
the same NAV owe different fees when they entered at different prices — plus the bulk
settlement (the only moment a fee becomes cash), its refusal when the fund's claim is
short, the sweeper end to end, a fund with no policy never being charged, and the book's
escrow against the clawback — every unit in a resting sell is charged as debt with nothing
collected, the audit row written with `charged_units = 0`, the clocks moved and the ledger
untouched, the debt (and only the seconds since, never the year again) being collected
once the order is cancelled; most units in one caps the charge at the free holding,
carries the rest as debt, and collects it on the next assessment after the cancel, with
the holding exactly empty and the escrow untouched either way. Note that
the accrual clocks are DB-stamped while `now` is caller-supplied, so those tests overshoot
a period boundary by an hour and compare amounts with a tolerance rather than for equality;
the sub-second jitter is 3e-8 of a year's fee and never accumulates. `piggybank/core/tests/kyc_gating.rs` covers the verification floor against real Postgres +
TigerBeetle: tier 0 is refused a deposit address **without the address gateway being reached
at all** (a call counter, since a gate placed after the port would look identical from the
return value while having already minted the key), tier 0 cannot withdraw *with a funded
claim* (so the refusal is the gate, not insolvency), tier 1 does both, the unconfigured-rail
`None` stays distinguishable from the unverified `Forbidden`, and a payment out of the
`fee` allocation to a tier-0 investor is never gated. The ownership suites (#245) —
`piggybank/core/tests/ownership_fee.rs` (settled fee cash landing on `service:fee` and
raising the `fee` holders' derived NAV, a retained withdrawal fee landing in banking's
treasury instead and leaving that NAV alone, nothing new on the
retired revenue claim, a hidden allocation's holder reading their position with its title
and price, a reserved allocation taking no mark and no fee policy),
`piggybank/core/tests/treasury_ownership.rs` (the treasury listing every allocation with
its holders, the hidden ones included; `held_by_users` a sum of user claims and not a
remainder; unheld value reported, not swallowed; per-allocation units reconciling after a
grant, a redemption and a fee charge), `piggybank/core/tests/seed_capital_proof.rs` (a
transfer to a user address is not seed capital, an external treasury arrival seeds the
depositor once per reference, an admin cannot seed onto themselves without a quorum,
`RecordDeposit` refuses the treasury and points at the seed) and
`piggybank/core/tests/ownership_contract.rs` (production's pre-#245 history built on a
database migrated to 0046, then `0047` applied over it: no holder moves, the company rows
read as `StoredIssuance::RetiredCompany` with their keys still taken, the seed's deposits
still gate their references, every retired value refused by a CHECK, the documented
rollback round-trips) — hit real Postgres + TigerBeetle too.
`piggybank/core/tests/relay_recovery.rs`
proves a parked event lands in the distinct `parked_at` state (never marked dispatched),
stays queryable, and is surfaced by `Reconciliation::scan`; that `Reaper::sweep` alerts on
a stuck `processing` withdrawal (never auto-voids it) while auto-cancelling an abandoned
`queued` one; and that an unparked `Dispatched` event for a failed withdrawal is re-parked
by the broadcast-state guard, never sent. They skip when `DATABASE_URL` is unset or TB is
unreachable. Drive the relay deterministically with `Relay::drain`, and the recovery jobs
with `Reconciliation::scan` / `Reaper::sweep` / `Dispatcher::sweep`.
