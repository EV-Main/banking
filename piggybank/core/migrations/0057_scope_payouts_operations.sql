-- 0057: payouts out of scope accounts and the operations that wait on concierge's approval
-- (`scope.v1.ScopeLedgerService.Payout`, `RequestPayout`, `RequestTransfer`,
-- `GetPendingOperation`, part 2b). Four new tables; `scope_policies` gains one column with a
-- constant default; `scope_transfers` gains a nullable column and its CHECKs are WIDENED
-- (dropped and re-added looser), so an approved transfer may cross scopes. Nothing existing is
-- narrowed: every row the previous binary wrote passes the new CHECKs, and every row it would
-- write still does.
--
-- Every statement is IF NOT EXISTS or DROP … IF EXISTS before an ADD, so re-applying after the
-- rollback below, with the tables kept, does not fail on them.
--
-- REVERSIBILITY: DROP TABLE IF EXISTS scope_operation_lines, scope_operations,
--   scope_payout_lines, scope_payouts;
--   ALTER TABLE scope_policies DROP COLUMN IF EXISTS scope_budget_24h;
--   ALTER TABLE scope_transfers DROP CONSTRAINT IF EXISTS scope_transfers_crosses_only_approved,
--     DROP CONSTRAINT IF EXISTS scope_transfers_two_accounts, DROP CONSTRAINT IF EXISTS scope_transfers_approved_names_operation,
--     DROP CONSTRAINT IF EXISTS scope_transfers_approved_to_own,
--     ADD CONSTRAINT scope_transfers_check CHECK (to_scope = scope),
--     ADD CONSTRAINT scope_transfers_check1 CHECK (from_label <> to_label),
--     DROP CONSTRAINT IF EXISTS scope_transfers_reason_check,
--     ADD CONSTRAINT scope_transfers_reason_check CHECK (reason = 'fee_sweep'),
--     DROP COLUMN IF EXISTS operation_id;
--   Safe only while no payout was recorded and no operation executed. Once one has, its
--   TigerBeetle transfers outlive the rows: drop them and the users' compartments still hold
--   the money with nothing in Postgres saying who paid it or on whose approval; an approved
--   transfer row fails the narrowed CHECKs.
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 57;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   tables, the column and the widened CHECKs may stay instead: the older binary never reads
--   the tables, writes only fee sweeps, and counts an approved transfer against a client's
--   automatic ceiling (stricter, never looser). An older relay meeting a `scope_payouts`
--   outbox event dispatches it without posting it (it does not know the kind); the payout
--   stays `pending`, still counted against its source, and the reaper of a build that knows
--   the kind replays it once one runs again. Roll back only with no operation awaiting: an
--   older binary does not execute approvals, so an approved one lapses unexecuted after 72 h.
SET LOCAL lock_timeout = '3s';

-- One daily budget per scope for everything that leaves its accounts without the platform
-- owners — every client's automatic payouts plus the scope-level decisions (D24(4)). 5000 USDT,
-- the scope-level daily cap concierge's floor holds for the same kinds. Constant default:
-- metadata-only, no rewrite.
ALTER TABLE scope_policies
	ADD COLUMN IF NOT EXISTS scope_budget_24h TEXT NOT NULL DEFAULT '5000000000000000000000' CHECK (scope_budget_24h ~ '^[0-9]+$');

-- An approved `RequestTransfer` executes as a `scope_transfers` row of reason `approved`, which
-- names its operation and alone may leave the scope. The table is days old and small, so the
-- re-added CHECKs scan nothing worth a NOT VALID step.
ALTER TABLE scope_transfers ADD COLUMN IF NOT EXISTS operation_id UUID;
CREATE UNIQUE INDEX IF NOT EXISTS scope_transfers_operation_idx ON scope_transfers (operation_id) WHERE operation_id IS NOT NULL;
ALTER TABLE scope_transfers
	DROP CONSTRAINT IF EXISTS scope_transfers_reason_check,
	ADD CONSTRAINT scope_transfers_reason_check CHECK (reason IN ('fee_sweep', 'approved')),
	DROP CONSTRAINT IF EXISTS scope_transfers_check,
	DROP CONSTRAINT IF EXISTS scope_transfers_crosses_only_approved,
	ADD CONSTRAINT scope_transfers_crosses_only_approved CHECK (to_scope = scope OR reason = 'approved'),
	DROP CONSTRAINT IF EXISTS scope_transfers_check1,
	DROP CONSTRAINT IF EXISTS scope_transfers_two_accounts,
	ADD CONSTRAINT scope_transfers_two_accounts CHECK (to_scope <> scope OR from_label <> to_label),
	DROP CONSTRAINT IF EXISTS scope_transfers_approved_names_operation,
	ADD CONSTRAINT scope_transfers_approved_names_operation CHECK ((reason = 'approved') = (operation_id IS NOT NULL)),
	DROP CONSTRAINT IF EXISTS scope_transfers_approved_to_own,
	ADD CONSTRAINT scope_transfers_approved_to_own CHECK (reason <> 'approved' OR (to_label NOT IN ('fee', 'treasury') AND to_scope <> 'main'));
