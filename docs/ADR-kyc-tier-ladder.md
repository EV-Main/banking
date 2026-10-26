# ADR — the KYC ladder, and where partner / company partner / service belong

**Status:** proposed. Awaiting the product owner's answer on the one question in
§ "The decision". Nothing below has been implemented, and nothing should be until the
answer lands — the answer decides whether most of the work happens at all.

**Issue:** [#214](https://github.com/EV-invest/banking/issues/214). Related: #179
(where the `>= 1` gate actually sits), #195 (limits are not attached to a tier at all),
#213 (the onboarding UI that would have to grow).

---

## Context — what `kyc_level` is today

`kyc_level` is a `uint32` on `banking.v1.UserProfile`
(`contracts/proto/banking/v1/users.proto:85`), with the rungs written as prose in the
comment above it (`:72-84`). It is **not** an enum, so the wire carries any number the
plane will store.

The identity plane owns the value and is its only writer: concierge
`domain/src/users.rs:37` caps it at `MAX_KYC_LEVEL = 3`, `runner/src/directory.rs:302`
re-checks the same bound on the `SetKycLevel` RPC, and
`runner/migrations/0013_kyc_level_range.sql:81-84` puts `CHECK (kyc_level BETWEEN 0 AND 3)`
on `users` (validated) and on `user_outbox` (deliberately and permanently `NOT VALID`).
A vendor verdict may grant at most tier 1 (`runner/src/ports.rs:241`,
`PROVIDER_MAX_TIER`), and a provider may only be *asked* for 1 or 2
(`runner/migrations/0010_kyc_cases.sql:46`, with `0014_kyc_requested_tier_intent.sql`
explaining why those two numbers differ on purpose). Banking mirrors the value over the
one-way lifecycle bridge and never authors it; its own ceiling lives in the cabinet BFF
(`wallet/backend/src/routes/admin.rs:62`) and in the operator console
(`wallet/frontend/views/admin/lib/format.ts:76`, `KYC_LEVELS = [0, 1, 2, 3]`, with
`KYC_LEVEL_KEYS` at `:109-114` and the `Select` over them at
`wallet/frontend/views/admin/users/ui/users-view.tsx:426-430`).

**The enforced surface is one comparison, repeated three times.** Everything reads
`>= KYC_LEVEL_VERIFIED` (= 1):

- `piggybank/core/src/application/wallet.rs:137` (`is_verified`), reached from
  `get_deposit_address` at `:257` — no deposit **address is issued** below tier 1.
- `piggybank/core/src/application/withdrawals.rs` — a withdrawal request is refused, and
  the tier is checked **again at dispatch**, because acceptance and payout can be hours
  apart.

Nothing anywhere reads `>= 2` or `>= 3`. Tiers 2 and 3 exist on paper and grant nothing
the code can observe; the proto comment says of tier 2's raised limits, in as many words,
that the limits themselves are not modelled yet. See also #179: the `>= 1` arm on the
deposit side gates **provisioning an address**, not the crediting of money that arrives on
one already issued — whatever a new rung is given, it inherits that placement.

---

## The decision

> Do we extend the scalar to `0..6`, or keep `kyc_level` as a measure of verification
> depth and put partner / company partner / service on a different axis?

**Recommendation: Option B — a different axis.** `partner`, `company partner` and
`service` become **roles** (or, if the role axis turns out to be the wrong home, a
`subject_type` attribute alongside it). `kyc_level` keeps meaning exactly one thing: how
thoroughly the subject behind the account has been checked.

Three reasons, in the order they will bite:

1. **`>=` would grant things nobody decided to grant.** Every gate over `kyc_level` is
   written as `>=`, so on the day the ceiling constant changes, a `service` (6) clears
   every gate an `elevated` individual (3) clears — today that reads "a machine API client
   may move money in and out", for free, with no line of code expressing the decision.
   The ordering is load-bearing and the three proposed rungs are not ordered against the
   existing ones.
2. **The ordering claim would be false.** "A company partner is verified more thoroughly
   than an individual who passed enhanced due diligence" is not a claim anyone would make,
   but it is exactly what the number encodes and what a future `>=` reads back out. A
   `u32` cannot warn the next gate author that its ordering is decorative.
3. **The axis already exists.** `string role = 17` sits in the same message
   (`contracts/proto/banking/v1/users.proto:87`), with its own RPC
   (`concierge` `contracts/proto/concierge/v1/directory.proto:67`, `SetRole`), its own
   admin vocabulary in concierge's admin and an owners' approval behind it.
   Partner, company partner and service are answers to "what kind of subject is this",
   which is the question `role` is for.

**Corollary for level 6 (service).** A machine client has no "how thoroughly was this
human checked" to record. What it needs is a **credential type and an access scope** —
which is how tokens already work in the identity plane, not how KYC tiers work. Modelling
a service as a KYC tier would mean issuing it a document-verification level it can never
have obtained. Under this recommendation `service` is not a tier and not, strictly, a
person-role either: it is a credential with a scope, and it should be designed as one.

### The decision table the issue asks for

Filled in with the recommended answer. "Permits" is the *enforced* permission — what code
reads — not an aspiration.

| Level | What is verified | What it permits | Limits | Who may assign |
| ----- | ---------------- | --------------- | ------ | -------------- |
| 0 registered | confirmed email only | nothing that moves money | n/a | automatic on sign-up |
| 1 verified | document, liveness, face match, sanctions/PEP | deposit **address issuance**, withdrawal request, withdrawal dispatch | none today — see below | vendor verdict (`PROVIDER_MAX_TIER = 1`) or a human operator |
| 2 enhanced | proof of address, source of funds | same as 1, plus a raised transaction ceiling once limits exist | **owner decision needed** (#195) | human operator only |
| 3 elevated | enhanced due diligence | same as 2, plus whatever the top limit band is | **owner decision needed** | human reviewer only, never automatic |
| ~~4 partner~~ | — | **not a `kyc_level`** under this recommendation | — | — |
| ~~5 company partner~~ | — | **not a `kyc_level`** | — | — |
| ~~6 service~~ | — | **not a `kyc_level`** | — | — |

Rows 4-6 move to a second table, on the axis that actually asks "what kind of subject is
this":

| Subject | Axis | What it is | What it would permit | Who may assign |
| ------- | ---- | ---------- | -------------------- | -------------- |
| partner | `role` | a partner who is a natural person | a partner surface (to be specified); money movement still requires `kyc_level >= 1` on the same account | an owners' approval, as `SetRole` already is |
| company partner | `role` (+ an organisation record, if one is ever needed) | a partner that is an organisation | as above; KYC of the organisation is KYB and is a separate depth scale, not a rung of this one | an owners' approval |
| service | credential + scope | a machine API client | exactly the scopes on its credential, nothing inherited from a tier | owner, by issuing the credential |

Two things this table makes explicit that the scalar hid:

- **A partner still has a `kyc_level`.** Being a partner does not verify anybody; a
  partner who must move money passes the same tier-1 gate every other person passes. Under
  Option A that fact disappears, because 4 is already `>= 1`.
- **A service has no `kyc_level` at all** — its account is not a verified human, and
  `0` is the honest value to store if a row must exist.

---

## Consequences

**If B is accepted:**

- No widening migration. `0013_kyc_level_range.sql` stays as it is, `MAX_KYC_LEVEL`
  stays 3, `KYC_LEVELS` stays `[0, 1, 2, 3]`, and the `admin.kyc.*` strings in the five
  catalogs stay four.
- The work moves to the role axis: naming the new roles, deciding what a partner surface
  is, and designing the service credential + scope. The `>=` gates keep meaning exactly
  what they mean today, and no existing comparison has to be re-read.
- The open limits question (#195, and the blank cells above) is **not** closed by this
  ADR and does not depend on it. Limits attach to `kyc_level`; that is true under either
  option.

**If A is chosen instead** (the owner may still prefer it — it is cheaper up front), then
three things become mandatory, not optional:

1. Every existing `>=` comparison is re-read and re-decided against the new rungs, and the
   proto comment states explicitly what `>=` means above 3 — or states that the ordering
   above 3 is meaningless, which every future gate author then has to know.
2. A **new** migration widens the range; `0013_kyc_level_range.sql` must not be edited
   (sqlx checksums applied migrations and they run at service start), and the widening
   must preserve the `NOT VALID` shape of the `user_outbox` constraint.
3. The contract is generated: a `users.proto` edit requires `nix run .#gen-api` and both
   sides committed together (`contracts/openapi.json` and
   `wallet/frontend/shared/contracts/gen/`), or the two planes drift apart silently.

---

## What is still open after this ADR

1. **The limits model.** Tiers 2 and 3 grant nothing until a limit band is attached to a
   tier and read somewhere. Owner decision, tracked in #195.
2. **The partner surface.** "Partner" has no behaviour in the platform yet; the role is
   only worth adding once something reads it.
3. **KYB.** A company partner's verification depth is an organisation's, not a person's.
   If it is ever needed it is a second depth scale, and reusing `kyc_level` for it would
   repeat exactly the mistake this ADR declines to make.
