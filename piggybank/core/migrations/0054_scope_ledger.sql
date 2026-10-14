-- 0054: the scope ledger's control plane (`scope.v1.ScopeLedgerService`, part 1). Expand only:
-- two new tables and one new policy column with a constant default. The money is in
-- TigerBeetle (`scope_account:<scope>:<label>`, code 31, since 0052); these rows are who
-- asked for what, under which key, and where each transfer stands.
--
-- Every statement is IF NOT EXISTS: the rollback below may leave the tables and the column in
-- place, and re-applying after it must not fail on them.
--
-- REVERSIBILITY: DROP TABLE IF EXISTS scope_transfers, scope_ledger_requests;
--   ALTER TABLE scope_policies DROP COLUMN IF EXISTS auto_transfer_per_op;
--   Safe only while no transfer has been recorded. Once one has, its TigerBeetle transfer
--   outlives the rows: drop them and the ledger still shows the money moved, with nothing in
--   Postgres saying who moved it or under which key a retry would find it.
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 54;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   tables and the column may stay instead of being dropped: the older binary never reads
--   them.
--   An older relay dispatches a `scope_ledger` event without posting it (it does not know the
--   kind); the row stays `pending`, still counted against the source account, and the reaper
--   of a build that knows the kind replays it once one runs again.
SET LOCAL lock_timeout = '3s';

-- The automatic per-operation ceiling on a client's `Transfer` (into `(scope, fee)`), next to
-- the 24 h one 0052 stored. 1000 USDT, like `auto_payout_per_op` (ORG-RESTRUCTURE-PLAN §3.9).
-- A constant default makes the column metadata-only: no table rewrite, no long lock.
ALTER TABLE scope_policies
	ADD COLUMN IF NOT EXISTS auto_transfer_per_op TEXT NOT NULL DEFAULT '1000000000000000000000' CHECK (auto_transfer_per_op ~ '^[0-9]+$');

-- Every mutating scope-ledger call that went through, by its idempotency key — unique per
-- (calling client, scope, RPC), as the contract has it. `terms` is the canonical rendering of
-- everything the request asked (built by the application layer), so a repeat is compared on
-- all of it and a key reused for other terms is told so. `result_id` names what the call
-- produced: the label for `open_account`, the transfer id for `transfer`. Written in the same
-- transaction as the thing it names, so a key never points at nothing.
CREATE TABLE IF NOT EXISTS scope_ledger_requests (
	client_id       TEXT NOT NULL CHECK (client_id ~ '^[a-z][a-z0-9_-]{0,63}$'),
	scope           TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$'),
	-- A shape, not a list: each later RPC (Payout, Reserve, …) adds its name without an ALTER.
	rpc             TEXT NOT NULL CHECK (rpc ~ '^[a-z_]{1,32}$'),
	idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
	terms           TEXT NOT NULL,
	result_id       TEXT NOT NULL,
	created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
	PRIMARY KEY (client_id, scope, rpc, idempotency_key)
);

-- A client's automatic transfer from an account it owns into the scope's `fee` collector
-- (`TransferReason::FEE_SWEEP`). The relay posts it from the outbox row written in the same
-- transaction and stamps `posted`, or `failed` when the ledger refuses it; both are terminal.
-- A `pending` row is promised away: it counts against the source account's spendable and
-- against the client's rolling 24 h ceiling until it is posted or fails.
CREATE TABLE IF NOT EXISTS scope_transfers (
	id             UUID PRIMARY KEY,
	client_id      TEXT NOT NULL CHECK (client_id ~ '^[a-z][a-z0-9_-]{0,63}$'),
	scope          TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$'),
	from_label     TEXT NOT NULL CHECK (from_label ~ '^[a-z0-9_:.-]{1,64}$'),
	to_scope       TEXT NOT NULL CHECK (to_scope ~ '^[a-z][a-z0-9_]{0,31}$'),
	to_label       TEXT NOT NULL CHECK (to_label ~ '^[a-z0-9_:.-]{1,64}$'),
	amount         TEXT NOT NULL CHECK (amount ~ '^[1-9][0-9]*$'),
	reason         TEXT NOT NULL CHECK (reason IN ('fee_sweep')),
	subject_ref    TEXT NOT NULL CHECK (length(subject_ref) BETWEEN 1 AND 128),
	memo           TEXT NOT NULL DEFAULT '' CHECK (length(memo) <= 140),
	state          TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'posted', 'failed')),
	failure_reason TEXT,
	created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
	-- Last transition or re-enqueue: the reaper waits from here before replaying again.
	updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
	-- A client's own transfer never leaves its scope; only banking's sweep and an approved
	-- RequestTransfer may, and neither is recorded here yet.
	CHECK (to_scope = scope),
	CHECK (from_label <> to_label),
	-- banking's own labels are never a client's source, and a fee sweep only lands in `fee` —
	-- the rules the use case and the domain already hold, kept by the table too.
	CHECK (from_label NOT IN ('fee', 'treasury')),
	CHECK (reason <> 'fee_sweep' OR to_label = 'fee'),
	CHECK ((state = 'failed') = (failure_reason IS NOT NULL))
);
-- The rolling 24 h ceiling per (client, scope).
CREATE INDEX IF NOT EXISTS scope_transfers_ceiling_idx ON scope_transfers (client_id, scope, created_at);
-- The Read-First sum of an account's in-flight debits, and the reaper's scan.
CREATE INDEX IF NOT EXISTS scope_transfers_pending_idx ON scope_transfers (scope, from_label) WHERE state = 'pending';
CREATE INDEX IF NOT EXISTS scope_transfers_stale_idx ON scope_transfers (updated_at) WHERE state = 'pending';
