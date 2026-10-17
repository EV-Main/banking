-- 0064: a scope service's request for a mandate (`scope.v1.MandateRequestService.CreateMandateRequest`)
-- — the terms the user is asked to grant, stored by banking so the user grants by the request's
-- id (`banking.v1.CompartmentService.GetMandateRequest/GrantMandate`) and not by limits a page
-- carries in a URL. Expand only: one new table. Numbered 0064 because 0063 is the payout lines index.
--
-- Every statement is IF NOT EXISTS, so re-applying after the rollback below, with the table
-- kept, does not fail on it.
--
-- REVERSIBILITY: DROP TABLE IF EXISTS scope_mandate_requests;
--   Dropping it loses only pending asks: every grant link already handed out reads NOT_FOUND
--   until the service asks again, and mandates already granted are untouched (they copy the
--   terms, they do not reference the request). Rolling the image back past this migration
--   also needs, as `postgres`, BEFORE the older image starts:
--   DELETE FROM _sqlx_migrations WHERE version = 64;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   table may stay instead of being dropped: the older binary never reads it.
SET LOCAL lock_timeout = '3s';

-- A request is not consumed by a grant: every grant is the user's own act behind a step-up,
-- and `expires_at` bounds how old the ask may be. An expired row reads as absent (by the
-- database's clock) and is never reaped. Caps are 18-dp base units as TEXT, like
-- `scope_mandates`, whose CHECKs these repeat.
CREATE TABLE IF NOT EXISTS scope_mandate_requests (
	id                  UUID PRIMARY KEY,
	scope               TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$' AND scope <> 'main'),
	client_id           TEXT NOT NULL CHECK (client_id ~ '^[a-z][a-z0-9_-]{0,63}$' AND client_id <> 'banking'),
	cap_per_op          TEXT NOT NULL CHECK (cap_per_op ~ '^[1-9][0-9]*$'),
	cap_24h             TEXT NOT NULL CHECK (cap_24h ~ '^[1-9][0-9]*$'),
	cap_outstanding     TEXT NOT NULL CHECK (cap_outstanding ~ '^[1-9][0-9]*$'),
	-- Set exactly when `operations` lists `book_fill`.
	cap_book_fill_24h   TEXT CHECK (cap_book_fill_24h ~ '^[1-9][0-9]*$'),
	operations          TEXT[] NOT NULL CHECK (cardinality(operations) BETWEEN 1 AND 3 AND operations <@ ARRAY['reserve', 'charge', 'book_fill']),
	-- The proposed end of the mandate itself; NULL: until revoked.
	mandate_expires_at  TIMESTAMPTZ,
	idempotency_key     TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
	-- The canonical rendering of the ask, compared when the client's key repeats.
	terms               TEXT NOT NULL,
	-- How long the ask can be granted.
	expires_at          TIMESTAMPTZ NOT NULL,
	created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
	UNIQUE (client_id, idempotency_key),
	CHECK ((cap_book_fill_24h IS NOT NULL) = ('book_fill' = ANY (operations)))
);
