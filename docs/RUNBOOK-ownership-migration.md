# Runbook — ownership data migration (`piggybank migrate-ownership`)

> **Historical reference.** Executed in production on 2026-09-27 (v1.0.0): the retired
> `fund` claim moved onto `service:fund`, the retired `fee` claim was already empty, and
> the reconciliation came back clean. The command, its module
> (`application/migrate_ownership.rs`) and its tests were removed with the contract step
> (C-9, migration `0047_ownership_contract.sql`); `piggybank migrate-ownership` now
> refuses by name. Nothing below can be run against a current build — it is kept as the
> record of what was done.

One-off, operator-run, after the first ownership release (#245, phase 1) is in
production and **before** the contract migration that removes the retired accounts.
Background: [`piggybank/core/PATTERNS.md`](../piggybank/core/PATTERNS.md) §Money plane
and the module docs of `piggybank/core/src/application/migrate_ownership.rs`.

## What it does

Every unit of value must be at a holder — a person's own claim, or units of an
allocation people hold. Before this migration three things are not:

| Legacy account | Where it goes |
| --- | --- |
| `fund` (code 1) — the seed capital claim | moved onto `service:fund`; units of the `fund` allocation minted to the named holders pro rata at NAV 1.00 |
| `fee` (code 40) — retained withdrawal / 2-and-20 / taker fees | moved onto `service:fee`; units of the `fee` allocation minted to the named holders pro rata at NAV 1.00 over **cash + every product's fee class at its dealing NAV** |
| `shares_fee:<product>` — a product's fee class | stays where it is (it is already the `fee` allocation's holding); it is only *priced* into `fee`'s value |
| `shares_company:<product>` — the retired company stake | `--company keep` (default): the run **refuses** while any is non-zero, naming the figure. `--company retire`: burnt (`Dr shares_outstanding / Cr shares_company`, no cash), supply shrinks by exactly the stake |

Per allocation (`fund` first, then `fee`) the cash hand-over and every holder's mint are
**one TigerBeetle linked chain**: they land together or not at all. The mints are posted
by the command itself, not by the relay, and their `unit_issuances` rows are written
already `applied` with the holders' `fund_positions` projections — the only writes in the
system that bypass the outbox, which is admissible because the command is run once, by
hand, after a dry run, with the reconciliation printed at the end. Every id is
deterministic (derived from the holder table), so a second run finds its own work and
reports `already applied`.

There is **no rollback** once the chain has landed. Hence the dry run and the `yes`.

Nor is the *tag* the ordinary rollback the spec assumes. The hub runs `sqlx::migrate!()`
without `ignore_missing`: once the new pod has applied 0045/0046, a pod of the previous
image refuses to boot (`VersionMissing(45)`) — the schema itself is backward compatible, the
version ledger is not. Rolling the image back therefore needs
`DELETE FROM _sqlx_migrations WHERE version IN (45, 46)` first, and only while no
`holder_grant` / `seed_capital` consilium row exists (the old `ConsiliumKind` parser would
fail on it). The release is effectively one-way from the moment the new pod migrates.

## Input: `holders.json`

```json
{
  "fund": [
    {"user_id": "<user id>", "share_bps": 8000},
    {"user_id": "<user id>", "share_bps": 2000}
  ],
  "fee": [
    {"user_id": "<user id>", "share_bps": 8000},
    {"user_id": "<user id>", "share_bps": 2000}
  ]
}
```

- `user_id` is either the id the central admin's `/admin/users` shows (the concierge id) or the
  banking `users.id` — the command accepts both, and the two may be mixed. Each id is
  looked up as a concierge id first, then as a banking id; an id that is neither is
  refused before the ledger is read. Every person must be active.
- Everything the migration derives (issuance keys, transfer ids) comes from the
  **banking** id the lookup lands on, so the same table in either form is the same run:
  a rerun in the other form is a no-op, not a second migration. Two lines naming one
  person in two forms are refused as a duplicate.
- Shares are basis points; each table must add up to exactly `10000`, nobody twice, no
  zero shares.
- Units are `floor(value × bps / 10000)` per holder; the **last holder listed** takes the
  rounding remainder (at most a few base units of 1e-18), so `Σ units == value` exactly
  and NAV is 1.00 to the base unit.
- The table is part of the migration's identity: rerunning with a different table after
  a run is refused (`Conflict`). Keep the file.

## Procedure (spec §4.3)

1. R1 is in production; the pod is up; migration `0045` applied (the `fee` / `fund`
   allocation rows exist). Owners have handed over the holder table and the decision on
   any company stake.
