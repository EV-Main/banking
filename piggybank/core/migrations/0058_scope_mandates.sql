-- 0058: users' mandates (`banking.v1.CompartmentService.GrantMandate/RevokeMandate/
-- ListMandates`) — the consent every Reserve stands on — and the record of book fills a
-- mandate's BOOK_FILL cap counts. Expand only: two new tables, one nullable column and one
-- index on `scope_holds`. Numbered 0058 because 0057 belongs to the payouts branch (#517); the
-- two touch no common object, so they apply in either order.
--
-- Every statement is IF NOT EXISTS, so re-applying after the rollback below, with the objects
-- kept, does not fail on them.
--
-- REVERSIBILITY: DROP TABLE IF EXISTS scope_book_fills, scope_mandates;
--   DROP INDEX IF EXISTS scope_holds_drawn_idx;
--   ALTER TABLE scope_holds DROP COLUMN IF EXISTS ledger_refused_at;
--   Dropping `scope_mandates` takes every user's consent with it: the older image then refuses
--   every Reserve (`NoMandates`), which is safe, and holds already standing keep their
--   `mandate_id` pointing at nothing — they are still captured and voided as before. Dropping
--   `ledger_refused_at` re-opens #514's gap (an unparked refused reserve placed after its hold
--   was voided), so drop it only with no parked `scope_holds` row in the outbox.
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 58;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   objects may stay instead of being dropped: the older binary never reads them.
SET LOCAL lock_timeout = '3s';

-- One mandate. Caps are 18-dp base units as TEXT, like every amount the control plane keeps.
-- A grant for a (user, scope, client) that already has a mandate stamps the old one
-- `replaced_at`: at most one row per triple is neither revoked nor replaced. `terms` is the
-- canonical rendering of what was granted, compared when the user's idempotency key repeats.
CREATE TABLE IF NOT EXISTS scope_mandates (
	id                UUID PRIMARY KEY,
	user_id           UUID NOT NULL REFERENCES users (id),
	scope             TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$' AND scope <> 'main'),
	client_id         TEXT NOT NULL CHECK (client_id ~ '^[a-z][a-z0-9_-]{0,63}$' AND client_id <> 'banking'),
	cap_per_op        TEXT NOT NULL CHECK (cap_per_op ~ '^[1-9][0-9]*$'),
	cap_24h           TEXT NOT NULL CHECK (cap_24h ~ '^[1-9][0-9]*$'),
	cap_outstanding   TEXT NOT NULL CHECK (cap_outstanding ~ '^[1-9][0-9]*$'),
	-- Set exactly when `operations` lists `book_fill`.
	cap_book_fill_24h TEXT CHECK (cap_book_fill_24h ~ '^[1-9][0-9]*$'),
	operations        TEXT[] NOT NULL CHECK (cardinality(operations) BETWEEN 1 AND 3 AND operations <@ ARRAY['reserve', 'charge', 'book_fill']),
	-- NULL: until revoked.
	expires_at        TIMESTAMPTZ,
	idempotency_key   TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
	terms             TEXT NOT NULL,
	created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
	revoked_at        TIMESTAMPTZ,
	replaced_at       TIMESTAMPTZ,
	UNIQUE (user_id, idempotency_key),
	CHECK ((cap_book_fill_24h IS NOT NULL) = ('book_fill' = ANY (operations)))
);
-- The one mandate a Reserve locks, and the guarantee that there is only one.
CREATE UNIQUE INDEX IF NOT EXISTS scope_mandates_current_idx ON scope_mandates (user_id, scope, client_id) WHERE revoked_at IS NULL AND replaced_at IS NULL;
-- ListMandates, newest first.
CREATE INDEX IF NOT EXISTS scope_mandates_user_idx ON scope_mandates (user_id, created_at DESC);

-- One capture of a hold into another user's compartment — the cash leg of a book fill — kept
-- with its time so the hold's mandate can cap a rolling 24 h of them. Written in the
-- capture's transaction; `version` is the capture's hold event.
CREATE TABLE IF NOT EXISTS scope_book_fills (
	hold_id    UUID NOT NULL REFERENCES scope_holds (id),
	version    BIGINT NOT NULL CHECK (version >= 2),
	mandate_id UUID NOT NULL REFERENCES scope_mandates (id),
	amount     TEXT NOT NULL CHECK (amount ~ '^[1-9][0-9]*$'),
	created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
	PRIMARY KEY (hold_id, version)
);
CREATE INDEX IF NOT EXISTS scope_book_fills_mandate_idx ON scope_book_fills (mandate_id, created_at);

-- Set when the relay parked the hold's reservation: the ledger never placed it, the row was
-- voided in the same transaction, and a later unpark of that reservation must not place it
-- after all (banking#514).
ALTER TABLE scope_holds ADD COLUMN IF NOT EXISTS ledger_refused_at TIMESTAMPTZ;
-- What a client draws on one compartment: its active holds and those placed in the last 24 h.
CREATE INDEX IF NOT EXISTS scope_holds_drawn_idx ON scope_holds (user_id, scope, client_id, created_at);
