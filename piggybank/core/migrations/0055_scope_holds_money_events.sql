-- 0055: holds on users' compartments (`ScopeLedgerService.Reserve/Capture/Void/GetHold`) and
-- the money-event stream scope services pull (`scope.v1.MoneyEvents`), part 2a. Expand only:
-- three new tables, nothing existing is touched. The money is in TigerBeetle — a hold is one
-- pending transfer `scope_claim:<scope>:<user> → hold_clearing` (code 51, transfer code 57);
-- these rows are who holds what under which mandate, and what each client was told.
--
-- Every statement is IF NOT EXISTS, so re-applying after the rollback below, with the tables
-- kept, does not fail on them.
--
-- REVERSIBILITY: DROP TABLE IF EXISTS money_event_deliveries, money_events, scope_holds;
--   Safe only while no hold has been placed. Once one has, its pending TigerBeetle transfer
--   outlives the row: drop it and the user's money stays locked on the ledger with nothing in
--   Postgres to say why or who may release it. The event tables alone may be dropped at any
--   time, at the price of every client's projection: their cursors then point past nothing.
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 55;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   tables may stay instead of being dropped: the older binary never reads them. An older
--   relay meeting a `scope_holds` outbox event dispatches it without applying it (it does not
--   know the kind), so roll back only with no hold placed.
SET LOCAL lock_timeout = '3s';

-- One hold a client placed on a user's compartment. `amount`, `captured` are 18-dp base
-- units as TEXT, like every amount the control plane keeps. `version` is the hold's last
-- event's; `pending_version` the event whose ledger reservation stands now — its TigerBeetle
-- id derives from it, so the next capture or release knows what to void. `auth_subject` is
-- kept as the client named the user, to echo it back; `user_id` is who it resolved to.
CREATE TABLE IF NOT EXISTS scope_holds (
	id              UUID PRIMARY KEY,
	client_id       TEXT NOT NULL CHECK (client_id ~ '^[a-z][a-z0-9_-]{0,63}$'),
	scope           TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$' AND scope <> 'main'),
	user_id         UUID NOT NULL REFERENCES users (id),
	auth_subject    TEXT NOT NULL CHECK (length(auth_subject) BETWEEN 1 AND 255),
	amount          TEXT NOT NULL CHECK (amount ~ '^[1-9][0-9]*$'),
	captured        TEXT NOT NULL DEFAULT '0' CHECK (captured ~ '^(0|[1-9][0-9]*)$'),
	state           TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'captured', 'voided', 'expired')),
	version         BIGINT NOT NULL DEFAULT 1 CHECK (version >= 1),
	pending_version BIGINT NOT NULL DEFAULT 1 CHECK (pending_version BETWEEN 1 AND version),
	mandate_id      UUID NOT NULL,
	subject_ref     TEXT NOT NULL CHECK (length(subject_ref) BETWEEN 1 AND 128),
	-- NULL: no expiry, the hold lives until captured or voided.
	expires_at      TIMESTAMPTZ,
	created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
	updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- The reaper's scan for holds past their expiry.
CREATE INDEX IF NOT EXISTS scope_holds_due_idx ON scope_holds (expires_at) WHERE state = 'active' AND expires_at IS NOT NULL;

-- The money plane's events as scope services see them, oldest first by `position`. Written
-- once — an account's opening with the account, a movement by the relay right after the
-- ledger took it — and never updated or compacted. `event_id` is derived from what the
-- event is about, so a relay that applies an event twice records it once. Appends serialize
-- on one advisory lock, so positions become visible in order and a consumer's cursor never
-- skips a row that commits late.
--
-- `balances` is `[{"label", "owner", "balance"}]`: the scope accounts the movement touched,
-- right after it, each with its owner — a client is shown only its own.
CREATE TABLE IF NOT EXISTS money_events (
	position           BIGSERIAL PRIMARY KEY,
	event_id           UUID NOT NULL UNIQUE,
	scope              TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$'),
	-- A shape, not a list: each later kind (charges, payouts, invoices) adds its name
	-- without an ALTER.
	kind               TEXT NOT NULL CHECK (kind ~ '^[a-z_]{1,32}$'),
	ref_id             TEXT NOT NULL DEFAULT '',
	aggregate_version  BIGINT NOT NULL CHECK (aggregate_version >= 1),
	auth_subject       TEXT NOT NULL DEFAULT '',
	amount             TEXT NOT NULL DEFAULT '' CHECK (amount ~ '^([0-9]+)?$'),
	from_label         TEXT,
	from_subject       TEXT,
	to_label           TEXT,
	to_subject         TEXT,
	subject_ref        TEXT NOT NULL DEFAULT '',
	initiator_client   TEXT NOT NULL DEFAULT '',
	balances           JSONB NOT NULL DEFAULT '[]'::jsonb,
	hold_remaining     TEXT CHECK (hold_remaining ~ '^[0-9]+$'),
	counterparty_scope TEXT NOT NULL DEFAULT '',
	failure_reason     TEXT NOT NULL DEFAULT '',
	occurred_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
	CHECK (from_label IS NULL OR from_subject IS NULL),
	CHECK (to_label IS NULL OR to_subject IS NULL)
);

-- Who receives which event: one row per receiving client, written with the event. Visibility
-- is decided once, when banking knows who owned what; `Pull` reads a client's own rows by
-- cursor and never re-derives it.
CREATE TABLE IF NOT EXISTS money_event_deliveries (
	client_id TEXT NOT NULL CHECK (client_id ~ '^[a-z][a-z0-9_-]{0,63}$'),
	position  BIGINT NOT NULL REFERENCES money_events (position),
	scope     TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$'),
	PRIMARY KEY (client_id, position)
);
