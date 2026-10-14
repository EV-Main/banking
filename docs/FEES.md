# Fees — the fund's 2 and 20

Two charges sit on a holding and they answer different questions.

**Management, 2% p.a. by default.** Rent on the capital parked in the product. It accrues
continuously with elapsed time and is charged whether the fund made money or not. Its base
is either the **invested capital** (the position's cost basis — the house default) or the
**market value** (`units × NAV`, the hedge-fund convention). Invested capital is the
default for one reason: it does not swell with a mark the operator posted, so the fee the
operator earns stays independent of the input the operator supplies.

**Performance, 20% of the gain by default.** A share of profit above a **per-investor
high-water mark**, crystallized at the end of a period (annual by default); crystallizing
again when an investor redeems is modelled but not wired (§ "Still open"). Optionally the
gain must first clear a **hurdle** accruing at `hurdle_bps` p.a. over the mark.

A product with no policy row charges nothing. The fee is opt-in per product, so it can
never appear on a fund whose prospectus did not promise it — and "no policy" is a different
fact from "a policy whose rates are zero", which is why `FeePolicy.configured` exists.

## Why the mark is per investor

A fund-level mark mutualizes the fee. An investor who subscribes after a drawdown rides the
recovery fee-free, while one who subscribed at the top pays on gains that only restored
their own loss. The industry's two fixes are *series accounting* (a new share class per
dealing day) and *equalization* (per-investor credits and debits against one class). This
ledger makes a third road cheap: the position projection already exists per
`(user, service)`, so the mark simply lives on it. Every investor's fee is measured against
their own entry price and nobody subsidises anybody.

An on-chain vault cannot do this — a share token cannot remember who bought when — which is
exactly why the mark lives here and not in a contract.

## Why the fee is taken in units, never in cash

