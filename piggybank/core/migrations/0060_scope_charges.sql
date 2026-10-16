-- 0060: one-off charges of a user's compartment (`scope.v1.ScopeLedgerService.Charge`).
-- Expand only: one new table and three indexes on it. The money is in TigerBeetle — a charge is
-- one posted transfer `scope_claim:<scope>:<user> → scope_account:<scope>:<label>` (transfer
-- code 60); these rows are who charged whom for what, under which mandate, and whether the
-- relay has posted it. Numbered 0060 because 0059 belongs to the fees/holds branch; the two
-- touch no common object, so they apply in either order.
--
-- Every statement is IF NOT EXISTS, so re-applying after the rollback below, with the table
-- kept, does not fail on it.
--
-- REVERSIBILITY: DROP TABLE IF EXISTS scope_charges;
--   Safe only while no charge has been made. Once one has, its TigerBeetle transfer outlives
--   the row: drop it and the ledger still shows the user's money in the scope's account with
--   nothing in Postgres saying which charge took it, and the mandate's 24 h cap forgets what
--   was drawn — so drop it only with no charge in the last 24 h.
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 60;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   table may stay instead of being dropped: the older binary never reads it. An older relay
--   meeting a `scope_charges` outbox event dispatches it without posting it (it does not know
--   the kind), so roll back only with no charge `pending`.
SET LOCAL lock_timeout = '3s';

-- One charge. `amount` is 18-dp base units as TEXT, like every amount the control plane keeps.
-- `state` is `pending` from the moment it is recorded until the relay posts it, then `posted`;
-- a charge the ledger refuses (or the relay parks) is `failed`, terminal: nothing moved, it no
-- longer counts against the mandate, and it is never carried out later. Only `posted` is
-- announced to the client (KIND_CHARGED).
CREATE TABLE IF NOT EXISTS scope_charges (
	id             UUID PRIMARY KEY,
	client_id      TEXT NOT NULL CHECK (client_id ~ '^[a-z][a-z0-9_-]{0,63}$' AND client_id <> 'banking'),
	scope          TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$' AND scope <> 'main'),
	user_id        UUID NOT NULL REFERENCES users (id),
	auth_subject   TEXT NOT NULL CHECK (length(auth_subject) BETWEEN 1 AND 255),
	-- banking's own labels are never credited by a client's charge (see the domain's
	-- `ScopeCharge::restore`).
	to_label       TEXT NOT NULL CHECK (to_label ~ '^[a-z0-9_:.-]{1,64}$' AND to_label NOT IN ('fee', 'treasury')),
	amount         TEXT NOT NULL CHECK (amount ~ '^[1-9][0-9]*$'),
	memo           TEXT NOT NULL DEFAULT '' CHECK (length(memo) <= 140),
	subject_ref    TEXT NOT NULL CHECK (length(subject_ref) BETWEEN 1 AND 128),
	-- The mandate the charge drew on. No FK on purpose: a mandate row is never deleted, and a
	-- charge must outlive any tidy-up of mandates.
	mandate_id     UUID NOT NULL,
	state          TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'posted', 'failed')),
	failure_reason TEXT,
	created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
	updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
	CHECK ((state = 'failed') = (failure_reason IS NOT NULL))
);
-- What a client has drawn on one compartment in the rolling 24 h: the mandate's caps count it.
CREATE INDEX IF NOT EXISTS scope_charges_drawn_idx ON scope_charges (user_id, scope, client_id, created_at);
-- A compartment's charges not yet on the ledger — promised away in every spend check — and the
-- reaper's scan for one whose event was lost.
CREATE INDEX IF NOT EXISTS scope_charges_pending_idx ON scope_charges (user_id, scope) WHERE state = 'pending';
-- The reaper's and the reconciliation's scan for pending charges by age.
CREATE INDEX IF NOT EXISTS scope_charges_pending_age_idx ON scope_charges (created_at) WHERE state = 'pending';
