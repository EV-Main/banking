# Consilium — multi-owner authorization

Two things in this platform must never be one person's decision: moving, repricing or
re-owning the platform's own money — a payment out of an allocation's pooled claim, a NAV
mark past the move guard, a change of fee terms beyond the house envelope, a new holder of
the `fee` / `fund` allocations, the attribution of a treasury arrival as somebody's seed —
and taking someone's ownership away. All are gated by a **consilium** — a quorum of fund
owners who each confirm from their mailbox.

The kinds, as `ConsiliumKind` (`domain/src/consilium.rs`) spells them: `payment`,
`valuation_override`, `fee_policy`, `holder_grant`, `seed_capital`. A sixth,
`revenue_payout`, was retired in #245 and removed from the enum, the contract and the
`consilium.kind` CHECK (`0047_ownership_contract.sql`). The platform holds no money outside
an allocation held by people, so "pay the fund out" is not a thing a quorum can do: cash
leaves `fee` / `fund` by a holder's redemption or by a payment order out of `service:fee` /
`service:fund` (§ Payments).

This document is the policy. It is written before the code because the failure modes
here are not bugs you notice in staging: they are a payment that left, or an owner who
lost their seat, and neither can be undone.

---

## The two planes

The platform already splits **money** (`banking`) from **identity/ownership**
(`concierge`). The consilium respects that split rather than collapsing it:

| Decision | Authorizing plane | Why there |
| --- | --- | --- |
| Move, reprice or re-own the platform's own money (a payment out of an allocation's claim, a valuation override, fee terms, a holder grant, a seed) | **banking** (`piggybank-core`) | The money plane must be able to _audit its own authorization_. `docs/ARCHITECTURE.md` explicitly rejects letting a concierge-signed artifact authorize money movement; a consilium verdict is exactly such an artifact, so it is computed, stored and verified in the money plane. |
| Remove a fund owner | **concierge** | Ownership is `Role::Owner`, a concierge-owned fact. Only concierge may mutate it, and the one-way bridge (concierge → banking) must stay one-way. |

Neither plane trusts the other's verdict. What crosses the seam is only:

- **the roster**, which banking already mirrors locally (`0016_user_role.sql`) — so a
  consilium never needs a live call to concierge at the moment it is authorized;
- **outbound mail**, which banking asks concierge to send over one new
  service-token-gated RPC, because concierge already owns the only mailer
  (`lettre` + `notification_deliveries` + backoff + daily budget), and standing up a
  second one would violate the "no hand-wired vendor SDK" rule.

That RPC takes a **typed** payload, never rendered HTML. A compromised money plane
must not become a phishing cannon pointed at the owners' mailboxes.

---

## Quorum arithmetic

Let `N` be the number of fund owners at the moment the request is opened.

```
threshold = floor(N / 2) + 1        # strictly more than half of ALL owners
voters    = owners \ { initiator }  # the initiator gets no vote and no token
```

The initiator is counted in the denominator but cannot vote. This is deliberate: if
opening a request removed you from the denominator, opening one would be a way to
lower the bar you have to clear.

| N   | threshold | voters | outcome              |
| --- | --------- | ------ | -------------------- |
| 2   | 2         | 1      | **impossible** — refused at open |
| 3   | 2         | 2      | unanimous peers      |
| 4   | 3         | 3      | unanimous peers      |
| 5   | 3         | 4      | 3 of 4               |
| 6   | 4         | 5      | 4 of 5               |
| 7   | 4         | 6      | 4 of 6               |

**A fund with fewer than 3 owners can never carry a consilium.** That is a real
consequence of the chosen rule, not an oversight, and it is why owner removal
enforces a floor (below). The open RPC refuses with an explicit error rather than
creating a request that can never reach quorum.

### The initiator is always an owner — and it takes two checks to say so

The table reads `N` as the owner count and `voters` as `N − 1`, which is only correct
because **the initiator is necessarily one of the `N`**. That fact is enforced in a place
the RBAC matrix does not suggest, so it is worth stating plainly:

- The **RPC boundary** gates each open on an operator permission — `OpenHolderGrant` on
  `bank:consilium:manage` (the owner-surface permission, `services/consilium.rs`), `OpenValuationOverride` on `ValuationPost`, `OpenPayment` on
  `PaymentOpen`, `ScheduleFeePolicy` on `AllocationManage`, `SeedCapital` on
  `CapitalManage` — and the matrix (`domain::authz::grants`) grants those to **`Admin`
  *and* `Owner`** — role-granting is the identity plane's concern, so the money plane
  treats the two as equivalent capability-wise. An admin with no seat therefore clears the
  boundary.
- The **domain** then refuses them: `Consilium::open` rejects an initiator absent from the
  snapshotted roster with `Forbidden("only a fund owner may open a consilium")`, covered by
  the `only_an_owner_may_open` unit test.

So the permission is necessary but not sufficient, and there is **no case in which a
non-owner opens a consilium and all `N` owners vote**. `voters = N − 1` always.

This asymmetry is deliberate and load-bearing, and it looks like an inconsistency to anyone
reading only the matrix — which is exactly the risk. Do not "reconcile" it by letting the
domain accept a non-owner initiator: that would make `voters = N`, silently change the
arithmetic the table describes, and let a principal with no seat move the owners' money
by proposing it. If the two ever need to agree, tighten the matrix, not the domain.

### Owner removal is a different rule

A removal passes when **either**:

- **(a) the target accepts it** from their own mailbox, or
- **(b) every owner except the target and the initiator votes to remove.**

Path (b) is unanimity over `owners \ {target, initiator}`. With exactly two owners
that set is **empty**, and "everyone in an empty set agreed" is vacuously true — which
would let either owner unilaterally expel the other. Path (b) therefore additionally
requires **at least one eligible peer voter**; with two owners only path (a) exists.

**Two different floors live in this feature, and they are not interchangeable.** The
**consilium** floor is **3** — below it `floor(N/2)+1` is unreachable for a consilium of
any kind, and it is what `OwnerList.below_payout_floor` reports (the field kept its name).
The **removal** floor is **2** — how far the roster may actually shrink. A reader who
assumes one constant will get one of them wrong; a fund of exactly 2 is legal, and simply
cannot carry a consilium until it admits a third.