A charge claws back units: `Dr FeeShares / Cr UserShares` on the share ledger. No USDT
moves. `FeeShares(svc)` is the product's **fee class** — the units the reserved `fee`
allocation holds in that product (`UnitHolder::Allocation(fee)`, #245). The `fee`
allocation is itself held by people through `fee` units, so a charge is owned the moment
it is taken: their NAV (`funds::nav_of`, computed, never posted) already carries the class
at the product's mark. Three properties follow, and they are the reason for the design.

1. **No chain fee, ever.** Nothing leaves custody when a fee is charged, so the fund pays no
   gas per investor per period. The manager converts an accumulated unit balance to cash
   *once*, in bulk, through `SettleFeeShares`.
2. **An investor can never be pushed negative.** The cash claim is not touched at all, and
   the clawback is capped by the units actually held — with TigerBeetle's
   `credits_must_not_exceed_debits` flag on `UserShares` as the ledger backstop underneath
   the application cap.
3. **Other investors are untouched.** `SharesOutstanding` does not move, so NAV per unit is
   unchanged. The charge is a transfer *between holders*, not a dilution of everyone —
   which is what makes the per-investor mark honest.

Whatever cannot be collected — the holder's units are locked by a queued redemption or
escrowed by a resting sell, or the residue falls below one base unit of share — is carried
as `fee_debt` and collected on the next assessment. It is never written off and never
becomes a negative balance.

## Escrowed units: owed on the whole position, taken from the free holding

A resting sell order moves a holder's units into the book's escrow (`BookShares`); a queued
redemption reserves them against the holding until the burn lands. Neither makes the units
somebody else's, and neither stops the fund from carrying the capital. So the fee is
**owed on the whole position** — the holding plus the escrow, reserved units included — and
**collected from what the holding can spare** right now, its available balance. The
shortfall goes to `fee_debt` like any other uncollectable amount.

The edge that made this a rule (#255): with *every* unit in a resting sell, nothing is
collectable. The assessment is still written — a `fee_assessments` row with
`charged_units = 0`, the whole charge in `fee_debt`, the accrual clock (and, if the period
closed, the crystallization clock and the mark) moved. No ledger transfer is posted, because
there is nothing to post. The next assessment after the order comes off collects the debt
plus the days since, and never the same window twice.

The alternative — pausing the clock while the units are escrowed and telling the holder —
was rejected, because it is the hole itself: a holder who kept an ask on the book deferred
their fee for as long as it rested and was billed the whole stretch in one blow the day it
came off, while the fund carried their capital the entire time. The debt road already
existed and adds no state.

Where the cap lives: `PositionSnapshot.units` is the position (`UserShares.posted +
BookShares.posted`), `PositionSnapshot.collectable` is the holding's available balance;
`assess` measures on the first and caps the clawback by the second. `FeeCharge::is_empty`
means "nothing owed", not "nothing collected" — the only case that persists nothing.

## The elapsed clock, and the obligation it places on everyone else

The management leg is `basis × rate × elapsed`, and **both factors live on the same row**.
That makes every writer of `fund_positions.cost_basis` part of the fee plane whether it
wants to be or not: moving the basis without moving the clock means the next assessment
charges the whole elapsed window on money that arrived at the end of it.

`infrastructure::fee_accrual::carry_accrual` is the discharge of that obligation. Before a
basis moves it computes what the *old* basis earned, carries it into `fee_debt`, and
restarts the clock. Both writers call it — the subscribe projection in the relay, and the
redemption settle.

Resetting the clock without settling would be worse than the bug it fixes: an investor
could top up a dollar a day and the elapsed window would never reach the sweeper's minimum
age, so they would never be charged at all.

The dormant case is the one that made this urgent. The sweeper's queue skips unit-less rows
(`units <> '0'`), so a position that goes to zero has its clock frozen for the whole
dormancy. Without the reset, an investor who exited and came back a year later was billed a
year of management fee on their returning capital.

## Concurrency

An assessment reads its snapshot outside a transaction and writes under the position row
lock. The lock orders two concurrent writes but cannot tell that the second one's inputs
went stale while it waited — so both would charge the same elapsed window. The accrual
clock *is* the version: `advance_position` writes conditionally on it, and a charge whose
clock has moved rolls back whole. Losing that race is a non-event, exactly like a fund with
no policy or a charge that floors to nothing.

This matters because a second core instance is a supported deployment — the relay is a
lock-enforced singleton (`pg_advisory_lock`) precisely because one runs — and the sweeper
has no such lock. It does not need one now.

## What the investor sees

The proto marks `GetFeePolicy` readable by any authenticated user: an investor is entitled
to know what they are paying before they subscribe, not after the first charge. Any user
who can see the product, that is — the terms, their history and the catalog of policies
follow the product's visibility exactly as `GetAllocation` and `GetFundNav` do: a product
hidden from the caller is `NOT_FOUND` (and absent from `ListFeePolicies`), unless they hold
`AllocationManage`. The cabinet surfaces it in three places.

- **The product page** shows the terms in a `Fees` card beside the unit supply, for holders
  and non-holders alike, and adds the caller's own accrued figures (management, performance
  at today's NAV, carried debt, total, and their high-water mark) when they hold a position.
  The `Value` stat opposite is stated *before* fees, and the card says so.
- **The activity timeline** carries a fifth operation kind. A fee charge is the only line
  item that reduces a holding without the investor doing anything, so leaving it out made
  units simply shrink with nothing to explain it.
- **The operation detail panel** breaks the charge into its two legs, the units taken, and
  the price they were taken at.

An **operator** sees none of the console side of this. Every `/api/admin/fees/*` route is
gated at the BFF to the `admin` and `owner` roles (`require_fee_admin` beside
`require_admin` in `cabinet/backend/src/routes/mod.rs`), the `Fees` entry is absent from
their rail, and opening `/admin/fees` by URL lands on a "not available to your role" notice
rather than a page of silent 403s. This is the coarse gate only: the money plane still
decides `AllocationManage` per call, and what an operator could always read as an
investor — one fund's terms and its pending change through `GET /api/funds/fee-policy` —
they still can. `/admin/allocations` is still a partial page for an
operator — some of its reads answer, the management calls do not — and is out of scope here.

## Settlement

`SettleFeeShares` is the one operation in the plane that moves cash. It runs once per period
for a whole fund rather than once per investor — the entire point of collecting in units.
The product buys its fee class back at the **day's dealing NAV**: `Dr SharesOutstanding /
Cr FeeShares` burns the class, `Dr service:<svc> / Cr service:fee` pays the `fee`
allocation (`Party::fee_payee()`, carried on the `SharesSettled` event as a required
`payee` field, `domain/src/fees.rs`). The `fee`
holders' NAV does not move — units of the product left, cash of the same value arrived
(`tests/ownership_fee.rs`). It is Read-First gated on the fund's claim covering the payout
**plus** what the product's queued redemptions would cost at the same NAV, and **refuses**
when short rather than queueing: nobody is waiting on it, and a fee that cannot be paid
today keeps accumulating as units at no cost. The settler is bound by the redeem cooldown
(`refuse_recent_poster`): within `VALUATION_REDEEM_COOLDOWN_SECS` (7 days) of their own
mark on the product the settlement is refused, because converting one's own mark into the
`fee` holders' cash is the same move the cooldown stops one step later.

The book's taker fee credits the same `service:fee`. The retained withdrawal fee does not:
it is banking's own and lands in banking's treasury, `scope_account:main:treasury` (D17) —
withdrawal events raised before that change name `service:fee` and still pay it there. A reserved allocation (`fee`, `fund`) charges no fee of its
own (`ScheduleFeePolicy` refuses it), takes no mark, and its holders exit only by
redemption: `Redeem` on `fee` is priced by its computed NAV (cash on `service:fee` plus
every product's fee class at that product's NAV, over the `fee` supply), paid out of
`service:fee`, and **refused rather than queued** when the cash cannot cover it — the
holder settles fee units into cash first, or redeems fewer. The cooldown on a `fee`
redemption is the cooldown on every product `fee` holds a class in. There is no other
road out: the pre-#245 revenue payout was removed (`piggybank/core/PATTERNS.md`
§ "Revenue payout — retired").

## Changing the terms

A fee policy used to be a single upsert: one `AllocationManage` holder called
`SetFeePolicy`, the row changed, and the next sweep charged the new rate against every
holder — with no ceiling, no notice, no record of what the old terms were, and no second
person involved. Issue #233 closes that. The write path is now `ScheduleFeePolicy`, and a
policy is a **versioned row that a change is promoted into**, never edited in place.

### The ceiling

`FeePolicy::new` refuses a management rate above **500 bps** (5% p.a.) and a performance
rate above **5000 bps** (50% of the gain) — `MAX_MANAGEMENT_BPS` and `MAX_PERFORMANCE_BPS`
in `domain/src/fees.rs`. The hurdle stays capped at 100%: it only ever lowers the fee. The
schema states the same two ceilings as `CHECK`s on `fee_policies` and `fee_policy_changes`
(migration `0036`), added `NOT VALID` so a pre-existing row cannot stop the pod from
booting; every new write is held to them.

### The house envelope, tightening, and who must agree

Two pure functions decide the requirement, and both have their own unit tests:

- **`FeePolicy::within_house_envelope`** — `management ≤ 200 && performance ≤ 2000 &&
  basis == invested_capital && crystallization == annual`, any hurdle. This is what the
  prospectus already promised.
- **`FeePolicy::tightens_from(current)`** — true when ANY leg gets dearer for the investor:
  either rate up, the hurdle down, the basis moved from invested capital onto the mark, or
  crystallization made more frequent. A product with no policy is measured as
  `FeePolicy::NONE` (0/0, invested capital, annual): it charges nothing, so any first
  positive rate tightens.

`requirement_for(current, next)` is then one line: **the owners' consilium exactly when the
change tightens the terms and lands outside the envelope — or lowers the hurdle, wherever
the terms sit; otherwise a single `AllocationManage` holder**. Loosening never needs a
quorum, however far outside the envelope the terms sit; a tightening that stays inside the
envelope is an administrator's call. The hurdle is the one exception, and deliberately so:
the envelope leaves it free because a hurdle only ever helps the investor, which is exactly
why taking a promised one away — even on otherwise house terms, even while every other leg
loosens — is a new bargain the owners must strike.

A change that needs the owners is opened as a `ConsiliumKind::FeePolicy` consilium by the
requester, who must therefore BE an owner — an administrator who is not one is refused
before anything is written, with a message that says so — and must state a `reason`,
which the owners read in their approval mail and which is part of what they sign. The
consilium's subject (`FeePolicySubject`: the change id, the product, the terms moved FROM
and TO, the reason and the requested effective moment) is domain-separated and hashed
exactly as a payout's terms are, and its source claim is the product's `FeeShares`, so the
existing "one open consilium per source claim" index gives one open fee-policy consilium
per product for free. The change, the consilium, its seats and their approval mails commit
in ONE transaction. The quorum, the mail, the 72h window and the roster rules are the ones
in `docs/CONSILIUM.md`.

### Notice

A change binds no earlier than **24 hours** (`MIN_NOTICE_SECS`) after the moment it was
SCHEDULED, whenever the product has at least one holder with units. A fund with no holders
has nobody to warn, and the change may take effect at once. The operator may ask for a
later `effective_from`; an earlier one is lifted to the minimum rather than refused, because
"as soon as allowed" is a legitimate request.

For a consilium-gated change the clock starts when the owners CARRY it — the moment the
consilium executes — not when it was proposed. Holders are then mailed
(`GovernanceMail::FeePolicyNotice`, one per holder, keyed
`fee-policy-notice:<change>:<holder>`, through the same `consilium_mail` queue every
governance mail leaves through, in the same transaction as the state change), with the
fund's name, the current terms (absent when the fund charged nothing), the proposed terms,
the moment they bind and the cabinet-relative product page (`/invest/<service>`, pinned to
concierge's own origin). On the administrator's path the same mails go out the moment the
change is scheduled.

A notice is queued for every holder with an account row, whether or not the identity plane
has mirrored them yet. Its addressee (`subject_user_id`, which concierge checks against the
recipient) is named by the mailer from `users.concierge_user_id` **at the moment it sends**,
not from whatever the queue row was written with (#325). Until the mirror lands, the notice
is **deferred**, not charged — units can be issued days before the holder's first cabinet
login, and a row charged an attempt per pass would be given up on five minutes after the
scheduling — so a mirror landing anywhere inside the notice period is reached on the next
pass (within the hour: the deferral backoff caps there). A holder still unmirrored a full
`DEFERRAL_CEILING` (24 hours, the notice period) after the scheduling is given up on with
the reason "recipient has no mirrored concierge user id", the row the acknowledgement below
is offered over. Only the notice is treated this way: an approval or a consent to an
unmirrored recipient is still charged per pass and retired within minutes — its token is
bound to a subject the caller had to resolve when it opened, so an empty mirror there is a
fault worth an alert, not a login still to come.

The owners, for their part, are told how the consilium ended: one outcome mail
(`GovernanceMail::PayoutOutcome`) to the initiator and to every seat on each closed state —
rejected, expired, cancelled, executed, execution failed — and one burn notice
(`GovernanceMail::TokenBurned`) to the same audience when a seat's token burns on five wrong
codes, exactly as for a payout or a payment. Both ride the same `PayoutOutcomeMail` and
describe the TERMS, not money: the fund line (from `fee_mail_fund` — the title, clipped, with
the slug, or the slug alone), the terms in force when the change was proposed (absent when
the fund charged nothing) and the proposed terms; the rail and the payment fields stay
empty, because the relay renders exactly one description. The initiator's reason rides the
outcome mail only — the burn notice carries none, as the contract has it, so an alert about
a brute-force attempt is not captioned with the initiator's words.

The mail is correct only against concierge **v0.8.0 or later** (concierge#72), which is
where the relay learned the fee description. An older relay does NOT refuse it: prost drops
the unknown fields 11–13 (`fund`, `current`, `proposed`), what is left names neither a rail
nor a payment, and v0.7.0 takes that as a payout with an empty rail and sends the owners a
"Payout rejected" (or "Payout token_burned") mail with blank Network and Address rows —
wrong, and delivered. So the order of the roll-out matters: concierge v0.8.0 first (in
production since 2026-09-15), this banking release after it. Rolled out the other way round,
the owners get a misleading letter, not a bounce.

### Versions and states

`fee_policy_changes` is the history. Every row carries the full five-field terms, a
`version` unique per product, the requirement it was held to, who requested it and when,
the consilium it waited on (if any), and the timestamps of its scheduling and its
application. `fee_policies` stays what it always was — the row **in force right now** — and
grows `version` and `effective_from`; existing rows were backfilled as version 1, `active`.

```
awaiting_consilium ──carried──▶ scheduled ──promoted──▶ active ──next change lands──▶ superseded
        │                          │
        └─rejected / expired /     └─ administrator cancels before it took effect ──▶ cancelled
          cancelled / voided ──▶ rejected
```

At most one change per product is `awaiting_consilium` or `scheduled` at a time
(`fee_policy_changes_single_pending_idx`); a second request is refused with a conflict that
says to cancel the pending one first. `CancelFeePolicyChange` withdraws a scheduled change
and, for one still awaiting the owners, withdraws its consilium in the same transaction —
which is why a consilium-gated change may be withdrawn only by the owner who proposed it or
by another owner: an administrator who could not open the quorum must not be able to close
it. Withdrawing a scheduled change also withdraws, in the same transaction, every holder
notice the mailer has not delivered yet (`consilium_mail.withdrawn_at`, migration `0043`): a
notice still queued behind a relay outage would otherwise tell its holder, once the relay
is back, that terms which will never bind "change on <date>" (#319). A withdrawn notice is
terminal — never drained, never counted as owed — and distinct from one given up on: nobody
failed to reach anybody. Notices already delivered stand; this plane sends no "cancelled"
mail after them. The mailer holds each row's lock across the relay call, so a cancel that
lands while a notice is being handed over waits for the outcome and then withdraws nothing
delivered — never a notice sent after the fact was taken back, never a delivered one on
record as withdrawn. Closing a consilium without a verdict — the administrator's cascade,
the owner's own withdrawal, the window running out, a roster change — likewise withdraws,
in the same transaction, the seats' approval invitations the mailer has not delivered yet,
blanking their token and code as a given-up row's are: an invitation still queued behind a
relay outage would otherwise ask its owner, once the relay is back, to vote on a consilium
nobody can act on (#342). Only the invitations go — the verdict mail the closing queues and
any burn notice already queued still reach the audience — and they are taken back before
the consilium and its seats are locked, so a cancel never waits on a delivery in flight
while holding the seat that delivery will mark as notified. Closing a payment order without
a verdict — the operator withdrawing it, or its window running out — withdraws its
undelivered consent invitation the same way, blanking the token and code, before the order
is locked (#368).

Every transaction over a product's terms — scheduling, the owners carrying, promotion —
opens by locking the product's `allocations` row. The requirement an operator's request
was judged by is re-taken under that lock against the live terms, and a disagreement (a
promotion committed while the request was being recorded) is a conflict asking for a
re-submit, never a change recorded against terms that no longer hold. The rate ceilings on
the history table hold only for rows that can still bind; a legacy row above today's
ceiling is superseded like any other, so an over-the-ceiling policy is precisely the one
that can always be lowered.

`effective_from` may be asked for at most 366 days ahead. A change over a held product is
refused while no governance mailer is configured: notices queued into a relay that never
runs are not notice.

### Promotion, and why the old rate is settled first

The fee sweeper wakes every 60 seconds and promotes every `scheduled` change whose
`effective_from` has passed. Promotion is one transaction per change: lock the product's
unit-holding positions; for every holder whose accrual clock stands before
`effective_from`, call `carry_accrual` **as of `effective_from`** — which reads the OLD row
from `fee_policies` on the same connection, prices the elapsed window at the old management
rate, carries it into `fee_debt` and restarts the clock — and only then upsert the new terms
into `fee_policies`, mark the change `active` and the previous active one `superseded`.

The order is the whole point. § "The elapsed clock" says nobody re-prices time that has
already passed, and a policy change is exactly a writer of one of the two factors: the
next assessment must not charge the window before `effective_from` at the new rate, up or
down. Settling it at the old rate first is what makes the new rate begin at
`effective_from` and not at whenever a holder was last swept. The performance leg needs no
such treatment: it is measured from each investor's own mark at the next crystallization,
under whichever terms are in force then.

A failure on one product warns and moves on; the change stays `scheduled` and is retried on
the next tick.

Promotion also waits for the notices — when the terms get dearer. A holder's notice that
has not been DELIVERED — still deferred behind a relay outage, refused until its attempts
ran out, or deferred past the ceiling (`docs/CONSILIUM.md`) — is a holder who was never
told, and terms that tighten on them (`FeePolicy::tightens_from` against the live row; no
row is measured as `FeePolicy::NONE`) do not bind over them: `promote` refuses with a
conflict naming how many are undelivered and how many of those the mailer has given up on,
the change stays `scheduled`, and after ten consecutive refusals the sweeper's warning
becomes an error. Delivery is the test, not attempts: a relay down since the change was
scheduled charges no attempt at all, and the deferral ceiling equals the notice period, so
counting only abandoned rows would let a change bind the very minute nobody could have
been told. Only the holders of the moment count — a recipient who has since redeemed every
unit holds nothing back.

A loosening binds regardless, with a `warn!` naming the undelivered count: nobody is worse
off, and a holder the identity plane cannot reach (an unverified mailbox; a mirror that
never lands, on which every notice to them is given up on a day after its scheduling) would
otherwise pin a product's terms forever, the lowering of a legacy rate above today's ceiling included. A tightening
over such a holder has one way through: the relay coming back promotes it by itself on the
next tick; for a holder who stays unreachable, an operator takes responsibility explicitly.

`AcknowledgeUndeliveredNotices` (`AllocationManage`, and the caller must be the one who
requested the change or a fund owner — the same rule as withdrawing a consilium-gated
change, so the holders' notice is never one administrator away from being waived on
somebody else's change) records on the change who acknowledged, when, and the banking ids
of exactly the holders whose notice the mailer had GIVEN UP on at that moment
(`fee_policy_changes.notices_waived_by / notices_waived_at / notices_waived_users`,
migration `0042`). Given up on, not merely undelivered: a notice still in the queue may yet
arrive, and its holder is then told — waiving it before the mailer has finished trying would
take responsibility for holders nobody has failed to reach (the requester pressing "take
responsibility" in the first minute after scheduling would otherwise name every holder).
Such a holder keeps holding the change until their notice is delivered or given up on.
`promote` then binds the tightening over the named holders and logs a `warn!` naming them by
id — and still refuses over anyone the record does not cover: a holder still in the queue
at the time, or one who had redeemed and bought back in untold, holds the change again, so
an acknowledgement never widens by itself. An acknowledgement covers every notice given up
on so far; a later one ADDS the holders given up on since and stands as the latest — the
list only grows, and the caller who extends it becomes `notices_waived_by` at `_at`, taking
responsibility for the whole list (the console's confirmation says as much). A repeat that
would add nobody returns the record unchanged, and the service's audit `warn!` is written
only by a call that recorded or extended it. A change that is not `scheduled`, one that
only loosens the terms (it binds by itself, so there is no protection to waive), or one
with no uncovered notice given up on yet has nothing to acknowledge and is refused rather
than silently stamped — an acknowledgement covering nobody would be a misleading line in
the history, and the schema refuses one too. Every change carries `undelivered_notices` /
`notices_given_up` / `notices_unacknowledged` on the wire (counted only while `scheduled`,
operators only) — the last being the given-up notices no acknowledgement covers, which is
what the admin console's "N holders could not be told" and its "take responsibility" offer
read: the acknowledgement is offered exactly while `notices_unacknowledged > 0`.

## Still open

**Exit crystallization is not wired.** `Trigger::Redemption` exists and is tested, but
nothing calls it: fees accrue on the schedule only. An investor who redeems between
crystallizations therefore leaves without paying the performance fee on the gain they are
realising. `carry_accrual` on the settle path closes the management half of that gap; the
performance half is the remaining work.

## Where the code lives

| Concern | Path |
| --- | --- |
| Arithmetic, pure and I/O-free | `domain/src/fees.rs` |
| Use cases | `piggybank/core/src/application/fees.rs` |
| Postgres adapters, the charge | `piggybank/core/src/infrastructure/fees.rs` |
| Settling the accrual before a basis moves | `piggybank/core/src/infrastructure/fee_accrual.rs` |
| The periodic worker | `piggybank/core/src/infrastructure/fee_sweeper.rs` |
| Changing the terms: history, notice, promotion | `piggybank/core/src/infrastructure/fee_policy_changes.rs` |
| Schema | `piggybank/core/migrations/0023_fee_policy.sql`, `0036_fee_policy_changes.sql`, `0041_fee_assessment_deferred_charge.sql`, `0042_fee_policy_notice_waiver.sql`, `0043_consilium_mail_withdrawn.sql` |
| Wire contract | `contracts/proto/banking/v1/fees.proto` |
| Integration tests (real PG + TigerBeetle) | `piggybank/core/tests/fee_policy.rs` |
