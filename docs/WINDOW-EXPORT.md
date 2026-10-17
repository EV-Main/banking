# Window export — the unit book, read-only (schema version 1)

Step 2 of the migration window (org restructure plan §6.5): `window-export` reads
allocations, their policies, valuations, fee history, the unit holdings in TigerBeetle and
the ownership of the reserved `fee` / `fund` allocations out of banking, for invest to
import. It **writes nothing**: Postgres is opened with `default_transaction_read_only=on`
and the snapshot is a `READ ONLY` transaction; TigerBeetle is only read through the
`Ledger` reads (`balance`, `share_holdings`).

The record types are `domain::window_export` (`domain/src/window_export.rs`). The importer
pins the `domain` crate by git and parses the file with them; this document is the
human-readable twin, and the module header is authoritative when they differ.

## Running it

```bash
DATABASE_URL=… TIGERBEETLE_ADDRESS=… TIGERBEETLE_CLUSTER_ID=… \
  window-export --out export.jsonl          # refuses to overwrite; no --out = stdout
```

- The hub must be in maintenance (`operations_mode.read_only = true`); otherwise the run
  refuses. `--allow-live` overrides for a rehearsal: the header says `"live": true` and the
  file must not be imported.
- `--out` creates the file with mode `0600` (it lists every holder's id and units).
- The Σ of units per allocation goes to stderr. Exit code is non-zero when the export is
  not `clean` (a supply its holders do not add up to, or an undrained outbox).
- Postgres is read in one `REPEATABLE READ` snapshot. TigerBeetle is read after it, so the
  file is a consistent cut only if the outbox was drained first (plan step 1); the trailer
  reports what was left.

## Format

JSON Lines. Every line is an object with a `"record"` tag. Order: `header`, then
`allocation`, `fee_policy`, `fee_policy_change`, `book_policy`, `valuation`, `position`,
`fee_assessment`, `fee_settlement`, `user`, then per allocation `share_holding`* `supply`
(`ownership` for `fee` / `fund`), and last `trailer`. A file without a trailer is truncated.

Conventions: units and money are **decimal strings of 18-dp base units**; ids are UUID
strings; allocations are named by `service` slug; timestamps are RFC 3339 UTC. Rows are
exported as stored, history included.

| `record` | What | Source |
| --- | --- | --- |
| `header` | `schema_version`, `source`=`banking`, `exported_at`, `operations_read_only`, `live`, `tigerbeetle_cluster_id` | `operations_mode` |
| `allocation` | registry row incl. `access`, `backing`, `unit_cap` | `allocations` |
| `fee_policy` | terms in force | `fee_policies` |
| `fee_policy_change` | full policy history, incl. consilium link and notice waiver | `fee_policy_changes` |
| `book_policy` | trading terms (no row = closed book) | `book_policies` |
| `valuation` | every NAV mark | `fund_valuations` |
| `position` | per holder: `cost_basis`, `high_water_mark`, `fee_debt`, accrual stamps, projected `units` | `fund_positions` |
| `fee_assessment` | every fee charge | `fee_assessments` |
| `fee_settlement` | every bulk conversion of fee-class units to cash | `fee_settlements` |
| `user` | `user_id` ↔ `concierge_user_id` for every holder in the file | `users` |
| `share_holding` | non-zero balance: `kind` = `user_shares` / `book_shares` (with `user_id`) or `fee_shares` (the `fee` allocation's fee class) | TigerBeetle Share ledger |
| `supply` | `shares_outstanding` and the allocation's cash claim (`posted` / `pending` / `locked`) | TigerBeetle |
| `ownership` | holders of `fee` / `fund` as banking's ownership view reads them (people, free + order-locked units summed), plus fee-class units `fee` holds per product | derived from the above |
| `trailer` | checksums, below | exporter |

The ledger is authoritative for units; `position.units` is a projection and is exported
only so the importer can compare.

### Trailer

- `counts` — lines per `record` kind, header and trailer included; Σ = number of lines.
- `allocations[]` — per allocation: `user_shares`, `book_shares`, `fee_shares`, `held`
  (their sum), `shares_outstanding`, `balanced` (`held == shares_outstanding`).
- `total_held`, `total_outstanding` — Σ over every allocation.
- `outbox_undispatched` — outbox events neither dispatched nor parked; the window needs 0.
- `pending_or_locked` — Σ base units shown as pending or locked on the supplies, the cash claims and every unit holding (in-flight two-phase transfers). The window needs `0`. Added to schema 1 with a default (`"0"` when absent), so the version did not change.
- `clean` — every allocation balanced, `outbox_undispatched == 0` and `pending_or_locked == 0`.

The importer recomputes `allocations[]` from its own tables after import; equality per
allocation is the "Σ units reconcile" of plan step 3.

## Test anchor

`service_arb` holders must read valeratrades 13 000 and inintel44 3 250 units
(`13000000000000000000000` / `3250000000000000000000`), Σ 16 250. With `jq`:

```bash
jq -c 'select(.record=="share_holding" and .service=="service_arb")' export.jsonl
jq -c 'select(.record=="trailer") | .allocations[] | select(.service=="service_arb")' export.jsonl
```

Map `user_id` to the person through the `user` records (`concierge_user_id`).

## Not exported

Subscriptions, redemptions, book orders and trades, `unit_issuances` and per-investor
`allocation_access_grants` — the window drains or cancels the first three first (plan step
1), and the importer starts from holdings, positions and fee history. Say so if invest
needs any of them and the schema gets a version bump.