**Floor.** A removal may not leave fewer than **2** owners. Dropping to 2 does suspend
every consilium — the table above shows why — but that is a *recoverable* state: two owners can
still admit a third and resume. An earlier draft of this policy set the floor at 3, and
it was wrong: at exactly 3 owners it made a bad actor unremovable forever, because
removal was blocked by the floor and admission (below) needs their agreement. A
recoverable pause beats a permanent deadlock. Resignation (removing yourself) needs no
consilium but is subject to the same floor.

### Admission is governed too — otherwise none of this holds

`SetRole` lets any single owner grant `Role::Owner`. Left alone, that defeats the whole
mechanism: a bad actor mints sock puppets *before* opening an invoice, and then reaches
quorum legitimately. With 2 honest owners and 4 puppets, N = 7, threshold = 4, voters =
6 — the four puppets carry it. Snapshotting the roster at open (pitfall 3) does not help,
because the stuffing happens beforehand.

So granting ownership is itself a consilium, and `SetRole` refuses to grant `owner`
outside it. **Admission passes on unanimity of every owner except the initiator, and
requires at least one such peer** — the same shape as removal path (b). Unanimity, not a
majority, because a minority must never be able to grow itself into a majority.

That closes the loop: no minority can add owners, and no minority can remove them.

`SetRole` therefore has **no carve-out at all**: it refuses to grant or strip `owner`
under every roster size, including an empty one — the bootstrap emergency access described
under [Genesis](#genesis--how-the-first-owners-are-seated) does not lift this. There are
exactly **two writers of `Role::Owner`** in the platform: the genesis seeding, and the
consilium, which performs its own write so there is no "an admission exists, therefore
grant" check to replay. Any third writer would be the whole mechanism's back door.

| Fund state | What is possible |
| --- | --- |
| 2 owners | admit a third (both must agree); no consilium of any kind; no removal except self-acceptance |
| 3 owners, one bad actor | the two honest owners remove them (peer set is non-empty, unanimity of one), leaving 2; then admit a replacement |
| 3 owners, two colluding | the same move, pointed the other way — they can expel the third and then admit whoever they like. The consilium supermajority does not protect a minority against a majority, and nothing here does. The cooling-off period below is what makes it visible. |
| N ≥ 3 | consilia at `floor(N/2)+1`; removal and admission as above |

**Residual, and stated honestly:** a fund of 2 whose owners disagree is stuck until one
of them accepts removal — there is no third party to break the tie, and inventing one
(a platform admin who can overrule owners) would hand that admin the fund. We prefer the
deadlock to the backdoor.

### What the composition actually guarantees — read this before trusting the table

Each rule above behaves exactly as written. Their *composition* is weaker than the
strongest of them, and it would be dishonest to leave that implicit.

The consilium rule alone is a supermajority: reaching quorum needs the initiator plus
`threshold` others, i.e. `floor(N/2) + 2` of `N` — unanimity at N=3 and N=4, 4 of 5,
5 of 6, 5 of 7. No minority can clear it. But the roster is reachable by a lesser bar:

1. Owners A, B, C. A and B are a bare majority and **cannot** carry a consilium — at N=3
   it needs both peers, and C would refuse.
2. A opens a removal of C. The peer set is `{B}`, so **one** REMOVE carries it. C is
   expelled, and C never had a vote that mattered.
3. N=2. A admits a puppet with B's agreement. N=3. Repeat until the roster is theirs.
4. A opens a payment out of `service:fee`. It now passes on the rule as written.

So the operative bound is not "a supermajority of owners" — it is **"any group that can
survive one removal round"**, which at N=3 is a bare 2 of 3. This is majority governance.
Nothing in this document protects a minority of owners against a determined majority,
and no arrangement of quorums can: whoever holds the majority holds the roster.

**What we do instead of pretending otherwise — a cooling-off period.** An executed
admission or removal blocks opening **any** consilium for 48 hours
(`require_settled_roster`, every kind: `application/consilium.rs`, `fees.rs`,
`payments.rs`) and voids any consilium already open (`void_open_for_roster_change`). That
does not stop the capture above; it makes it *visible before the money can move*. The
expelled owner receives their removal mail and has two days in which no consilium can be
authorized — time to raise it with a human, or to act. Capture becomes noticed rather than
silent and instant, which is the honest thing a mechanism can buy here.

The cost is real and accepted: a legitimate roster change also delays a legitimate
consilium by two days.

---

## Genesis — how the first owners are seated

The admission rule cannot bootstrap itself. It needs a non-empty peer set, so it can
never seat the *second* owner: a fund of one could never grow, and a fund of zero could
never start. Something outside the rule has to seat the founders exactly once, and a
brand-new deployment has to be operable in the minutes before that happens.

**The persisted `users.role` column in concierge is the single source of truth about
ownership.** Ownership is a stored fact and nothing else computes it. Both mechanisms
below are keyed off one predicate — *is the persisted owner roster empty?* — and both
switch off permanently the first time the answer becomes no.

### The genesis list

**`OWNER_SUBJECTS` (concierge) seats the founders, once.** At startup, *if the persisted
owner roster is empty*, every id on the list that has a user row is written to
`role='owner'` through the ordinary write path, emitting a `ROLE_CHANGED` into the outbox
like any other grant. Nothing is special about the resulting rows: they are seats, indexed
and counted like any other. Ids with no user row are skipped — the list seats people who
already exist, it does not conjure them.

**Seeding refuses to run if fewer than two ids resolve.** A one-owner fund is not a
smaller version of a working fund; it is a dead end. Admission requires a non-empty "all
owners except the initiator", so a sole owner can never admit the second, and no API path
can lower a roster of one back to zero to let genesis retry. A fund seeded with one owner
is unrecoverable. Seating nobody and saying why in the boot log is strictly better, and is
what happens.

**After genesis the list is inert forever.** The condition is "the roster is empty", and
the roster can never become empty again: `MIN_OWNERS = 2` is a floor that both expulsion
and resignation refuse to cross. "Empty the roster so the seed fires again" is unreachable
through any API, and editing `OWNER_SUBJECTS` on a live deployment does nothing at all.
It is a bootstrap, not a standing privilege.

### Emergency access, and why it is a one-way door

Concierge keeps an environment-driven emergency elevation for the same window — an
operator has to be able to reach the console of a system that has no owners yet — but it
is **self-extinguishing**: it applies only while the persisted owner roster is empty. The
instant the first owner exists, the list means nothing, and no API call can put the roster
back to zero, because both routes out of ownership stop at the `MIN_OWNERS = 2` floor. The
first successful genesis closes the door permanently. This is what makes an environment
variable an acceptable bootstrap: it is not a privilege that can be re-armed, it is a
privilege that expires on first use and cannot be restored from outside the machine.

On a deployment whose genesis list resolves, the window is closed by the same boot that
opens it. It stays open only in the case it exists for: `OWNER_SUBJECTS` unset, or too few
of its ids resolving to seat a viable roster — the state where someone has to log in and
fix the configuration, and there is by definition no owner to authorise them.

**It cannot be used to stuff a quorum, by construction.** Quorum stuffing needs a roster
to add seats to; the moment a roster exists, elevation is already off. And even inside the
window `SetRole` refuses to grant `owner` **unconditionally** — including on an empty
roster. Emergency access confers `operator`/`admin` capabilities. It does not seat owners;
only genesis seeding and the consilium do.

**While it is active, it says so out loud.** The API marks a role that came from emergency
access as such, and the console renders it as emergency access rather than as ownership.
This is the part worth carrying forward as a lesson rather than an implementation note:
**the original bug was never that the elevation existed — it was that it was invisible.**
The admin console drew "Owner" for a subject the consilium counted as nobody, because one
surface read a per-request effective role and the other read the stored roster. Two
surfaces disagreeing about who owns the fund, with neither admitting it, is how an
operator ends up trusting a seat that does not exist. A temporary power that names itself
is auditable; the same power wearing the same label as a real seat is a trap.

### The money plane has no emergency access at all

Banking's `ADMIN_SUBJECTS` is gone and nothing replaced it. The money plane learns who
owns the fund only from the bridge — a `ROLE_CHANGED` event mirrored onto `users.role` —
and its RBAC gate reads that column and nothing else: no local row means no privilege, and
no environment variable can add one. That also removes the second UUID list an operator
previously had to keep synchronised by hand, across two id spaces that hold different
values for the same person (concierge ids and banking ids).

**The consequence is that on a fresh installation banking's owner surfaces are dead** —
no consilium can be opened, no governance RPC answers — until concierge has seated the owners
and the bridge has carried `ROLE_CHANGED` across. That is the design, not a defect: the
identity plane seats, the money plane follows. There is deliberately no way to make the
money plane believe in an owner the identity plane has not persisted.

**So deploy order is load-bearing: concierge first, banking second.** Ship banking first
and you get a window in which the money plane has no owner and no fallback. Ship concierge
first and the window is empty: the roster is already persisted and mirrored by the time
the money plane starts asking.

**Residual, stated plainly:** whoever controls the environment at the moment of the very
first boot chooses the founders and holds emergency access until they are seated. That is
unavoidable — someone has to start the system — and it is bounded three ways: the window
closes on first genesis and cannot reopen, the elevation inside it cannot grant ownership,
and everything it does is labelled as emergency access while it happens. From the second
boot onward that operator has no path to ownership that does not go through a consilium of
the owners they seated. They can still suspend or disable an owner, which is a lesser
power: it can withhold a quorum, never manufacture one.

---

## What can go wrong, and what closes it

### Roster

1. **Roster drift mid-vote.** The eligible voter set and `threshold` are snapshotted
   when the request is opened. Votes are additionally re-validated at execution: an
   approver who is no longer an owner does not count. Changing the roster can
   therefore only make approval _harder_, never easier.

   **This is eventual, not instantaneous, and the bound is the bridge.** The money plane
   counts against its own mirror of `users.role`, fed by the asynchronous
   concierge → banking lifecycle pull (`BRIDGE_POLL_SECS`, 5s nominal, unbounded if the
   bridge is wedged). An owner removed in concierge stays countable in banking for that
   lag, and because their vote arrives by emailed token rather than a session, revoking
   sessions does not stop them. Monitor bridge lag; treat a wedged bridge as a
   governance incident, not just a staleness one.
2. **Quorum-lowering.** Kicking owners after opening a consilium cannot shrink the
   frozen `threshold`, and the kicked owner's vote is voided by (1).
3. **Roster stuffing.** An owner added after the request opened is not in the
   snapshot and gets no token, so new owners cannot be minted to reach quorum.
4. **The initiator's vote.** Enforced by _never minting them a token_, not by a check
   at submit time. There is no code path that can accept it.

### The emailed link and the code

5. **Mail scanners click links.** Gmail, Outlook SafeLinks and corporate gateways
   issue automatic `GET`s on every URL in a message. So the `GET` is strictly
   read-only: it renders a redacted summary and nothing else. The vote is a `POST`
   carrying the **secret code**, which only a human who opened the message can type.
   This — not two-channel security — is what the code is actually for; it turns a
   scanned link into a deliberate act.
6. **Token leakage** via `Referer`, proxy logs and browser history. The approval pages
   send `Referrer-Policy: no-referrer`, the token is single-use with a 72h TTL, and on
   its own it can only _read_.
7. **Code brute force.** 10 characters from an unambiguous alphabet (Crockford
   base32 minus `I L O U`) ≈ 46 bits. Five failed attempts burn the token and notify
   every owner. The attempt counter is incremented **in the same transaction, before
   the comparison** — otherwise concurrent requests slip past the limit — and it is
   incremented **only for a seat that has not yet answered**. An unconditional
   increment is incompatible with the `CHECK (attempts <= 5)` that backs the ceiling: a
   seat that answered correctly on its fifth try sits at exactly 5 without burning (the
   correct-code path never burns), and the next retry would violate the constraint and
   turn that seat's every future request into a 503.
8. **Timing side channels.** Token and code are compared in constant time, as hashes.
9. **Secrets at rest.** Only SHA-256 of the token and of the code are stored. The
   plaintext code exists in the concierge delivery row until the message is sent, and
   that payload is nulled on success.
10. **Enumeration.** Unknown, expired, spent and wrong-state tokens all produce one
    identical response. A caller cannot tell which they hit.
11. **Replay.** `UNIQUE (request_id, voter_user_id)` plus a one-shot `decided_at`. A
    repeat of the _same_ decision is an idempotent no-op; a _different_ one is refused.

#### One specification for both planes

Both planes implement an emailed-token flow, and they must not be extracted into a
shared crate — the plane split forbids sharing this state, and the rules genuinely
differ (a majority threshold over N tokens here, unanimity over one token there). But
duplicated code with *two* specifications is what bites. So the behaviour is specified
once, here, and each plane's tests assert against this table:

| Situation | What happens |
| --- | --- |
| Reading an invitation | Costs nothing. Never consumes an attempt, in either plane. |
| Correct code | The vote is recorded. The attempt counter is **not** reset — a token that has been guessed at stays closer to burning. |
| Wrong code | Counts an attempt, and answers `INVALID_ARGUMENT` with the remaining count. This is not an enumeration oracle: it is only reachable by someone already holding a valid, live, unspent token. |
| 5th wrong code | The token burns permanently and every owner is notified. The counter is clamped at 5, so it can never exceed the ceiling its own `CHECK` constraint enforces. |
| Token against an already-closed request | Refused **before** an attempt is counted, so a mail scanner cannot spend a human's budget. |
| Repeating the same decision | Idempotent success, and it costs **no** attempt — a seat that has already answered is charged nothing, so a double-click can never walk it to the burn ceiling. |
| A different second decision | Refused; the first answer stands. |
| Unknown / expired / spent / burned token | One identical `NOT_FOUND`, indistinguishable from each other. |

### Binding the vote to what was actually approved

12. **Editing the invoice after approvals.** There is no edit RPC — the request is
    immutable. `payload_hash` is stored at open and re-verified at execution, and the
    email shows the full amount, asset, network and destination address plus the
    hash prefix, so an owner approves a thing they can see. Changing anything means
    cancel and reopen; votes are not carried over.
13. **Truncated addresses.** The destination is rendered in full, monospace. A
    `0x1234…abcd` in an approval email is an invitation to approve the wrong address.

### Executing the money move

14. **TOCTOU on the tally.** The count and the transition to `Approved` happen in one
    Postgres transaction with `SELECT … FOR UPDATE` on the request row.
15. **Double execution.** Idempotent per kind: a mark's id
    `uuid_v5(…, "consilium:valuation-override")`, a grant's key `holder-grant:<consilium>`
    (`issuance::grant_units`), a seed's subscription id `seed_subscription_id(tx_ref)`; a
    payment's `record_approval` and a fee change's `schedule_approved` are idempotent on
    the row. The effect is written once (`ConsiliumEffect`, projected onto
    `executed_*_id`).
16. **Concurrent approved consilia overdrawing one claim.** At most **one** open
    consilium **per source claim** — `consilium_single_open_per_source_idx ON
    consilium (source_claim) WHERE state = 'open'`. `source_claim()` per kind
    (`domain/src/consilium.rs`): a payment — the order's source; an override — the
    product's `service:<id>`; a fee change — `FeeShares(svc)`; a grant —
    `service:<fee|fund>`; a seed — `service:fund`. What the index does not do is block a
    request over a different claim. This
    removes the race rather than trying to win it. Insufficient cover at execution is
    still handled: the existing solvency Read-First rejects it, the request lands in
    `ExecutionFailed` with the reason visible to owners, and nothing retries silently.
17. **Stale approvals.** Requests expire after 72h. An expired request can never
    execute, even if a vote arrives late.

### Owner removal

18. **Vacuous unanimity** with two owners — closed by the "at least one peer voter"
    rule above.
19. **The last owners.** The `MIN_OWNERS = 2` floor keeps a fund from being emptied of
    governance entirely, while staying recoverable. Both expulsion and resignation
    refuse to cross it, which is also what makes the genesis list inert after the first
    boot: the roster can never return to empty, so the seed can never fire twice.
20. **Mutual expulsion race.** At most one open removal per target; at execution the
    initiator's own ownership is re-checked, so a removal opened by someone who has
    since lost their seat is void.
21. **Sock-puppet quorum.** Ownership cannot be granted by one owner: `SetRole` refuses
    `owner` under every roster size, with no bootstrap window to slip through — the
    admission consilium performs its own write, and unanimity of the other owners is the
    only route. Exactly two writers of `owner` exist (genesis seeding, the consilium);
    a third would make every other control here decorative, because a bad actor could
    simply mint the majority they need before opening an invoice.
22. **A privileged role that hides where it came from.** The money plane computes no
    effective role at all: its RBAC gate reads only the bridge-mirrored `users.role`, so a
    caller with no local row holds nothing and no environment variable can add one. Its
    `ADMIN_SUBJECTS` allowlist was removed for exactly that reason — it minted owners the
    consilium could not count, and a second UUID list an operator had to keep synchronised
    across two id spaces by hand. Concierge does still elevate from configuration, but only
    while the owner roster is empty, never to `owner`, and never silently: the API and the
    console both label such a role as emergency access. An unlabelled temporary power is
    the failure this whole item exists to prevent.

### The live consilium page

23. **The socket is not the source of truth.** It carries a version number, never a
    tally. The client re-fetches the authoritative snapshot when the version moves, so
    a stale or spoofed frame can never render a wrong count.
24. **Authorization at handshake.** The websocket verifies `ev_access` exactly as the
    REST routes do, and closes when the token expires.
25. **No secrets on the wire.** No tokens, no codes, no email addresses.
26. **Multiple replicas.** Correctness never depends on the in-process broadcast: the
    stream also re-reads on an interval, so a second replica cannot go blind.
27. **CSP and deadlines.** The websocket origin is added to `connect-src`, and the
    route is mounted outside the BFF's request-timeout layer.

### There is no way around it

28. **The direct payout RPC.** HISTORY. `BalanceService.RequestRevenuePayout` used to
    move the fund's revenue on ONE Admin/Owner's say-so, gated on
    `Permission::RevenuePayout` — the same permission that merely lets someone *open* a
    consilium. Any principal who could propose a payout could equally well skip the
    proposal and take the money, so the whole mechanism was decorative. Both
    `BalanceService.RequestRevenuePayout` and `ConsiliumService.OpenRevenuePayout` are
    gone from the contract with #245: there is no direct payout and no payout consilium;
    the fund's earnings are the `fee` allocation, held by people. `CancelRevenuePayout` /
    `ListRevenuePayouts` were removed with it (C-9); production held no payout row.
29. **A mechanism that silently does nothing.** Every approval token reaches its owner
    through exactly one route — the `consilium_mail` queue drained into concierge. On a
    build where that seam is not compiled in, a consilium would open, mail nobody, and
    expire 72h later having been unvotable from the first instant, while every screen
    reported it healthy. Opening now **refuses** when no mailer is wired. Refusing at
    open rather than at boot is deliberate: refusing at boot would take the whole money
    plane down over a feature that is off by default and orthogonal to all of it.

---

## Payments

A consilium authorizes a decision over the owners' money; a **payment order**
(`domain/src/payments.rs`, `PaymentsService`) generalizes the money move without
generalizing the authorization: it names two ends of the platform and an amount, and
the policy below decides who must agree. The consilium stays exactly what it is — a payment
out of fund-owned money simply opens one.

### Three tiers, derived from the destination

| destination | tier | effect on execution |
| --- | --- | --- |
| an external wallet address | **L1** `external` | a `Withdrawal` (the ordinary saga: queue, dispatch, watchers, reaper) |
| a product's pooled claim `service:<id>` | **L2** `service` | one posted transfer |
| `user:<id>` (the retired `piggybank` / `revenue` kinds are refused by name at the RPC, `services/payments.rs`) | **L3** `internal` | one posted transfer |

The tier is never supplied. A caller-supplied tier would be a second statement of a fact the
destination already makes, and the only interesting failure is the one where the two disagree.
An external *source* is unrepresentable: money cannot arrive from an address by anyone's
say-so — that is a deposit, which a chain watcher attests.

An **L1 payment leaves only from an investor's claim.** An allocation's pooled money — a
product's or the reserved `fee`/`fund` — has no `WithdrawalSource` (#237) and `OpenPayment`
refuses it: cash leaves a reserved allocation through a holder's redemption and their own
withdrawal (`application/payments.rs`).

### The requirement is a function of the SOURCE, and nothing else

| source | who must agree | how |
| --- | --- | --- |
| `service:<id>` — a product's, or the reserved `fee` / `fund` (`domain/src/payments.rs`) | the **owner consilium**, at every tier | opened inside `OpenPayment`; `ConsiliumTerms::Payment` carries the order's id inside the hashed subject |
| `user:<id>` (an investor's own claim) | **that investor**, at every tier | one emailed consent seat — token, code, 72h, five attempts, the table in "One specification for both planes" |

There is deliberately **no cell in which one admin moves fund money alone**. `service:fee → user:<x>`
on a single say-so would be the retired revenue payout reopened through the back door: the
recipient can then withdraw under their own authority. The matrix
(`bank:payment:open`, Admin and Owner) gates *proposing*; the source decides who
authorizes.

Everything the execution will check is checked at **open**, so nobody spends 72 hours
approving an order that fails the moment it is carried: the source covers the amount now
(posted minus reserved), an external shape is possible on the rail (configured, above the
minimum, the investor active and verified), and a consent subject holds a verified mailbox
and a mirrored identity-plane id — without the latter no consent mail can be addressed, and
an order nobody can be asked about must not exist.

### The consent seat's two pins

A consent is answered by someone who is not signed in, up to 72 hours after it was mailed. Two
facts are frozen at open beside the seat and re-read, fail-closed, both when the code is
compared and again under the order's lock at execution:

- the subject's folded revoke floor (`GREATEST(concierge_token_version, token_version)`) — so
  `RevokeTokens` on **either** plane voids a consent already in flight;
- SHA-256 of the subject's mirrored `users.email` — so a mailbox change at the identity
  provider cannot redirect a live token to an address that is no longer theirs.

A pin that has moved rejects a pending order at consent (no attempt is charged; the holder is
told why, because holding a live token already proves the seat exists) and fails an approved
one at execution, releasing an L2/L3 reservation on the way out.

**The L1 window.** Execution reads the pins, creates the withdrawal, then records the effect.
A revocation can land between the first and the last, and by then a withdrawal exists. Two
things close that window, and neither works without the other:

- **A payment's withdrawal is never dispatched on creation.** A self-service withdrawal on a
  liquid rail leaves for custody in the same transaction that records it, and past `Queued`
  nothing may void it (the broadcast may have landed — the cardinal rule). A payment order
  therefore creates its withdrawal `Queued` whatever the rail holds, and the dispatcher
  sends it on its next sweep — which also means every payment withdrawal passes
  `require_dispatchable`: the pause, the freeze and the verification floor are re-read at
  the moment the money leaves, not only when the order was opened 72 hours earlier.
- **`record_execution` re-reads the pins under the order's lock**, after share-locking the
  subject's `users` row so a revocation still committing is waited for rather than read
  around, and, in that same transaction, **cancels the withdrawal it is refusing**.

What this does NOT cover: a withdrawal the dispatcher has already sent. Between the sweep
that dispatched it and the record that refuses it, the pins can still move, and then the
effect that exists is recorded as it is and the movement of the pins is logged at error,
not lied about. That gap is the dispatcher's cadence, not 72 hours — and the money that
left in it left through the same policy gate every other withdrawal passes.

### Execution

Only an `approved` order executes. Both approval paths and the sweeper reach `execute`, and
each of these makes it safe to reach more than once:

- the payload hash is re-taken over the stored terms; for a consilium-backed order the
  **order row** is re-hashed against the consilium's `payload_hash` too, and the approval is
  recorded against the consilium's id checked under the order's lock against the
  `payment_approval` link — a quorum over one order can never carry another;
- an L2/L3 order's reservation (`Dr <source> / Cr clearing`, raised on approval) must have
  **actually landed** before the settlement is recorded: `saga_steps` is asked for the
  reserve's deterministic transfer id and TigerBeetle is asked whether it holds it. Not yet
  means nothing is recorded and the next sweep asks again. A reservation the ledger
  **refused** — the outbox row is parked — fails the order rather than waiting forever; the
  operator sees the row in `/api/admin/outbox/parked`, and unparking it re-drives the relay;
- an L1 order's withdrawal id is `uuid_v5(payment_id, "payment:withdrawal")`, and a refusal
  from the withdrawal path is re-read against that id before being believed — the loser of
  the two-caller race records the withdrawal that exists, not a phantom failure.

`execution_failed` is terminal and retried by nothing: the reason is what the initiator (and,
for a consilium, every owner) reads.

### Decided over concierge's approvals (the default)

Org restructure plan §4.4 moves the decision out of this plane. `PAYMENT_APPROVALS`
(`config::PaymentApprovalMode`, read once at boot) says how a NEW order is seated; unset or
`concierge` is the default, `consilium` is the rollback. An order keeps the seat it was opened
with, and every part of the old path keeps running in both modes.

| source | seat (default) | seat (`PAYMENT_APPROVALS=consilium`) |
| --- | --- | --- |
| fund-owned | a `bank:payment` approval in scope `main`, decided by the platform owners (`payment_concierge_approval`) | banking's consilium (`payment_approval`) |
| an investor's claim | that investor's own answer, signed in (`payment_session_consent`) | the emailed token and code (`payment_consent`) |

**Fund money.** The request that opens the order proposes the approval in the same breath,
WITH THE PROOF of who asked (D24(1)): the initiator's own concierge access token in
`x-ev-user-token` and their `auth_subject` as `requested_by`. With it concierge keeps the
initiator off the owners' roster, so nobody votes on their own payment. Without a token the
order is not written; if the proposal does not open (concierge away, a stale token, a hash
that disagrees, too little window) the order is closed with the reason and the source is free
to open again. The executor never proposes — the proof is one token for one request and is not
kept — and closes an order whose request died before it did. The executor
(`application/payment_approvals.rs`, run by the consilium sweeper's minute) watches the
approval and — once it passed — re-reads it by id, checks kind, scope `main`, `subject_ref`
and the payload hash against the stored order, approves the order **once**
(`record_concierge_approval`, under the order's row lock), runs the same `execute` as every
other approval, and reports the outcome. A decision is served until its outcome is reported,
so every step is idempotent: a second approval for one order, a decision for an order that
ended otherwise, and one whose hash is not the order's are reported `VOID` and move nothing.
The decision's `execute_by` is stored with the seat and an order is not executed after it
(`execution_failed`); a refusal by concierge to take an outcome is kept on the seat
(`last_error`). The order's window is 7 days (`CONCIERGE_TTL_SECS`): 72 hours open, a veto
delay of 24 hours (up to 72 by policy), and 72 hours to execute. Treasury spending stays
closed (`TREASURY_SPENDING_CLOSED`).

**The proof's freshness.** Concierge accepts the token only if minted within 120 seconds. The
cabinet session renews its access token (15-minute TTL) only when it is about to expire, so the
console cannot make one fresh on demand: until the identity plane offers a forced renewal, an
open with an older token fails with that reason. That is why the first rollout sets
`PAYMENT_APPROVALS=consilium` and switches only once a fresh-token path exists.

**Rolling back.** The way back is the flag, never the image. Set `PAYMENT_APPROVALS=consilium`
(new orders use the old path), wait until every order seated on `payment_concierge_approval` /
`payment_session_consent` has ended (the query is in migration 0066's header; they end by
themselves within 7 days), and only then consider anything else. An image older than this
release cannot propose: concierge already holds a newer catalog. (The catalog gate logs that
refusal at error and proposes on, so `bank:*` operations are not stopped on top of it.)

**Investor money.** No token and no code: the order is seated on the investor, a notice
(`bank:money-movement`, through concierge's scope inbox) tells them, and they answer in the
cabinet (`PaymentConfirmationService`, BFF `/api/wallet/payments/**`). Approving needs a fresh
sign-in (`step_up_required`), declining never does. The pins are the mailed seat's: sessions
revoked or the mailbox changed since open void the answer and, later, the execution.

**Notices.** `PaymentConsent` and `PaymentOutcome` become `EmitScopeNotification` on topic
`bank:money-movement`, written to `scope_notification_outbox` in the transaction of the fact
and delivered by a singleton worker. `SendGovernanceMail` is not on this path at all.

### Mail

Both approval mails leave through the same `consilium_mail` queue, written in the same
transaction as the seat they carry a token for, drained by the singleton worker into
concierge's relay. Two kinds, two templates: `PAYMENT_APPROVAL` (two ends in words, for the
owners) and `PAYMENT_CONSENT` (the same, for the one investor whose money it is); a holder
grant and a seed borrow `PAYMENT_APPROVAL` (§ Holder grant and seed capital). The retired
revenue payout's `PAYOUT_APPROVAL` left `GovernanceMail` and the `consilium_mail.kind` CHECK
with #245 (`0047_ownership_contract.sql`). The consent mail names the subject's
identity-plane id in its typed payload, and concierge refuses it unless that id is the
addressee, so the money plane cannot fan one consent out to a second mailbox. Beside the canonical label the destination carries what a
person recognises it by — the receiving investor's masked mailbox, the product's title — so
"investor 8f3e…" is not approved for the wrong person; the label alone is what the digest
binds.

A consent that burns (five wrong codes) or is voided by a moved pin closes the order —
`rejected` while it was pending, `execution_failed` (reservation released, a still-queued
withdrawal cancelled) once it was approved — and, in that same transaction, queues a
`PAYMENT_OUTCOME` notice (#238) to **the investor whose money it was and the staff member
who opened the order**, one copy each (one when they are the same person), each under its
own key `payment:<id>:outcome:<recipient>`. **`EMAIL_CHANGED` goes to the initiator only**:
the investor's copy would reach the mailbox that just replaced theirs — possibly
unverified, possibly an attacker's — so it is not sent, and an investor who opened the
order on their own money is told nothing by mail. An order still `approved` when the void is
found — at the L1 window, or by an execution retried after an earlier attempt created the
withdrawal and died — has that withdrawal cancelled in the same transaction while it is
still `Queued`; one already dispatched is recorded as executed and logged at error, and
nobody is told nothing moved. Finding no withdrawal at the void does not mean none will
exist: `execute` has two callers and creates the withdrawal outside the order's lock, so
the one that found the consent still valid can insert it after the other has already
closed the order. That late withdrawal's record is refused, and the refusal cancels it
in the same transaction while it is still `Queued` — over any closed order (failed,
rejected, expired, withdrawn), not only a voided one; dispatched before that, it is logged
at error for an operator to reconcile. That cancel runs only if the late caller's record
does: if its process dies (or its transaction fails) between creating the withdrawal and
recording it, nothing revisits a closed order and the withdrawal stays `Queued` — the
dispatcher refusing a payment's withdrawal whose order is no longer `approved` is the
remaining guard, tracked in #443. **The notice never holds up the transition it reports.** It
is written inside a savepoint: if queuing it fails for any reason, only the savepoint is
rolled back, the failure is logged at error with the payment id, and the burn, the
rejection, the void of the withdrawal and the order's failure commit regardless — a broken
mail queue must not undo a burn and its attempt. The notice says what died and why in closed
words concierge phrases itself — `TOKEN_BURNED`/`WRONG_CODES`, or `INVALIDATED` with
`SESSIONS_REVOKED` or `EMAIL_CHANGED` — beside the tier, the two ends as the consent
invitation named them, the amount spelled as money (`1200.5 USDT`) and the order's id; it
carries no link and no code. Every copy names the investor by their identity-plane id, and
concierge sends it only to that person or to an admin/owner, so it cannot be aimed
elsewhere. Only the transition that closes the order writes it: a retried submission, a
second execution attempt or a sweep over the closed order finds it closed and queues
nothing, and the keys make a replayed write a no-op. An investor who answers — approving
or refusing — an order that expires, one its operator withdraws, and a fund-owned order
its consilium refuses send no such notice; `PAYOUT_OUTCOME` stays the consilium's, to
seated owners only. The initiator also reads the verdict and its reason on the payments
screen. An order
closed without a verdict — withdrawn by its operator, or expired — takes back the consent
invitation the relay has not delivered yet, token and code blanked, before the order is
locked, exactly as a closing consilium takes back its invitations (#368).

---

## Valuation override

The third kind (`valuation_override`, banking#232) authorizes no money move at all: it
records a **NAV mark the move guard refuses**. It exists because the price is fund-owned
money by another name — every redemption settles at `units × NAV`, out of the fund's
pooled `service:<id>` claim — and the guard used to be lifted by a flag on the very
request it guarded. One admin could inflate NAV, redeem at the inflated price out of other
investors' cash, and withdraw. The flag is gone from the contract (`PostFundValuationRequest`
reserves field 3), and there is no other way past the guard.

### What the direct post can still do

`PostFundValuation` caps a move at `MAX_NAV_MOVE_PCT` (50%) against the previous mark AND
against the mark anchoring a rolling 7-day window — the newest mark at least that old, or
the fund's first mark while the fund is younger than the window. Against the previous
mark alone the cap was a rate limit with no rate (+49% seventeen times in an afternoon);
against the anchor it bounds what one poster can move the price by in a week to the cap
itself, however many steps they take.

### The terms, and what is deliberately not in them

`ValuationOverrideTerms { service, aum }`, under its own frozen domain prefix
(`banking.v1.ValuationOverrideTerms\0`) so a hash over a mark can never be a valid
signature over a payout or a payment. The **NAV is not frozen into the terms**: it is
derived from the live unit supply at execution, because units can be minted or burned
during the 72h vote and a NAV frozen at open would misprice the fund the owners approved
marking. The owners approve an AUM, which is the figure an operator actually knows.

### Gates

| moment | gate | why |
| --- | --- | --- |
| open | `ValuationPost` at the RPC boundary; the initiator must hold an owner seat (the domain refuses otherwise) | the poster proposes, the owners decide — the second actor is the vote, not a second permission that would recreate the flag one role over |
| open | a wired governance mailer; a settled roster (48h cooling-off) | the same two every kind applies: repricing every redemption is the same class of decision as moving the owners' money |
| open | the allocation exists (any state); `units_outstanding > 0` | so the vote is over a computable price, refused now rather than after 72h of approving |
| open | one open request per `service:<id>` claim | `source_claim()` is the fund's claim, so the override queues behind (or blocks) a payment out of the same claim — the owners must not vote on the price and on a drain of the pool at once |
| execute | the kind-agnostic checks every consilium passes (hash, live roster, grace, roster change) | unchanged |
| execute | `funds::record_valuation` — the SAME writer the direct post uses, guard not consulted | one writer, so the two paths cannot drift; the id is `uuid_v5(consilium_id, "consilium:valuation-override")`, so a retried execution finds the mark rather than appending a second |

`posted_by` on the recorded mark is the initiator's user id, in the spelling the direct
RPC records (`claims.sub`), so the redeem cooldown below binds the proposer exactly as it
binds a direct poster.

### The redeem cooldown

A subject who posted a mark for a fund within `VALUATION_REDEEM_COOLDOWN_SECS` (7 days)
cannot redeem from that fund — refused with `failed_precondition` at **request** and
again at **settle**, because settle is where the cash is priced and a queued redemption
can outlive a later inflating mark. The same cooldown binds `SettleFeeShares` on that
product (`application/fees.rs`, #245 M-1): converting one's own mark into the `fee`
holders' cash is the move it exists to stop. A reserved allocation takes no mark of its
own, so its cooldown is the cooldown on **every product it holds a class in**
(`refuse_recent_poster`, `application/funds.rs`, H-3).

### What was rejected

A hard ceiling `NAV × units_outstanding ≤ cash in the fund's claim`. In-kind products
(`piggybank/core/PATTERNS.md` § In-kind issuance) are valued at their asset with zero cash
in the claim; that invariant would make them unmarkable.

### Mail

Until concierge grows a dedicated kind (the follow-up to banking#232), the owners are rung
through the existing `PAYMENT_APPROVAL` template with every label spelled out — source
"<product> — NAV valuation", destination "AUM <n> USDT", amount = AUM, and a fixed
reason naming the guard. The mail is the doorbell; the approval page, which renders the
real terms, is the truth.
---

## Fee policy

The fourth kind, `ConsiliumKind::FeePolicy`, guards a product's fee TERMS rather than a money
move. It exists because one administrator could set a 100% management rate with no delay,
no version and no notice, and the sweeper would collect it (#233). The full policy — the
rate ceilings, the house envelope, what counts as tightening, the 24h notice and the
promotion that settles the old rate first — is in `docs/FEES.md` § "Changing the terms";
this section covers only how the kind sits in the consilium.

- **When a consilium is needed.** Exactly when the change TIGHTENS the terms AND lands
  OUTSIDE the house envelope (2-and-20 on invested capital, annual), or LOWERS the hurdle
  wherever the terms sit. Everything else is one `AllocationManage` holder's call and never
  reaches this aggregate.
- **Who may open it.** The requester, who must therefore be an owner. `ScheduleFeePolicy`
  refuses an administrator who is not one before writing anything — the alternative would
  be a consilium opened by a seat that does not exist.
- **The subject** is `FeePolicySubject { change_id, service, from, to, reason,
  requested_effective_from }`, domain-separated (`banking.v1.FeePolicySubject\0`) and hashed
  as every other kind is. The change's id is inside the hash, so an approval of one change
  is not a signature over another that names the same numbers; so is the requester's
  `reason` (required, non-empty — the approval mail is refused without one), so an approval
  given for one justification is not an approval of the same numbers under another. `from`
  is absent for a product that charged nothing, which the mail states differently from a
  policy whose rates are zero.
- **The source claim** is the product's `FeeShares(service)` — the account the new terms
  will collect into — so `consilium_single_open_per_source_idx` yields one open fee-policy
  consilium per product, and a payment over a different claim is not blocked by it.
- **Execution** is not a money move. Carrying the quorum moves the change from
  `awaiting_consilium` to `scheduled`, fixes `effective_from` at
  `max(requested, now + 24h)` (or `now` if the product has no holders), and enqueues one
  `FeePolicyNotice` to every holder — in the transaction that records the effect
  (`executed_fee_policy_change_id`). The sweeper then promotes the change into
  `fee_policies` once the moment arrives. The same execution gates apply as to every kind:
  hash re-verified, quorum re-checked against the live roster, roster change voids,
  staleness grace.
- **Every non-approval verdict** — rejection, expiry, withdrawal, voiding after a roster
  change, a failed execution — closes the change as `rejected` in the verdict's own
  transaction, exactly as a payment order is closed under its consilium.
- **Mail.** Owners are asked with `FeePolicyApproval` (`GOVERNANCE_MAIL_KIND_FEE_POLICY_APPROVAL`:
  the fund's display name, the current and the proposed terms, the reason, and the token).
  Holders are told with `FeePolicyNotice` (`GOVERNANCE_MAIL_KIND_FEE_POLICY_NOTICE`: the same
  two sets of terms, the moment they bind, and the cabinet-relative product page
  `/invest/<service>`, addressed by identity as a consent is). No outcome mail is sent for
  this kind: the owners read the verdict in the consilium room, and the holders learn of a
  carried change from their notice. Wiring an outcome template is tracked under #233.

---

## Holder grant and seed capital

The fifth and sixth kinds (#245) seat a person as a holder of the platform's own money — the
`fee` and `fund` allocations, hidden from the catalog and held by people through units.
Neither is a cash move out of the fund, but both change who owns it, so both are the owners'
quorum and never one administrator's write.

- **Holder grant** (`ConsiliumKind::HolderGrant`, `HolderGrantTerms { allocation, user,
  units }`, prefix `banking.v1.HolderGrantTerms\0`): mint `units` of a reserved allocation to
  an active, mirrored user. Priced at the allocation's live NAV at execution. Effect:
  `executed_issuance_id`, minted through `issuance::grant_units` under a key derived from
  the consilium (idempotent on retry). The operator's `IssueUnits` refuses a reserved
  allocation outright.
- **Seed capital** (`ConsiliumKind::SeedCapital`, `SeedCapitalTerms { tx_ref, network,
  amount, depositor }`, prefix `banking.v1.SeedCapitalTerms\0`): a chain-proven arrival on
  the treasury, from outside every wallet we control, attributed to `depositor` as their
  deposit and their subscription into `fund`. The AMOUNT is under the signature — the chain
  is consulted at open with it as an assertion, so the owners vote over a transfer that
  exists and is worth exactly that. Open also refuses a reference already booked, a
  depositor who is missing or not active, a closed `fund`, a stale price. Effect:
  `executed_subscription_id` — the subscription under `balance::seed_subscription_id(tx_ref)`,
  written by the same `balance::seed_fund_capital` the RPC used to call directly; it is
  idempotent by the reference, so the carrying vote and the sweeper cannot book twice, and a
  re-execution repairs a first attempt that died between the deposit and the subscription.
  `SeedCapital` (the RPC, `CapitalManage` admits to the door; the caller must hold a seat —
  checked FIRST at open, ahead of the chain and deposit-log gates, so an admin with no seat
  cannot probe them) OPENS this consilium over `depositor_user_id` (resolved like any admin
  target; empty = the caller) with `expected_amount` required, and answers
  `{recorded: false, amount, consilium_id}` (`balance.proto`).
- **Open gates for a grant:** a wired mailer, a settled roster, an active mirrored user, the
  allocation registered, its price fresh (`open_holder_grant`); execution re-runs the cap,
  NAV and active-user checks through `grant_units`, and the backing stays `cash`. A holder
  LEAVES only by redemption at the allocation's derived NAV; `IssueUnits`/`RetireUnits` and
  every registry write refuse a reserved slug (`refuse_on_reserved`). Effect ids on the
  wire: `executed_issuance_id`, `executed_subscription_id` (`consilium.proto`).
- **Source claim**, both: the allocation's own `service:<fee|fund>`, so one open grant per
  reserved allocation, one open seed, serialized against a payment out of that claim.
- **Mail.** Both borrow `PAYMENT_APPROVAL` for the invitation and `PAYOUT_OUTCOME` for the
  verdict, as the valuation override does (EV-invest/concierge#94 tracks the dedicated
  kinds): a grant spells its amount in units, a seed names the arrival by reference and the
  depositor by masked mailbox.

---

## Audit

Every vote records who, when, from which IP and user agent, and against which
`payload_hash`. Nothing is deleted: a rejected, expired or failed request stays
readable. The domain-event log carries the same facts, so the ledger and the
governance record can be reconciled after the fact.
