-- 0067: who asked for a self-service withdrawal and from where (`AuditContext`, wallet
-- hardening PR0). Expand only: five nullable columns, nothing existing is touched.
--
-- The columns are the BFF's reading of the request, written once with the row and never
-- an authorization input: `origin` only selects a limit profile, which can only tighten.
-- Rows from before this migration, treasury payouts and requests from a caller that
-- predates the audit block are NULL.
--
-- REVERSIBILITY: ALTER TABLE withdrawals DROP COLUMN IF EXISTS origin, DROP COLUMN IF EXISTS
--   client_ip, DROP COLUMN IF EXISTS user_agent, DROP COLUMN IF EXISTS intent_id,
--   DROP COLUMN IF EXISTS stepup_method;
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 67;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   columns may stay instead of being dropped: the older binary never reads them.
SET LOCAL lock_timeout = '3s';

ALTER TABLE withdrawals
    ADD COLUMN IF NOT EXISTS origin        TEXT CHECK (origin IS NULL OR char_length(origin) <= 128),
    ADD COLUMN IF NOT EXISTS client_ip     TEXT CHECK (client_ip IS NULL OR char_length(client_ip) <= 64),
    ADD COLUMN IF NOT EXISTS user_agent    TEXT CHECK (user_agent IS NULL OR char_length(user_agent) <= 256),
    ADD COLUMN IF NOT EXISTS intent_id     TEXT CHECK (intent_id IS NULL OR char_length(intent_id) <= 64),
    ADD COLUMN IF NOT EXISTS stepup_method TEXT CHECK (stepup_method IS NULL OR char_length(stepup_method) <= 32);

-- The velocity window reads one user's recent rows by time under the claim lock.
CREATE INDEX IF NOT EXISTS withdrawals_user_recent_idx ON withdrawals (user_id, created_at DESC) WHERE source = 'user';