-- The automatic ceiling sums a client's own transfers only; approved ones are not counted.
CREATE INDEX IF NOT EXISTS scope_transfers_auto_ceiling_idx ON scope_transfers (client_id, scope, created_at) WHERE operation_id IS NULL;

-- A payout out of a scope account into users' compartments in the scope: a client's automatic
-- one (`Payout`), or banking executing an approved `RequestPayout` (`operation_id`). The relay
-- posts its lines as ONE linked chain from the outbox row written in the same transaction and
-- stamps `posted`, or `failed` when the ledger refuses the chain; both are terminal. A
-- `pending` row is promised away: it counts against the source's spendable and, when
-- automatic, against the client's ceiling and the scope's budget. Amounts are 18-dp base
-- units as TEXT.
CREATE TABLE IF NOT EXISTS scope_payouts (
	id             UUID PRIMARY KEY,
	client_id      TEXT NOT NULL CHECK (client_id ~ '^[a-z][a-z0-9_-]{0,63}$'),
	scope          TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$' AND scope <> 'main'),
	from_label     TEXT NOT NULL CHECK (from_label ~ '^[a-z0-9_:.-]{1,64}$' AND from_label NOT IN ('fee', 'treasury')),
	total          TEXT NOT NULL CHECK (total ~ '^[1-9][0-9]*$'),
	recipients     INTEGER NOT NULL CHECK (recipients BETWEEN 1 AND 1000),
	reason         TEXT NOT NULL CHECK (reason IN ('redemption', 'book', 'fee_rebate', 'approved')),
	operation_id   UUID UNIQUE,
	subject_ref    TEXT NOT NULL CHECK (length(subject_ref) BETWEEN 1 AND 128),
	memo           TEXT NOT NULL DEFAULT '' CHECK (length(memo) <= 140),
	state          TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'posted', 'failed')),
	failure_reason TEXT,
	created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
	-- Last transition or re-enqueue: the reaper waits from here before replaying again.
	updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
	CHECK ((reason = 'approved') = (operation_id IS NOT NULL)),
	CHECK ((state = 'failed') = (failure_reason IS NOT NULL))
);
-- The client's rolling 24 h ceiling, and the scope's budget across clients.
CREATE INDEX IF NOT EXISTS scope_payouts_ceiling_idx ON scope_payouts (client_id, scope, created_at);
CREATE INDEX IF NOT EXISTS scope_payouts_budget_idx ON scope_payouts (scope, created_at);
-- The Read-First sum of an account's in-flight debits, and the reaper's scan.
CREATE INDEX IF NOT EXISTS scope_payouts_pending_idx ON scope_payouts (scope, from_label) WHERE state = 'pending';
CREATE INDEX IF NOT EXISTS scope_payouts_stale_idx ON scope_payouts (updated_at) WHERE state = 'pending';

-- A payout's lines, one per user; `line_no` is the leg's place in the chain, from which its
-- ledger id derives. `auth_subject` is kept as the client named the user, to echo it back.
CREATE TABLE IF NOT EXISTS scope_payout_lines (
	payout_id    UUID NOT NULL REFERENCES scope_payouts (id),
	line_no      INTEGER NOT NULL CHECK (line_no BETWEEN 0 AND 999),
	user_id      UUID NOT NULL REFERENCES users (id),
	auth_subject TEXT NOT NULL CHECK (length(auth_subject) BETWEEN 1 AND 255),
	amount       TEXT NOT NULL CHECK (amount ~ '^[1-9][0-9]*$'),
	PRIMARY KEY (payout_id, line_no),
	UNIQUE (payout_id, user_id)
);