2. **Fresh marks.** Every product whose fee class the `fee` allocation holds must have a
   valuation posted within the last 24 h (`MAX_NAV_AGE_SECS`): the migration prices the
   fee class at the product's dealing NAV and refuses a stale one.
3. **Snapshot before**: the central admin's `/admin/treasury` (the `retired` figures and each
   allocation's line) and the C-0 snapshot. The outbox must be drained (no parked rows,
   nothing pending on `fund` / `fee` — the command refuses in-flight pendings).
4. Feed the table over stdin and **dry-run**. The classifier blocks `kubectl exec` on
   production from a session — the operator runs these by hand. Not `kubectl cp`: the
   image is the binaries plus `fakeNss` (`flake.nix`, `containers.piggybank`) — no `tar`,
   no `/tmp`, no `PATH` — so the file goes in as stdin and the binary is called by its
   absolute path; the pod has two containers, so `-c` names the hub's
   (`ev-banking-piggybank`, `gitops/clusters/rpi5/apps/banking/manifests.yaml`):

   ```sh
   kubectl -n ev-banking exec -i deploy/ev-banking-piggybank -c ev-banking-piggybank -- \
     /bin/piggybank migrate-ownership --holders /dev/stdin --dry-run < holders.json
   ```

   The command uses the pod's own env (`DATABASE_URL`, `TIGERBEETLE_*`) and touches
   neither gRPC nor the relay; the server keeps running beside it. Read the plan: the
   retired balances, `fee`'s value (cash + priced fee classes), every holder's units, the
   company stakes, and `ledger before`. It must match the snapshot.
5. **Run** — with the table on stdin the interactive `yes` would read EOF and cancel, so
   the confirmation is the flag; the owner has read the dry run:

   ```sh
   kubectl -n ev-banking exec -i deploy/ev-banking-piggybank -c ev-banking-piggybank -- \
     /bin/piggybank migrate-ownership --holders /dev/stdin --yes [--company retire] < holders.json
   ```

6. Read `=== applied ===`, `=== ledger after ===` and `=== reconciliation ===`. Exit 0
   and `migration complete` means: both retired claims at 0, `Σ holders ==
   SharesOutstanding` for `fee` and `fund`, nothing unheld, cash invariant balanced.
7. Snapshot after: the central admin's `/admin/treasury` shows `fee` and `fund` with their holders,
   `retired` at zero; the reconciliation log stays quiet on the next scans. Only then
   C-9 / R2.

## What is printed

- `=== migrate-ownership: plan ===` — first every id the table named and the banking user
  it resolved to (`concierge <id> -> banking <id>`, or `banking <id> (given as a banking
  id)`) — check these against the console; then per allocation: the retired balance that moves,
  the allocation's claim before, each priced fee class, the value, the status
  (`PENDING` / `already applied` / `nothing to migrate`) and every holder's units with
  `[minted]` / `[row]` markers; then every company stake and what happens to it.
- `=== ledger before ===` / `=== ledger after ===` — the two retired claims and both
  reserved allocations' ownership pictures (claim, supply, holders, fee classes, NAV).
- `=== applied ===` — per allocation whether the chain was posted and how many rows were
  written or already present; the company stakes retired.
- `=== reconciliation ===` — the server's own scan, once.

## When it exits 1

- **Before anything moved** (a refusal from the plan): the message names the cause — a
  table that does not add up, an unknown or disabled person (the id as written), one
  person named twice in two id forms, a stale mark, a non-zero
  company stake under `keep`, units already outstanding on `fee` / `fund` (someone was
  seated before the migration: the owners decide), in-flight pendings on a retired
  account, or a table that differs from an applied run. Fix the input or the state and
  rerun; nothing was written.
- **After the run, with `FINDING:` lines**: the movements are on the ledger and cannot be
  rolled back. Do not rerun blindly. Compare `ledger after` with the plan, read the
  reconciliation's findings, and treat it as an incident (`docs/RUNBOOK-withdrawals.md`
  style: TB wins, recovery is an operator action).
- **Crash between the chain and the rows** (the process died after `=== applied ===`
  started printing, or the rows are missing while the treasury shows the holders): rerun
  the same command with the same table. The plan reports `already applied on the
  ledger; N rows missing, will be written from the ledger` and writes them with the
  amounts read back from TigerBeetle.

## Known limits

- A legacy outbox row replayed *after* the run (an old fee event still naming `fee`
  code 40) leaves a residue on a retired claim. The reconciliation counts it as
  `retired_claims`; the treasury shows it under `retired`. It is fee income belonging to
  the `fee` holders; moving it is a separate, explicit operator action — the migration's
  cash leg is idempotent and will not move it again.
- The command is removed together with the retired accounts in the contract step (C-9).
