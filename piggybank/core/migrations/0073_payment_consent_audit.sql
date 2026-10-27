-- 0073: a session consent is an executed wallet intent, audited like a withdrawal
-- (`AuditContext`). Expand only: three nullable columns beside the existing `client_ip` and
-- `user_agent`, nothing existing is touched. Pending seats, answers from before this migration
-- and answers from a caller that predates the audit block are NULL.
--
-- REVERSIBILITY: ALTER TABLE payment_session_consent DROP COLUMN IF EXISTS origin,
--   DROP COLUMN IF EXISTS intent_id, DROP COLUMN IF EXISTS stepup_method;
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 73;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   columns may stay instead of being dropped: the older binary never reads them.
SET LOCAL lock_timeout = '3s';

ALTER TABLE payment_session_consent
    ADD COLUMN IF NOT EXISTS origin        TEXT CHECK (origin IS NULL OR char_length(origin) <= 128),
    ADD COLUMN IF NOT EXISTS intent_id     TEXT CHECK (intent_id IS NULL OR char_length(intent_id) <= 64),
    ADD COLUMN IF NOT EXISTS stepup_method TEXT CHECK (stepup_method IS NULL OR char_length(stepup_method) <= 32);
