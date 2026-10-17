# Window tool — banking's side of the unit migration window

The write side of org restructure plan §6.5 (steps 1, 4, 5, 6 and the fee-share burn that
precedes the export). `window-export` ([`WINDOW-EXPORT.md`](./WINDOW-EXPORT.md)) is step 2 and
only reads. Everything here is run **by hand against the primary**, by an operator, one
subcommand at a time. Module docs: `piggybank/core/src/infrastructure/window_tool/`.

```bash
nix build .#window-tool            # ships just the binary
DATABASE_URL=… TIGERBEETLE_ADDRESS=… TIGERBEETLE_CLUSTER_ID=… window-tool <command>
```

## What protects the money

- **The anchor.** `service_arb` holders must read 13 000 and 3 250 units, Σ 16 250. Every
  command reads it first and last and exits non-zero if the shape is wrong or any holder's
  figure moved. Units already closed by the window are counted from `window_closures`, checked
  against the ledger transfer, so the figure is the same before and after step 5.
- **Dry run by default.** `drain`, `burn-fee-shares`, `remap`, `close-share-accounts` print a
  plan and write nothing without `--yes`. `maintenance on` is the one exception.
- **Idempotent.** Each command plans from what the ledger says now and uses deterministic ids;
  a rerun after a crash finds its own transfers.
- **Cash never changes size.** `remap` and `close-share-accounts` check
  `Σ custody == Σ claims` and that both totals are unchanged around the move.
- **One at a time.** A session advisory lock refuses a second mutating run.
- **No rollback command.** `remap` and `close-share-accounts` post ledger transfers; undoing
  one is an inverse transfer by hand, decided by the owners. Dry-run first, always.

## Order of a window

| # | Command | Plan step | Notes |
| --- | --- | --- | --- |
| 1 | `maintenance on` | 1 | flips `operations_mode.read_only`; `maintenance status` is the dashboard |
| 2 | `drain --yes` | 1 | cancels open orders and open consilia, waits the hub's relay out. Repeat until `DRAINED`. Queued redemptions are only reported; `--release-queued-redemptions` hands the units back |
| 3 | `burn-fee-shares --yes` | before 2 | retires the `fee` allocation's fee class in `service_arb`. Needs a fresh valuation mark for the product (the burn is priced at its dealing NAV). Rerun until it reports the fee class at 0 |
| 4 | `window-export --out export.jsonl` | 2 | must exit 0 (`clean`) |
| 5 | invest imports the file; its Σ per allocation reconciled | 3 | outside this tool |
| 6 | `remap --yes [--treasury-from-fee <usdt>]` | 4 | see below |
| 7 | `close-share-accounts --export export.jsonl --yes` | 5 | `--yes` means invest's import is done and reconciled |
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
