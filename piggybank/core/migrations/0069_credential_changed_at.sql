-- 0069: when a user's sign-in credentials last changed, as far as the identity plane tells us
-- (wallet hardening PR0, WALLET-INPAGE-SPEC control (e): no withdrawals to NEW addresses for
-- 6 h after a password, email or passkey change). Expand only: one nullable column.
--
-- Written by the lifecycle bridge when the mirrored email address changes and when a
-- SESSIONS_REVOKED event raises the revoke floor. A passkey or password change that revokes
-- no session has no event at all and is NOT seen here (see the gap noted in the bridge).
-- NULL = never seen a change.
--
-- REVERSIBILITY: ALTER TABLE users DROP COLUMN IF EXISTS credential_changed_at;
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 69;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   column may stay instead of being dropped: the older binary never reads it.
SET LOCAL lock_timeout = '3s';

ALTER TABLE users ADD COLUMN IF NOT EXISTS credential_changed_at TIMESTAMPTZ;
