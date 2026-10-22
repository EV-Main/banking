-- 0070: the scope whose cabinet a queued notice links into (`EmitRequest.link_scope`,
-- concierge v0.20.0, banking#580). Expand only: one column with a constant default.
--
-- Safe while the previous image runs: it never names the column, so its inserts take the
-- default '' and its worker ignores what the new image stored. '' is what every notice
-- meant before the column existed: the platform cabinet (`CABINET_URL`). A constant
-- default makes ADD COLUMN a catalog-only change on Postgres 11+, so no row is rewritten.
--
-- REVERSIBILITY: ALTER TABLE scope_notification_outbox DROP COLUMN IF EXISTS link_scope;
--   Notices still queued then lose their scope and link into the platform cabinet.
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 70;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   column may stay instead of being dropped: the older binary never reads it.
SET LOCAL lock_timeout = '3s';

-- The shape concierge accepts (`[a-z][a-z0-9_]{0,31}`): a name it would refuse would fail
-- the emit attempt after attempt, so it is refused here, in the transaction of the fact.
ALTER TABLE scope_notification_outbox
    ADD COLUMN IF NOT EXISTS link_scope TEXT NOT NULL DEFAULT ''
        CONSTRAINT scope_notification_outbox_link_scope_check CHECK (link_scope = '' OR link_scope ~ '^[a-z][a-z0-9_]{0,31}$');
