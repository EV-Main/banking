-- 0053: `scopes` becomes a projection of concierge's ScopeEvents stream. 0052 seeded the
-- two scopes that existed by hand; from here concierge's SCOPE_CHANGED keeps every row's
-- state, and registers the scopes 0052 could not know about. Expand only.
--
-- `version` is the scope's own version on concierge (`Scope.version`, the event's
-- `sequence`): an event applies only when it is newer than the row, so a redelivered or
-- reordered snapshot can never roll a FROZEN back to ACTIVE. The seeded rows start at 0,
-- which every version concierge hands out exceeds.
--
-- REVERSIBILITY: DROP TABLE scope_events_parked, scope_events_cursor;
--   ALTER TABLE scopes DROP COLUMN version;
--   Safe at any time: the rows the stream registered stay, with the state they last had.
--   Re-applying afterwards replays the stream from position 0, which is idempotent only
--   because `version` comes back at 0 for every row and the stream's snapshots are whole.
SET LOCAL lock_timeout = '3s';

ALTER TABLE scopes ADD COLUMN version BIGINT NOT NULL DEFAULT 0 CHECK (version >= 0);

-- The consumer's cursor on concierge's scope stream: one row, as `bridge_cursor` is for
-- the lifecycle stream. Advanced only after a whole pulled batch is applied or parked.
CREATE TABLE scope_events_cursor (
	id         BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
	position   BIGINT NOT NULL DEFAULT 0 CHECK (position >= 0),
	updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO scope_events_cursor (id) VALUES (TRUE);

-- Events this build could not read, kept by their stream position so a later build can
-- re-pull exactly that one (`after = position - 1, limit = 1`) and apply it. The cursor
-- moves past them: unlike the lifecycle stream, a scope event held at the head would stop
-- every other scope's freeze from arriving. Only what locates the event is kept, never
-- its decode, which already lost what this build could not name.
CREATE TABLE scope_events_parked (
	position       BIGINT PRIMARY KEY,
	event_id       TEXT NOT NULL,
	scope          TEXT NOT NULL,
	kind           INTEGER NOT NULL,
	reason         TEXT NOT NULL,
	parked_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
	last_replay_at TIMESTAMPTZ
);
