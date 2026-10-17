# Window tool — banking's side of the unit migration window

The write side of org restructure plan §6.5 (steps 1, 4, 5, 6 and the fee-share burn that
precedes the export). `window-export` ([`WINDOW-EXPORT.md`](./WINDOW-EXPORT.md)) is step 2 and
only reads. Everything here is run **by hand against the primary**, by an operator, one
subcommand at a time. Module docs: `piggybank/core/src/infrastructure/window_tool/`.

```bash
nix build .#window-tool            # ships just the binary
DATABASE_URL=… TIGERBEETLE_ADDRESS=… TIGERBEETLE_CLUSTER_ID=… window-tool <command>
```

## Before you run anything

- **Connect to Postgres directly** (the primary's own port), never through pgbouncer in
  transaction mode: the single-run lock is a session advisory lock, and a pooler hands the
  session to someone else.
- Every command prints, first, `postgres host:port/db  tigerbeetle address cluster N`. Read it.
- **Every write** (`--yes`, including `maintenance on --yes`) needs
  `--expect-cluster-id <id>` (must equal `TIGERBEETLE_CLUSTER_ID`) and
  `--anchor-holder <concierge user id>:<units>` twice (valeratrades `13000`, inintel44
  `3250`). The anchor then checks *people*, not just amounts: banking holders are mapped to
  their concierge ids and must be exactly those two with exactly those units; the export file
  is checked the same way before anything is closed.
- `close-share-accounts` prints the file's `export sha256`. With `--yes` it needs
  `--import-digest <that hex>` — copied from what invest printed for the file it imported — and
  refuses on any difference. The format is fixed: SHA-256 over the whole file's bytes, lowercase
  hex. (A direct read-only check of invest's `window_import` row is not built in; the digest is
  the tie between the two sides.)

## What protects the money

- **The anchor.** `service_arb` holders must read 13 000 and 3 250 units, Σ 16 250. Every
  command reads it first and last and exits non-zero if the shape is wrong or any holder's
  figure moved. Units already closed by the window are counted from `window_closures`, checked
  against the ledger transfer, so the figure is the same before and after step 5.
- **Dry run by default.** `maintenance on`, `drain`, `burn-fee-shares`, `remap`,
  `close-share-accounts` print a plan and write nothing without `--yes`.
- **Idempotent.** Each command plans from what the ledger says now and uses deterministic ids;
  a rerun after a crash finds its own transfers.
- **Cash is checked per account.** `remap` verifies each source claim at 0 and each destination
  at *before + moved*; `Σ custody == Σ claims` must hold before and after. The totals themselves
  are not compared: deposits keep landing in maintenance.
- **Drained first.** `remap` and `close-share-accounts` refuse unless `drain`'s verdict is clean,
  which includes withdrawals in flight and pending compartment moves.
- **Maintenance stops the background and the operators too.** While `operations_mode` is on the
  fee sweeper charges and promotes nothing, and the admin RPCs issue/retire units, settle fee
  units and post valuations are refused. The tool's own burn is not an RPC and still runs.
- **One at a time.** A session advisory lock refuses a second mutating run.
- **No rollback command.** `remap` and `close-share-accounts` post ledger transfers; undoing
  one is an inverse transfer by hand, decided by the owners. Dry-run first, always.

## Order of a window

| # | Command | Plan step | Notes |
| --- | --- | --- | --- |
| 1 | `maintenance on --yes` | 1 | flips `operations_mode.read_only`; `maintenance status` is the dashboard |
| 2 | `drain --yes` | 1 | cancels open orders and open consilia, waits the hub's relay out. Repeat until `DRAINED`. Queued redemptions are only reported; `--release-queued-redemptions` hands the units back |
| 3 | `burn-fee-shares --yes` | before 2 | retires the `fee` allocation's fee class in `service_arb`. Needs a fresh valuation mark for the product (the burn is priced at its dealing NAV). Rerun until it reports the fee class at 0 |
| 4 | `window-export --out export.jsonl` | 2 | must exit 0 (`clean`) |
| 5 | invest imports the file; its Σ per allocation reconciled | 3 | outside this tool |
| 6 | `remap --yes [--treasury-from-fee <usdt>]` | 4 | see below |
| 7 | `close-share-accounts --export export.jsonl --yes --import-digest <sha256>` | 5 | the digest is invest's: its import is done and reconciled |
| 8 | `reconcile --final` | 6 | hub reconciliation + leftovers + anchor |

Leaving maintenance is not a subcommand: it is the release's decision, in the admin console.

## `remap`

| From | To |
| --- | --- |
| `service:service_arb` | `scope_account:sa:alloc:service_arb` |
| `service:<other product>` | `scope_account:invest:alloc:<product>` |
| `service:fee` | `scope_account:invest:alloc:fee`, less the carve-out |
| `service:fund` | `scope_account:invest:alloc:fund` |
| the carve-out, from `service:fee` | `scope_account:main:treasury` |

- **Labels.** The plan's §6.5 shorthand `ScopeAccount(invest, fee|fund)` is read as the
  `alloc:fee` / `alloc:fund` accounts of §3.3: the bare label `fee` is banking's own collector
  in every scope (the fee route of migration 0059 already sweeps invest's fees to
  `alloc:fee`). If the owner meant the bare labels, that is a change of `destination()`.
- **Treasury carve-out.** Banking's own fee income that landed on `service:fee` before D17 is
  not distinguishable on the ledger. Nothing goes to the treasury unless
  `--treasury-from-fee <usdt>` names the amount, taken from the owner's snapshot; more than
  `service:fee` holds is refused.
- Destination accounts are registered with the `invest` client as owner (the engine that
  opens allocation accounts), including `sa/alloc:service_arb`. `service_arb` holds no cash,
  so for it the step only opens the account.
- A claim with anything pending or locked is refused: drain first.

## `close-share-accounts`

Every non-zero unit holding listed in the export (`user_shares`, `fee_shares`) is moved off
against the product's supply in one linked chain per product, then recorded in
`window_closures` (migration 0065: account, units, transfer id). Supplies and holdings both
end at zero, so the Share ledger adds up at every instant and the hub's reconciliation has
nothing to alert on.

The export is the contract: a holding whose units changed since the export, a live balance
the file does not list, resting-sell units (`book_shares`), a `live` or unclean file are all
refusals that name the account. Marking the account **codes** retired
(`AccountCode::is_retired`) is left to the release that removes the share ledger from banking;
`window_closures` is the data-side record until then.

## Exit codes and reading the output

`0` — the step did what it said and the anchor is intact. `1` — a refusal, a failed check, a
state that is not final (`NOT DRAINED`, `not applied yet`, `NOT OK`), or a broken anchor.
After a failure the anchor is printed anyway: it is the first thing to know.

## Limits of undoing

- **The point of no return is the drain.** Before it, `maintenance on` can be switched off in
  the admin console and nothing is lost. `drain` cancels orders and consilia, and `burn-fee-shares`
  retires units: neither can be reversed by the tool (a cancelled consilium is reopened by its
  initiator from scratch; burnt fee units are re-accrued, not restored).
- **Do not lift maintenance** until the release that removes the share ledger from banking is
  out. Between `close-share-accounts` and that release the hub would otherwise serve units that
  no longer exist here.
- **`ANCHOR BROKEN` between the posting and the rows.** If `close-share-accounts` dies after the
  ledger chain and before `window_closures` is written, the anchor cannot see the closed units
  and every command fails with `ANCHOR BROKEN`. It is healed by running the same
  `close-share-accounts` again: it recognises the closed accounts by their transfers and writes
  the missing rows from the ledger first.
- **The export is `0600`** and carries every holder's id and units; move it only over the channel
  the owners use for secrets.
