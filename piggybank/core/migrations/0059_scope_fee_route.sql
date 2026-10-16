-- 0059: banking's own route of swept fees out of `(<scope>, fee)` (`scope/v1/ledger.proto`, "the
-- fee route"), and the index behind `ScopeLedgerService.ListHolds`. Expand only: one nullable
-- column and two indexes are added, and the CHECKs of `scope_transfers` are WIDENED (dropped and
-- re-added looser), so a route row can leave `fee` and cross into `main`. Nothing existing is
-- narrowed: every row the previous binary wrote passes the new CHECKs, and every row it would
-- write still does.
--
-- A route is one `scope_transfers` row of reason `fee_route`, owned by `banking`, written in the
-- same transaction that stamps the client's `fee_sweep` `posted` and naming it in
-- `fee_route_of`. The unique index on that column is the idempotency key of the route: a replay
-- of the stamp (a relay that restarted, the reaper) finds the route already there and writes
-- nothing. The relay posts the row like any other scope transfer.
--
-- Every statement is IF NOT EXISTS or DROP … IF EXISTS before an ADD, so re-applying after the
-- rollback below, with the column kept, does not fail on it.
--
-- REVERSIBILITY: DROP INDEX IF EXISTS scope_holds_client_active_idx;
--   ALTER TABLE scope_transfers DROP CONSTRAINT IF EXISTS scope_transfers_fee_route_shape,
--     DROP CONSTRAINT IF EXISTS scope_transfers_reserved_source,
--     ADD CONSTRAINT scope_transfers_from_label_check1 CHECK (from_label NOT IN ('fee', 'treasury')),
--     DROP CONSTRAINT IF EXISTS scope_transfers_crosses_only_approved,
--     ADD CONSTRAINT scope_transfers_crosses_only_approved CHECK (to_scope = scope OR reason = 'approved'),
--     DROP CONSTRAINT IF EXISTS scope_transfers_reason_check,
--     ADD CONSTRAINT scope_transfers_reason_check CHECK (reason IN ('fee_sweep', 'approved')),
--     DROP COLUMN IF EXISTS fee_route_of;
--   Safe only while no route was recorded. Once one has, its TigerBeetle transfer outlives the
--   row: drop it and the ledger still shows the fees moved to `alloc:fee` or the treasury with
--   nothing in Postgres saying why; a route row fails the narrowed CHECKs, so DELETE them first
--   and only with no `pending` one (its transfer may still post).
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 59;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   column, the widened CHECKs and the index may stay instead: the older binary never reads the
--   column, never writes a route (a fee swept after the rollback simply stays on `fee`, as it
--   did before this migration) and cannot parse a `fee_route` row's reason, so its reaper only
--   logs and skips one still `pending` — roll back with none pending.
SET LOCAL lock_timeout = '3s';

-- The sweep a route was made for. A route names exactly one `fee_sweep`; one `fee_sweep` has at
-- most one route. NULL for every other row, and NULLs do not collide in a unique index.
ALTER TABLE scope_transfers ADD COLUMN IF NOT EXISTS fee_route_of UUID REFERENCES scope_transfers (id);
CREATE UNIQUE INDEX IF NOT EXISTS scope_transfers_fee_route_idx ON scope_transfers (fee_route_of);

-- The table is days old and small, so the re-added CHECKs scan nothing worth a NOT VALID step.
ALTER TABLE scope_transfers
	DROP CONSTRAINT IF EXISTS scope_transfers_reason_check,
	ADD CONSTRAINT scope_transfers_reason_check CHECK (reason IN ('fee_sweep', 'approved', 'fee_route')),
	-- banking's own labels are never a client's source; the one row that leaves `fee` is banking's
	-- own route (`scope_transfers_fee_route_shape` pins it to `fee`).
	DROP CONSTRAINT IF EXISTS scope_transfers_from_label_check1,
	DROP CONSTRAINT IF EXISTS scope_transfers_reserved_source,
	ADD CONSTRAINT scope_transfers_reserved_source CHECK (reason = 'fee_route' OR from_label NOT IN ('fee', 'treasury')),
	DROP CONSTRAINT IF EXISTS scope_transfers_crosses_only_approved,
	ADD CONSTRAINT scope_transfers_crosses_only_approved CHECK (to_scope = scope OR reason IN ('approved', 'fee_route')),
	-- A route is banking's, out of `fee`, with no operation behind it, and names its sweep; no
	-- other row names one. Where it goes (`alloc:fee` in invest, `treasury` in `main` elsewhere)
	-- is the domain's rule, not a CHECK, so a scope that changes it needs no migration.
	DROP CONSTRAINT IF EXISTS scope_transfers_fee_route_shape,
	ADD CONSTRAINT scope_transfers_fee_route_shape CHECK (
		(reason = 'fee_route') = (fee_route_of IS NOT NULL)
		AND (reason <> 'fee_route' OR (client_id = 'banking' AND from_label = 'fee' AND operation_id IS NULL))
	);

-- `ListHolds`: a client's active holds in a scope, by hold id. `scope_holds_active_idx` (0058)
-- leads with the user, which the listing does not know.
CREATE INDEX IF NOT EXISTS scope_holds_client_active_idx ON scope_holds (client_id, scope, id) WHERE state = 'active';
