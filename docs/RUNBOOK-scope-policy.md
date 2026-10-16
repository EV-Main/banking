# Runbook — setting a scope's policy

A scope concierge announces reaches banking over the scope stream
(`piggybank/core/src/infrastructure/scope_events.rs`, migration 0053). Banking registers it
**closed to moves in**: its `scope_policies` row carries `min_kyc_level = 2147483647`
(`CLOSED_UNTIL_SET_KYC`), a level no user has, because concierge's KYC levels run 0–3
(`docs/ADR-kyc-tier-ladder.md`). The owner chose this in the review of #511. A scope's rules
are banking's to set, and the column defaults would let every user in with no limits.

While the scope is closed:

- `MoveFunds` from `main` into it is refused with `kyc_level`.
- `GetScopePolicy` and the compartment listing return `caller_eligible = false`, with
  `min_kyc_level = 2147483647` on the wire.
- Moving money **out** of the scope back to `main` is never checked against the policy,
  so it keeps working.

The seeded scopes `invest` and `sa` have their own rows and are not affected. Migration 0061
(banking#523) set them to level 1, 1000 USDT per move and 5000 USDT per rolling 24 h, with no
balance cap, and only where the row still held the 0052 defaults.

There is no RPC for this yet. Until there is, an operator writes the row by hand with the
SQL below.

## Who runs it, and where

An operator runs this by hand on the production database host, as `postgres` over the
socket, against the `banking` database:

```bash
sudo -u postgres psql banking
```

Agents do not run it. Check which host serves production before you connect: `fallback
status`, the `ev-infra` skill.

## 1. Find the scopes waiting for a policy

```sql
SELECT s.id, s.state, s.display_name, s.version, p.min_kyc_level, p.max_per_move,
       p.max_moved_in_24h, p.max_balance, p.updated_at
  FROM scopes s JOIN scope_policies p ON p.scope = s.id
 WHERE p.min_kyc_level = 2147483647
 ORDER BY s.created_at;
```

## 2. Set the policy

Every column, and what it takes:

| Column | Meaning | Value |
| --- | --- | --- |
| `min_kyc_level` | the KYC level a user needs to move money in | `0`–`3`; `2147483647` closes the scope |
| `max_per_move` | largest single move in | USDT in **18-decimal base units**, as text; `NULL` for no limit |
| `max_moved_in_24h` | per-user total moved in over a rolling 24 h | same |
| `max_balance` | ceiling on one user's compartment, applied to their own moves in only | same |
| `auto_payout_per_op`, `auto_payout_24h`, `auto_transfer_24h` | how far the scope's services may move money without an approval | same, never `NULL`; defaults 1000 / 5000 / 5000 USDT |

1 USDT is `'1000000000000000000'`, and 1000 USDT is `'1000000000000000000000'`. The `CHECK`s
refuse a leading zero, a decimal point and a zero `max_*` (to say "no limit", write `NULL`,
not `'0'`).

Write it in a transaction, and only while the row is still closed. That way a policy
someone else has already set is never overwritten:

```sql
BEGIN;
UPDATE scope_policies
   SET min_kyc_level    = 1,
       max_per_move     = '1000000000000000000000',  -- 1000 USDT
       max_moved_in_24h = '5000000000000000000000',  -- 5000 USDT
       max_balance      = NULL,
       updated_at       = now()
 WHERE scope = '<scope id>' AND min_kyc_level = 2147483647;
-- Expect UPDATE 1. UPDATE 0 means the id is wrong or the policy was already set:
-- ROLLBACK and look at step 1 again.
COMMIT;
```

To change a policy that is already set, use the same `UPDATE` without the
`AND min_kyc_level = 2147483647`. Read the row first and name it in the incident or
issue you are acting on.

## 3. Check after

- Run the step 1 query again. The scope is no longer listed.
- `SELECT * FROM scope_policies WHERE scope = '<scope id>';` shows your values and a fresh
  `updated_at`.
- `MoveFunds` reads the row on every move, so the new policy applies to the next move.
  The scope list behind wallet and balance listings is cached per process for 30 s, so
  `caller_eligible` in a listing can lag by up to that long.

## Closing a scope to moves in again

Set `min_kyc_level = 2147483647`. Money already in the scope can still be moved back to
`main`. To freeze or close the scope itself, do it on concierge. Its `SCOPE_CHANGED`
reaches banking over the stream, and banking does not author scope state.

## When the scope stream alerts

While banking does not follow concierge, a scope frozen or closed there may still take money
in here. Read `SELECT * FROM scope_events_cursor;` and compare `position` with the head of
concierge's stream.

- **`SCOPE STREAM STALLED`** (reconciliation, ERROR). No well-formed page for over 5 minutes.
  Check that concierge is up and that `BRIDGE_SERVICE_TOKEN` matches the token concierge expects. The
  same pass can still log `reconciliation: clean`, because the stream sits outside the ledger
  invariants.
- **`… rewound or replaced …`** or **`… breaks the stream's contract …`** (ERROR). Concierge's
  stream no longer matches the cursor, for example after a restore of concierge's database.
  Nothing is applied and the cursor holds. Find out why the stream moved before you touch the
  cursor. Then set `position` and `last_event_id` to an event concierge still serves and
  restart piggybank. Without a restart it resumes on the next continuity check, within 5
  minutes.
- **`scope registry not following concierge … UNIMPLEMENTED`** (WARN). Concierge predates
  ScopeEvents. Scopes keep their last state until a concierge that serves the stream is
  deployed.

## Follow-up

An operator RPC on `CompartmentService` (a permissioned `SetScopePolicy`, with the
console screen over it) is meant to replace this runbook. It is not built yet.
