-- 0062: an idempotency key on self-service withdrawals (`WalletService.RequestWithdrawal`,
-- banking#530) — a retried POST from the withdraw page returns the first withdrawal instead
-- of opening a second. Expand only: one nullable column and one partial unique index.
-- Numbered 0062 because 0059-0061 belong to the sibling branches (A, B, C).
--
-- Old clients send no key: the column stays NULL and the partial index does not see the row,
-- so their behaviour is unchanged. Rows from before this migration are NULL too.
--
-- REVERSIBILITY: DROP INDEX IF EXISTS withdrawals_user_idempotency_key;
--   ALTER TABLE withdrawals DROP COLUMN IF EXISTS idempotency_key;
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 62;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   objects may stay instead of being dropped: the older binary never reads them.
SET LOCAL lock_timeout = '3s';

ALTER TABLE withdrawals ADD COLUMN IF NOT EXISTS idempotency_key TEXT
    CHECK (idempotency_key IS NULL OR length(idempotency_key) BETWEEN 1 AND 128);

-- Per user, not global: two users minting the same UUID-less key must not collide.
CREATE UNIQUE INDEX IF NOT EXISTS withdrawals_user_idempotency_key
    ON withdrawals (user_id, idempotency_key) WHERE idempotency_key IS NOT NULL;
