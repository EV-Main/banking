-- 0052: scopes and users' compartments in them (D9), scope accounts and banking's treasury
-- (D17). Expand only: four new tables, and two CHECKs widened so banking's treasury can be
-- the source of a payment order and of the withdrawal that executes one. The money itself is
-- in TigerBeetle (`scope_claim:<scope>:<user>` code 21, `scope_account:<scope>:<label>` code
-- 31); these rows are the registry, the policies, the account owners and the moves.
--
-- REVERSIBILITY: DROP TABLE compartment_moves, scope_accounts, scope_policies, scopes;
--   ALTER TABLE payments DROP CONSTRAINT payments_from_kind_check,
--     ADD CONSTRAINT payments_from_kind_check CHECK (from_kind IN ('user', 'service'));
--   ALTER TABLE withdrawals DROP CONSTRAINT withdrawals_source_check,
--     ADD CONSTRAINT withdrawals_source_check CHECK (source IN ('user'));
--   Safe only while no move has been recorded and nothing was paid out of the treasury. Once
--   a move has, its TigerBeetle transfer and the compartment balance it created outlive the
--   rows: drop them and the balances still exist in TigerBeetle with nothing in Postgres to
--   say whose they are; a treasury payment row fails the narrowed CHECKs.
SET LOCAL lock_timeout = '3s';

-- The scopes banking knows. Filled from concierge's scope stream once that is wired; until
-- then the two that exist are seeded below. `main` is the implicit root (the user's own
-- wallet) and never a row.
CREATE TABLE scopes (
	id           TEXT PRIMARY KEY CHECK (id ~ '^[a-z][a-z0-9_]{0,31}$' AND id <> 'main'),
	state        TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'frozen', 'closed')),
	display_name TEXT NOT NULL CHECK (length(display_name) BETWEEN 1 AND 128),
	created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
	updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- What it takes to move money into a scope, and how far its services may move money
-- without an approval. Amounts are 18-dp USDT base units as TEXT, like every amount the
-- control plane keeps; a NULL limit is no limit. The automatic ceilings default to
-- 1000 / 5000 / 5000 USDT (`scope/v1/ledger.proto`).
CREATE TABLE scope_policies (
	scope              TEXT PRIMARY KEY REFERENCES scopes (id),
	min_kyc_level      INTEGER NOT NULL DEFAULT 0 CHECK (min_kyc_level >= 0),
	max_per_move       TEXT CHECK (max_per_move ~ '^[1-9][0-9]*$'),
	max_moved_in_24h   TEXT CHECK (max_moved_in_24h ~ '^[1-9][0-9]*$'),
	max_balance        TEXT CHECK (max_balance ~ '^[1-9][0-9]*$'),
	auto_payout_per_op TEXT NOT NULL DEFAULT '1000000000000000000000' CHECK (auto_payout_per_op ~ '^[0-9]+$'),
	auto_payout_24h    TEXT NOT NULL DEFAULT '5000000000000000000000' CHECK (auto_payout_24h ~ '^[0-9]+$'),
	auto_transfer_24h  TEXT NOT NULL DEFAULT '5000000000000000000000' CHECK (auto_transfer_24h ~ '^[0-9]+$'),
	updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A scope's own accounts and who owns them: only the owner debits one. `scope` may be
-- `main` (banking's treasury), so there is no foreign key to `scopes`.
CREATE TABLE scope_accounts (
	scope        TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$'),
	label        TEXT NOT NULL CHECK (label ~ '^[a-z0-9_:.-]{1,64}$'),
	owner_client TEXT NOT NULL CHECK (length(owner_client) BETWEEN 1 AND 64),
	created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
	PRIMARY KEY (scope, label)
);

-- A user's own move between `main` and one scope, recorded once per idempotency key; the
-- relay posts it to TigerBeetle from the outbox row written in the same transaction and
-- stamps `posted` — or `failed` when the ledger refuses it. Both are terminal: a failed
-- move is never executed later. A `pending` row whose event was lost (an older relay that
-- did not know the kind, during a rolling deploy) is re-enqueued by the reaper; the ledger
-- transfer id is derived from the move id, so a replay can never post twice.
CREATE TABLE compartment_moves (
	id              UUID PRIMARY KEY,
	user_id         UUID NOT NULL,
	from_scope      TEXT NOT NULL CHECK (from_scope ~ '^[a-z][a-z0-9_]{0,31}$'),
	to_scope        TEXT NOT NULL CHECK (to_scope ~ '^[a-z][a-z0-9_]{0,31}$'),
	amount          TEXT NOT NULL CHECK (amount ~ '^[1-9][0-9]*$'),
	idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
	state           TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'posted', 'failed')),
	failure_reason  TEXT,
	created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
	-- Last transition or re-enqueue: the reaper waits from here before replaying again.
	updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
	CHECK ((from_scope = 'main') <> (to_scope = 'main')),
	CHECK ((state = 'failed') = (failure_reason IS NOT NULL)),
	UNIQUE (user_id, idempotency_key)
);
-- The Read-First sums of a user's in-flight moves, and the reaper's scan.
CREATE INDEX compartment_moves_pending_idx ON compartment_moves (user_id, from_scope, to_scope) WHERE state = 'pending';
CREATE INDEX compartment_moves_stale_idx ON compartment_moves (updated_at) WHERE state = 'pending';
-- The rolling 24 h "moved in" sum reads one user's recent moves into one scope.
CREATE INDEX compartment_moves_in_idx ON compartment_moves (user_id, to_scope, created_at);

INSERT INTO scopes (id, display_name) VALUES ('invest', 'Invest'), ('sa', 'Service-Arb');
INSERT INTO scope_policies (scope) VALUES ('invest'), ('sa');
-- banking's own: the treasury in `main`, and the fee collector it opens in every scope.
INSERT INTO scope_accounts (scope, label, owner_client) VALUES
	('main', 'treasury', 'banking'),
	('invest', 'fee', 'banking'),
	('sa', 'fee', 'banking');

-- banking's treasury as a payment's source (`Party::Treasury`, owners' consilium) and the
-- withdrawal that executes an L1 one (`source = 'treasury'`, no user — the 0024 CHECK
-- `(source = 'user') = (user_id IS NOT NULL)` already says so). Both tables are small and
-- every existing row passes, so the re-validation is a short scan under the lock timeout.
ALTER TABLE payments
	DROP CONSTRAINT payments_from_kind_check,
	ADD CONSTRAINT payments_from_kind_check CHECK (from_kind IN ('user', 'service', 'treasury'));
ALTER TABLE withdrawals
	DROP CONSTRAINT withdrawals_source_check,
	ADD CONSTRAINT withdrawals_source_check CHECK (source IN ('user', 'treasury'));