-- An operation a client asked for (`RequestPayout` / `RequestTransfer`), stored whole: the
-- approval banking opens is rendered from this row (`payload_hash`, `lines_digest`) and the
-- execution reads this row, never a later call. It leaves `awaiting_approval` exactly once,
-- under `FOR UPDATE`; every other state is terminal.
CREATE TABLE IF NOT EXISTS scope_operations (
	id                UUID PRIMARY KEY,
	client_id         TEXT NOT NULL CHECK (client_id ~ '^[a-z][a-z0-9_-]{0,63}$'),
	scope             TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$' AND scope <> 'main'),
	kind              TEXT NOT NULL CHECK (kind IN ('bank:scope_payout', 'bank:account_transfer', 'bank:scope_transfer')),
	from_label        TEXT NOT NULL CHECK (from_label ~ '^[a-z0-9_:.-]{1,64}$' AND from_label NOT IN ('fee', 'treasury')),
	-- Transfers: the destination. NULL for payouts.
	to_scope          TEXT CHECK (to_scope ~ '^[a-z][a-z0-9_]{0,31}$' AND to_scope <> 'main'),
	to_label          TEXT CHECK (to_label ~ '^[a-z0-9_:.-]{1,64}$' AND to_label NOT IN ('fee', 'treasury')),
	total             TEXT NOT NULL CHECK (total ~ '^[1-9][0-9]*$'),
	distribution      BOOLEAN NOT NULL DEFAULT false,
	-- Decided when the request was stored, under the scope's budget lock (D24(4)).
	over_scope_budget BOOLEAN NOT NULL DEFAULT false,
	lines_digest      TEXT CHECK (lines_digest ~ '^[0-9a-f]{64}$'),
	payload_hash      TEXT NOT NULL CHECK (payload_hash ~ '^[0-9a-f]{64}$'),
	subject_ref       TEXT NOT NULL CHECK (length(subject_ref) BETWEEN 1 AND 128),
	memo              TEXT NOT NULL DEFAULT '' CHECK (length(memo) <= 140),
	state             TEXT NOT NULL DEFAULT 'awaiting_approval' CHECK (state IN ('awaiting_approval', 'executed', 'rejected', 'failed')),
	-- The concierge approval; NULL until it is opened.
	approval_id       TEXT UNIQUE CHECK (length(approval_id) BETWEEN 1 AND 128),
	-- The level concierge resolved it to at open: a scope-level one draws on the scope's
	-- budget, a platform one does not.
	approval_level    TEXT CHECK (approval_level IN ('platform', 'scope')),
	result_id         TEXT,
	failure_reason    TEXT,
	created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
	decided_at        TIMESTAMPTZ,
	updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
	-- When the executor next opens its approval or asks where it stands, and how often it
	-- already tried: the backoff after concierge did not answer.
	next_attempt_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
	attempts          INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
	CHECK ((kind = 'bank:scope_payout') = (lines_digest IS NOT NULL)),
	CHECK ((kind = 'bank:scope_payout') = (to_label IS NULL)),
	CHECK ((to_scope IS NULL) = (to_label IS NULL)),
	CHECK (kind <> 'bank:account_transfer' OR (to_scope = scope AND to_label <> from_label)),
	CHECK (kind <> 'bank:scope_transfer' OR to_scope <> scope),
	CHECK (kind = 'bank:scope_payout' OR NOT (distribution OR over_scope_budget)),
	CHECK ((state = 'executed') = (result_id IS NOT NULL)),
	CHECK ((state IN ('rejected', 'failed')) = (failure_reason IS NOT NULL)),
	CHECK ((state = 'awaiting_approval') = (decided_at IS NULL))
);
-- The executor's scan of what to open or to ask about.
CREATE INDEX IF NOT EXISTS scope_operations_due_idx ON scope_operations (next_attempt_at) WHERE state = 'awaiting_approval';
-- The scope's budget: live and recently executed payouts decided inside the scope.
CREATE INDEX IF NOT EXISTS scope_operations_budget_idx ON scope_operations (scope, kind) WHERE state IN ('awaiting_approval', 'executed');

-- A requested payout's lines, as they will be paid.
CREATE TABLE IF NOT EXISTS scope_operation_lines (
	operation_id UUID NOT NULL REFERENCES scope_operations (id),
	line_no      INTEGER NOT NULL CHECK (line_no BETWEEN 0 AND 999),
	user_id      UUID NOT NULL REFERENCES users (id),
	auth_subject TEXT NOT NULL CHECK (length(auth_subject) BETWEEN 1 AND 255),
	amount       TEXT NOT NULL CHECK (amount ~ '^[1-9][0-9]*$'),
	PRIMARY KEY (operation_id, line_no),
	UNIQUE (operation_id, user_id)
);
