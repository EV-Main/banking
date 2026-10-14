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
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 53;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version
--   (as docs/RUNBOOK-withdrawals.md does for the signer's 0009). The tables may stay
--   instead of being dropped: the older binary never reads them.
--   Re-applying afterwards replays the stream from position 0. The snapshots are whole and
--   `version` comes back at 0, so every scope ends where concierge has it — except that a
--   scope already `closed` here sees its earlier ACTIVE snapshots as a reopen: each one logs
--   a false "concierge reopened a CLOSED scope" `error!`, and the row stays closed.
SET LOCAL lock_timeout = '3s';

ALTER TABLE scopes ADD COLUMN version BIGINT NOT NULL DEFAULT 0 CHECK (version >= 0);

-- The consumer's cursor on concierge's scope stream: one row, as `bridge_cursor` is for
-- the lifecycle stream. Advanced only after a whole pulled batch is applied or parked.
--   `last_event_id`         the event at `position`. concierge never deletes stream rows,
--                           so the same position answering another id (or nothing) means
--                           the stream was rewound or replaced; the consumer checks it at
--                           start and every few minutes and stops following on a mismatch.
--                           NULL only while `position` is 0.
--   `last_pulled_at`        the last pull concierge answered with a well-formed page, empty
--                           included. Reconciliation alerts when it falls behind.
--   `last_unimplemented_at` the last pull answered UNIMPLEMENTED — a concierge that predates
--                           ScopeEvents. A stall explained by it is a warning, not an alert.
CREATE TABLE scope_events_cursor (
	id                    BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
	position              BIGINT NOT NULL DEFAULT 0 CHECK (position >= 0),
	last_event_id         TEXT,
	last_pulled_at        TIMESTAMPTZ,
	last_unimplemented_at TIMESTAMPTZ,
	updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO scope_events_cursor (id) VALUES (TRUE);

-- Events this build could not read, kept by their stream position so a later build can
-- re-pull exactly that one (`after = position - 1, limit = 1`) and apply it. The cursor
-- moves past them: unlike the lifecycle stream, a scope event held at the head would stop
-- every other scope's freeze from arriving. Only what locates the event is kept, never
-- its decode, which already lost what this build could not name.
CREATE TABLE scope_events_parked (
	position       BIGINT PRIMARY KEY CHECK (position > 0),
	event_id       TEXT NOT NULL,
	scope          TEXT NOT NULL,
	kind           INTEGER NOT NULL,
	reason         TEXT NOT NULL,
	parked_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
	last_replay_at TIMESTAMPTZ
);
